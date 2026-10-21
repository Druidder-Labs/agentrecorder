# Security

## Assets and boundaries

Assets: terminal output and input (code, secrets, passwords), the environment of the managed process, the live PTY, recordings in `state.db`.

| Boundary | Assumption |
| --- | --- |
| Same Unix user | Trusted; it can already read `~/.ssh` or ptrace the agent |
| Other local users | Untrusted; must not attach, read or control |
| root | Out of scope |
| Bytes from the managed process | Untrusted; they are replayed into a terminal |
| Network | Not used |

## Controls

| Threat | Control | Where |
| --- | --- | --- |
| Another user attaches or controls a session | State directories 0700, sockets 0600, peer uid checked (`SO_PEERCRED`, `getpeereid`) | `layout.rs`, `peer.rs` |
| Another user reads recordings | umask 077: `state.db`, WAL and logs are 0600 | `main.rs` |
| Environment secrets persisted | The environment is never written; only argv and cwd | `supervisor.rs` |
| Typed passwords persisted | Input recording off unless `--record-input` | `run.rs` |
| Output captures secrets | `run` prints `(recording output)`; `--no-record`, `--max-record-mb` | `run.rs` |
| Recordings kept forever | Retention (`AGENTRECORDER_RETENTION_DAYS`, default 30), `prune`, `rm` | `run.rs`, `sessions.rs` |
| Deleted data stays on disk | `secure_delete=ON`, orphan cleanup, `wal_checkpoint(TRUNCATE)`; a reader that blocks the checkpoint is reported and a later `prune` scrubs | `store/lib.rs` |
| A live session is deleted | Only `exited` or `lost` sessions; a session whose supervisor still runs is `unreachable` | `state.rs` |
| Replayed output attacks the terminal | Allowlist: OSC 0-2, 7, 8 (`http`, `https`, `mailto`), 133 and non-query colors; CSI cursor, erase, scroll, SGR, tabs, DEC modes (cursor, wrap, origin, reverse, blink, alternate screen, synchronized output), cursor style, soft reset. Every query and report, DCS, APC, PM, SOS, C1, OSC 52 and vendor sequences are dropped; REP capped at 1024. The client resets modes afterwards and discards input typed during playback. `--raw` disables | `sanitize.rs` |
| Re-execution of destructive commands | `replay` executes nothing; `recover --restart` and `--resume` need confirmation or `-y` and a TTY | `recover.rs` |
| Malformed or slow protocol peers | Frames over 4 MiB and unknown kinds close the connection; `Hello` plus request within 5 s | `protocol.rs` |
| Unsafe process state inherited by the agent | Default signals, empty mask, close-on-exec above fd 2, the caller's umask | `pty.rs` |
| Supervisor git commands touch the repository | Read-only, `GIT_OPTIONAL_LOCKS=0`, 3 s bound; only commit id, paths and counts stored | `supervisor.rs` |
| Output injects text via notifications or titles | Control characters removed, 512 and 256 character caps | `modes.rs` |
| Transcript and hook data leak prompts | `inspect` reads the transcript read-only and takes only usage, ids, timestamps, origins and prompt text (shown only with `--prompts`, control characters replaced). `agentrecorder hook` keeps an allowlist of metadata fields; `prompt`, `tool_input` and `last_assistant_message` are dropped in the hook process; at `Stop` it records only token sums and model names. It prints nothing on stdout | `transcript.rs`, `hook.rs` |
| Attaching a session to itself | Refused when `AGENTRECORDER_SESSION` equals the target | `attach.rs` |
| Terminal left in raw mode | Restored on detach, exit, error, SIGTERM, SIGINT, SIGHUP | `attach.rs` |
| A shared bundle leaks a secret or a prompt | Prompt text enters a bundle only with `--prompts`. `--redact*` masks output, input, snapshots, payloads, metadata, the cast header and prompt text; `--cut` removes a range; patterns are not stored; files are 0600 | `bundle.rs` |
| Recorded output runs script in the dashboard | Data embedded as JSON with `<`, `>`, `&` escaped; `textContent` only; CSP with inline script and style only and no `connect-src` | `dashboard.html` |
| A bundle is edited afterwards | Hash chain over every event; `verify` rechecks events, chain, digests and anchors | `chain.rs` |
| A bundle for a session the reviewer did not issue | `run --anchor-token` seeds the chain with the token's SHA-256; only the digest is stored; `verify --token` | `chain.rs` |
| A zip bundle escapes its directory | Entries read by exact name under the manifest's prefix, never extracted; `view` writes only `index.html` under `views/ID`, id without `/` | `bundle.rs` |
| Corrupt store | Reported with the path, never overwritten | `store/lib.rs` |

Each row has an end-to-end or unit test; `grep` the control's name in `crates/*/tests` and the module.

## Residual risks

- argv is stored and visible in `ps`; a secret passed as an argument is persisted.
- Output recording stores what the agent prints. Use `--no-record` for sessions that print credentials.
- `--record-input` stores passwords typed at no-echo prompts.
- The attach snapshot is unfiltered; a same-user process can write to the terminal anyway.
- Redaction is pattern based. Redacted events are not hash-checked.
- A bundle is self-reported without anchors, and covers only the recorded terminal.
- An anchor token binds a session, not a person. It is visible to same-user processes and in shell history unless passed through `AGENTRECORDER_ANCHOR_TOKEN`.
- The vendored `asciinema-player` runs with `'wasm-unsafe-eval'`; version and checksum in `crates/agentrecorder/src/vendor/asciinema-player/NOTICE`.
- `install.sh` checks the sha256 attached to the same release: it detects corruption, not a compromised release. `cargo install --locked` builds from source.
- `secure_delete` does not reach filesystem snapshots, backups or SSD wear leveling; full-disk encryption covers those.
- `agent.hook` events and `client_pid` are self-reported; any same-user process can send them.

## Reporting

Report vulnerabilities through GitHub security advisories on the repository, not in public issues.
