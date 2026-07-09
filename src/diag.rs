//! Optional startup / hot-path diagnostics.
//!
//! Enable with environment variable `FERRITE_DIAG=1` (also accepts `true` / `yes`).
//!
//! **Release builds hide the Windows console**, so when diagnosing hangs use the
//! trace file: `%TEMP%\ferrite_startup_trace.log` (see [`trace_path`]).

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const UPDATE_SUMMARY_INTERVAL: u64 = 300;

thread_local! {
    static UPDATE: Cell<u64> = const { Cell::new(0) };
    static UPDATE_START: Cell<Option<Instant>> = const { Cell::new(None) };
    static UPDATE_METRICS: RefCell<UpdateMetrics> = RefCell::new(UpdateMetrics::default());
}

static ENABLED: OnceLock<bool> = OnceLock::new();
static TRACE_PATH: OnceLock<PathBuf> = OnceLock::new();
static START_INSTANT: OnceLock<Instant> = OnceLock::new();

#[derive(Default)]
struct UpdateMetrics {
    samples_ms: Vec<f64>,
    max_ms: f64,
    over_16ms: u64,
    over_32ms: u64,
    over_threshold: u64,
}

impl UpdateMetrics {
    fn reset(&mut self) {
        self.samples_ms.clear();
        self.max_ms = 0.0;
        self.over_16ms = 0;
        self.over_32ms = 0;
        self.over_threshold = 0;
    }
}

/// Whether `FERRITE_DIAG` is enabled for this process.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("FERRITE_DIAG")
            .ok()
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
    })
}

/// Path to the startup trace log (written when `FERRITE_DIAG` is on).
pub fn trace_path() -> &'static PathBuf {
    TRACE_PATH.get_or_init(|| std::env::temp_dir().join("ferrite_startup_trace.log"))
}

/// Append a timestamped line to the trace file and mirror to `log::warn!`.
/// Safe to call before `env_logger` is initialized.
pub fn trace(step: &str) {
    if !enabled() {
        return;
    }
    let path = trace_path();
    let line = format!(
        "{:>7.3}s  {}",
        START_INSTANT
            .get_or_init(Instant::now)
            .elapsed()
            .as_secs_f64(),
        step
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
    // May run before env_logger::init — file is the source of truth.
    let _ = log::warn!("[diag] {line}");
}

/// Reset trace file at process start (call once from `main`).
pub fn trace_reset() {
    if !enabled() {
        return;
    }
    let _ = START_INSTANT.set(Instant::now());
    let path = trace_path();
    let header = format!("Ferrite startup trace — {}\n", chrono_lite_timestamp());
    let _ = std::fs::write(path, header);
    // Also drop a pointer file users can find easily
    let pointer = std::env::temp_dir().join("ferrite_DIAG_LOG_HERE.txt");
    let _ = std::fs::write(
        &pointer,
        format!(
            "Open this log while diagnosing Ferrite:\n{}\n",
            path.display()
        ),
    );
}

fn chrono_lite_timestamp() -> String {
    // Avoid adding chrono dependency — wall clock via humantime-style from std only
    format!("{:?}", std::time::SystemTime::now())
}

/// Start one `FerriteApp::update` diagnostics sample.
pub fn update_start() -> u64 {
    UPDATE_START.with(|c| c.set(Some(Instant::now())));
    let n = UPDATE.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    });
    if enabled() && n == 1 {
        trace("first FerriteApp::update started");
    } else if enabled() && n <= 10 {
        trace(&format!("FerriteApp::update #{n} started"));
    } else if enabled() && n % 60 == 0 {
        trace(&format!("FerriteApp::update #{n}"));
    }
    n
}

/// Finish the current `FerriteApp::update` sample.
pub fn update_end(threshold_ms: u64) {
    if !enabled() {
        return;
    }
    let Some(start) = UPDATE_START.with(|c| c.get()) else {
        return;
    };
    let elapsed = start.elapsed();
    let update = UPDATE.with(|c| c.get());
    record_update_metrics(update, elapsed, threshold_ms);
    if update <= 10 {
        trace(&format!(
            "FerriteApp::update #{} finished in {:.0}ms",
            update,
            elapsed.as_secs_f64() * 1000.0
        ));
    } else if elapsed >= Duration::from_millis(threshold_ms) {
        trace(&format!(
            "FerriteApp::update #{} took {:.0}ms (SLOW)",
            update,
            elapsed.as_secs_f64() * 1000.0
        ));
    }
}

