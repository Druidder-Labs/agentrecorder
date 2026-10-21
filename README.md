# agentrecorder

Run it. Inspect it. Recover it. Replay it.

A coding agent started in a terminal dies with that terminal. When it misbehaves, the output has already scrolled away. After a crash or a reboot, nothing says what survived. `agentrecorder` runs the agent for you and keeps the answers:

- **Run.** The agent keeps running when you close the tab, lose SSH or your client crashes. Reattach from any terminal and the screen comes back.
- **Inspect.** Process tree, CPU, memory, time since the last output, git HEAD before and after. Measured values only; anything else is marked `unavailable`.
- **Recover.** After a crash or reboot it states what is left (a live process, an exit code and the last screen, or only the recording) and offers the agent's own resume command. It never restarts anything on its own.
- **Replay.** Play the session back with its original timing, or export it as asciicast. Replay renders recorded bytes; it never re-executes a command.

Works with Claude Code, Codex CLI, Gemini CLI, OpenCode and any other program, inside the terminal you already use. One binary, a local SQLite file, no daemon to configure, no account.

## Install

Linux or macOS, Rust 1.95 or newer.

```
git clone https://github.com/Druidder-Labs/agentrecorder
cd agentrecorder
cargo install --path crates/agentrecorder --locked
```

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
| `inspect ID` | Measured process, terminal and git data; `--json` for scripts |
| `recover ID` | Reattach if alive; otherwise report what is left and offer `--restart` or `--resume` |
| `replay ID` | Play back recorded output; `--speed`, `--from`, `--instant` |
| `stop ID` | Stop every process in the session, escalating to SIGKILL |
| `export ID` | JSON Lines, asciicast v2 or asciicast v3 |
| `rm ID`, `prune` | Delete finished sessions and their recordings |

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
- `CHANGELOG.md`

## Development

```
just check        # fmt-check, clippy -D warnings, all tests
just check-ci     # check plus cargo deny and a quick benchmark run
```

## License

MIT
