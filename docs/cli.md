# Command reference

Session ids accept unique prefixes; `last` (or `latest`) is the newest session. Every command has `--help`.

## State directory

`$AGENTRECORDER_HOME`, default `$XDG_STATE_HOME/agentrecorder` or `~/.local/state/agentrecorder`: `state.db` (SQLite), `run/` (one socket per live session), `logs/` (supervisor stderr per session), `views/` (dashboards written by `view`, one folder per session id; opening a zip bundle replaces that session's local dashboard). Directories are 0700, files 0600.

A newer binary migrates `state.db` in place. An older binary refuses it, naming both versions and the update command, and leaves the file untouched. Keep one installation per machine.

Requirements: Linux or macOS. Building needs Rust 1.95 or newer.

## Commands

| Command | Purpose |
| --- | --- |
| `run [-d] [--no-record] [--record-input] [--max-record-mb N] [--no-agent-integration] [--detach-key K] [--cwd DIR] [--anchor FILE] [--anchor-token T] -- CMD...` | Start CMD supervised and attach; `-d` prints the id instead. `--anchor` appends an anchor line to FILE every 30 s; `--anchor-token` (or `AGENTRECORDER_ANCHOR_TOKEN`) seeds the hash chain with the token's SHA-256 |
| `sessions [--json]` (alias `ls`) | List sessions: id, start, status, duration, exit, directory, command |
| `attach ID [--detach-key K]` | Attach this terminal. Detach with `Ctrl-\` (`--detach-key ctrl-a`, `none`, or `AGENTRECORDER_DETACH_KEY`) |
| `detach ID` | Detach whichever client is attached |
| `mark ID [LABEL]` | Record a `user.mark` now (label capped at 256 characters); prints the offset `replay --from` accepts |
| `inspect ID [--json] [--prompts]` | Measured values: process tree, CPU, RSS, terminal size, idle time, notifications, git HEAD at start and exit, files changed, agent session id, agent state, tool calls, tokens per prompt, OSC 133 command counts, markers, resume verdict. Unmeasured values are listed under NOT MEASURED with the reason |
| `recover ID [--restart\|--resume] [-y] [--detached] [--detach-key K]` | Reattach if alive; otherwise show what is left and offer a restart or the agent's resume command |
| `replay ID [--speed N] [--from T\|--from-mark N\|LABEL] [--max-idle S] [--instant] [--raw] [--follow]` | Play back recorded output. `--follow` keeps playing while the session runs; `--from-mark` takes a number (1 is first) or an exact label |
| `stop ID [--signal HUP\|TERM\|INT\|KILL] [--timeout S]` | Signal every process in the session, SIGKILL after the timeout (default 5 s) |
| `export ID [--format jsonl\|asciicast-v2\|asciicast-v3] [--follow]` | Events as JSON Lines, or an asciicast with markers (v3 carries the exit status) |
| `bundle ID [-o DIR\|FILE] [--zip] [--force] [--redact REGEX]... [--redact-literal TEXT]... [--redact-secrets] [--cut FROM-TO]... [--prompts] [--diff] [--dry-run] [--json]` | Write `agentrecorder-ID/` (or `agentrecorder-ID.zip`): `index.html`, `session.cast`, `events.jsonl`, `chain.jsonl`, `manifest.json`, and `diff.patch` with `--diff`. `--prompts` adds each prompt's text; `--diff` adds the code changes. `--dry-run` lists what would change and writes nothing |
| `anchor ID [--interval S]` | Print one anchor line every S seconds (default 30) and one at exit; a finished session prints one line |
| `verify DIR\|FILE.zip [--anchors FILE] [--token T] [--json]` | Check file digests, the hash chain, every anchor, and that the genesis is the token's SHA-256. Exit 0 when all pass |
| `view DIR\|FILE.zip\|ID [--no-open]` | Open a bundle's dashboard as it was written, or write one for a finished session to `views/ID` (private to this user) with everything a bundle can hold: prompt text and code changes included, nothing redacted |
| `rm ID...`, `prune --older-than 30d` | Delete finished sessions and their recordings |
| `completions SHELL` | bash, zsh, fish or elvish |
| `hook [--agent NAME]` | Record a hook event read from stdin in the session named by `AGENTRECORDER_SESSION`; always exits 0, prints nothing |

## Session states

| Status | Meaning |
| --- | --- |
| `running` | Process alive, client attached |
| `detached` | Process alive, no client |
| `starting` | Supervisor has not bound its socket yet (first 10 s) |
| `exited` | Process ended; exit code or signal recorded |
| `lost` | Supervisor ended without recording an exit (host restarted, supervisor killed) |
| `unreachable` | Socket missing or silent, but the supervisor process still exists |

## Recovery

| Situation | `recover` |
| --- | --- |
| Client disconnected | Reattaches and redraws the screen |
| Process exited | Shows the exit and the last screen. `--restart` runs the same command as a new session (Claude Code gets a new session id); `--resume` runs the agent's resume command |
| Supervisor killed or host rebooted | Shows the metadata and offers `--restart` and `--resume` |
| Socket deleted, supervisor alive | Asks the supervisor to recreate it (SIGUSR1). If it does not answer the session is `unreachable`: `rm` refuses it, `stop` sends SIGTERM to the supervisor |

Both options ask for confirmation unless `-y` is given and refuse without a TTY. The environment is not stored; the new session uses the current shell's.

## Scripting

`run` off a terminal behaves like a pipe: stdin at end of file sends Ctrl-D, stdout waits for the reader, the exit code is CMD's, and the recording is complete before `run` returns (in a git work tree that includes the exit git snapshot: up to 3 s waiting for the start snapshot, 3 s for the exit snapshot and 5 s for the diff hash). `run -d` prints the id and returns.

`replay --follow` and `export --follow` poll every 100 ms, return when the session ends or on Ctrl-C (exit 130), and lag about 0.1 s behind the program. `export --follow | jq` streams.

## Agent integration

### Claude Code

For a `claude` command (also behind `env`), `run` inserts:

- `--session-id <uuid>`, so `recover --resume` reopens that exact conversation. Skipped when the arguments already pick or skip a session (`--session-id`, `--resume`, `-r`, `--continue`, `-c`, `--fork-session`, `--from-pr`, `--teleport`, `--no-session-persistence`), start elsewhere (`--bg`, `--cloud`, `--desktop`, `--tmux`), ask for `--help` or `--version`, or name a subcommand. A uuid the user passed is recorded as is.
- `--settings '<json>'` with command hooks for `SessionStart`, `UserPromptSubmit`, `PermissionRequest`, `Notification`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure` and `SessionEnd`, each running `agentrecorder hook --agent claude` with a 5 s timeout. Skipped when `--settings` is already present. The user's settings files are not touched. Text output shows the value as `--settings <agentrecorder hooks>`; `--json` and `export` keep the full argv.

`--no-agent-integration` turns both off. Without a recorded id, `--resume` falls back to `claude --continue`. Codex CLI, Gemini CLI and OpenCode resume their most recent session; no flags are inserted for them.

`agentrecorder hook` records `hook_event_name`, `session_id`, `transcript_path`, `cwd`, `source`, `reason`, `model`, `permission_mode`, `notification_type`, `message` and `tool_name`, each length-capped, and for tool events a `tool` summary: the first line of the Bash command (up to 500 bytes; later lines, such as a heredoc body, are counted, not kept) and its description, the file path, the Grep or Glob pattern, the URL, the subagent type, the lines added and removed by Edit, MultiEdit, Write and NotebookEdit, the first 300 bytes of the error text, which can include tool output, and whether the call was interrupted. File contents passed to edit tools, the prompt and the assistant reply are never recorded; a one-line command can itself contain code, such as `python3 -c '...'`. The `tool` summary is dropped while output is not recorded (`--no-record`, or after `--max-record-mb` is reached). At `Stop` and `SessionEnd` it also sums the transcript and records `usage` (tokens, API calls, models). Another agent's hook can call `agentrecorder hook --agent NAME` with the same fields; installing hooks for other agents is not automated.

### Resume verdict

A session that runs `claude --resume <uuid>` gets a verdict from its first `SessionStart` hook: `confirmed` (source `resume`, expected id), `mismatch`, `pending` (less than 90 s, no hook yet) or `unconfirmed` (process ended or 90 s passed). `inspect` shows it as `Resume`; `recover --resume --detached` waits up to 15 s for it.

### Agent state

From the last hook that changes it, never from screen inactivity:

| Hook | State |
| --- | --- |
| `SessionStart`, `Stop`, `StopFailure` | `idle` |
| `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure` | `working` |
| `PermissionRequest`, `Notification` with `permission_prompt` | `needs_input` |
| `SessionEnd`, or process gone | `ended` |

Claude Code fires no hook when a prompt is dismissed with Esc or a turn is interrupted, so the state can lag; `inspect` shows the age of the evidence.

### Tool calls, tokens, interactions, files

- `Tool calls`: counted from `PostToolUse` and `PostToolUseFailure`, total, failed and per tool. Denied calls and calls still running at exit fire no hook and are not counted (a refusal at a permission prompt, by choosing No or pressing Esc, is read from the transcript and shown as declined); a `PostToolUse` that reports the call as interrupted is counted and shown as failed.
- `Tokens`: read at `inspect` time from the transcript named by the last hook. `message.usage` of `assistant` lines is summed once per `requestId`; lines timestamped outside the session are skipped and counted as `outside_session`. Gives input, output, cache read, cache creation, API calls and models. Subagent transcripts are not included. Cost is always `unavailable` (no price table).
- Transcript signals: interruptions of the agent's work (`[Request interrupted by user]`), refusals at a permission prompt (`[Request interrupted by user for tool use]`, written both for No and for Esc on the prompt) and context compactions are counted (`tokens.interrupts`, `tokens.refusals` and `tokens.compactions` in `inspect --json`; their times are in a bundle's `manifest.json`), and each interaction carries the length of the agent's text answer and the model. Interruptions do not start an interaction.
- `Interactions`: the same tokens split by prompt. An interaction starts at each `user` line the user sent (text, paste, image, slash command, task notification); tool results, meta lines and compaction summaries do not start one. Each lists offset, origin, prompt length, image count and the tokens of the replies until the next one. Prompt text is hidden unless `--prompts` (100 characters in text output, full in `--json`).
- `Files changed`: work tree at exit against the HEAD recorded at start (`git diff --name-status` plus untracked), with the status letter and whether the path was already dirty at start. Capped at 500 paths; the count is exact. `unavailable` while running, when lost, or without a commit at start.

