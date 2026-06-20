//! Global output mode. Mirrors oclif's `--json`: in JSON mode every command
//! prints a single JSON object on success (and on error) and suppresses the
//! human-readable status/progress chatter.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

static JSON: AtomicBool = AtomicBool::new(false);

pub fn set_json(v: bool) {
    JSON.store(v, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    JSON.load(Ordering::Relaxed)
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

/// Terminal error output as a JSON object (used by main's error handler).
pub fn emit_error(message: &str) {
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({ "success": false, "message": message }))
            .unwrap_or_default()
    );
}
