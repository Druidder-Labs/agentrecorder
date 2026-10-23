# Witnessed

The verifiable record of AI-assisted work.

Witnessed records your coding-agent sessions, shows you every decision you made along the way, and gives a reviewer a record they can check without trusting you.

## Install

```
curl -fsSL https://raw.githubusercontent.com/Druidder-Labs/witnessed/main/install.sh | sh
```

Linux and macOS. The script verifies the release checksum and installs one binary to `~/.local/bin`.

## Quickstart

**1. Record.** Run your agent through `witnessed`. It works the same as before; `Ctrl-\` detaches and the agent keeps running.

```
witnessed run -- claude
```

**2. Review your decisions.** Every approval, refusal, interruption and correction, with what happened next and a link into the replay. Mark each one: would you decide again?

```
witnessed review
witnessed review --mark p42 --verdict differently --note "should have run the tests first"
```

**3. Share.** One file a reviewer opens offline: an overview, the prompts in order, your decisions, the code changes, and a hash chain over every event.

```
witnessed bundle --prompts --diff --redact-secrets --zip
witnessed verify ~/witnessed-a91f2c.zip
```

`witnessed view` opens any session in your browser.

See a recorded session: https://druidder-labs.github.io/witnessed/. Its `hello-world.zip` and `anchors.jsonl` are next to it, so `witnessed verify hello-world.zip --anchors anchors.jsonl` checks it.

## How it works

- **Record.** A supervised terminal records every byte with timing in one local SQLite file. No daemon, no account, nothing leaves your machine. Claude Code gets the deepest integration: tool calls, approvals, refusals, tokens. Codex CLI, Gemini CLI, OpenCode and any other program are recorded as terminals.
- **Review.** Decision moments come from the agent's hooks and transcript. Outcomes come from git: how many lines the session added are still in `HEAD`, and which later commits reverted them.
- **Prove.** Each event is chained with SHA-256. Anchors streamed to a reviewer while the session runs, and a token they issued, turn "these files agree" into "this is what happened". `verify` says exactly what it proved.

It records the terminal it runs in. It does not see other windows, devices or people.

## Privacy

Terminal output is recorded by default, keyboard input only with `--record-input`, and the code diff at session end so outcomes can be measured. `--no-record` turns all of it off. Environment variables are never stored. Recordings are private to your user and deleted after 30 days. Threat model: [docs/security.md](docs/security.md).

## Docs

- [Commands](docs/cli.md)
- [Architecture](docs/architecture.md)
- [Event model and bundle format](docs/event-model.md)
- [Security](docs/security.md)
- [Performance](docs/performance.md)
- [Releases](docs/release.md)

## Contributing

```
just check      # format, lint, test
just check-ci   # what CI runs
```

MIT license.