## Shell integration

When the shell emits OSC 133 marks, `inspect` counts `Commands` and `Failures`: a `D` after a `C`, with the exit code as the first parameter of `D`. Each one is a `shell.command_finished` event. A minimal zsh setup:

```
precmd()  { local s=$?; print -n "\e]133;D;$s\a\e]133;A\a" }
preexec() { print -n "\e]133;C\a" }
```

Claude Code runs its tools outside the terminal, so agent sessions have no marks.

## Sharing a session

```
agentrecorder run --anchor anchors.jsonl --anchor-token "$TOKEN" -- claude
agentrecorder bundle last --prompts --diff --redact-secrets --dry-run
agentrecorder bundle last --prompts --diff --redact-secrets --redact-literal 'hunter2' --cut 12:03-12:09 --zip
agentrecorder view agentrecorder-3f9a1c.zip
agentrecorder verify agentrecorder-3f9a1c.zip --anchors anchors.jsonl --token "$TOKEN"   # the reviewer runs this
```

Send the reviewer the bundle and, when integrity matters, the `anchors.jsonl` they received while the session ran (synced folder, `tail -f anchors.jsonl | curl -T - URL`). A reviewer who issues one token per session passes it to the candidate; the token is never stored, its SHA-256 is the chain genesis, so `verify --token` fails for a bundle made without it. The token identifies the session, not the person.

