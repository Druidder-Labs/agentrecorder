# Security and threat model

## Assets

- Terminal output: source code, file contents, tokens printed by tools, customer data.
- Terminal input: passwords typed at prompts, API keys pasted into agents.
- Environment variables of the managed process: API keys and cloud credentials.
- The live PTY: anyone who can write to it can run commands as the user.
- Recordings at rest in `state.db`.

## Trust boundaries

| Boundary | Assumption |
| --- | --- |
| Same Unix user | Trusted. Any process running as the user can already read `~/.ssh`, ptrace the agent, or read its memory. `agentrecorder` does not defend against it. |
| Other local users | Untrusted. They must not attach, read recordings, or send control requests. |
| root | Out of scope. |
| Bytes produced by the managed process | Untrusted. They are replayed into the user's terminal and can contain escape sequences. |
| Network | Not used. `agentrecorder` opens no network sockets. |

## Controls

| Threat | Control | Code or test |
| --- | --- | --- |
| Another user attaches or sends `stop` | State directories are created 0700 and tightened if looser; ownership is checked; sockets are chmod 0600; the supervisor rejects peers whose uid differs (`SO_PEERCRED` / `getpeereid`), which also covers systems that ignore socket file permissions | `layout.rs`, `peer.rs`, `state_files_are_private` |
| Another user reads recordings | The process umask is 077, so `state.db`, its WAL and logs are 0600 | `main.rs`, `state_files_are_private` |
| Secrets in environment variables are persisted | The environment is never written to the store or logs. Only argv and cwd are stored. | `input_is_recorded_only_when_enabled_and_env_is_never_stored` |
| Typed passwords are persisted | Input recording is off by default and requires `--record-input` | same test |
| Output recording captures secrets | Output recording is on by default because replay and recovery depend on it; `run` (attached) prints `(recording output)` at start, while `run -d` prints only the session id; `--no-record` turns it off; `--max-record-mb` caps it | `run.rs` |
| Recordings kept forever | Finished sessions older than `AGENTRECORDER_RETENTION_DAYS` (default 30, 0 disables) are deleted when a new session starts; `agentrecorder prune` and `agentrecorder rm` delete on demand | `run.rs`, `sessions.rs` |
| Deleted data remains on disk | `secure_delete=ON` overwrites freed pages. `rm` and `prune` also delete events left without a session row, and always end with `wal_checkpoint(TRUNCATE)`. If another agentrecorder process holds a read transaction, the checkpoint cannot complete; `rm` and `prune` then warn that the data stays in `state.db-wal` until that process finishes, and running `prune` again afterwards scrubs it. | `delete_removes_events_and_session`, `delete_under_a_reader_reports_pending_wal_and_a_later_checkpoint_scrubs`, `orphan_events_are_removed` |
| A live session is deleted | `rm` and retention delete only `exited` or `lost` sessions. A session whose socket is missing while its supervisor process (matched by command line and session id) still runs is `unreachable`, not `lost`. | `missing_socket_with_live_supervisor_is_unreachable_not_lost` |
| Replayed output attacks the terminal | `replay` keeps only allowlisted OSC sequences (0-2 titles, 7 working directory, 8 hyperlinks, 133 prompt marks, and color settings that are not queries); everything else, including OSC 52 clipboard writes and vendor sequences such as OSC 1337 and 5522, is dropped. CSI sequences are also allowlisted: only cursor movement, erase, insert and delete, scrolling regions, SGR, tab stops, DECSET and DECRST modes, cursor style (`CSI Ps SP q`) and soft reset (`CSI ! p`) pass. Every report or query (DSR, DA, XTVERSION, DECRQM, DECRQSS, DECRQPSR, DECRQCRA, XTQMODKEYS, XTREPORTCOLORS, XTSMGRAPHICS, window ops, kitty keyboard) is dropped, as are DECID (`ESC Z`), all DCS, APC, PM and SOS strings, and C1 control characters in raw and UTF-8 encoded form. A sequence that contains a C0 or non-ASCII byte is dropped whole (including `ESC` followed by a raw C1 byte), OSC strings that contain C1 are dropped, and CAN, SUB and the C1 string terminator (raw or UTF-8 encoded) end strings as they do in terminals. Modes are allowlisted too: only cursor, wrap, origin, reverse video, blink, alternate screen and synchronized output (`CSI ? Pm h/l`) and insert and newline modes (`CSI Pm h/l`) pass, so replay cannot turn on mouse or focus reporting, bracketed paste or keyboard lock; REP counts above 1024 are dropped; OSC 7 is dropped and OSC 8 links are kept only for `http`, `https` and `mailto`; charset designations pass but `ESC %` and `ESC SP` sequences do not. After replay the client resets colors and modes and discards terminal input that arrived during playback. A recording therefore cannot write the clipboard or make the terminal type a response into the shell. `--raw` disables the filter. | `sanitize.rs` unit tests, `replay_honours_speed_offset_and_filters_dangerous_sequences` |
| Accidental re-execution of destructive commands | `replay` never executes anything. `recover --restart` and `--resume` start a new session only after an interactive confirmation or `--yes`, and refuse without a TTY | `recover_exited_session_shows_history_and_restarts_only_on_request` |
| Oversized or malformed protocol frames | Frames over 4 MiB and unknown frame kinds close the connection; the `Hello` frame and any control request must arrive within 5 s in total, so a peer cannot hold a connection thread by sending bytes slowly | `protocol.rs` |
| The agent inherits restrictive or unsafe process state from agentrecorder | The child starts with default signal dispositions and an empty signal mask (an ignored SIGINT or SIGHUP in the caller does not carry over), descriptors above 2 inherited from the calling shell are closed on exec, and the caller's umask is restored, so the 077 umask that protects agentrecorder's own files does not make the agent's files private | `the_child_gets_default_signals_the_users_umask_and_no_inherited_fds` |
| Supervisor-run git commands interfere with the user's repository | `git rev-parse` and `git status` run with `GIT_OPTIONAL_LOCKS=0`, never write, and are bounded to 3 s; only the commit id and a count of changed paths are stored | `notifications_idle_time_and_git_state_are_measured` |
| Output can inject text through notifications or titles | Notification and title text is stored with control characters removed and a length cap (512 and 256 characters) | `modes.rs` unit tests |
| Attaching a session to itself | Refused when `AGENTRECORDER_SESSION` equals the target id | `attach.rs` |
| Terminal left in raw mode | The client restores the saved terminal settings on detach, exit, errors, and SIGTERM, SIGINT or SIGHUP | `killing_the_client_restores_the_terminal` |
| Corrupt store hides recordings | `agentrecorder` reports the path and does not overwrite the file | `corrupt_store_is_reported_not_overwritten` |

## Residual risks

- argv is stored. A secret passed as a command-line argument (for example `--token xyz`) is persisted and shown by `inspect`. It is also visible to other users through `ps` on most systems.
- Output recording stores whatever the agent prints, including secrets it echoes. Use `--no-record` for sessions that print credentials.
- `--record-input` stores everything typed, including passwords at no-echo prompts.
- The live session snapshot is sent unfiltered on attach. A process running as the user can already write anything to the user's terminal, so this adds no capability.
- SQLite `secure_delete` does not reach copies made by filesystem snapshots, backups or SSD wear leveling. Full-disk encryption is the control for those.
- `client_pid` in `session.attached` is self-reported.
- Process metrics come from `sysinfo` and are readable by the same user only.

## Reporting

Report vulnerabilities privately to the maintainers through GitHub security advisories on the repository, not in public issues.
