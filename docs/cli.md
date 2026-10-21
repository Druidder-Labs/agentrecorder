# Command reference

Every command prints its flags with `--help`. Session ids accept unique prefixes.

## State directory

State lives in `$AGENTRECORDER_HOME`, default `$XDG_STATE_HOME/agentrecorder` or `~/.local/state/agentrecorder`. It holds `state.db` (SQLite, compiled in), `run/` (one Unix socket per live session) and `logs/` (supervisor stderr per session) and `views/` (dashboards written by `view ID`; `rm` deletes a session's copy). Directories are created 0700 and files 0600.

Requirements: Linux or macOS. `cargo install agentrecorder --locked` builds from crates.io and needs Rust 1.95 or newer, because of `sysinfo` 0.39. No Docker, database server, account or API key.

## Commands

| Command | Purpose |
| --- | --- |
| `run [-d] [--no-record] [--record-input] [--max-record-mb N] [--no-agent-integration] [--detach-key K] [--cwd DIR] -- CMD...` | Start CMD in a new supervised session and attach, or print the id with `-d` |
| `sessions [--json]` (alias `ls`) | List sessions and their status (see below) |
| `attach ID [--detach-key K]` | Attach this terminal; detach with `Ctrl-\` (`--detach-key ctrl-a`, `none`, or the `AGENTRECORDER_DETACH_KEY` variable) |
| `detach ID` | Detach whichever client is attached |
| `mark ID [LABEL]` | Record a `user.mark` event in a running session, at the current time; the label loses control characters and is capped at 256 characters. Prints the offset in the `HH:MM:SS.s` form that `replay --from` accepts |
| `inspect ID [--json]` | Measured values only: process tree, CPU, RSS, terminal size, time since the last output, window title, terminal notifications (OSC 9, 99, 777, BEL), git HEAD at start and exit, byte and event counts, warnings, the agent session id when one is recorded, the agent state reported by its hooks, command and failure counts from OSC 133 shell integration marks, the markers with their offsets, and for a `claude --resume <uuid>` session whether the resume was confirmed |
| `recover ID [--restart\|--resume] [-y] [--detached] [--detach-key K]` | Reattach if alive; otherwise explain what was lost and offer an explicit restart (same command, working directory and recording cap) or the agent's resume command |
| `replay ID [--speed N] [--from HH:MM:SS\|--from-mark N\|LABEL] [--max-idle S] [--instant] [--raw] [-f\|--follow]` | Play back recorded output; never re-executes commands. `--follow` keeps playing new output while the session runs. `--from-mark` starts at a marker, by number (1 is the first) or exact label; on a terminal, replay prints `[agentrecorder] mark N: LABEL` on stderr as it passes each one |
| `stop ID [--signal HUP\|TERM\|INT\|KILL] [--timeout S]` | Signal every process in the session, escalating to SIGKILL after the timeout (default 5 s) |
| `export ID [--format jsonl\|asciicast-v2\|asciicast-v3] [-f\|--follow]` | Events as JSON Lines, or output as an asciicast file with markers (v3 also carries the exit status). `--follow` keeps writing new events while the session runs |
| `bundle ID -o DIR [--redact REGEX]... [--cut FROM-TO]...` | Write a shareable directory for a finished session: `index.html` (dashboard), `session.cast` (asciicast v3), `events.jsonl`, `chain.jsonl` and `manifest.json` (see Sharing a session) |
| `anchor ID [--interval S]` | Print one hash-chain anchor line every S seconds (default 30) while the session runs and a last one when it ends; for a finished session, one line. Exit 130 on Ctrl-C |
| `verify DIR [--anchors FILE]` | Check a bundle's file digests and hash chain, and that every anchor in FILE matches; exit 0 when everything matches, 1 otherwise |
| `view DIR\|ID [--no-open]` | Open the dashboard of a bundle, or write one for a finished session to `views/ID` and open it; `--no-open` prints the path only |
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

`replay --follow` and `export --follow` read the recording, then poll it every 100 ms for new events while the session runs. They return when the session is `exited` or `lost`, after one last read, or on Ctrl-C (exit code 130). New output appears about 0.1 s after the program wrote it (100 ms poll plus the recorder's 20 ms batch). `export --follow` flushes after every line, so `agentrecorder export ID --follow | jq` streams. On a finished session both behave as without `--follow`.

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

`inspect` shows it as `Resume` (`resume` in `--json`). `recover --resume --detached` waits up to 15 s for a verdict other than `pending`, and for an ended process until its exit code is recorded, and prints it on stderr before the new session id; attached, the verdict is printed after the client detaches or the agent exits. Claude Code reports `No conversation found` and exits with code 1 for an unknown id, which gives `unconfirmed`.

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

## Shell integration

When the shell emits OSC 133 marks (the FinalTerm semantic prompt protocol), `inspect` fills `Commands` and `Failures`. A command is counted when `133;D` follows `133;C`; `D` without a preceding `C` (the first prompt, an empty Enter) is ignored. The exit code is the first parameter of `D` (`133;D;2`); a non-zero code is a failure, and a `D` without a code is counted as a command with an unknown exit code. Each counted command is recorded as `shell.command_finished`. Without marks, both lines are `unavailable`.

A minimal zsh setup that produces the marks:

```
precmd()  { local s=$?; print -n "\e]133;D;$s\a\e]133;A\a" }
preexec() { print -n "\e]133;C\a" }
```

The marks come from the output stream, so any program in the session can emit them; the counts describe what the session printed, not what the kernel ran. Claude Code runs its tools outside the terminal, so agent sessions have no marks.

## Sharing a session

`bundle` turns a finished session into a directory that someone without `agentrecorder` can open, for example a technical interview reviewer, a bug report or a pull request:

```
agentrecorder run -- claude                        # work as usual
agentrecorder anchor 3f9a1c | tee anchors.jsonl     # optional, in a second terminal while it runs
agentrecorder bundle 3f9a1c -o review --redact 'sk-[A-Za-z0-9]+' --cut 00:12:03-00:12:09
agentrecorder view review                           # dashboard in the browser
agentrecorder verify review --anchors anchors.jsonl # what the receiver runs
```

- `index.html` is a single file with the data inline: summary tiles, the terminal player, a timeline of markers, agent hook events, OSC 133 commands, git snapshots, notifications and warnings (click a row to seek), agent activity by tool, and the integrity data. It opens from disk; the terminal player loads `asciinema-player` 3.17.0 from jsDelivr with a subresource integrity hash, and without network the rest of the page still works.
- `--redact REGEX` (Rust `regex` syntax, matched on bytes) replaces every match with `*` of the same length in output and input (matched across event boundaries), screen snapshots, every string in event payloads, the stored session metadata and the asciicast header. The patterns are not written to the bundle; the manifest records how many there were and how many matches they replaced.
- `--cut FROM-TO` removes terminal output, input, snapshots, notifications, hook events, OSC 133 commands and marks recorded between two offsets (seconds, `MM:SS` or `HH:MM:SS`); lifecycle events stay. The cast carries a `cut` marker at the start of each range. The screen after a cut can look wrong until the program redraws it.
- Every recorded event is in `chain.jsonl` with its hash and fate (`kept`, `redacted`, `cut`), so a receiver sees how many events were edited and where.

What a bundle proves:

- `verify` without anchors shows that the files agree with each other: a kept event that was edited after bundling fails. Whoever made the bundle can still build a consistent bundle from edited data, so without anchors the bundle is self-reported.
- With anchors that a third party received while the session ran, `verify` shows that the events up to the last anchor are the ones that existed at that time, apart from the ones marked redacted or cut. Events after the last anchor are reported as not anchored.
- No bundle shows what happened outside the recorded terminal: another terminal, another device or another person.

`bundle` refuses a running session. `anchor` must run with the same `agentrecorder` version that later writes the bundle, because the hash covers the exported JSON line of each event.

## Recording and retention

- Terminal output is recorded by default; `--no-record` disables it and `--max-record-mb` caps it (default 256 MiB).
- Keyboard input is not recorded unless `--record-input` is given.
- Environment variables are never stored. The command line and working directory are.
- Finished sessions older than `AGENTRECORDER_RETENTION_DAYS` (default 30; 0 disables) are deleted when a new session starts.
- Replay strips terminal sequences that could write the clipboard, query the terminal or inject input; `--raw` keeps them. Exported asciicast files are not filtered.

Details: `security.md`.

## Platform notes

- Linux and macOS only. CI runs the test suite on both.
- On macOS, when output is produced faster than it is read, the kernel PTY can emit `\r\r\n` for a newline; recordings contain what the kernel delivered.
- CPU and memory figures cover the session's process tree as reported by the OS at inspection time.
- The end-to-end test for the main workflow is `cargo test -p agentrecorder --test e2e mvp_workflow`.