- `index.html` opens offline and makes no requests. Four tabs:
  - Overview, written for a non-technical reviewer: a plain-language summary, what was and was not recorded, guidance against ranking by speed or tokens, an evaluation guide with six competencies (understanding the problem, directing the AI, judgment and control, verifying the result, quality of the result, communication), each with the moments that support it, a rating a person gives and a note, debrief questions generated from the session, a glossary, and buttons to download the evaluation as Markdown or print it. Ratings and notes stay in the browser (`localStorage`) until downloaded; the page never rates.
  - Timeline: summary numbers, signals for the reviewer (prompt heuristics, interruptions, approvals, subagents, slash commands, compactions, test runs and the exit of the last one, files edited by the agent versus changed otherwise, commits, pace), and one card per prompt with the thinking time before it, the agent's steps and the tokens, beside the terminal player. Times seek the player; it shortens idle gaps to 3 s, and the page maps recorded times to player times through the cast.
  - Code changes: the diff (with `--diff`), per file, marked as edited by the agent in which prompt or changed otherwise, with its check against the session end.
  - Details: changed files and commits, every event, session details, integrity data and the exact `verify` command.
  The terminal player is `asciinema-player` 3.17.0, vendored.
- `--redact REGEX` (Rust regex on bytes), `--redact-literal TEXT`, `--redact-secrets` (OpenAI and Anthropic keys, GitHub and GitLab tokens, AWS key ids, Google API keys, Slack tokens, bearer tokens, JWTs, private key blocks; `SECRET_PATTERNS` in `bundle.rs`) replace matches with `*` of the same length in output, input, snapshots, payload strings, metadata and the cast header. Patterns are not written to the bundle. `--dry-run` prints each match with offset, event type, first four characters and length. A pattern that matches nothing warns.
- `--cut FROM-TO` (seconds, `MM:SS` or `HH:MM:SS`) removes output, input, snapshots, notifications, hooks, commands and marks in the range; lifecycle events stay. The screen after a cut can look wrong until the program redraws.
- Prompt data comes from the agent transcript, read when the bundle is written: time, wait since the previous answer (`Stop` hook), agent time until its `Stop`, or until the interruption when the person pressed Esc, minus the time each permission prompt waited for the person (from `PermissionRequest` until the terminal shows activity again, which is when the prompt was answered; the agent's idle notification repaint is ignored), tools called, tokens. Agent work whose prompt was removed by `--cut`, or that comes before the first prompt in the bundle, is shown on its own, without a prompt. Without `--prompts` only the length is included; with it the text, after redaction. Prompts inside a `--cut` range are dropped. This data is not in the hash chain: treat it as self-reported.
- `--diff` adds `diff.patch`: `git diff --binary` against the HEAD recorded at start, plus untracked files, from the session's working directory when the bundle is written. The diff covers the whole repository from its top level, also when the session ran in a subdirectory, with paths relative to the top level, and does not run textconv or external diff drivers; filters configured in the repository run, as with any `git diff`. It is written as raw bytes. Diffs over 32 MiB, or untracked files over 32 MiB in total, are not hashed or included. When the session ended, the supervisor recorded only the SHA-256 and size of the same diff in the exit `git.snapshot`, using a temporary index (the repository's own index is not touched); `bundle` and `verify` compare against it, so the result says `matches the session end`, `differs from the session end` (the work tree changed afterwards) or `not compared` (no hash in the recording), or `redacted` (verify then reports that the diff differs and that the bundle says redaction changed it). The diff is computed before redaction, so a pattern that masks a path in a git snapshot does not hide the hash. The redaction patterns apply to the diff; `--cut` does not, so code written during a cut range is still in it.
- `--zip` writes the same files into one deflate zip under a directory named after the file; `verify` and `view` read it directly.
- `chain.jsonl` lists every event with its hash and fate (`kept`, `redacted`, `cut`).

