//! Auto-inject orchestration: wait for the TUI to settle, type the
//! prompt, wait for claude's Stop hook, send `/exit`.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, trace};

use crate::child::now_micros;
use crate::hook;

const QUIESCE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Write `payload` to `writer`. With `char_delay > 0` we type one char at
/// a time and flush after each — useful when the child has its own input
/// rate limiter or you want to simulate human typing.
fn write_payload<W: Write>(writer: &mut W, payload: &str, char_delay: Duration) -> io::Result<()> {
    if char_delay.is_zero() {
        writer.write_all(payload.as_bytes())?;
    } else {
        let mut buf = [0u8; 4];
        let mut iter = payload.chars().peekable();
        while let Some(ch) = iter.next() {
            let s = ch.encode_utf8(&mut buf);
            writer.write_all(s.as_bytes())?;
            writer.flush()?;
            if iter.peek().is_some() {
                thread::sleep(char_delay);
            }
        }
    }
    writer.flush()
}

/// Block until the vt100 screen hash hasn't changed in `threshold`, or
/// `stop` is signalled. Returns `true` if we observed quiescence.
fn wait_for_quiescence(
    start: Instant,
    last_change_us: &AtomicU64,
    stop: &AtomicBool,
    threshold: Duration,
) -> bool {
    let threshold_us = u64::try_from(threshold.as_micros()).unwrap_or(u64::MAX);
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let last_us = last_change_us.load(Ordering::Relaxed);
        if last_us > 0 {
            let idle_us = now_micros(start).saturating_sub(last_us);
            if idle_us >= threshold_us {
                debug!(
                    "screen quiescent (~{:?} idle)",
                    Duration::from_micros(idle_us)
                );
                return true;
            }
        }
        thread::sleep(QUIESCE_POLL_INTERVAL);
    }
}

pub struct Ctx<'a> {
    pub writer: Box<dyn Write + Send>,
    pub payload: &'a str,
    pub char_delay: Duration,
    pub start: Instant,
    pub last_change_us: &'a AtomicU64,
    pub stop: &'a AtomicBool,
    pub sentinel: &'a Path,
    pub quiesce: Duration,
}

/// Drive the child end-to-end:
///   1. wait for the TUI to render and settle (heuristic — the Stop hook
///      can't fire before the first turn);
///   2. type the prompt + CR;
///   3. wait for the Stop-hook sentinel to appear (definitive);
///   4. type `/exit\r\n`.
pub fn run(mut ctx: Ctx<'_>) -> io::Result<()> {
    debug!(
        "waiting for first screen quiescence ({:?}) before prompt",
        ctx.quiesce
    );
    if !wait_for_quiescence(ctx.start, ctx.last_change_us, ctx.stop, ctx.quiesce) {
        debug!("pre-prompt quiescence wait cancelled (child exited first)");
        return Ok(());
    }

    let mut prompt = String::with_capacity(ctx.payload.len().saturating_add(1));
    prompt.push_str(ctx.payload);
    prompt.push('\r');
    write_payload(&mut ctx.writer, &prompt, ctx.char_delay)?;

    debug!("waiting for Stop hook sentinel: {}", ctx.sentinel.display());
    if !hook::wait_for_file(ctx.sentinel, ctx.stop)? {
        debug!("sentinel wait cancelled (child exited first)");
        return Ok(());
    }

    trace!("Stop hook fired, injecting /exit");
    write_payload(&mut ctx.writer, "/exit\r\n", ctx.char_delay)?;
    Ok(())
}
