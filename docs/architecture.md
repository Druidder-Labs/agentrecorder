# Architecture

## Components

```
agentrecorder (CLI client)          crates/agentrecorder
  |-- run / attach ----------> Unix socket (0600) --> supervisor (one process per session)
  |-- sessions / inspect ----> SQLite store + socket status request; may mark a session lost
  |-- rm / prune ------------> SQLite store: deletes finished sessions, checkpoints the WAL
  |-- recover / replay ------> SQLite store (read)
  |-- export ----------------> SQLite store (read)
  |-- bundle / view / anchor -> SQLite store (read); bundle and view write a directory with the dashboard
  `-- verify ----------------> bundle directory only (no store)

supervisor                     crates/agentrecorder-supervisor
  |-- client.rs     public: status, stop, detach, Attachment (typed; the only way to talk to a session)
  |-- layout.rs     public: state directory, socket and log paths, permission checks
  |-- screen.rs     public: ScreenModel, the vt100 screen used by the supervisor, recover and replay
  |-- modes.rs      private: tracks modes vt100 does not (focus, kitty keyboard, modifyOtherKeys, scroll region, title) and notifications
  |-- pty.rs        private, with three re-exported helpers; all unsafe code lives here
  |-- supervisor.rs private: reader, writer, listener and per-connection threads
  |-- recorder.rs   private: bounded queue -> batching writer thread
  |-- protocol.rs   private: length-prefixed frames
  `-- peer.rs       private: peer uid check

store                          crates/agentrecorder-store
  |-- events.rs     typed event bodies (`Body`), the single definition of the event schema
  `-- lib.rs        schema and migrations, typed `Status` and `LostReason`, guarded transitions, deletion
```

Module interfaces:

- `agentrecorder_supervisor::client` is the seam between the CLI and a running session. Callers get `SessionStatus`, `ConnectError::{NotRunning, Io, Rejected}` and an `Attachment` that yields `AttachEvent`s with typed `DetachReason`s; frame formats and protocol versions stay private to the crate.
- `agentrecorder_supervisor::screen::ScreenModel` rebuilds a terminal screen from output bytes and resizes, and reports terminal notifications (OSC 9, 99, 777 and BEL) found in the bytes. The supervisor uses it for the live screen and reattach snapshots; `recover` (last screen) and `replay --from` (seek) use it on recorded events.
- `agentrecorder_store::Body` is the event schema. Writers construct `Body` values; readers call `Event::body()` and match. `export` writes the stored JSON unchanged, so events from a newer writer pass through even when this binary cannot decode them.
- `agentrecorder_store::Status` (`Starting`, `Running`, `Exited`, `Lost`) is the stored lifecycle. Transitions are guarded in SQL: `mark_started` only from `starting`, `mark_lost` only from `starting` or `running`, `mark_exited` from any non-exited state.
- `crates/agentrecorder/src/state.rs` turns a stored session plus a liveness probe into one `State` (`Starting`, `Running(SessionStatus)`, `Exited`, `Lost`, `Unreachable`). Every command branches on that enum.

The layers map to the requested separation as follows.

| Layer | Code |
| --- | --- |
| CLI | `crates/agentrecorder/src/main.rs`, `commands/` |
| Execution control | `crates/agentrecorder/src/attach.rs`, `state.rs`, `commands/run.rs`, `commands/sessions.rs`, `crates/agentrecorder-supervisor/src/client.rs` |
| Session supervisor | `crates/agentrecorder-supervisor/src/supervisor.rs` |
| PTY and process runtime | `crates/agentrecorder-supervisor/src/pty.rs` |
| Event collection | `crates/agentrecorder-supervisor/src/recorder.rs` |
| Persistent event store | `crates/agentrecorder-store/src/lib.rs`, `events.rs` |
| Inspection, recovery, replay | `crates/agentrecorder/src/commands/{inspect,recover,replay,export}.rs`, `procinfo.rs`, `render.rs`, `sanitize.rs`, `crates/agentrecorder-supervisor/src/screen.rs` |
| Sharing | `crates/agentrecorder/src/commands/bundle.rs` (`bundle`, `anchor`, `verify`, `view`), `chain.rs` (hash chain), `dashboard.html` (template compiled into the binary) |

There is no TUI in the MVP. `ratatui` was not added because no command needs a full-screen interface yet.

## Process model

The life of one session, from `run` to exit:

```
client (run)                supervisor (__supervise)               CMD           store (state.db)
    |                               |                               |                   |
    |-- open store, retention, id ------------------------------------------------------>|
    |-- exec self, setsid -------->|                               |                   |
    |                               |-- insert session row ---------------------------->|
    |                               |-- open PTY, spawn ----------->|                   |
    |                               |-- bind run/ID.sock            |                   |
    |                               |-- status = running ------------------------------>|
    |-- status? (poll 5 ms) ------->|                               |                   |
    |-- attach -------------------->|                               |                   |
    |<-- held output or snapshot ---|                               |                   |
    |-- input --------------------->|-- PTY writer ---------------->|                   |
    |                               |<-- PTY reader ----------------|                   |
    |                               |-- recorder batches ------------------------------>|
    |<-- output frames -------------|                               |                   |
    |                               |<-- exit ----------------------|                   |
    |                               |-- process.exited, git, terminated --------------->|
    |                               |   (waits for commit)          |                   |
    |<-- Exited frame --------------|                               |                   |
    |   exit with CMD's code        |-- remove socket, exit         |                   |
