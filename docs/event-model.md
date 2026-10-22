# Event model

Schema version 1 (`EVENT_SCHEMA_VERSION` in `crates/agentrecorder-store/src/lib.rs`). Adding a type or a field keeps the version; removing, renaming or changing a meaning increments it.

## Envelope

`agentrecorder export SESSION` writes one JSON object per line:

```json
{"schema_version": 1, "event_id": "a91f2c-1", "session_id": "a91f2c", "seq": 1,
 "timestamp": "2026-10-20T12:00:00.123456789Z", "mono_ns": 2305208,
 "event_type": "process.started", "payload": {"pid": 18420, "command": ["claude"]}}
```

| Field | Meaning |
| --- | --- |
| `event_id` | `<session_id>-<seq>` |
| `seq` | Per-session, contiguous from 0, assigned when persisted. The ordering key |
| `timestamp` | Wall clock, UTC RFC 3339 with nanoseconds; can jump with the system clock |
| `mono_ns` | Nanoseconds since the supervisor started; use for durations. Can be a few microseconds out of `seq` order across threads |
| `payload` | Type-specific object or `null` |
| `data_b64` | Raw bytes, base64, on `terminal.output` and `terminal.input` |

## Types

| Type | Payload |
| --- | --- |
| `session.created` | `command`, `cwd`, `rows`, `cols`, `record_output`, `record_input`, `supervisor_pid`, `restarted_from`, `max_record_bytes`, `term`, `agent_session_id` (when known) |
| `process.started` | `pid`, `command` |
| `session.attached` | `client`, `client_pid` (self-reported), `rows`, `cols` |
| `session.detached` | `client`, `reason`: `client_request`, `client_disconnected`, `detach_requested`, `attached_elsewhere`, `client_too_slow`, `process_exited`, `pty_write_failed`, `protocol_error` |
| `terminal.output` | bytes in `data_b64`; chunks within 5 ms merged up to 64 KiB |
| `terminal.input` | bytes; only with `--record-input` |
| `terminal.resized` | `rows`, `cols` |
| `terminal.snapshot` | `rows`, `cols`, `reason` (`interval`, `after_drop`); bytes that redraw the screen. Every 8 MiB and after drops; not part of the output stream |
| `terminal.notification` | `source` (`osc9`, `osc99`, `osc777`, `bell`), `text` (control characters removed, 512 max) |
| `session.stop_requested` | `signal`, `grace_ms`, `origin` (`client` or `supervisor_signal_<n>`) |
| `process.exited` | `pid`, `exit_code` or `signal`, `duration_ms` |
| `session.terminated` | `reason` (`process_exited`, `stopped`, `spawn_failed`), `exit_code`, `signal`, `dropped_events`. Always last |
| `git.snapshot` | `phase` (`start`, `exit`), `head`, `dirty_paths`, `dirty`; at exit `changed_paths`, `changed`, `new_commits`, `commits` (`{hash, subject}` of the commits since the start HEAD, newest first, capped at 100, subjects at 200 characters), `diff_sha256` and `diff_bytes` (of the diff `bundle --diff` writes; contents are not stored; absent when no HEAD was recorded at start, the snapshot took longer than 3 s, the diff longer than 5 s, the diff or the untracked files are over 32 MiB, or git failed); `new_commits` counts all commits since the start HEAD while `commits` lists up to 100; `changed` lists `{status, path}` against the start HEAD plus untracked as `?`. `root` is the repository's top level; paths are relative to it. Lists capped at 500, counts exact. Only in a git work tree |
| `runtime.warning` | `code`, `message`: `events_dropped`, `recording_limit_reached`, `stop_escalated`, `input_dropped`, `client_output_discarded`, `pre_attach_output_dropped`, with code-specific counts |
| `runtime.error` | `code` (`spawn_failed`, `resize_failed`), `message` |
| `shell.command_finished` | `exit_code` (null when the OSC 133 mark has none) |
| `user.mark` | `label` |
| `agent.hook` | `agent`, `hook_event_name`, and when sent: `session_id`, `transcript_path`, `cwd`, `source`, `reason`, `model`, `permission_mode`, `notification_type`, `message`, `tool_name`; at `Stop` and `SessionEnd` `usage` (`api_calls`, `input`, `output`, `cache_read`, `cache_creation`, `models`, `outside_session`); on tool events `tool` (`command`, `description`, `path`, `pattern`, `url`, `subagent`, `lines_added`, `lines_removed`, `error`, `interrupted`; absent with `--no-record`). Self-reported, recorded best effort |

A session without `session.terminated` was lost; `sessions` shows `lost` and `inspect` shows `lost_reason`.

## Storage

`state.db`, table `events` keyed by `(session_id, seq)`; `payload` as JSON text, `data` as a BLOB with `data_len` beside it. `PRAGMA user_version` is 5 (2: `data_len`; 3: kind index; 4: `agent_session_id`; 5: `anchor_token_digest`). Older stores migrate in place on first open. There is no downgrade: an older binary refuses a newer store, naming both versions and the update command, and leaves the file as it was. The payload shapes are the `Body` enum in `events.rs`; a unit test round-trips every variant.

## Other formats

`export --format asciicast-v2|asciicast-v3` writes `o` (output), `r` (resize) and `m` (markers for warnings, attach, detach, stop requests, notifications); v3 adds `x` (exit) and millisecond intervals without drift. Invalid UTF-8 becomes U+FFFD; DEL and C1 are escaped. Casts are not sanitized. OpenTelemetry export is not implemented.

## Bundle format

`bundle` writes a directory, or with `--zip` one file with the same entries under a directory named after it (`bundle_version` 1):

| File | Content |
| --- | --- |
| `manifest.json` | `format`, `bundle_version`, `generator`, `created_at`, `session`, `command_display`, `program`, `duration_ms`, `event_counts`, `chain` (`algorithm`, `events`, `genesis`, `token_bound`, `head`), `fates`, `redaction` (`patterns`, `matches`, `events`), `cuts`, `files` (`sha256`, `bytes` of the data files and `diff.patch`), `prompts` (`available`, `reason`, `source`, `text`, `cut`, `interrupts`, `refusals` and `compactions` as offsets in ms, `interactions`: `index`, `t_ms`, `origin`, `chars`, `images`, `api_calls`, `input`, `output`, `cache_read`, `cache_creation`, `total`, `reply_chars`, `model`, and `text` with `--prompts`; from the agent transcript, not hash-chained) |
| `events.jsonl` | The envelope above, one line per event not cut; redacted events carry redacted bytes |
| `chain.jsonl` | One line per recorded event: `seq`, `leaf`, `chain`, `fate` (`kept`, `redacted`, `cut`) |
| `session.cast` | asciicast v3 with a `cut FROM-TO` marker per range |
| `diff.patch` | With `--diff`: the code changes against the start HEAD; `manifest.json` `diff` has `status`, `bytes`, `files`, `base`, `sha256_at_exit` |
| `index.html` | The dashboard with everything inline; not listed in `files` |

Hash chain `sha256-chain-v1`, lowercase hex: `leaf` is SHA-256 of the exported JSON line before redaction; `chain` is SHA-256 of the previous chain (or the genesis for seq 0) followed by the leaf, as raw bytes. The genesis is 32 zero bytes or the SHA-256 of the `run --anchor-token` token; `chain.token_bound` says which. An anchor line carries `session_id`, `seq`, `chain`, `genesis` and `timestamp`.