/// Record a named checkpoint inside the current update call.
pub fn update_checkpoint(label: &'static str) {
    if !enabled() {
        return;
    }
    let update = UPDATE.with(|c| c.get());
    if update > 0 && update <= 10 {
        trace(&format!(
            "FerriteApp::update #{} checkpoint: {}",
            update, label
        ));
    }
}

fn record_update_metrics(update: u64, elapsed: Duration, threshold_ms: u64) {
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    UPDATE_METRICS.with(|cell| {
        let mut metrics = cell.borrow_mut();
        metrics.samples_ms.push(elapsed_ms);
        metrics.max_ms = metrics.max_ms.max(elapsed_ms);
        if elapsed_ms >= 16.0 {
            metrics.over_16ms += 1;
        }
        if elapsed_ms >= 32.0 {
            metrics.over_32ms += 1;
        }
        if elapsed_ms >= threshold_ms as f64 {
            metrics.over_threshold += 1;
        }

        if update > 0 && update % UPDATE_SUMMARY_INTERVAL == 0 && !metrics.samples_ms.is_empty() {
            let mut samples = metrics.samples_ms.clone();
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            trace(&format!(
                "FerriteApp::update summary through update #{}: samples={}, p50={:.1}ms, p95={:.1}ms, p99={:.1}ms, max={:.1}ms, updates>=16ms={}, updates>=32ms={}, updates>=threshold({}ms)={}",
                update,
                samples.len(),
                percentile(&samples, 0.50),
                percentile(&samples, 0.95),
                percentile(&samples, 0.99),
                metrics.max_ms,
                metrics.over_16ms,
                metrics.over_32ms,
                threshold_ms,
                metrics.over_threshold
            ));
            metrics.reset();
        }
    });
}

fn percentile(sorted_samples: &[f64], quantile: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let clamped = quantile.clamp(0.0, 1.0);
    let idx = ((sorted_samples.len() - 1) as f64 * clamped).round() as usize;
    sorted_samples[idx]
}

/// Log when a scoped operation exceeds `threshold`.
pub struct SlowScope {
    label: &'static str,
    start: Instant,
    threshold: Duration,
}

impl SlowScope {
    pub fn new(label: &'static str, threshold_ms: u64) -> Self {
        Self {
            label,
            start: Instant::now(),
            threshold: Duration::from_millis(threshold_ms),
        }
    }
}

impl Drop for SlowScope {
    fn drop(&mut self) {
        if !enabled() {
            return;
        }
        let elapsed = self.start.elapsed();
        if elapsed >= self.threshold {
            trace(&format!(
                "slow {} {:.0}ms (update #{})",
                self.label,
                elapsed.as_secs_f64() * 1000.0,
                UPDATE.with(|c| c.get())
            ));
        }
    }
}

#[macro_export]
macro_rules! diag_slow {
    ($label:expr, $threshold_ms:expr) => {
        let _diag_scope = $crate::diag::SlowScope::new($label, $threshold_ms);
    };
}

/// One-shot event (deduped by `key` per process).
pub fn event_once(key: &'static str, message: impl AsRef<str>) {
    if !enabled() {
        return;
    }
    static SEEN: OnceLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
        OnceLock::new();
    let set = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = set.lock().unwrap_or_else(|e| e.into_inner());
    if guard.insert(key) {
        trace(&format!("{key}: {}", message.as_ref()));
    }
}

/// Repeatable event.
pub fn event(key: &'static str, message: impl AsRef<str>) {
    if !enabled() {
        return;
    }
    trace(&format!("{key}: {}", message.as_ref()));
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_clamps_and_selects_sorted_samples() {
        let samples = [1.0, 2.0, 4.0, 8.0, 16.0];

        assert_eq!(percentile(&samples, -1.0), 1.0);
        assert_eq!(percentile(&samples, 0.50), 4.0);
        assert_eq!(percentile(&samples, 0.95), 16.0);
        assert_eq!(percentile(&samples, 2.0), 16.0);
    }

    #[test]
    fn percentile_empty_samples_returns_zero() {
        assert_eq!(percentile(&[], 0.95), 0.0);
    }
}
