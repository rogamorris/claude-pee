//! PTY-level child handling: build the spawn `CommandBuilder` and drain
//! the master, feeding bytes into a `vt100` parser so the quiescence
//! detector in [`crate::inject`] can see screen-state changes via
//! `last_change_us`.

use std::ffi::OsString;
use std::hash::{DefaultHasher, Hasher};
use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use portable_pty::{CommandBuilder, PtySize};

pub const PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// Resolve the binary to spawn. `CLAUDE_PEE_EXEC` wins if non-empty;
/// otherwise fall back to the `claude` CLI on `PATH`.
pub fn child_exec() -> OsString {
    std::env::var_os("CLAUDE_PEE_EXEC")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| OsString::from("claude"))
}

/// Build the child argv: `<exec> --session-id <id> --settings <json> [forwarded...]`.
pub fn build_command(
    forwarded: &[OsString],
    session_id: &str,
    settings_json: &str,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(child_exec());
    cmd.arg("--session-id");
    cmd.arg(session_id);
    cmd.arg("--settings");
    cmd.arg(settings_json);
    for arg in forwarded {
        cmd.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    cmd
}

/// Microseconds since `start`, saturating at `u64::MAX` (we'll never get
/// near 2^64 µs in practice).
pub fn now_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn hash_screen(parser: &vt100::Parser) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(parser.screen().contents().as_bytes());
    h.finish()
}

/// Drain the PTY master and feed bytes into a `vt100` parser. Each time
/// `parser.screen().contents()` hashes to a new value we publish the
/// timestamp on `last_change_us` for the quiescence waiter to consume.
pub fn drain<R: Read>(mut reader: R, start: Instant, last_change_us: &AtomicU64) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    let mut parser = vt100::Parser::new(PTY_SIZE.rows, PTY_SIZE.cols, 0);
    let mut last_hash = hash_screen(&parser);
    last_change_us.store(now_micros(start), Ordering::Relaxed);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = buf.get(..n).unwrap_or(&[]);
        parser.process(chunk);
        let new_hash = hash_screen(&parser);
        if new_hash != last_hash {
            last_change_us.store(now_micros(start), Ordering::Relaxed);
            last_hash = new_hash;
        }
    }
    Ok(())
}
