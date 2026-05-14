//! Transcript discovery, tailing, and per-`--output-format` output.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use log::{debug, error, trace};

use crate::args::OutputFormat;

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

fn tail(path: &Path, format: OutputFormat, stop: &AtomicBool) -> io::Result<()> {
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
        handle_line(&line, format)?;
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
    if let Err(e) = tail(&path, format, stop) {
        error!("tail error: {e}");
    }
}
