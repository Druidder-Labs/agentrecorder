# Command reference

Every command prints its flags with `--help`. Session ids accept unique prefixes.

## State directory

State lives in `$AGENTRECORDER_HOME`, default `$XDG_STATE_HOME/agentrecorder` or `~/.local/state/agentrecorder`. It holds `state.db` (SQLite, compiled in), `run/` (one Unix socket per live session) and `logs/` (supervisor stderr per session). Directories are created 0700 and files 0600.

Requirements: Linux or macOS. `cargo install agentrecorder --locked` builds from crates.io and needs Rust 1.95 or newer, because of `sysinfo` 0.39. No Docker, database server, account or API key.

## Commands

| Command | Purpose |
| --- | --- |
| `run [-d] [--no-record] [--record-input] [--max-record-mb N] [--no-agent-integration] [--detach-key K] [--cwd DIR] -- CMD...` | Start CMD in a new supervised session and attach, or print the id with `-d` |
| `sessions [--json]` (alias `ls`) | List sessions and their status (see below) |
| `attach ID [--detach-key K]` | Attach this terminal; detach with `Ctrl-\` (`--detach-key ctrl-a`, `none`, or the `AGENTRECORDER_DETACH_KEY` variable) |
| `detach ID` | Detach whichever client is attached |
| `inspect ID [--json]` | Measured values only: process tree, CPU, RSS, terminal size, time since the last output, window title, terminal notifications (OSC 9, 99, 777, BEL), git HEAD at start and exit, byte and event counts, warnings, the agent session id when one is recorded, the agent state reported by its hooks, and for a `claude --resume <uuid>` session whether the resume was confirmed |
| `recover ID [--restart\|--resume] [-y] [--detached] [--detach-key K]` | Reattach if alive; otherwise explain what was lost and offer an explicit restart (same command, working directory and recording cap) or the agent's resume command |
| `replay ID [--speed N] [--from HH:MM:SS] [--max-idle S] [--instant] [--raw]` | Play back recorded output; never re-executes commands |
| `stop ID [--signal HUP\|TERM\|INT\|KILL] [--timeout S]` | Signal every process in the session, escalating to SIGKILL after the timeout (default 5 s) |
| `export ID [--format jsonl\|asciicast-v2\|asciicast-v3]` | Events as JSON Lines, or output as an asciicast file with markers (v3 also carries the exit status) |
| `rm ID...` | Delete finished sessions and their recordings |
| `prune --older-than 30d` | Delete finished sessions older than the given age |
| `hook [--agent NAME]` | Read an agent hook event as JSON on stdin and record it as `agent.hook` in the session named by `AGENTRECORDER_SESSION`; always exits 0 and prints nothing on stdout (see Agent integration) |

## Session states

| Status | Meaning |
| --- | --- |
| `running` | The process is alive and a client is attached |
| `detached` | The process is alive and no client is attached |
| `starting` | The supervisor has not bound its socket yet (first 10 s) |
| `exited` | The process ended; exit code or signal is recorded |
| `lost` | The supervisor ended without recording an exit: the host restarted, or the supervisor was killed |
| `unreachable` | The socket is missing or not answering, but the supervisor process for this id still exists |

## What recovery means

| Situation | What `agentrecorder recover` does |
| --- | --- |
| The client disconnected (terminal closed, SSH dropped, client killed) | The process is still running; it reattaches and redraws the screen. |
| The process exited or crashed | Shows exit code or signal and the last recorded screen. `--restart` starts a new session with the same command after confirmation, minus the `--session-id` and hook settings that `run` inserted, so Claude Code gets a new id; for Claude Code, Codex, Gemini CLI and OpenCode, `--resume` runs the agent's own resume command instead (see Agent integration). Process memory is gone. |
| The supervisor was killed or the host rebooted | Shows persisted metadata and history and offers the same `--restart` and `--resume` options. Arbitrary processes are not restored. |
| The socket file was deleted but the supervisor still runs | The CLI asks the supervisor to recreate it (SIGUSR1) and the session works again. If the supervisor cannot answer (for example it is stopped), the session is `unreachable`, not `lost`: `rm` refuses to delete it, and `stop` sends SIGTERM to the supervisor. |

## Scripting

`agentrecorder run -- CMD` behaves like a pipe when it is not on a terminal. When stdin is not a terminal and reaches end of file, the client sends Ctrl-D to the session, so a shell fed from a file ends on its own. When stdout is not a terminal, output waits for the reader instead of being dropped. The client exits with CMD's exit code, and the recording is complete before it exits, so `export` can follow `run` directly. Use `run -d` to start a session and return immediately with its id.

## Agent integration

### Session ids

When the command is `claude` (directly or behind `env`), `run` generates a UUID v4 and inserts `--session-id <uuid>` right after the program name, so `recover --resume` runs `claude --resume <uuid>` and reopens that exact conversation. The id is stored on the session and shown by `inspect` as `Agent session`; the recorded command line includes the inserted flag.

No id is inserted when the arguments already pick or skip a session (`--session-id`, `--resume`, `-r`, `--continue`, `-c`, `--fork-session`, `--from-pr`, `--teleport`, `--no-session-persistence`), start elsewhere (`--bg`, `--cloud`, `--desktop`, `--tmux`), ask for `--help` or `--version`, or name a subcommand such as `mcp`. A UUID the user passed with `--session-id`, `--resume` or `-r` is recorded as is. `--no-agent-integration` turns insertion of the id and of the hooks off. Without a recorded id, `--resume` falls back to `claude --continue`, the most recent conversation in the working directory.

Claude Code rejects a `--session-id` that was already used (`Session ID ... is already in use`), so `recover --restart` removes the recorded id and `run` inserts a new one. Codex CLI, Gemini CLI and OpenCode keep their "most recent session" resume commands; their session flags are not inserted.

### Hooks

For the same `claude` commands, `run` also inserts `--settings '<json>'` with command hooks for `SessionStart`, `UserPromptSubmit`, `PermissionRequest`, `Notification`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure` and `SessionEnd`. Each hook runs `agentrecorder hook --agent claude` with a 5 s timeout. The text output of `inspect` and `recover` shows the inserted value as `--settings <agentrecorder hooks>`; `inspect --json`, `sessions --json` and `export` keep the full argv. Hooks passed with `--settings` are added to the user's own hooks for that run; nothing is written to the user's settings files. Hooks are not inserted when the arguments already contain `--settings` (a second one would replace either the user's or agentrecorder's), ask for `--help` or `--version`, or name a subcommand. They are inserted with `--resume` and `--continue`.

