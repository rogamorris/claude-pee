//! Production-shaped lifecycle tests using a controllable fake Claude child.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
session=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--session-id" ]; then session="$2"; shift 2; continue; fi
  shift
done
project="$HOME/.claude/projects/fake"
mkdir -p "$project"
transcript="$project/$session.jsonl"
printf 'ready\n'

if [ "${FAKE_MODE:-normal}" = "never-quiescent" ]; then
  while :; do printf 'working %s\n' "$(date +%s%N)"; sleep 0.02; done
fi

IFS= read -r prompt || true
if [ "${FAKE_MODE:-normal}" = "no-output" ]; then sleep 30; fi
if [ "${FAKE_MODE:-normal}" = "exit-early" ]; then exit 7; fi

printf '%s\n' '{"type":"assistant","message":{"type":"message","stop_reason":"tool_use","content":[{"type":"text","text":"Checking"},{"type":"tool_use","name":"Read"}]}}' >> "$transcript"
printf '%s\n' '{"type":"assistant","message":{"type":"message","stop_reason":"end_turn","content":[{"type":"text","text":"Completed review"}]}}' >> "$transcript"

if [ "${FAKE_DESCENDANT:-0}" = "1" ]; then
  python3 -c 'import os, signal, sys, time; os.setsid(); signal.signal(signal.SIGHUP, signal.SIG_IGN); open(sys.argv[1], "w").write(str(os.getpid())); time.sleep(30)' "$FAKE_DESCENDANT_PID_FILE" &
  while [ ! -s "$FAKE_DESCENDANT_PID_FILE" ]; do sleep 0.01; done
fi
if [ "${FAKE_MODE:-normal}" = "exit-after-output" ]; then exit 0; fi
if [ "${FAKE_MODE:-normal}" = "normal" ]; then
  touch "$TMPDIR/claude-pee-$session.done"
  IFS= read -r exit_command || true
  exit 0
fi
if [ "${FAKE_MODE:-normal}" = "ignored-exit" ]; then
  touch "$TMPDIR/claude-pee-$session.done"
  IFS= read -r exit_command || true
fi
sleep 30
"#;

const FAKE_PS: &str = r#"#!/bin/sh
root="$CLAUDE_PEE_TRACK_ROOT_PID"
printf '%s %s\n' "$root" "1"
if [ -f "$FAKE_DESCENDANT_PID_FILE" ]; then
  descendant=$(cat "$FAKE_DESCENDANT_PID_FILE")
  printf '%s %s\n' "$descendant" "$root"
fi
"#;