```

A detach (`Ctrl-\`, `agentrecorder detach`, a lost terminal) ends only the client; the supervisor, CMD and recording continue, and the next `attach` starts again at the attach step.

`agentrecorder run -- CMD` does the following:

1. Opens the store, applies retention, allocates a 6-hex-digit session id from `/dev/urandom`, and checks that the socket path fits the platform limit before anything is created. For a Claude Code command it inserts `--session-id <uuid>` and `--settings` with hooks that call `agentrecorder hook` (`adapters::prepare`, rules in `cli.md`, Agent integration) and passes the uuid to the supervisor, which stores it as `agent_session_id`.
2. Re-executes its own binary as `agentrecorder __supervise --id ID ...` with `setsid()`, stdin and stdout on `/dev/null`, stderr appended to `logs/ID.log`. The supervisor has no controlling terminal, so closing the client's terminal does not send it SIGHUP.
3. The supervisor inserts the session row, opens a PTY with `IUTF8` set, spawns CMD as a session leader with the PTY slave as its controlling terminal, binds `run/ID.sock`, and only then moves the row to `running`. Between fork and exec the child gets default signal dispositions and an empty signal mask, every inherited descriptor above 2 is marked close-on-exec, and the umask of the shell that ran `agentrecorder` is restored (agentrecorder itself runs with umask 077). `TERM` is inherited; when it is empty, `xterm-256color` is set. The same reset is applied when the supervisor itself is started.
4. The client polls every 5 ms until the supervisor answers a status request (10 s timeout), then attaches. Unless `-d` was given, `run` passes `--hold-output`: the supervisor keeps the output produced before the first attach, up to 4 MiB, and sends it to that client instead of a screen snapshot, also when the process already exited, so a script reading `run`'s stdout gets every byte from the start. Past 4 MiB the buffer is dropped, a `runtime.warning` with code `pre_attach_output_dropped` is recorded, and the first client gets the snapshot. With `-d` it prints the id and exits. If CMD already finished, the client prints its recorded output and exits with its code.

One supervisor per session was chosen over one server for all sessions (tmux, Zellij). A crash in one supervisor affects one session. dtach, abduco, shpool and zmx use the same model.

The supervisor is a set of OS threads, not a Tokio runtime. Each session has one PTY, at most one attached client and a few short-lived control connections; blocking I/O on dedicated threads keeps the code small and makes backpressure explicit.

### Supervisor threads

- PTY reader: reads up to 64 KiB from the master, counts bytes, offers the chunk to the recorder (non-blocking), feeds the screen model under the terminal lock, then hands the chunk to the attached client. For an interactive client (its stdout is a terminal) the chunk is offered without waiting, under the terminal lock. When the 256-frame queue is full, the reader discards that client's output instead of waiting, so the managed process never runs at the pace of a terminal. Once the queue is down to half, the reader (on new output) or the client writer (every 100 ms) sends a redraw of the current screen from the screen model, under the same lock, and the client is in step again; a `runtime.warning` with code `client_output_discarded` records the discarded bytes. This is the discard-and-redraw approach of tmux (`tty.c`). A client that stops reading entirely is detached with reason `client_too_slow` when its socket's 5 s write timeout expires. For a non-interactive client (stdout is a pipe or file) the send waits for the client, the way a pipe applies backpressure, so scripts receive all output and the exit code. Backpressure ends when the client is detached (`agentrecorder detach`, another attach) or the session is stopped: the pending send is abandoned, and a client that does not take its final frame within 1 s has its socket shut down, so a stalled pipe never freezes the session or blocks `stop`.
- Snapshots: when output recording is on, the reader writes a `terminal.snapshot` event (the screen state as bytes) every 8 MiB of output and right after the recorder reports dropped events. Replay and `recover` start from the latest snapshot instead of parsing the whole recording, and replay redraws from a snapshot after a gap.
- Notifications: OSC 9, 99 and 777 notifications and BEL are recorded as `terminal.notification` events; repeats of the same kind within 1 s are counted but not recorded.
- Client writer (one per attached client): writes queued frames, then a final `Detached` or `Exited` frame. Interactive clients get a 5 s socket write timeout, so a client that stops reading cannot keep the supervisor alive.
- PTY writer: writes client input to the master from a 1024-chunk queue. Connection threads never block on it: when the queue is full, the chunk is dropped and recorded as a `runtime.warning` with code `input_dropped`, so a `Detach` frame is always read. Input typed before a detach stays queued and still reaches the process.
- Listener: accepts connections and starts one thread per connection; accept errors back off for 100 ms.
- Connection thread: checks the peer uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS), then reads the `Hello` frame and, for control connections, the request, with one 5 s deadline for both. It dispatches `attach` or `control` (`status`, `stop`, `detach`, `mark`, `agent_hook`). `mark` records a `user.mark` event through the recorder, so markers share the session's sequence and clock. `agent_hook` comes from `agentrecorder hook` running inside the session; the supervisor records it as `agent.hook` through the same bounded path as output, so a hook flood cannot use the slots reserved for lifecycle events.
- Stop timer: one per `stop` request that arms escalation; sends SIGKILL when the grace period ends.
- Signal handler: SIGTERM, SIGINT and SIGHUP stop the session (see Stop); SIGUSR1 binds the socket again, which repairs a session whose socket file was deleted. The CLI sends SIGUSR1 itself when it finds a live supervisor without a socket.
- Git snapshots: when the working directory is a git work tree, `git rev-parse HEAD` and `git status --porcelain` run once at start and once at exit (with `GIT_OPTIONAL_LOCKS=0`, so they never take the index lock), recorded as `git.snapshot` events with the commit id and a count of changed paths.
- Recorder: see Persistence.
- Main thread: waits for the session leader, finishes a pending stop (below), drains output for up to 500 ms, records `process.exited`, the exit `git.snapshot` and `session.terminated`, waits until the recorder confirms every event and the exit metadata are committed (up to 65 s), then hands `Exited` to the client writer and removes the socket. A client therefore never sees the exit before the recording is complete, so `agentrecorder run ...; agentrecorder export ...` in a script reads the whole session. It waits for a non-interactive client to read everything (up to 120 s), or up to 10 s for an interactive one, and exits with the child's exit code, or 128 + signal when the child was killed.

Because the socket outlives the commit, a client never sees "socket gone, row still running" for a session that exited normally. A client that finds no socket for a `running` row checks whether a process with that pid is still `agentrecorder __supervise --id ID`; if so, the session is `unreachable`, not `lost`.

### Attach and screen restore

The supervisor keeps a `ScreenModel` with 5000 lines of scrollback. On attach it resizes the PTY to the client's size (rows and columns are capped at 1000) and sends a snapshot, in frames of at most 64 KiB: up to 1000 scrollback lines (normal screen only), then the scroll region, the visible screen, cursor and input modes, then the modes vt100 does not track (focus reporting, kitty keyboard flags, modifyOtherKeys, window title). When the session is on the alternate screen, the snapshot starts with `CSI ? 1049 h`, so a full-screen program is redrawn on the alternate screen. Snapshot and client registration happen under one lock, so no output is lost or duplicated between them. A new attach detaches the previous client with reason `attached_elsewhere`.

On detach, exit, error, or SIGTERM, SIGINT or SIGHUP to the client, the client restores the terminal settings it saved, resets modes it may have inherited (all mouse modes and encodings, focus reporting, bracketed paste, keypad mode, modifyOtherKeys, kitty keyboard flags, scroll region) and leaves the alternate screen if the session had entered it. The detach key is not matched inside a bracketed paste, and both kitty (`CSI 92;5u`) and modifyOtherKeys (`CSI 27;5;92~`) encodings are recognised.

When stdin is not a terminal and reaches end of file (for example input from a file or a pipe), the client sends Ctrl-D (the terminal's end-of-file character) once and keeps reading output; it exits with CMD's exit code. The detach key is matched across reads; a partial kitty-protocol sequence of three or more bytes is held until the next read, so ESC and `ESC [` are forwarded at once.

The detach key defaults to `Ctrl-\`. The client matches both the raw byte and the kitty keyboard protocol form (`CSI 92;5u`), because some agents enable that protocol. `--detach-key none` disables it.

### Stop

`stop` sends the chosen signal (default SIGHUP, which ends interactive shells that ignore SIGTERM) first to the other members of the managed process's Unix session (same session id, found with `getsid`; this covers background jobs in other process groups), then to the PTY's foreground process group, then to the session leader's process group. Members are signalled before the leader because on macOS the terminal is revoked as soon as the leader exits. If the leader exits while members are still running, the main thread waits for them until the grace period ends and then sends them SIGKILL, recording `stop_escalated`. If the leader is still running when the grace period (default 5 s) ends, the stop timer sends SIGKILL to the whole session.

A later `stop` while one is pending sends its signal immediately, so `stop --signal KILL` always takes effect; only the first request arms the timer. SIGTERM, SIGINT or SIGHUP delivered to the supervisor itself trigger the same sequence with a 3 s timeout; `agentrecorder stop` uses that path for an `unreachable` session. Processes that called `setsid` themselves (daemons) are outside the session and are not signalled. The reply to `stop` is sent before signalling, so the client always gets an answer even when the supervisor exits right after.

## Recovery model

| Scenario | Detection | Behavior |
| --- | --- | --- |
| A: client disconnected | Socket answers a `status` request | `recover` reattaches |
| B: process exited | Session row has `status = exited` | Show exit data and the last recorded screen; `--restart` starts a new session with the same argv, working directory and recording cap after confirmation; `--resume` runs the agent's resume command for known agents |
| C: supervisor lost | Socket missing or refusing connections while the row says `running`, and no live `agentrecorder __supervise --id ID` process | Row becomes `lost` with `lost_reason` `host_restarted` (boot time differs by more than 5 s) or `supervisor_terminated`; show metadata and history, and offer `--restart` and `--resume` |
| Unreachable | Socket missing or not answering, but the supervisor process for this id exists | Reported as `unreachable`; `rm` refuses it; `stop` sends SIGTERM to the supervisor |

The environment is not stored, so `--restart` and `--resume` use the environment of the shell that runs `recover`. Resume adapters (`crates/agentrecorder/src/adapters.rs`) find the program behind a leading `env` and keep that prefix. A Claude Code session with a recorded `agent_session_id` resumes with `claude --resume <id>`, which is exact, and the new session's first `SessionStart` hook confirms it or not (`agent_state::resume_check`, verdicts in `cli.md`, Resume confirmation). Otherwise `claude`, `codex`, `gemini` and `opencode` map to their documented "continue the most recent session" command, which can pick a different conversation if the agent ran again in the same directory. `--restart` removes the inserted `--session-id` and `--settings` pairs before re-running the argv; a `--settings` value is treated as inserted only when its hooks all call `agentrecorder hook`.

## Replay model

| Kind | Status |
| --- | --- |
| Observational replay: re-render recorded output with timing | Implemented (`agentrecorder replay`) |
| Command re-execution: run the original argv again | Only as an explicit new session through `recover --restart`; never through `replay` |
| Deterministic process replay (rr-style) | Not implemented; requires recording syscalls and nondeterminism, out of scope |

`replay --from T` starts from the latest `terminal.snapshot` at or before T, feeds the remaining earlier output into a `ScreenModel` without timing, prints its state, then plays the rest. Timing is scheduled against a fixed start time, so per-event delays do not add up. A resize in the recording prints a notice; after replay the client resets colors and modes and discards terminal input that arrived during playback. `--max-idle` (default 3 s) caps gaps. Output passes through `sanitize.rs` unless `--raw` is given (see `docs/security.md`).

## Persistence

One SQLite database, `state.db`, in WAL mode with `synchronous=NORMAL` and `secure_delete=ON`. Each supervisor is the only writer of its session's events, so per-session sequence numbers are assigned without coordination. CLI commands write only lifecycle corrections (`mark_lost`) and deletions. Readers run concurrently through WAL.

The recorder batches up to 512 events, 4 MiB or 20 ms per transaction and merges output chunks that arrive within 5 ms of each other, up to 64 KiB. A write that fails because the database is busy or locked is retried every 100 ms for up to 10 s (60 s for the exit metadata); other errors are not retried. Readers fetch events in pages of 256 rows, each in its own short read transaction, so a long replay never pins the WAL. The WAL is capped with `journal_size_limit` (4 MiB); new stores use `auto_vacuum=INCREMENTAL`; an index on `(session_id, kind, seq, data_len)` serves kind filters. `rm` and `prune` delete orphaned events, run `incremental_vacuum` until every free page is returned to the file system and end with `wal_checkpoint(TRUNCATE)`; automatic retention does this only when it deleted something.

If the store cannot be opened when a session starts, `run` fails. If writes fail later, the session keeps running and failures are logged to `logs/ID.log`.

## Platform notes

| Topic | Linux | macOS |
| --- | --- | --- |
| Peer credential check | `SO_PEERCRED` | `getpeereid` |
| Socket path limit | 108 bytes | 104 bytes; `agentrecorder` errors and asks for a shorter `AGENTRECORDER_HOME` |
| PTY end of output | `read` returns `EIO` | `read` returns 0 or `EIO`; the terminal is revoked when the session leader exits |
| Doubled carriage returns | Not observed | When the reader falls behind a fast writer, the kernel emits `\r\r\n` in place of `\r\n` (reproduced with a plain Python `pty.fork` reader that sleeps 0.2 ms per read: 155 occurrences in `seq 1 300000`). Recordings contain what the kernel delivered. |
| Process metrics | `/proc` via `sysinfo` | `libproc` via `sysinfo`; processes of other users are not visible |
| Boot time | `/proc/stat` `btime` | `kern.boottime` |

Windows is not supported: the design depends on Unix PTYs, `setsid` and Unix sockets.
