//! Runtime configuration assembled once from CLI args + env vars +
//! per-run state (UUID, sentinel path, hook settings JSON).

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use uuid::Uuid;

use crate::args::{OutputFormat, ParsedArgs};
use crate::hook;

const DEFAULT_QUIESCE_MS: u64 = 500;

#[derive(Debug)]
pub struct SecondOpinionConfig {
    pub run_id: String,
    pub lifecycle_path: PathBuf,
    pub inference: Duration,
    pub normal_cleanup: Duration,
    pub forced_cleanup: Duration,
}

/// Read a non-negative integer env var into a `Duration`. Returns
/// `Duration::ZERO` if unset or unparseable.
fn read_ms(name: &str) -> Duration {
    std::env::var_os(name)
        .and_then(|v| v.to_string_lossy().parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_millis)
}

/// Quiescence threshold. Default `DEFAULT_QUIESCE_MS`; override with
/// `CLAUDE_PEE_QUIESCE_MS=<n>` (must be > 0). Quiescence detection is
/// always on — there is no opt-out.
fn quiesce() -> Duration {
    let ms = std::env::var_os("CLAUDE_PEE_QUIESCE_MS")
        .and_then(|v| v.to_string_lossy().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_QUIESCE_MS);
    Duration::from_millis(ms)
}

pub struct Config {
    pub session_id: String,
    pub forwarded: Vec<OsString>,
    pub inject_payload: Option<String>,
    pub output_format: OutputFormat,
    pub char_delay: Duration,
    pub quiesce: Duration,
    pub sentinel: PathBuf,
    pub settings_json: String,
    pub second_opinion: Option<SecondOpinionConfig>,
}

impl Config {
    /// Resolve everything: mint a session UUID, derive the sentinel path
    /// and the `--settings` JSON, pick up the timing knobs from env.
    pub fn build(parsed: ParsedArgs) -> Self {
        let second_opinion = match (
            parsed.run_id,
            parsed.lifecycle_path,
            parsed.inference_seconds,
        ) {
            (Some(run_id), Some(path), Some(inference_seconds)) => Some(SecondOpinionConfig {
                run_id,
                lifecycle_path: PathBuf::from(path),
                inference: Duration::from_secs(inference_seconds),
                normal_cleanup: Duration::from_secs(parsed.normal_cleanup_seconds),
                forced_cleanup: Duration::from_secs(parsed.forced_cleanup_seconds),
            }),
            _ => None,
        };
        let session_id = Uuid::new_v4().to_string();
        let sentinel = hook::sentinel_path(&session_id);
        let settings_json = hook::stop_hook_settings(&sentinel);
        Self {
            session_id,
            forwarded: parsed.forwarded,
            inject_payload: parsed.inject,
            output_format: parsed.output_format,
            char_delay: read_ms("CLAUDE_PEE_INJECT_CHAR_DELAY_MS"),
            quiesce: quiesce(),
            sentinel,
            settings_json,
            second_opinion,
        }
    }
}