fn write_fake(root: &Path) -> PathBuf {
    let path = root.join("fake-claude");
    fs::write(&path, FAKE_CLAUDE).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn write_fake_ps(root: &Path) -> PathBuf {
    let path = root.join("fake-ps");
    fs::write(&path, FAKE_PS).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn invoke(
    mode: &str,
    descendant: bool,
    quiesce_ms: u64,
) -> (Output, String, Duration, Option<u32>) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let fake = write_fake(root.path());
    let fake_ps = write_fake_ps(root.path());
    let lifecycle = root.path().join("lifecycle.jsonl");
    let descendant_pid_file = root.path().join("descendant.pid");
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_claude-pee"))
        .env("CLAUDE_PEE_EXEC", fake)
        .env("CLAUDE_PEE_QUIESCE_MS", quiesce_ms.to_string())
        .env("CLAUDE_PEE_INJECT_CHAR_DELAY_MS", "0")
        .env("FAKE_MODE", mode)
        .env("FAKE_DESCENDANT", if descendant { "1" } else { "0" })
        .env("FAKE_DESCENDANT_PID_FILE", &descendant_pid_file)
        .env(
            "CLAUDE_PEE_PS_EXEC",
            if mode == "cleanup-probe-failure" {
                root.path().join("missing-ps")
            } else {
                fake_ps
            },
        )
        .env("HOME", home)
        .env("TMPDIR", root.path())
        .args([
            "--second-opinion-run-id",
            "run-test",
            "--second-opinion-lifecycle",
            lifecycle.to_str().unwrap(),
            "--second-opinion-inference-seconds",
            "2",
            "--second-opinion-normal-cleanup-seconds",
            "1",
            "--second-opinion-forced-cleanup-seconds",
            "1",
            "-p",
            "review this",
        ])
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let records = fs::read_to_string(lifecycle).unwrap();
    let descendant_pid = fs::read_to_string(descendant_pid_file)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    (output, records, elapsed, descendant_pid)
}

#[test]
fn normal_hook_completion_returns_one_review_and_clean_outcome() {
    let (output, records, elapsed, _) = invoke("normal", false, 20);
    assert!(
        output.status.success(),
        "stderr={} lifecycle={records}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Completed review\n"
    );
    assert!(records.contains("\"phase\":\"prompt_submitted\""));
    assert!(records.contains("\"phase\":\"output_complete\""));
    assert!(records.contains("\"outcome\":\"completed_clean\""));
    assert!(elapsed < Duration::from_secs(3));
}

#[test]
fn missing_hook_forces_cleanup_and_preserves_completed_review() {
    let (output, records, elapsed, _) = invoke("missing-hook", false, 20);
    assert!(
        output.status.success(),
        "stderr={} lifecycle={records}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Completed review\n"
    );
    assert!(records.contains("\"outcome\":\"completed_forced_cleanup\""));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn silent_inference_is_bounded_and_returns_no_review() {
    let (output, records, elapsed, _) = invoke("no-output", false, 20);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(records.contains("\"reason\":\"inference_deadline\""));
    assert!(records.contains("\"outcome\":\"failed_incomplete\""));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn pre_prompt_wait_is_cancelled_by_launch_relative_deadline() {
    let (output, records, elapsed, _) = invoke("never-quiescent", false, 5_000);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        !records.contains("\"phase\":\"prompt_submitted\""),
        "lifecycle={records}"
    );
    assert!(records.contains("\"outcome\":\"failed_incomplete\""));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn surviving_descendant_is_killed_before_review_is_released() {
    let (output, records, elapsed, descendant_pid) = invoke("normal", true, 20);
    assert!(
        output.status.success(),
        "stderr={} lifecycle={records}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Completed review\n"
    );
    assert!(
        records.contains("\"outcome\":\"completed_forced_cleanup\""),
        "lifecycle={records}"
    );
    let descendant_pid = descendant_pid.expect("fake descendant pid");
    let status = Command::new("kill")
        .args(["-0", &descendant_pid.to_string()])
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "descendant {descendant_pid} survived cleanup"
    );
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn ignored_exit_is_forced_without_duplicating_review() {
    let (output, records, elapsed, _) = invoke("ignored-exit", false, 20);
    assert!(
        output.status.success(),
        "stderr={} lifecycle={records}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Completed review\n"
    );
    assert!(records.contains("\"outcome\":\"completed_forced_cleanup\""));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn child_exit_before_output_is_incomplete_and_content_free() {
    let (output, records, elapsed, _) = invoke("exit-early", false, 20);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(records.contains("\"outcome\":\"failed_incomplete\""));
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn final_drain_recovers_terminal_output_written_immediately_before_exit() {
    let (output, records, elapsed, _) = invoke("exit-after-output", false, 20);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Completed review\n"
    );
    assert!(records.contains("\"outcome\":\"completed_clean\""));
    let output_complete = records
        .find("\"phase\":\"output_complete\"")
        .expect("output_complete record");
    let cleanup_started = records
        .find("\"phase\":\"cleanup_started\"")
        .expect("cleanup_started record");
    assert!(output_complete < cleanup_started);
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn cleanup_verification_failure_is_bounded_and_withholds_review() {
    let (output, records, elapsed, _) = invoke("cleanup-probe-failure", false, 20);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(records.contains("\"outcome\":\"cleanup_failed\""));
    assert!(elapsed < Duration::from_secs(5));
}
