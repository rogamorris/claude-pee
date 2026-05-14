//! `claude-pee` — spawn `claude` (override via `CLAUDE_PEE_EXEC`) in a
//! PTY with `--session-id <UUIDv4>` injected as its first argument,
//! optionally feed it a `-p` payload, then tail the session transcript
//! in `~/.claude/projects/` and emit output per `--output-format`.
//!
//! Termination is signalled by claude's own `Stop` hook (wired in via
//! `--settings`), which touches a sentinel file. Diagnostic logs go to
//! stderr via the `log` facade — `RUST_LOG=debug` for the tailer trace.

use std::error::Error;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

use log::{debug, error};
use portable_pty::native_pty_system;

mod args;
mod child;
mod config;
mod hook;
mod inject;
mod transcript;

use config::Config;

type BoxError = Box<dyn Error + Send + Sync + 'static>;

fn run() -> Result<u8, BoxError> {
    let parsed = args::parse(std::env::args_os().skip(1))?;
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
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let last_change_us = AtomicU64::new(0);
    debug!("child-pid={:?}", spawned.process_id());

    let status = thread::scope(|scope| -> Result<_, BoxError> {
        let last_change = &last_change_us;
        let stop_ref = &stop;
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
