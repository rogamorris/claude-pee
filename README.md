# claude-pee

A drop-in front-end for the [`claude`](https://docs.claude.com/en/docs/claude-code) CLI that:

- spawns `claude` in a PTY,
- assigns a fresh `--session-id <UUIDv4>`,
- forwards every other flag verbatim,
- optionally injects a one-shot prompt via `-p`,
- tails the matching session transcript jsonl, and
- prints just the assistant's reply to stdout (`text` by default, or `json`/`stream-json`).

Termination is driven by claude's own **Stop hook** — no flaky screen-idle heuristics. When claude finishes a turn, the hook touches a sentinel file; claude-pee sees it, sends `/exit`, and exits with the child's status code.

## Install

```bash
cargo build --release
cp target/release/claude-pee ~/.local/bin/      # or anywhere on PATH
```

Requires Rust 1.85+ (edition 2024).

## Quick start

```bash
claude-pee -p "what is 2 + 2"
# → 4
```

## Drop-in for `claude`

The simplest way to make `claude` go through claude-pee everywhere you already type it:

```sh
# ~/.zshrc / ~/.bashrc
alias claude='claude-pee'
```

This is safe because shell aliases are resolved only in interactive shells. When claude-pee internally does `spawn("claude")`, it goes through `$PATH` and finds the real binary — no recursion.

If you need the replacement visible to scripts too, put a `claude` shim earlier in `$PATH` and point `CLAUDE_PEE_EXEC` at the real claude:

```sh
ln -s "$(which claude-pee)" ~/bin/claude
export CLAUDE_PEE_EXEC=/usr/local/bin/claude   # absolute path to the real one
```

## Flags

Owned by claude-pee (consumed, never forwarded):

| Flag | Purpose |
| --- | --- |
| `-p PROMPT` / `-p=PROMPT` | One-shot prompt to inject. Triggers auto-`/exit` after claude responds. |
| `--output-format text\|json\|stream-json` | What to print on stdout. Default `text`. |

Everything else is passed through to `claude` after `--session-id <UUID> --settings <hook-json>`. So `claude-pee --permission-mode plan -p hi` becomes:

```
claude --session-id <UUID> --settings '<json>' --permission-mode plan
```

## Output formats

| Format | What goes to stdout |
| --- | --- |
| `text` (default) | The assistant's plain text reply. Thinking-only and tool-use-only turns are skipped. |
| `json` | The assistant message transcript line, verbatim JSON (one per turn). Result lines too. |
| `stream-json` | Every transcript line as it lands, verbatim. |

Diagnostic logs go to **stderr** via [`log`](https://docs.rs/log) + `env_logger`. Default level `info` (silent). Set `RUST_LOG=debug` for the per-line tailer trace; `RUST_LOG=trace` also surfaces "tailing &lt;path&gt;" and "injecting /exit".

## Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `CLAUDE_PEE_EXEC` | `claude` | Binary to spawn. Empty value behaves like unset. |
| `CLAUDE_PEE_QUIESCE_MS` | `500` | Milliseconds the TUI must be idle before the prompt is injected. Heuristic — needed because the Stop hook can't fire before the first turn. Set higher if your machine is slow. Must be `> 0`. |
| `CLAUDE_PEE_INJECT_CHAR_DELAY_MS` | `0` | Per-character typing delay (simulates human typing or works around input rate-limiters). |
| `RUST_LOG` | `info` | Standard env_logger filter. |

## How termination works

1. At startup claude-pee computes a sentinel path `${TMPDIR}/claude-pee-<UUID>.done` and builds a `--settings` JSON containing:

   ```json
   {
     "hooks": {
       "Stop": [
         { "hooks": [ { "type": "command", "command": "touch '<sentinel>'" } ] }
       ]
     }
   }
   ```

   That gets passed to `claude --settings …` so claude registers the hook for this session only — no permanent config changes.

2. claude-pee waits for the PTY screen to go quiescent (so the TUI is ready to receive input), then types the prompt + `\r`.

3. When claude finishes its turn, it runs the registered `Stop` hook, which `touch`es the sentinel.

4. claude-pee sees the sentinel appear, sends `/exit\r\n`, and waits for the child to exit.

5. The sentinel is removed on exit via an RAII guard (even on error/panic paths).

## Examples

```bash
# Text answer only
claude-pee -p "tell me a joke"

# Full assistant message as JSON (one line)
claude-pee -p "tell me a joke" --output-format json | jq

# Stream every transcript event live
claude-pee -p "do a long task" --output-format stream-json | jq -c .

# Override the executable (e.g. point at a build)
CLAUDE_PEE_EXEC=~/code/claude/dist/claude claude-pee -p hi

# Slow down injection (simulate typing)
CLAUDE_PEE_INJECT_CHAR_DELAY_MS=50 claude-pee -p hello

# Bigger pre-prompt grace window on a slow machine
CLAUDE_PEE_QUIESCE_MS=2000 claude-pee -p hi

# Diagnose what the tailer is doing
RUST_LOG=debug claude-pee -p hi
```

## Caveats

- **Without `-p` it's a passthrough** — no auto-`/exit`, so you'll need to exit claude yourself (interactive use). Useful if you just want the session-id assignment and the transcript path printed (set `RUST_LOG=trace`).
- The pre-prompt **quiescence wait is heuristic** — the Stop hook can't help here because no turn has happened yet. If you see the prompt being typed before claude is ready, raise `CLAUDE_PEE_QUIESCE_MS`.
- Depends on claude honouring `--session-id`, `--settings`, and `/exit`. Tested against current claude code; if the schema changes upstream, this could break.
- Stdout is for the parsed output only. There is no "echo the raw PTY to my stdout" mode (it would interleave with the parsed output).

## Source layout

```
src/main.rs        entry + run() orchestration
src/args.rs        CLI parsing
src/config.rs      Config: args + env → resolved runtime config
src/child.rs       PTY spawn, drain, vt100 screen hashing
src/hook.rs        Stop-hook sentinel + --settings JSON + RAII cleanup
src/inject.rs      auto-inject: TUI wait → prompt → sentinel → /exit
src/transcript.rs  jsonl discovery + tail + per-format output
```

Lints are strict (`clippy::all`/`pedantic`/`nursery`/`cargo` denied, `unsafe_code` forbidden, no `unwrap`/`expect`/`panic` outside tests). `pre-commit` runs `cargo fmt`, `check`, `clippy`, `test`, and `doc` on every commit.
