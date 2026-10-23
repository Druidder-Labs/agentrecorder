# Architecture

## Components

```
witnessed (CLI)                 crates/witnessed-cli
  |-- run / attach ---------> Unix socket (0600) --> supervisor, one process per session
  |-- sessions / inspect ---> store + socket status; may mark a session lost
  |-- recover / replay / export / bundle / view / anchor --> store (read)
  |-- review ---------------> store (read, annotations), transcript, git
  |-- rm / prune -----------> store (delete, checkpoint)
  `-- verify ---------------> bundle only

  src/commands/   one module per subcommand: run, sessions (attach, detach, mark, stop, rm, prune),
  |               inspect, recover, replay, export, review, bundle, view, verify, anchor, hook
  |-- bundle.rs   BundleArgs, warnings, dry run, output directory and zip writing
  |-- verify.rs   Source (directory or zip), Failure / Proof / Gap, Verdict rendered as text or JSON
  |-- view.rs     dashboard from a session (assemble + write_bundle) or from a bundle
  `-- anchor.rs   hash-chain anchors streamed while a session runs
  src/bundle/     the bundle pipeline shared by `bundle` and `view`
  |-- assemble.rs Item / Fate, Include, Edits, assemble, Story, ReviewData, Manifest, Summary, write_bundle, dashboard
  |-- cut.rs      CutRange, CutReport, apply_cuts
  |-- diff.rs     DiffStatus, DiffInfo, session_diff
  `-- redact.rs   SECRET_PATTERNS, mask, apply_redactions, drop_binary_patches

supervisor                          crates/witnessed-supervisor
  |-- client.rs     public: status, stop, detach, Attachment (the only way to talk to a session)
  |-- layout.rs     public: state directory, paths, permission checks
  |-- screen.rs     public: ScreenModel (vt100 screen for live, recover and replay)
  |-- modes.rs      modes vt100 does not track, notifications
  |-- pty.rs        all unsafe code
  |-- supervisor/   one session
  |   |-- mod.rs        `Config`, `run`: spawn, wait, exit sequencing
  |   |-- shared.rs     `Shared`, `Term`, resize, notifications, `Latch`, `Gauge`
  |   |-- reader.rs     PTY reader, held output, snapshots
  |   |-- delivery.rs   `Client`, client writer, deliver, discard and resync
  |   |-- connection.rs listener, handshake, control requests, attach
  |   `-- stop.rs       stop, signal escalation, supervisor signal handlers
  |-- recorder.rs   bounded queue -> batching writer thread
  |-- git/
  |   |-- mod.rs        public: `output`, `worktree_diff`
  |   `-- snapshot.rs   start and exit `git.snapshot` events, session diff
  |-- protocol.rs   length-prefixed frames
  `-- peer.rs       peer uid check

store                               crates/witnessed-store
  |-- events.rs     `Body`: the event schema
  `-- lib.rs        schema, migrations, `Status`, guarded transitions, deletion
```

| Layer | Code |
| --- | --- |
| CLI | `crates/witnessed-cli/src/main.rs`, `commands/` |
| Execution control | `attach.rs`, `state.rs`, `commands/run.rs`, `supervisor/src/client.rs` |
| Supervisor, PTY, recorder | `supervisor/src/supervisor/`, `supervisor/src/{pty,recorder}.rs`, `supervisor/src/git/snapshot.rs` |
| Store | `store/src/{lib,events}.rs` |
| Inspect, recover, replay | `commands/{inspect,recover,replay,export}.rs`, `procinfo.rs`, `agent_state.rs`, `transcript.rs`, `render.rs`, `sanitize.rs`, `screen.rs` |
| Review | `commands/review.rs`, `decisions.rs` (decision moments from hooks and transcript, `Kind`), `outcome.rs` (session lines in `HEAD` and the work tree, later commits), `transcript.rs` (`locate`, `usage`) |
| Sharing | `commands/{bundle,view,verify,anchor}.rs`, `bundle/{assemble,cut,diff,redact}.rs`, `chain.rs`, `dashboard.html`, `vendor/asciinema-player/` |

Seams: `client` exposes `SessionStatus`, `ConnectError` and `Attachment` with typed `DetachReason`s; frames stay private. `Body` is the schema; `export` passes stored JSON through unchanged, so events from a newer writer survive an older reader. `Status` transitions are guarded in SQL. `state.rs` folds a session row plus a liveness probe into one `State` (`Starting`, `Running`, `Exited`, `Lost`, `Unreachable`) that every command branches on; `Need` (`Any`, `Finished`, `Live`) names what a command requires of it. `transcript::locate` is the one place that finds the agent transcript from the hooks; `inspect::SessionFacts` reads the event log once for `inspect`. `bundle::assemble` loads, cuts and redacts a session into typed `Story`, `DiffInfo`, `Manifest` and `Summary` values; redaction masks typed strings directly and the review material once it is serialized.

## Process model

```
client (run)              supervisor (__supervise)         CMD        store
  |-- open store, retention, allocate id ---------------------------->|
  |-- exec self, setsid -->|                                          |
  |                        |-- insert session row ------------------->|
  |                        |-- open PTY, spawn ----------->|          |
  |                        |-- bind run/ID.sock, status=running ----->|
  |-- attach ------------->|                                          |
  |<-- held output/snapshot|                                          |
  |-- input -------------->|-- PTY writer ---------------->|          |
  |<-- output frames ------|<-- PTY reader ----------------|          |
  |                        |-- recorder batches ------------------->|
  |                        |<-- exit ----------------------|          |
  |                        |-- exited, git, terminated; wait commit ->|
  |<-- Exited frame -------|-- remove socket, exit                    |