`agentrecorder hook` reads the event JSON from stdin and sends it to the supervisor of the session named by `AGENTRECORDER_SESSION`, which every managed process inherits. It records only `hook_event_name`, `session_id`, `transcript_path`, `cwd`, `source`, `reason`, `model`, `permission_mode`, `notification_type`, `message` and `tool_name`, each with a length cap. The prompt, tool input and assistant reply are never recorded. It exits 0 and prints nothing on stdout in every case, including outside a session, so it never changes what the agent does; errors go to stderr.

### Resume confirmation

A session whose command resumes a Claude Code UUID (`claude --resume <uuid>` or `-r <uuid>`, as `recover --resume` runs it) gets a verdict from its first `SessionStart` hook:

| Verdict | Condition |
| --- | --- |
| `confirmed` | `SessionStart` arrived with `source` `resume` and the expected session id |
| `mismatch` | `SessionStart` arrived with another session id or source |
| `pending` | No `SessionStart` yet, less than 90 s after the process started |
| `unconfirmed` | No `SessionStart` and the process ended (the exit code is named), or 90 s passed |

`inspect` shows it as `Resume` (`resume` in `--json`). `recover --resume --detached` waits up to 15 s for a verdict other than `pending` and prints it on stderr before the new session id; attached, the verdict is printed after the client detaches or the agent exits. Claude Code reports `No conversation found` and exits with code 1 for an unknown id, which gives `unconfirmed`.

### Agent state

`inspect` shows `Agent state` (`agent_state` in `--json`) from the last `agent.hook` event that changes it, with the source, the event and its age. It is never inferred from screen output or inactivity.

| Hook event | State |
| --- | --- |
| `SessionStart`, `Stop`, `StopFailure` | `idle` |
| `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure` | `working` |
| `PermissionRequest`, `Notification` with `notification_type` `permission_prompt` | `needs_input` |
| `SessionEnd` | `ended` |

Other events leave the state unchanged. When the process is no longer running and no `SessionEnd` arrived, the state is `ended` with the source `<agent> hooks and session status`. Without hook events the state is `unavailable`. Claude Code fires no hook when a permission prompt is dismissed with Esc or a turn is interrupted, so the state stays `needs_input` or `working` until the next hook; the age shows how old the evidence is.

The command is agent-neutral: another agent's command hook can call `agentrecorder hook --agent <name>` if it passes the same fields. Installing hooks for Codex CLI, Gemini CLI or OpenCode is not automated and has not been tested.

## Recording and retention

- Terminal output is recorded by default; `--no-record` disables it and `--max-record-mb` caps it (default 256 MiB).
- Keyboard input is not recorded unless `--record-input` is given.
- Environment variables are never stored. The command line and working directory are.
- Finished sessions older than `AGENTRECORDER_RETENTION_DAYS` (default 30; 0 disables) are deleted when a new session starts.
- Replay strips terminal sequences that could write the clipboard, query the terminal or inject input; `--raw` keeps them. Exported asciicast files are not filtered.

Details: `security.md`.

## Platform notes

- Linux and macOS only. CI runs the test suite on Linux; on macOS it runs only where a developer runs `just check`.
- On macOS, when output is produced faster than it is read, the kernel PTY can emit `\r\r\n` for a newline; recordings contain what the kernel delivered.
- CPU and memory figures cover the session's process tree as reported by the OS at inspection time.
- The end-to-end test for the main workflow is `cargo test -p agentrecorder --test e2e mvp_workflow`.
