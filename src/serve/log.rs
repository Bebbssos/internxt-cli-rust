//! Shared logging for the serve/mount backends, gated by a global `--verbose`
//! flag so the per-operation request traces (`[OPEN] …`, `[READ] …`, …) that
//! every backend emits don't spam stderr by default.
//!
//! - `trace` — per-op request logging; printed only when `--verbose` is set.
//! - `warn`  — warnings/errors and other low-frequency, always-relevant lines
//!   (upload/download failures, etc.); always printed.
//!
//! The flag is a process-global `AtomicBool` set once by `serve`/`mount` before
//! any backend starts, so the free `log`/`vlog` helpers in each backend can
//! consult it without threading a config through every call.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Enable/disable verbose per-op tracing (called once from `serve`/`mount`).
pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

/// Whether verbose tracing is on.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Verbose-only trace line (per-op request logging). No-op unless `--verbose`.
pub fn trace(msg: &str) {
    if verbose() {
        eprintln!("{msg}");
    }
}

/// Always-printed warning/error line.
pub fn warn(msg: &str) {
    eprintln!("{msg}");
}
