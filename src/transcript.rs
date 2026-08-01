//! Transcript discovery, tailing, and per-`--output-format` output.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, error, trace};

use crate::args::OutputFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub text: String,
    pub complete: bool,
    pub elapsed: Duration,
}

pub const fn completed_within(observation: &Observation, deadline: Duration) -> bool {
    observation.complete && crate::lifecycle::within_deadline(observation.elapsed, deadline)
}

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Walk `~/.claude/projects/*/` looking for `<session_id>.jsonl`. UUIDs
/// are globally unique so this sidesteps reimplementing Claude Code's
/// project-path encoding.
fn find(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    let projects = Path::new(&home).join(".claude/projects");
    let target = format!("{session_id}.jsonl");
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry.path().join(&target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Extract the assistant's visible text from a nested Anthropic message
/// envelope: `message.content` is either a string or an array of
/// `{type, text}` blocks; we concatenate the `text` of every block.
fn extract_message_text(json: &serde_json::Value) -> String {
    let Some(content) = json.get("message").and_then(|m| m.get("content")) else {
        return String::new();
    };
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(serde_json::Value::as_str) {
                out.push_str(t);
            }
        }
        return out;
    }
    content.as_str().unwrap_or("").to_owned()
}

pub fn observe_line(line: &str, elapsed: Duration) -> Option<Observation> {
    let json = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if json.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    let message = json.get("message")?;
    if message.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return None;
    }
    let text = extract_message_text(&json);
    if text.trim().is_empty() {
        return None;
    }
    let complete = message
        .get("stop_reason")
        .and_then(serde_json::Value::as_str)
        == Some("end_turn");
    Some(Observation {
        text,
        complete,
        elapsed,
    })
}

#[cfg(test)]
mod deadline_tests {
    use super::{Observation, completed_within};
    use std::time::Duration;

    fn completed_at(elapsed_ms: u64) -> Observation {
        Observation {
            text: "review".to_owned(),
            complete: true,
            elapsed: Duration::from_millis(elapsed_ms),
        }
    }

    #[test]
    fn completed_observation_is_accepted_below_and_at_but_not_above_deadline() {
        let deadline = Duration::from_secs(1);
        assert!(completed_within(&completed_at(999), deadline));
        assert!(completed_within(&completed_at(1_000), deadline));
        assert!(!completed_within(&completed_at(1_001), deadline));
    }
}

