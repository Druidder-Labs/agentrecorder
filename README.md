# agentrecorder

Run it. Inspect it. Recover it. Replay it.

`agentrecorder` is a local execution runtime for AI coding agents (Claude Code, Codex CLI, Gemini CLI, OpenCode) and any other terminal program. It runs the program in a PTY owned by a detached supervisor, so the program keeps running when the terminal closes. It records a structured, ordered event log of the session in SQLite and replays the recorded output with timing. It also reports what can and cannot be recovered after a crash or reboot.

It is not a terminal emulator, a multiplexer layout engine, or an AI assistant. It works inside the terminal you already use (Ghostty, Kitty, Alacritty, iTerm2, Terminal.app, any xterm-compatible terminal, or SSH).

## Install

Requirements: Linux or macOS, a Rust toolchain (1.95 or newer, required by `sysinfo` 0.39). No Docker, database server, account or API key.

```
git clone https://github.com/Druidder-Labs/agentrecorder
cd agentrecorder
cargo install --path crates/agentrecorder --locked
```

SQLite is compiled in. State lives in `$AGENTRECORDER_HOME`, default `$XDG_STATE_HOME/agentrecorder` or `~/.local/state/agentrecorder`.

## The core workflow

```
$ agentrecorder run -- bash
[agentrecorder] session 3f9a1c started (recording output); detach with Ctrl-\
$ echo hello
hello
$ sleep 30 && echo done          # press Ctrl-\ while it runs
[agentrecorder] detached from session 3f9a1c; reattach with: agentrecorder attach 3f9a1c

$ agentrecorder sessions
SESSION   COMMAND         STATUS          DURATION  EXIT
3f9a1c    bash            detached             12s  -

$ agentrecorder attach 3f9a1c          # screen and scrollback are redrawn
$ echo back                       # press Ctrl-\ again

$ agentrecorder inspect 3f9a1c
EXECUTION PROFILE
----------------------------------------
Session:         3f9a1c
Command:         bash
Status:          detached
...
PROCESS
PID:             18420
CPU:             0.0% (process tree, 250 ms sample)
Memory:          2.7 MiB RSS (process tree)
Child processes: 0
...
EXECUTION
Commands:        unavailable (no shell or agent integration)

$ agentrecorder stop 3f9a1c
session 3f9a1c stopped; killed by signal 1

$ agentrecorder replay 3f9a1c --speed 2
```

The same sequence runs as an automated test against real PTYs: `cargo test -p agentrecorder --test e2e mvp_workflow`.

Agents work the same way:

```
agentrecorder run -- claude
agentrecorder run -d -- codex          # start detached, print the session id
```

## Commands

| Command | Purpose |
| --- | --- |
| `agentrecorder run [-d] [--no-record] [--record-input] [--max-record-mb N] [--detach-key K] [--cwd DIR] -- CMD...` | Start CMD in a new supervised session and attach (or print the id with `-d`) |
| `agentrecorder sessions [--json]` (alias `ls`) | List sessions: `running` (a client is attached), `detached`, `exited`, `lost`, `starting`, or `unreachable` (the socket is missing but the supervisor process still exists) |
| `agentrecorder attach ID [--detach-key K]` | Attach this terminal; detach with `Ctrl-\` (`--detach-key ctrl-a`, `none`, or the `AGENTRECORDER_DETACH_KEY` variable) |
| `agentrecorder detach ID` | Detach whichever client is attached |
| `agentrecorder inspect ID [--json]` | Measured values only: process tree, CPU, RSS, terminal size, time since the last output, window title, terminal notifications (OSC 9, 99, 777, BEL), git HEAD at start and exit, byte and event counts, warnings |
| `agentrecorder recover ID [--restart\|--resume] [-y] [--detached] [--detach-key K]` | Reattach if alive; otherwise explain what was lost and offer an explicit restart (same command, working directory and recording cap) or the agent's resume command |
| `agentrecorder replay ID [--speed N] [--from HH:MM:SS] [--max-idle S] [--instant] [--raw]` | Play back recorded output; never re-executes commands |
| `agentrecorder stop ID [--signal HUP\|TERM\|INT\|KILL] [--timeout S]` | Signal every process in the session, escalating to SIGKILL after the timeout |
| `agentrecorder export ID [--format jsonl\|asciicast-v2\|asciicast-v3]` | Events as JSON Lines, or output as an asciicast file with markers (v3 also has the exit status) |
| `agentrecorder rm ID...`, `agentrecorder prune --older-than 30d` | Delete finished sessions and their recordings |

Session ids accept unique prefixes.

In scripts, `agentrecorder run -- CMD` behaves like a pipe. When stdin is not a terminal and reaches end of file, the client sends Ctrl-D to the session, so a shell fed from a file ends on its own. When stdout is not a terminal, output waits for the reader instead of being dropped. The client exits with CMD's exit code. Use `run -d` to start a session and return immediately.

## What recovery means

| Situation | What `agentrecorder recover` does |
| --- | --- |
| The client disconnected (terminal closed, SSH dropped, client killed) | The process is still running; it reattaches and redraws the screen. |
| The process exited or crashed | Shows exit code or signal and the last recorded screen. `--restart` starts a new session with the same command after confirmation; for Claude Code, Codex, Gemini CLI and OpenCode, `--resume` runs the agent's own resume command instead. Process memory is gone. |
| The supervisor was killed or the host rebooted | Shows persisted metadata and history and offers the same `--restart` and `--resume` options. Arbitrary processes are not restored. |
| The socket file was deleted but the supervisor still runs | The CLI asks the supervisor to recreate it (SIGUSR1) and the session works again. If the supervisor cannot answer (for example it is stopped), the session is `unreachable`, not `lost`: `rm` refuses to delete it, and `stop` sends SIGTERM to the supervisor. |

## Recording and privacy

- Terminal output is recorded by default (`--no-record` disables it; `--max-record-mb` caps it, default 256).
- Keyboard input is not recorded unless `--record-input` is given.
- Environment variables are never stored. The command line and working directory are.
- State files are readable only by your user. Finished sessions older than 30 days are deleted when a new session starts (`AGENTRECORDER_RETENTION_DAYS`, 0 disables).
- Replay strips terminal sequences that could write your clipboard or inject input (`--raw` keeps them).

Details: `docs/security.md`.

## Development

```
just check        # fmt-check, clippy -D warnings, all tests
just check-ci     # check plus cargo deny and a quick benchmark run
just bench        # full benchmark, writes bench-results.json
```

## Documentation

- `docs/architecture.md`: components, process model, recovery and replay models, platform notes
- `docs/event-model.md`: event schema v1
- `docs/security.md`: threat model and controls
- `docs/performance.md`: measured baseline and event-loss behavior
- `CHANGELOG.md`: notable changes

## Platform limitations

- Linux and macOS only.
- On macOS, when output is produced faster than it is read, the kernel PTY can emit `\r\r\n` for a newline; recordings contain what the kernel delivered.
- CPU and memory figures cover the session's process tree as reported by the OS at inspection time.

## License

MIT
