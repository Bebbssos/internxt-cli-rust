//! Global output mode. Mirrors oclif's `--json`: in JSON mode every command
//! prints a single JSON object on success (and on error) and suppresses the
//! human-readable status/progress chatter.

use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Adapts an indicatif [`ProgressBar`] to core's [`internxt_core::ProgressSink`],
/// so the streaming transfer primitives can drive the terminal bar without
/// depending on indicatif.
struct BarSink(ProgressBar);

impl internxt_core::ProgressSink for BarSink {
    fn inc(&self, bytes: u64) {
        self.0.inc(bytes);
    }
}

/// Wrap a progress bar as a shared [`internxt_core::ProgressSink`] to hand to a
/// core transfer. Clones the bar handle (cheap; shares the underlying state).
pub fn bar_sink(pb: &ProgressBar) -> Arc<dyn internxt_core::ProgressSink> {
    Arc::new(BarSink(pb.clone()))
}

static JSON: AtomicBool = AtomicBool::new(false);
static NON_INTERACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_json(v: bool) {
    JSON.store(v, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// Non-interactive mode (`-x`/`--non-interactive`): the CLI never prompts for
/// input and errors out instead when a required value is missing.
pub fn set_non_interactive(v: bool) {
    NON_INTERACTIVE.store(v, Ordering::Relaxed);
}

pub fn is_non_interactive() -> bool {
    NON_INTERACTIVE.load(Ordering::Relaxed)
}

/// Terminal success output: the JSON object in JSON mode, else the human line.
pub fn emit(human: &str, json: Value) {
    if is_json() {
        println!("{}", serde_json::to_string(&json).unwrap_or_default());
    } else {
        println!("{human}");
    }
}

/// Transient status / progress line — suppressed entirely in JSON mode.
pub fn status(msg: &str) {
    if !is_json() {
        println!("{msg}");
    }
}

/// Status line on stderr — for commands whose stdout carries binary data
/// (e.g. `download-file --stdout`). Suppressed in JSON mode like `status`.
pub fn status_err(msg: &str) {
    if !is_json() {
        eprintln!("{msg}");
    }
}

/// A byte-oriented progress bar for transfers, drawn on stderr (so it never
/// pollutes piped stdout data). Returns a hidden bar in JSON mode. `verb` is the
/// leading label, e.g. "Uploading" / "Downloading".
pub fn progress_bar(total: u64, verb: &str) -> ProgressBar {
    if is_json() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{bar:30.cyan/blue}] {percent:>3}% {bytes}/{total_bytes} ({binary_bytes_per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    pb.set_message(verb.to_string());
    pb
}

/// Terminal error output as a JSON object (used by main's error handler).
pub fn emit_error(message: &str) {
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({ "success": false, "message": message }))
            .unwrap_or_default()
    );
}