/// Process a single jsonl line against the requested output format. Two
/// special shapes are recognised for `--output-format json`/`text`:
///   * top-level `"type":"result"` — the legacy summary line carrying a
///     `result` string field.
///   * nested `message.type:"message"` — the Anthropic assistant
///     response, wrapped under a `"type":"assistant"` transcript entry.
///
/// All other lines are silently skipped for `json`/`text`. For
/// `stream-json` every line is forwarded verbatim.
fn handle_line(line: &str, format: OutputFormat) -> io::Result<()> {
    if matches!(format, OutputFormat::StreamJson) {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{line}")?;
        stdout.flush()?;
        return Ok(());
    }

    let json = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(e) => {
            debug!("parse failed: {e}");
            return Ok(());
        }
    };

    let top_type = json.get("type").and_then(serde_json::Value::as_str);
    let nested_type = json
        .get("message")
        .and_then(|m| m.get("type"))
        .and_then(serde_json::Value::as_str);
    debug!("parsed type={top_type:?} message.type={nested_type:?}");

    let is_result = top_type == Some("result");
    let is_message = nested_type == Some("message");
    if !is_result && !is_message {
        return Ok(());
    }

    // Skip assistant turns with no visible text (thinking-only,
    // tool-use-only). Result lines always print.
    let text = if is_result {
        json.get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else {
        extract_message_text(&json)
    };
    if !is_result && text.is_empty() {
        return Ok(());
    }

    match format {
        OutputFormat::StreamJson => {} // handled above
        OutputFormat::Json => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{line}")?;
            stdout.flush()?;
        }
        OutputFormat::Text => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{text}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn tail(
    path: &Path,
    format: OutputFormat,
    stop: &AtomicBool,
    observations: Option<&Sender<Observation>>,
    start: Option<Instant>,
) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut pending = String::new();
    let mut line_no: u64 = 0;
    // After `stop` is signalled (child exited), do one last drain of any
    // lines that landed in the race between the child's final write and
    // our next poll.
    let mut final_pass = false;
    loop {
        let n = reader.read_line(&mut pending)?;
        if n == 0 || !pending.ends_with('\n') {
            if final_pass {
                debug!("transcript drain complete");
                return Ok(());
            }
            if stop.load(Ordering::Relaxed) {
                debug!("stop signalled, draining transcript one last time");
                final_pass = true;
                continue;
            }
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        line_no = line_no.saturating_add(1);
        let line = pending.trim_end_matches('\n').to_owned();
        pending.clear();
        let preview: String = line.chars().take(200).collect();
        debug!("line[{line_no}] ({} bytes): {preview}", line.len());
        if let (Some(sender), Some(started)) = (observations, start)
            && let Some(observation) = observe_line(&line, started.elapsed())
        {
            drop(sender.send(observation));
        }
        if observations.is_none() {
            handle_line(&line, format)?;
        }
    }
}

/// Top-level transcript thread: poll for the jsonl, then tail it.
pub fn run(session_id: &str, format: OutputFormat, stop: &AtomicBool) {
    debug!("phase=1 polling for {session_id}.jsonl");
    let path = loop {
        if let Some(p) = find(session_id) {
            break p;
        }
        if stop.load(Ordering::Relaxed) {
            debug!("phase=1 stopped (no transcript found)");
            return;
        }
        thread::sleep(POLL_INTERVAL);
    };
    trace!("tailing {}", path.display());
    if let Err(e) = tail(&path, format, stop, None, None) {
        error!("tail error: {e}");
    }
}

pub fn run_observed(
    session_id: &str,
    format: OutputFormat,
    stop: &AtomicBool,
    observations: &Sender<Observation>,
    start: Instant,
) {
    let path = loop {
        if let Some(path) = find(session_id) {
            break path;
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    };
    if let Err(error) = tail(&path, format, stop, Some(observations), Some(start)) {
        error!("tail error: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{Observation, observe_line};

    #[test]
    fn terminal_end_turn_text_is_complete() {
        let line = r#"{"type":"assistant","message":{"type":"message","stop_reason":"end_turn","content":[{"type":"text","text":"Review"}]}}"#;
        assert_eq!(
            observe_line(line, std::time::Duration::from_secs(1)),
            Some(Observation {
                text: "Review".to_owned(),
                complete: true,
                elapsed: std::time::Duration::from_secs(1),
            })
        );
    }

    #[test]
    fn tool_use_turn_with_text_is_observed_but_not_complete() {
        let line = r#"{"type":"assistant","message":{"type":"message","stop_reason":"tool_use","content":[{"type":"text","text":"Checking"},{"type":"tool_use","name":"Read"}]}}"#;
        assert_eq!(
            observe_line(line, std::time::Duration::from_secs(1)),
            Some(Observation {
                text: "Checking".to_owned(),
                complete: false,
                elapsed: std::time::Duration::from_secs(1),
            })
        );
    }

    #[test]
    fn thinking_tool_results_empty_text_and_malformed_lines_are_not_observed() {
        let elapsed = std::time::Duration::ZERO;
        assert!(observe_line(r#"{"type":"assistant","message":{"type":"message","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"private"}]}}"#, elapsed).is_none());
        assert!(observe_line(r#"{"type":"tool_result","content":"private"}"#, elapsed).is_none());
        assert!(observe_line(r#"{"type":"assistant","message":{"type":"message","stop_reason":"end_turn","content":[{"type":"text","text":"  "}]}}"#, elapsed).is_none());
        assert!(observe_line("not json", elapsed).is_none());
    }
}