What `verify` proves:

| `result` | Meaning |
| --- | --- |
| `ok: consistent; not anchored, token not checked` | The files agree with each other. Whoever made the bundle could still have built it from edited data |
| `ok: consistent, anchored` | Events up to the last anchor are the ones that existed when the anchor was received, except those marked redacted or cut |
| `ok: consistent, anchored, token matched` | Plus the session was started with that token |
| `..., anchored, diff matches the session end` | With `--diff`: the code changes are the ones in the work tree when the session ended. Proven only when an anchor covers the session-end record; otherwise verify reports `diff matches the recorded hash, but the session end is not anchored` |
| `FAILED` | Named file, event or anchor differs |

`--json` carries `proves` and `gaps`. No bundle shows what happened outside the recorded terminal.

`bundle` refuses a running session and an existing output unless `--force`. `anchor` and `bundle` must run the same `agentrecorder` version.

## Recording and retention

- Output recorded by default; `--no-record` disables, `--max-record-mb` caps (default 256 MiB). Input only with `--record-input`.
- Environment variables are never stored; the command line and working directory are.
- Finished sessions older than `AGENTRECORDER_RETENTION_DAYS` (default 30, 0 disables) are deleted when a session starts.
- Replay strips sequences that could write the clipboard, query the terminal or inject input; `--raw` keeps them. Exported casts are not filtered.

## Platform notes

- Linux and macOS; CI tests both.
- On macOS a PTY read that falls behind can deliver `\r\r\n` for a newline; recordings keep what the kernel delivered.
- CPU and memory cover the session's process tree at inspection time.
