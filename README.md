# agentrecorder

A flight recorder for coding agents. It runs the agent in a supervised terminal, records every byte with timing, and answers afterwards: what happened, what did the agent do, and can someone else trust the recording.

- **Replay** a session with its original timing, or export it as asciicast. Nothing is re-executed.
- **Inspect** tool calls, tokens per prompt, files changed, process tree, idle time, exit status. Measured or reported values only; the rest is marked `unavailable`.
- **Share** a bundle that opens offline, with a hash chain over every event. Anchors streamed to a reviewer during the session prove nothing was edited afterwards.
- **Recover** after a crash or reboot: what is left, and the agent's own resume command. Nothing restarts on its own.
- **Run** detached: close the tab, lose SSH, reattach from any terminal with the screen restored.

Works with Claude Code, Codex CLI, Gemini CLI, OpenCode and any other program. One binary, one SQLite file, no daemon, no account. Claude Code gets the deepest integration (exact resume, agent state, tool calls and tokens); the others are recorded the same way but resume their most recent session and report no hook events.

If all you need is a session that survives a closed tab, tmux or the agent's background mode already do that.

## Install

Linux (x86_64, aarch64) or macOS (Apple silicon, Intel).

```
curl -fsSLO https://raw.githubusercontent.com/Druidder-Labs/agentrecorder/main/install.sh
sh install.sh          # to ~/.local/bin; AGENTRECORDER_VERSION and AGENTRECORDER_INSTALL_DIR override
```

Or `cargo install agentrecorder --locked` (Rust 1.95 or newer). Completions: `agentrecorder completions zsh`.

## Use

```
agentrecorder run -- claude              # Ctrl-\ detaches, the agent keeps running
agentrecorder sessions
agentrecorder attach last
agentrecorder inspect last
agentrecorder replay last --speed 2
agentrecorder view last                  # the dashboard, on this machine only
agentrecorder bundle last --prompts --diff --redact-secrets --zip -o ~/review.zip   # one file to send, outside the repository
```

| Command | What it does |
| --- | --- |
| `run -- CMD` | Start CMD supervised and attach; `-d` detached; `--anchor FILE`, `--anchor-token T` for reviewers |
| `sessions` | List sessions |
| `attach ID`, `detach ID` | Connect this terminal, or disconnect the attached one |
| `mark ID [LABEL]` | Mark a moment; `replay --from-mark` starts there |
| `inspect ID` | Process, terminal, git, tool calls, tokens; `--prompts`, `--json` |
| `recover ID` | Reattach, or report what is left and offer `--restart` or `--resume` |
| `replay ID` | Play back; `--speed`, `--from`, `--instant`, `--follow` |
| `stop ID` | Stop every process, escalating to SIGKILL |
| `export ID` | JSON Lines or asciicast v2/v3; `--follow` |
| `bundle ID` | Shareable directory or `--zip` with an overview and evaluation guide for reviewers, the prompts in order and the code changes; `--prompts`, `--diff`, `--redact-secrets`, `--redact`, `--cut`, `--dry-run` |
| `view DIR\|ID` | Open a dashboard in the browser |
| `anchor ID`, `verify DIR` | Stream anchors during a session; check a bundle against them and `--token` |
| `rm ID`, `prune` | Delete finished sessions |
| `hook` | Record an agent hook event; `run` wires it into Claude Code |

`ID` accepts a unique prefix or `last`. Every command has `--help`; the reference is `docs/cli.md`.

## Privacy

Output is recorded by default (`--no-record` to disable); keyboard input only with `--record-input`. Environment variables are never stored. State files are 0600, recordings older than 30 days are deleted, and replay strips sequences that could write the clipboard or inject keystrokes. Threat model: `docs/security.md`.

## Documentation

- `docs/cli.md`: command reference, session states, recovery, agent integration, sharing
- `docs/architecture.md`: components, process model, persistence
- `docs/event-model.md`: event schema and bundle format
- `docs/security.md`: threat model and controls
- `docs/performance.md`: baseline numbers and bounded resources
- `docs/release.md`: release flow
- `CHANGELOG.md`

## Development

```
just check          # fmt-check, clippy -D warnings, tests
just check-ci       # plus cargo deny, cargo package, quick bench
just clean-package  # when `just package` fails on a type just added to a workspace crate
```

MIT license.
