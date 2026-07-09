# Runtime Diagnostics

Ferrite has an opt-in runtime diagnostics path for startup and UI frame timing.
It is disabled by default and writes to a temp-file trace when enabled:

```powershell
$env:FERRITE_DIAG = "1"
cargo run -- path\to\file.md
```

On Windows release builds Ferrite hides the console, so the trace file is the
source of truth:

```text
%TEMP%\ferrite_startup_trace.log
```

A helper pointer file is also written:

```text
%TEMP%\ferrite_DIAG_LOG_HERE.txt
```

## What It Records

Startup milestones:

- `main() entered`
- CLI parse and single-instance acquisition
- settings and locale
- icon and native options setup
- `eframe::run_native`
- `FerriteApp::new`
- initial path handling

Frame milestones:

- first UI frame
- first ten frames
- every 60th frame marker
- slow frames over the configured threshold
- every 300 frames: sample count, p50, p95, p99, max, frames over 16 ms,
  frames over 32 ms, and frames over the slow-frame threshold

## Interpretation

Use this as a measurement scaffold, not as a benchmark suite. The next
measurement phase should capture logs for:

- cold launch to first frame
- small Markdown open
- large Markdown open
- typing for 30 seconds
- rendered Markdown scroll
- worker or plugin stress scenarios when those paths are enabled

Do not use the diagnostic path to justify a GUI framework rewrite by itself.
Framework escalation still requires repeatable measurements after cache and
worker fixes have been applied.
