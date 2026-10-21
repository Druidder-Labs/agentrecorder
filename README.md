# agentrecorder

A flight recorder for coding agents.

When an agent misbehaves, the output has already scrolled away. After a crash or a reboot, nothing says what survived. When someone asks what the agent did, there is nothing to show. `agentrecorder` runs the agent, records every byte it prints with timing, and answers afterwards:

- **Replay.** Play the session back with its original timing, or export it as asciicast. Replay renders recorded bytes; it never re-executes a command.
- **Inspect.** Tool calls and token usage from the agent's hooks and transcript, with the tokens of each prompt (the prompt text with `--prompts`), files changed in the git work tree, process tree, CPU, memory, time since the last output, command and failure counts when the shell emits OSC 133 marks, and for Claude Code whether the agent is working, idle or waiting for you. Measured or reported values only; anything else is marked `unavailable`.
- **Share.** Bundle a session into a dashboard that opens offline, with a hash chain over every event. Anchors streamed to a third party while the session ran let a reviewer verify that nothing was edited afterwards.
- **Recover.** After a crash or reboot it states what is left (a live process, an exit code and the last screen, or only the recording) and offers the agent's own resume command. It never restarts anything on its own.
- **Run.** The supervisor keeps the agent running when you close the tab, lose SSH or your client crashes; reattach from any terminal and the screen comes back. tmux, Zellij or the agent's own background mode cover this part too; the recording is what they lack.

Works with Claude Code, Codex CLI, Gemini CLI, OpenCode and any other program, inside the terminal you already use. One binary, a local SQLite file, no daemon to configure, no account.

## When to use it

- You need to review what an agent did after the fact: a timed recording, markers, exit status, tool calls, tokens, files changed, git HEAD before and after, and a JSON Lines or asciicast export.
- You need to show a session to someone else, such as an interviewer or a pull request reviewer, and they need to trust that it was not edited.
- You build tooling that needs a machine-readable event stream of a terminal session.
- You run agents without tmux, or several agents at once, and want each one isolated in its own supervisor.

If an agent only has to survive a closed tab, tmux, Zellij or the agent's own background mode already do that, and `agentrecorder` adds little. Claude Code has the deepest integration (exact resume, agent state, tool calls and tokens from its hooks and transcript); Codex CLI, Gemini CLI and OpenCode are recorded the same way but resume their most recent session, which can be a different conversation, and report no hook events unless their hooks are configured by hand.

## Install

Linux (x86_64, aarch64) or macOS (Apple silicon, Intel). Prebuilt binaries are attached to each GitHub release; `install.sh` picks the one for this machine, checks its sha256 and copies it to `~/.local/bin`:

```
curl -fsSLO https://raw.githubusercontent.com/Druidder-Labs/agentrecorder/main/install.sh
sh install.sh                      # AGENTRECORDER_VERSION=vX.Y.Z and AGENTRECORDER_INSTALL_DIR=... override the defaults
```

With Rust 1.95 or newer, `cargo install agentrecorder --locked` builds from crates.io instead, and `cargo install --path crates/agentrecorder --locked` builds from a checkout. Shell completions: `agentrecorder completions zsh` (also `bash`, `fish`, `elvish`).

## Try it

```
$ agentrecorder run -- claude            # work as usual; Ctrl-\ detaches, the agent keeps running
[agentrecorder] detached from session 3f9a1c; reattach with: agentrecorder attach 3f9a1c

$ agentrecorder sessions
SESSION   STARTED    STATUS        DURATION  EXIT       DIRECTORY                 COMMAND
3f9a1c    12m ago    detached       12m 04s  -          ~/src/app                 claude

$ agentrecorder attach last              # screen and scrollback are redrawn; `last` is the newest session
$ agentrecorder inspect last             # process tree, CPU, memory, idle time, git state
$ agentrecorder replay last --speed 2    # watch what happened
$ agentrecorder bundle last --redact-secrets && agentrecorder view last   # share it
```

The same sequence, with `bash` in place of `claude`, runs as an automated test against real PTYs on every commit.

## Commands

| Command | What it does |
| --- | --- |
| `run -- CMD...` | Start CMD in a supervised session and attach; `-d` starts it detached |
| `sessions` | List sessions and their status |
| `attach ID`, `detach ID` | Connect this terminal to a session, or disconnect the attached one |
| `mark ID [LABEL]` | Mark a moment in a running session; `replay --from-mark` starts there |
| `inspect ID` | Measured process, terminal, git, tool call and token data, tokens per prompt; `--prompts` shows the prompt text, `--json` for scripts |
| `recover ID` | Reattach if alive; otherwise report what is left and offer `--restart` or `--resume` |
| `replay ID` | Play back recorded output; `--speed`, `--from`, `--instant`, `--follow` for a running session |
| `stop ID` | Stop every process in the session, escalating to SIGKILL |
| `export ID` | JSON Lines, asciicast v2 or asciicast v3; `--follow` streams a running session |
| `bundle ID` | Write a shareable directory: dashboard, asciicast, events, hash chain; `--redact-secrets`, `--redact`, `--cut` and `--dry-run` edit and preview it first |
| `view DIR\|ID` | Open the dashboard of a bundle or a finished session in the browser |
| `anchor ID`, `verify DIR` | Stream hash-chain anchors to a third party while a session runs (`run --anchor FILE` does it for you); check a bundle against them |
| `rm ID`, `prune` | Delete finished sessions and their recordings |
| `hook` | Record an agent hook event; `run` wires it into Claude Code for you |

Every command has `--help`. The full reference, including scripting behavior and what each recovery case means, is in `docs/cli.md`.

## Sharing a session

`agentrecorder bundle last` writes a directory that opens in any browser, offline: summary, terminal player, a clickable timeline of markers, agent hook events, commands and git state, and the integrity data. `--redact-secrets` masks common key and token shapes, `--redact-literal` masks exact text, `--cut` removes a time range, and `--dry-run` shows what would change before anything is written. For a reviewer, such as a technical interview or a pull request, `anchor` streams hash-chain anchors while the session runs and `verify` checks the bundle against them later. A bundle shows how a session went in the recorded terminal; it does not show what happened outside it. Details: `docs/cli.md`, Sharing a session.

## Recording and privacy

- Output is recorded by default; `--no-record` turns it off. Keyboard input is recorded only with `--record-input`.
- Environment variables are never stored. State files are readable only by your user, and recordings older than 30 days are deleted automatically.
- Replay strips terminal sequences that could write your clipboard or inject keystrokes.

Threat model and controls: `docs/security.md`.

## Documentation

- `docs/cli.md`: command reference, session states, recovery cases, scripting, platform notes
- `docs/architecture.md`: components, process model, replay model
- `docs/event-model.md`: event schema and bundle format
- `docs/security.md`: threat model and controls
- `docs/performance.md`: measured baseline and event-loss behavior
- `docs/release.md`: how releases and crates.io publishing work
- `CHANGELOG.md`

## Development

```
just check        # fmt-check, clippy -D warnings, all tests
just check-ci     # check plus cargo deny, cargo package and a quick benchmark run
just clean-package  # when `just package` fails locally on a type that was just added to a workspace crate
```

## License

MIT
