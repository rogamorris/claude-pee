//! `claude-pee` — spawn `claude` (override via `CLAUDE_PEE_EXEC`) in a
//! PTY with `--session-id <UUIDv4>` injected as its first argument,
//! optionally feed it a `-p` payload, then tail the session transcript
//! in `~/.claude/projects/` and emit output per `--output-format`.
//!
//! Termination is signalled by claude's own `Stop` hook (wired in via
//! `--settings`), which touches a sentinel file. Diagnostic logs go to
//! stderr via the `log` facade — `RUST_LOG=debug` for the tailer trace.

use std::collections::HashSet;
use std::error::Error;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, error};
use portable_pty::native_pty_system;

mod args;
mod child;
mod config;
mod hook;
mod inject;
mod lifecycle;
mod transcript;

use config::Config;
use lifecycle::{Emitter, Outcome, Phase};

type BoxError = Box<dyn Error + Send + Sync + 'static>;

#[allow(clippy::too_many_lines)]
fn run() -> Result<u8, BoxError> {
    let parsed = args::parse(std::env::args_os().skip(1))?;
    if parsed.capabilities {
        let payload = serde_json::json!({
            "wrapper": "claude-pee",
            "wrapper_version": env!("CARGO_PKG_VERSION"),
            "lifecycle_schema": lifecycle::SCHEMA_VERSION,
            "lifecycle": true,
        });
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{payload}")?;
        stdout.flush()?;
        return Ok(0);
    }
    let lifecycle_fields = [
        parsed.run_id.is_some(),
        parsed.lifecycle_path.is_some(),
        parsed.inference_seconds.is_some(),
    ];
    if lifecycle_fields.iter().any(|present| *present)
        && !lifecycle_fields.iter().all(|present| *present)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "second-opinion run id, lifecycle path, and inference duration must be supplied together",
        )
        .into());
    }
    if parsed.run_id.as_deref().is_some_and(str::is_empty) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "second-opinion run id must not be empty",
        )
        .into());
    }
    let cfg = Config::build(parsed);
    let _sentinel_guard = hook::SentinelGuard::new(cfg.sentinel.clone());
    debug!(
        "session-id={} output-format={:?} sentinel={}",
        cfg.session_id,
        cfg.output_format,
        cfg.sentinel.display()
    );

    let pty = native_pty_system().openpty(child::PTY_SIZE)?;
    let mut spawned = pty.slave.spawn_command(child::build_command(
        &cfg.forwarded,
        &cfg.session_id,
        &cfg.settings_json,
    ))?;
    drop(pty.slave);

    let reader = pty.master.try_clone_reader()?;
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let last_change_us = Arc::new(AtomicU64::new(0));
    debug!("child-pid={:?}", spawned.process_id());

    if let Some(second_opinion) = &cfg.second_opinion {
        let process_id = spawned
            .process_id()
            .ok_or_else(|| io::Error::other("spawned child has no process id"))?;
        let mut lifecycle = Emitter::open(
            &second_opinion.lifecycle_path,
            second_opinion.run_id.clone(),
            start,
        )?;
        lifecycle.emit(Phase::Launched, None, "")?;

        let outcome = (|| -> Result<_, BoxError> {
            let last_change = Arc::clone(&last_change_us);
            drop(thread::spawn(move || {
                child::drain(reader, start, &last_change)
            }));

            let (observation_tx, observation_rx) = mpsc::channel();
            let session_id = cfg.session_id.clone();
            let transcript_stop = Arc::clone(&stop);
            let output_format = cfg.output_format;
            drop(thread::spawn(move || {
                transcript::run_observed(
                    &session_id,
                    output_format,
                    &transcript_stop,
                    &observation_tx,
                    start,
                );
            }));

            let (prompt_tx, prompt_rx) = mpsc::channel();
            if let Some(payload) = cfg.inject_payload.as_deref() {
                let writer = pty.master.take_writer()?;
                let sentinel = cfg.sentinel.clone();
                let payload = payload.to_owned();
                let inject_stop = Arc::clone(&stop);
                let inject_last_change = Arc::clone(&last_change_us);
                let char_delay = cfg.char_delay;
                let quiesce = cfg.quiesce;
                drop(thread::spawn(move || {
                    let ctx = inject::Ctx {
                        writer,
                        payload: &payload,
                        char_delay,
                        start,
                        last_change_us: &inject_last_change,
                        stop: &inject_stop,
                        sentinel: &sentinel,
                        quiesce,
                        prompt_submitted: Some(prompt_tx),
                    };
                    if let Err(error) = inject::run(ctx) {
                        error!("auto-inject failed: {error}");
                    }
                }));
            }

            let mut prompt_emitted = false;
            let mut output_emitted = false;
            let mut review: Option<String> = None;
            let mut cleanup_started: Option<Instant> = None;
            let mut forced_started: Option<Instant> = None;
            let mut forced = false;
            let mut forced_error = false;
            let mut child_exit_code: Option<u32> = None;
            let mut known_descendants = HashSet::new();
            let mut tree_tracking_failed = false;

            loop {
                if cleanup_started.is_none() && !prompt_emitted && prompt_rx.try_recv().is_ok() {
                    lifecycle.emit(Phase::PromptSubmitted, None, "")?;
                    prompt_emitted = true;
                }

                while cleanup_started.is_none()
                    && let Ok(observation) = observation_rx.try_recv()
                {
                    if !prompt_emitted && prompt_rx.recv_timeout(Duration::from_millis(50)).is_ok()
                    {
                        lifecycle.emit(Phase::PromptSubmitted, None, "")?;
                        prompt_emitted = true;
                    }
                    if !output_emitted {
                        lifecycle.emit(Phase::OutputObserved, None, "")?;
                        output_emitted = true;
                    }
                    if transcript::completed_within(&observation, second_opinion.inference)
                        && review.is_none()
                    {
                        lifecycle.emit(Phase::OutputComplete, None, "")?;
                        lifecycle.emit(Phase::CleanupStarted, None, "")?;
                        review = Some(observation.text);
                        cleanup_started = Some(Instant::now());
                    }
                }

                match child::descendant_processes(process_id) {
                    Ok(descendants) => known_descendants.extend(descendants),
                    Err(_) => tree_tracking_failed = true,
                }

                if let Some(status) = spawned.try_wait()? {
                    child_exit_code = Some(status.exit_code());
                }

                if child_exit_code.is_some()
                    && child::process_tree_absent(process_id, &known_descendants)?
                {
                    stop.store(true, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(120));
                    while let Ok(observation) = observation_rx.try_recv() {
                        if !prompt_emitted
                            && prompt_rx.recv_timeout(Duration::from_millis(50)).is_ok()
                        {
                            lifecycle.emit(Phase::PromptSubmitted, None, "")?;
                            prompt_emitted = true;
                        }
                        if !output_emitted {
                            lifecycle.emit(Phase::OutputObserved, None, "")?;
                            output_emitted = true;
                        }
                        if transcript::completed_within(&observation, second_opinion.inference)
                            && review.is_none()
                        {
                            lifecycle.emit(Phase::OutputComplete, None, "")?;
                            if cleanup_started.is_none() {
                                lifecycle.emit(Phase::CleanupStarted, None, "child_exited")?;
                                cleanup_started = Some(Instant::now());
                            }
                            review = Some(observation.text);
                        }
                    }
                    if cleanup_started.is_none() {
                        lifecycle.emit(Phase::CleanupStarted, None, "child_exited")?;
                    }
                    let result = match (
                        review.as_ref(),
                        forced,
                        forced_error || tree_tracking_failed,
                    ) {
                        (Some(_), false, false) => Outcome::CompletedClean,
                        (Some(_), true, false) => Outcome::CompletedForcedCleanup,
                        (Some(_) | None, _, true) => Outcome::CleanupFailed,
                        (None, _, false) => Outcome::FailedIncomplete,
                    };
                    lifecycle.emit(Phase::Terminated, Some(result), "process_group_absent")?;
                    if matches!(
                        result,
                        Outcome::CompletedClean | Outcome::CompletedForcedCleanup
                    ) && let Some(text) = review
                    {
                        let mut stdout = io::stdout().lock();
                        writeln!(stdout, "{text}")?;
                        stdout.flush()?;
                    }
                    return Ok(result);
                }

                if review.is_none()
                    && cleanup_started.is_none()
                    && start.elapsed() > second_opinion.inference
                {
                    stop.store(true, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(120));
                    while let Ok(observation) = observation_rx.try_recv() {
                        if !prompt_emitted
                            && prompt_rx.recv_timeout(Duration::from_millis(50)).is_ok()
                        {
                            lifecycle.emit(Phase::PromptSubmitted, None, "")?;
                            prompt_emitted = true;
                        }
                        if !output_emitted {
                            lifecycle.emit(Phase::OutputObserved, None, "")?;
                            output_emitted = true;
                        }
                        if transcript::completed_within(&observation, second_opinion.inference)
                            && review.is_none()
                        {
                            lifecycle.emit(Phase::OutputComplete, None, "")?;
                            review = Some(observation.text);
                        }
                    }
                    lifecycle.emit(Phase::CleanupStarted, None, "inference_deadline")?;
                    cleanup_started = Some(Instant::now());
                    forced = true;
                    forced_started = Some(Instant::now());
                    if child::terminate_process_tree(process_id, &known_descendants).is_err() {
                        forced_error = true;
                    }
                }

                if let Some(cleanup_start) = cleanup_started {
                    if !forced && cleanup_start.elapsed() >= second_opinion.normal_cleanup {
                        forced = true;
                        forced_started = Some(Instant::now());
                        if child::terminate_process_tree(process_id, &known_descendants).is_err() {
                            forced_error = true;
                        }
                    }
                    if forced
                        && forced_started.is_some_and(|started| {
                            started.elapsed() >= second_opinion.forced_cleanup
                        })
                    {
                        stop.store(true, Ordering::Relaxed);
                        lifecycle.emit(
                            Phase::Terminated,
                            Some(Outcome::CleanupFailed),
                            "process_group_present",
                        )?;
                        return Ok(Outcome::CleanupFailed);
                    }
                }

                thread::sleep(Duration::from_millis(20));
            }
        })()?;

        return Ok(u8::from(!matches!(
            outcome,
            Outcome::CompletedClean | Outcome::CompletedForcedCleanup
        )));
    }

    let status = thread::scope(|scope| -> Result<_, BoxError> {
        let last_change = last_change_us.as_ref();
        let stop_ref = stop.as_ref();
        scope.spawn(move || child::drain(reader, start, last_change));
        let session_id = cfg.session_id.as_str();
        scope.spawn(move || transcript::run(session_id, cfg.output_format, stop_ref));

        if let Some(payload) = cfg.inject_payload.as_deref() {
            let writer = pty.master.take_writer()?;
            let sentinel = cfg.sentinel.as_path();
            scope.spawn(move || {
                let ctx = inject::Ctx {
                    writer,
                    payload,
                    char_delay: cfg.char_delay,
                    start,
                    last_change_us: last_change,
                    stop: stop_ref,
                    sentinel,
                    quiesce: cfg.quiesce,
                    prompt_submitted: None,
                };
                if let Err(e) = inject::run(ctx) {
                    error!("auto-inject failed: {e}");
                }
            });
        }

        let status = spawned.wait()?;
        stop.store(true, Ordering::Relaxed);
        Ok(status)
    })?;

    Ok(u8::try_from(status.exit_code()).unwrap_or(1))
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            let mut stderr = io::stderr().lock();
            drop(writeln!(stderr, "claude-pee: {e}"));
            ExitCode::FAILURE
        }
    }
}
