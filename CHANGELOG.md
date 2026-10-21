# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `agentrecorder run`, `sessions`, `attach`, `detach`, `inspect`, `recover`, `replay`, `stop`, `export`, `rm` and `prune`.
- One detached supervisor per session that owns the PTY, survives client disconnects and records a typed event log in SQLite.
- Reattach redraws the screen, up to 1000 lines of scrollback, and the modes vt100 does not track (focus reporting, kitty keyboard flags, modifyOtherKeys, scroll region, title).
- `recover` distinguishes a live session, an exited process, a lost supervisor (host restart or crash) and an unreachable supervisor; `--restart` and `--resume` run only on request.
- Observational `replay` with speed, offset, idle cap, escape-sequence allowlist, screen snapshots for seeking and resync after dropped output.
- `export` as JSON Lines, asciicast v2 and asciicast v3 (markers and exit status).
- `inspect` reports measured values only: process tree, CPU, RSS, output idle time, terminal notifications (OSC 9, 99, 777 and BEL), git HEAD at start and exit.
- A missing socket is recreated: the CLI sends SIGUSR1 to a live supervisor.
- Benchmarks (`bench/bench.py`), PTY integration tests on Linux and macOS, `cargo deny` policy, workspace lints, CI with an MSRV job.
