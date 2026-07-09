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

const FRAME_SUMMARY_INTERVAL: u64 = 300;

thread_local! {
    static FRAME: Cell<u64> = const { Cell::new(0) };
    static FRAME_START: Cell<Option<Instant>> = const { Cell::new(None) };
    static FRAME_METRICS: RefCell<FrameMetrics> = RefCell::new(FrameMetrics::default());
}

static ENABLED: OnceLock<bool> = OnceLock::new();
static TRACE_PATH: OnceLock<PathBuf> = OnceLock::new();
static START_INSTANT: OnceLock<Instant> = OnceLock::new();

#[derive(Default)]
struct FrameMetrics {
    samples_ms: Vec<f64>,
    max_ms: f64,
    over_16ms: u64,
    over_32ms: u64,
    over_threshold: u64,
}

impl FrameMetrics {
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

/// Advance the per-frame counter (call once per `App::update`).
pub fn next_frame() -> u64 {
    FRAME_START.with(|c| c.set(Some(Instant::now())));
    let n = FRAME.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    });
    if enabled() && n == 1 {
        trace("first UI frame started");
    } else if enabled() && n <= 10 {
        trace(&format!("UI frame {n} started"));
    } else if enabled() && n % 60 == 0 {
        trace(&format!("UI frame {n}"));
    }
    n
}

/// Log when the previous frame's `update()` exceeded `threshold_ms`.
pub fn frame_end(threshold_ms: u64) {
    if !enabled() {
        return;
    }
    let Some(start) = FRAME_START.with(|c| c.get()) else {
        return;
    };
    let elapsed = start.elapsed();
    let frame = FRAME.with(|c| c.get());
    record_frame_metrics(frame, elapsed, threshold_ms);
    if elapsed >= Duration::from_millis(threshold_ms) {
        trace(&format!(
            "UI frame {} took {:.0}ms (SLOW)",
            frame,
            elapsed.as_secs_f64() * 1000.0
        ));
    }
}

fn record_frame_metrics(frame: u64, elapsed: Duration, threshold_ms: u64) {
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    FRAME_METRICS.with(|cell| {
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

        if frame > 0 && frame % FRAME_SUMMARY_INTERVAL == 0 && !metrics.samples_ms.is_empty() {
            let mut samples = metrics.samples_ms.clone();
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            trace(&format!(
                "frame summary through frame {}: samples={}, p50={:.1}ms, p95={:.1}ms, p99={:.1}ms, max={:.1}ms, >=16ms={}, >=32ms={}, >=threshold({}ms)={}",
                frame,
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
                "slow {} {:.0}ms (frame {})",
                self.label,
                elapsed.as_secs_f64() * 1000.0,
                FRAME.with(|c| c.get())
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

/// Repeatable event (rate-limited to avoid log spam).
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