```

`run`:

1. Opens the store, applies retention, allocates a 6-hex id from `/dev/urandom`, checks the socket path length. For Claude Code, inserts `--session-id` and `--settings` (`adapters::prepare`).
2. Re-executes itself as `witnessed __supervise --id ID` with `setsid()`, stdin and stdout on `/dev/null`, stderr to `logs/ID.log`. No controlling terminal, so closing the client's terminal sends no SIGHUP.
3. The supervisor opens a PTY (`IUTF8`), spawns CMD as session leader, binds the socket, then marks the row `running`. The child gets default signal dispositions, an empty mask, close-on-exec on inherited descriptors above 2, the caller's umask (witnessed itself runs with 077) and `xterm-256color` when `TERM` is empty.
4. The client polls status every 5 ms (10 s timeout) and attaches. Without `-d` the supervisor holds up to 4 MiB of output produced before the first attach and sends it instead of a snapshot; past that it drops the buffer and records `pre_attach_output_dropped`.

Detach ends only the client. One supervisor per session (like dtach, abduco, shpool): a crash affects one session. Threads, not an async runtime: one PTY, one attached client, a few control connections.

### Supervisor threads

`run` is single-shot per process: threads are never joined and the signal handlers are registered once. The threads share one `Arc<Shared>` (`supervisor/shared.rs`); the screen model and the attached client live under its `term` mutex.

- **PTY reader** (`reader.rs`): reads up to 64 KiB, offers the chunk to the recorder without blocking, updates the screen model, hands it to the client. An interactive client (stdout is a terminal) has a 256-frame queue; when full, output for that client is discarded and, once the queue is half empty, the current screen is redrawn (`client_output_discarded` warning). A client that stops reading is detached as `client_too_slow` after the 5 s socket write timeout. A pipe client gets backpressure instead, released on detach or stop (1 s to take the last frame).
- **Snapshots**: a `terminal.snapshot` every 8 MiB of output and after dropped events; replay and `recover` start from the latest one.
- **Notifications**: OSC 9, 99, 777 and BEL become `terminal.notification`; repeats within 1 s are counted, not recorded.
- **Client writer** (`delivery.rs`): one per client, writes queued frames and a final `Detached` or `Exited`.
- **PTY writer**: 1024-chunk input queue; when full the chunk is dropped and `input_dropped` recorded, so a `Detach` frame is always read.
- **Listener and connection threads** (`connection.rs`): peer uid check (`SO_PEERCRED`, `getpeereid`; a rejection is logged), `Hello` plus request within 5 s, then `attach` or a control request (`status`, `stop`, `detach`, `mark`, `agent_hook`). A malformed frame from an attached client detaches it as `protocol_error`; a closed socket as `client_disconnected`. Marks, hook events, resizes, notifications, `input_dropped` and `client_output_discarded` go through the recorder's bounded path, so a client-driven flood cannot take the 512 lifecycle slots.
- **Signals** (`stop.rs`): SIGTERM, SIGINT and SIGHUP stop the session; SIGUSR1 rebinds the socket (the CLI sends it when the socket file is missing).
- **Git** (`git/snapshot.rs`): `git rev-parse HEAD` and `git status --porcelain` at start and exit with `GIT_OPTIONAL_LOCKS=0`, as `git.snapshot` events. The exit snapshot waits on the start one's `OnceLock`, which the start thread always sets within its 3 s bound.
- **Recorder** (`recorder.rs`): the batching writer; `finish` sets a shutdown flag before queueing the shutdown message, so a worker stuck behind a full queue and a stalled store ends after its current batch instead of blocking the exit.
- **Main** (`mod.rs`): waits for the leader, finishes a pending stop, drains output for 500 ms, records `process.exited`, the exit snapshot (storing the session-end diff for `review`) and `session.terminated`, waits for the commit (up to 65 s), then sends `Exited`, removes the socket and waits on a condition variable for the connection threads to finish (up to 120 s). A client that attaches after the exit waits on the same `recorded` latch. A client never sees the exit before the recording is complete. Exits with the child's code, or 128 + signal.

### Attach and screen restore

The supervisor keeps a `ScreenModel` with 5000 lines of scrollback. On attach it resizes the PTY (capped at 1000x1000) and sends up to 1000 scrollback lines, the scroll region, the visible screen, cursor and modes, in 64 KiB frames, under one lock with registration. Alternate-screen sessions redraw on the alternate screen. A new attach detaches the previous client as `attached_elsewhere`.

The client restores its terminal on detach, exit, error and signals, resets mouse, focus, paste, keypad and keyboard modes, and leaves the alternate screen. The detach key (`Ctrl-\`, raw or kitty `CSI 92;5u`) is not matched inside a bracketed paste. Off a terminal, stdin at end of file sends Ctrl-D once.

### Stop

`stop` signals (default SIGHUP) the other members of the managed process's Unix session first, then the foreground process group, then the leader's group, because macOS revokes the terminal when the leader exits. After the grace period (default 5 s) everything left gets SIGKILL (`stop_escalated`). A later `stop` sends its signal at once; only the first arms the timer. Signals to the supervisor itself run the same sequence with 3 s. Daemons that called `setsid` are not reached.

## Recovery

| Scenario | Detection | Behavior |
| --- | --- | --- |
| Client disconnected | Socket answers `status` | Reattach |
| Process exited | Row `exited` | Show exit and last screen; `--restart`, `--resume` |
| Supervisor lost | Socket gone or silent, row `running`, no `__supervise` process | Row becomes `lost` (`host_restarted` when boot time differs by more than 5 s, else `supervisor_terminated`); offer `--restart`, `--resume` |
| Unreachable | Socket gone or silent, supervisor process alive | `rm` refuses; `stop` sends SIGTERM to the supervisor |

Resume adapters (`adapters.rs`) keep a leading `env` prefix. Claude Code with a recorded `agent_session_id` resumes exactly and the first `SessionStart` hook confirms it; `claude`, `codex`, `gemini` and `opencode` otherwise map to their "most recent session" command. `--restart` strips the inserted `--session-id` and `--settings` first.

## Replay

Observational only: recorded output re-rendered with timing. `replay --from T` starts from the latest snapshot at or before T and feeds the rest into a `ScreenModel` without timing. Timing is scheduled against a fixed start, so delays do not accumulate; `--max-idle` (default 3 s) caps gaps. Output passes through `sanitize.rs` unless `--raw`. Re-execution exists only as `recover --restart`; deterministic replay is out of scope.

## Persistence

One SQLite file in WAL mode, `synchronous=NORMAL`, `secure_delete=ON`, `journal_size_limit` 4 MiB, incremental auto-vacuum. Each supervisor is the only writer of its session; CLI commands write only `mark_lost` and deletions.

The recorder batches 512 events, 4 MiB or 20 ms per transaction and merges output chunks within 5 ms up to 64 KiB. Busy or locked writes retry every 100 ms for 10 s (60 s for exit metadata); each attempt waits up to the 5 s `busy_timeout` first, so a held lock costs a batch up to 15 s and exit metadata up to 65 s before the write is reported as failed. `Store::open` retries a busy initialization every 50 ms for 10 s, each attempt under the same 5 s `busy_timeout`. Busy and locked errors are `Error::Busy` (`is_transient`); corruption and a newer schema are `Error::Corrupt` and `Error::NewerSchema`. Readers page 256 rows per short transaction (`Store::events`, which `for_each_event` wraps), so a writer is never blocked by a reader and a reader sees a contiguous prefix of the session. `rm` and `prune` delete orphans, run `incremental_vacuum` to completion and end with `wal_checkpoint(TRUNCATE)`. A store that cannot be opened fails `run`; later write failures are logged to `logs/ID.log` and the session continues.

## Platform notes

| Topic | Linux | macOS |
| --- | --- | --- |
| Peer credentials | `SO_PEERCRED` | `getpeereid` |
| Socket path limit | 108 bytes | 104 bytes |
| PTY end of output | `EIO` | 0 or `EIO`; terminal revoked when the leader exits |
| Doubled carriage returns | Not observed | `\r\r\n` when the reader falls behind |
| Process metrics | `/proc` | `libproc`; other users' processes not visible |
| Boot time | `/proc/stat` | `kern.boottime` |

A fast exit on macOS was tested on Darwin 27.0.0 with 2 MB queued in the PTY and a reader slowed to 10 ms per read: the output arrived complete with and without a slave fd held open, so no mitigation is applied for output loss at exit.

Windows is not supported (Unix PTYs, `setsid`, Unix sockets).
