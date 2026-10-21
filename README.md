# agentrecorder

Run it. Inspect it. Recover it. Replay it.

A flight recorder for coding agents. When an agent misbehaves, the output has already scrolled away; after a crash or a reboot, nothing says what survived. `agentrecorder` runs the agent, records every byte it prints with timing, and answers what happened afterwards:

- **Run.** The agent keeps running when you close the tab, lose SSH or your client crashes. Reattach from any terminal and the screen comes back.
- **Inspect.** Process tree, CPU, memory, time since the last output, git HEAD before and after, command and failure counts when the shell emits OSC 133 marks, and for Claude Code whether the agent is working, idle or waiting for you, as reported by its hooks. Measured or reported values only; anything else is marked `unavailable`.
- **Recover.** After a crash or reboot it states what is left (a live process, an exit code and the last screen, or only the recording) and offers the agent's own resume command. It never restarts anything on its own.
- **Replay.** Play the session back with its original timing, or export it as asciicast. Replay renders recorded bytes; it never re-executes a command.

Works with Claude Code, Codex CLI, Gemini CLI, OpenCode and any other program, inside the terminal you already use. One binary, a local SQLite file, no daemon to configure, no account.

## When to use it

- You need to review what an agent did after the fact: a timed recording, markers, exit status, git HEAD before and after, and a JSON Lines or asciicast export.
- You run agents without tmux, or several agents at once, and want each one isolated in its own supervisor.
- You build tooling that needs a machine-readable event stream of a terminal session.

If an agent only has to survive a closed tab, tmux, Zellij or the agent's own background mode already do that, and `agentrecorder` adds little. Claude Code has the deepest integration (exact resume, agent state from hooks); Codex CLI, Gemini CLI and OpenCode resume their most recent session, which can be a different conversation.

## Install

Linux or macOS, Rust 1.95 or newer.

```
cargo install agentrecorder --locked
```

To build from a checkout instead: `cargo install --path crates/agentrecorder --locked`.

## Try it

```
$ agentrecorder run -- claude            # work as usual; Ctrl-\ detaches, the agent keeps running
[agentrecorder] detached from session 3f9a1c; reattach with: agentrecorder attach 3f9a1c

$ agentrecorder sessions
SESSION   COMMAND         STATUS          DURATION  EXIT
3f9a1c    claude          detached         12m 04s  -

$ agentrecorder attach 3f9a1c            # screen and scrollback are redrawn
$ agentrecorder inspect 3f9a1c           # process tree, CPU, memory, idle time, git state
$ agentrecorder replay 3f9a1c --speed 2  # watch what happened
```

The same sequence, with `bash` in place of `claude`, runs as an automated test against real PTYs on every commit.

## Commands

| Command | What it does |
| --- | --- |
| `run -- CMD...` | Start CMD in a supervised session and attach; `-d` starts it detached |
| `sessions` | List sessions and their status |
| `attach ID`, `detach ID` | Connect this terminal to a session, or disconnect the attached one |
| `mark ID [LABEL]` | Mark a moment in a running session; `replay --from-mark` starts there |
| `inspect ID` | Measured process, terminal and git data; `--json` for scripts |
| `recover ID` | Reattach if alive; otherwise report what is left and offer `--restart` or `--resume` |
| `replay ID` | Play back recorded output; `--speed`, `--from`, `--instant`, `--follow` for a running session |
| `stop ID` | Stop every process in the session, escalating to SIGKILL |
| `export ID` | JSON Lines, asciicast v2 or asciicast v3; `--follow` streams a running session |
| `rm ID`, `prune` | Delete finished sessions and their recordings |
| `hook` | Record an agent hook event; `run` wires it into Claude Code for you |

Every command has `--help`. The full reference, including scripting behavior and what each recovery case means, is in `docs/cli.md`.

## Recording and privacy

- Output is recorded by default; `--no-record` turns it off. Keyboard input is recorded only with `--record-input`.
- Environment variables are never stored. State files are readable only by your user, and recordings older than 30 days are deleted automatically.
- Replay strips terminal sequences that could write your clipboard or inject keystrokes.

Threat model and controls: `docs/security.md`.

## Documentation

- `docs/cli.md`: command reference, session states, recovery cases, scripting, platform notes
- `docs/architecture.md`: components, process model, replay model
- `docs/event-model.md`: event schema
- `docs/security.md`: threat model and controls
- `docs/performance.md`: measured baseline and event-loss behavior
- `docs/release.md`: how releases and crates.io publishing work
- `CHANGELOG.md`

## Development

```
just check        # fmt-check, clippy -D warnings, all tests
just check-ci     # check plus cargo deny and a quick benchmark run
```

## License

MIT
