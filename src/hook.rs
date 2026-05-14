//! The Stop-hook plumbing: a sentinel file claude `touch`es when its turn
//! ends, plus the `--settings` JSON that wires it in. Used by
//! [`crate::inject`] as a definitive "claude is done" signal.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Sentinel file the child's Stop hook will `touch` when claude finishes
/// a turn. We use the session UUID in the filename so concurrent runs
/// don't collide.
pub fn sentinel_path(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-pee-{session_id}.done"))
}

/// POSIX shell single-quoting: wrap the value in single quotes and
/// escape any internal single quotes via the `'\''` idiom.
fn shell_single_quote(s: &str) -> String {
    let escaped = s.replace('\'', r"'\''");
    format!("'{escaped}'")
}

/// Build the `--settings` JSON that injects a `Stop` hook into claude's
/// configuration. The hook runs `touch <sentinel>` when the agent
/// finishes a turn, giving claude-pee a definitive "done" signal
/// independent of any screen-quiescence heuristic.
pub fn stop_hook_settings(sentinel: &Path) -> String {
    let path = sentinel.to_string_lossy();
    let command = format!("touch {}", shell_single_quote(&path));
    let settings = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": command
                }]
            }]
        }
    });
    settings.to_string()
}

/// RAII cleanup: remove the sentinel file on drop. Best-effort — silently
/// ignores errors (e.g. the file never appeared because claude exited
/// without firing the hook).
pub struct SentinelGuard {
    path: PathBuf,
}

impl SentinelGuard {
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SentinelGuard {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

/// Block until `path` exists or `stop` is signalled. Returns `true` if
/// the file appeared, `false` if we bailed because the child exited.
pub fn wait_for_file(path: &Path, stop: &AtomicBool) -> io::Result<bool> {
    loop {
        if path.try_exists()? {
            return Ok(true);
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        thread::sleep(POLL_INTERVAL);
    }
}
