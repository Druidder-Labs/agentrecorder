# Event model

Schema version: 1 (`EVENT_SCHEMA_VERSION` in `crates/agentrecorder-store/src/lib.rs`).

## Envelope

`agentrecorder export SESSION` writes one JSON object per line:

```json
{
  "schema_version": 1,
  "event_id": "a91f2c-1",
  "session_id": "a91f2c",
  "seq": 1,
  "timestamp": "2026-10-20T12:00:00.123456789Z",
  "mono_ns": 2305208,
  "event_type": "process.started",
  "payload": {"pid": 18420, "command": ["claude"]}
}
```

| Field | Meaning |
| --- | --- |
| `schema_version` | Envelope version. Consumers reject versions they do not know. |
| `event_id` | `<session_id>-<seq>`; unique within a store. |
| `session_id` | 6 lowercase hex characters. |
| `seq` | Per-session sequence number starting at 0, contiguous, assigned by the supervisor when the event is persisted. This is the ordering key. |
| `timestamp` | Wall clock (UTC, RFC 3339, nanoseconds) when the event was created. It can jump if the system clock changes. |
| `mono_ns` | Nanoseconds since the supervisor started, from a monotonic clock. Use it for durations and replay timing. Events from different threads can arrive a few microseconds out of `mono_ns` order; `seq` is authoritative and replay clamps negative gaps to zero. |
| `event_type` | One of the types below. |
| `payload` | Type-specific JSON object, or `null`. |
| `data_b64` | Present on `terminal.output` and `terminal.input`: the raw bytes, base64 encoded. Bytes are not assumed to be UTF-8. |

## Types

| Type | Payload | Notes |
| --- | --- | --- |
| `session.created` | `command`, `cwd`, `rows`, `cols`, `record_output`, `record_input`, `supervisor_pid`, `restarted_from`, `max_record_bytes`, `term`, `agent_session_id` | First event of every session. `max_record_bytes` and `term` are absent in recordings made before they were added; `agent_session_id` is present only when the agent's own session id is known (see `cli.md`, Agent integration); `recover --restart` reuses the cap and asciicast export uses `term`. |
| `process.started` | `pid`, `command` | |
| `session.attached` | `client`, `client_pid`, `rows`, `cols` | `client_pid` is reported by the client and not verified. |
| `session.detached` | `client`, `reason` | Reasons: `client_request`, `client_disconnected`, `detach_requested`, `attached_elsewhere`, `client_too_slow`, `process_exited`, `pty_write_failed`, `protocol_error`. |
| `terminal.output` | `null` | Data in `data_b64`. Chunks within 5 ms are merged up to 64 KiB. |
| `terminal.input` | `null` | Only with `--record-input`. |
| `terminal.resized` | `rows`, `cols` | |
| `session.stop_requested` | `signal`, `grace_ms`, `origin` | `origin` is `client` or `supervisor_signal_<n>`. |
| `process.exited` | `pid`, `exit_code`, `signal`, `duration_ms` | Exactly one of `exit_code` and `signal` is non-null. |
| `session.terminated` | `reason`, `exit_code`, `signal`, `dropped_events` | Last event; the recorder discards anything sent after it. `reason` is `process_exited`, `stopped` or `spawn_failed` (exit code 127). |
| `terminal.snapshot` | `rows`, `cols`, `reason` (`interval` or `after_drop`) | Data in `data_b64`: escape sequences that redraw the screen and restore modes. Written every 8 MiB of output and after dropped events; used for seeking, `recover` and resync. Not part of the output stream. |
| `terminal.notification` | `source` (`osc9`, `osc99`, `osc777`, `bell`), `text` | Text has control characters removed and is capped at 512 characters. Repeats of one source within 1 s are not recorded. |
| `git.snapshot` | `phase` (`start` or `exit`), `head`, `dirty_paths` | Only when the working directory is a git work tree. `head` is null before the first commit. |
| `runtime.warning` | `code`, `message`, code-specific fields | Codes: `events_dropped` (`dropped_events`, `dropped_bytes`), `recording_limit_reached` (`limit_bytes`), `stop_escalated` (`signal`), `input_dropped` (`bytes`), `client_output_discarded` (`bytes`, `client`: output an attached terminal did not keep up with; the screen was redrawn), `pre_attach_output_dropped` (`limit_bytes`: more output arrived before the first attach of `run` than the 4 MiB the supervisor holds). |
| `runtime.error` | `code`, `message` | Codes: `spawn_failed`, `resize_failed`. |
| `shell.command_finished` | `exit_code` | One per OSC 133 `D` mark that follows a `C` mark (see `cli.md`, Shell integration). `exit_code` is `null` when the mark carries none. Recorded best effort, like `agent.hook`. |
| `user.mark` | `label` | Written by `agentrecorder mark` while the session runs; `label` may be empty. Listed by `inspect`, used by `replay --from-mark`, exported to asciicast as an `m` event `mark: LABEL`. |
| `agent.hook` | `agent`, `hook_event_name`, and when the agent sent them `session_id`, `transcript_path`, `cwd`, `source`, `reason`, `model`, `permission_mode`, `notification_type`, `message`, `tool_name` | Reported by the agent through `agentrecorder hook` (see `cli.md`, Agent integration) and not verified. Recorded best effort: dropped and counted like output when the recorder queue is near full. |

A session that ends without `session.terminated` was lost: the supervisor was killed or the host stopped. `agentrecorder sessions` reports it as `lost`; `sessions --json` and `inspect` show `lost_reason` (`host_restarted` or `supervisor_terminated`).

## Storage

Events live in the `events` table of `state.db` (`crates/agentrecorder-store/src/lib.rs`), keyed by `(session_id, seq)`. `payload` is stored as JSON text, `data` as a BLOB and `data_len` as its length, so counting bytes does not read the blobs. Session metadata (status, pids, exit data, boot time) lives in the `sessions` table; the supervisor updates it, and CLI commands may only mark a session `lost` or delete it. `PRAGMA user_version` holds the database schema version (4); older stores are migrated in place the first time a newer binary opens it (version 2 added `data_len`, version 3 the `events_by_kind` index, version 4 the `sessions.agent_session_id` column).

The payload shapes in the table above are defined once, as the `Body` enum in `crates/agentrecorder-store/src/events.rs`; a unit test round-trips every variant through its stored form.

## Other formats

- `agentrecorder export SESSION --format asciicast-v2` (or `asciicast`) writes asciicast v2 and `--format asciicast-v3` writes asciicast v3 for asciinema players. Both carry `o` (output), `r` (resize) and `m` (markers for warnings, attach, detach, stop requests and notifications); v3 also carries `x` (exit status) and intervals rounded to milliseconds without cumulative drift. The header carries the command and the recorded `TERM`. Bytes that are not valid UTF-8 become U+FFFD in these formats only; DEL and C1 characters are escaped as `\uXXXX`. Exported casts are not sanitized: `asciinema play` writes them to the terminal unfiltered.
- OpenTelemetry export is not implemented. A possible mapping is one span per session with lifecycle events as span events; terminal bytes are never exported to OTel.

## Bundle format

`agentrecorder bundle` writes a directory (`bundle_version` 1 in `manifest.json`):

| File | Content |
| --- | --- |
| `manifest.json` | `format` (`agentrecorder-bundle`), `bundle_version`, `generator`, `created_at`, `session` (the stored session row), `command_display`, `program`, `duration_ms`, `event_counts`, `chain` (`algorithm`, `events`, `head`), `fates`, `redaction` (`patterns`, `matches`, `events`), `cuts` (`from_ms`, `to_ms`, `events`) and `files` (`sha256` and `bytes` of `events.jsonl`, `chain.jsonl` and `session.cast`) |
| `events.jsonl` | The export envelope above, one line per event that was not cut; redacted events carry the redacted bytes |
| `chain.jsonl` | One line per recorded event, cut or not: `seq`, `leaf`, `chain`, `fate` (`kept`, `redacted` or `cut`) |
| `session.cast` | asciicast v3 built from the bundled events, with a `cut FROM-TO` marker per cut range |
| `index.html` | The dashboard, with the manifest, the cast and the non-byte events inline. It is not listed in `files` |

Hash chain `sha256-chain-v1`, all hashes lowercase hex:

- `leaf` is SHA-256 of the event's JSON line as `export` prints it, without the newline, computed before redaction.
- `chain` for seq 0 is SHA-256 of 32 zero bytes followed by the leaf; for seq n it is SHA-256 of the previous `chain` followed by the leaf, both as raw 32-byte values.
- `manifest.json` `chain.head` is the last `chain`.

`agentrecorder anchor` prints lines of `session_id`, `seq`, `chain` (the same value as `chain.jsonl` at that seq) and `timestamp` (when the line was printed, informational).

## Compatibility rules

- Adding an event type or a payload field is compatible and keeps version 1.
- Removing or renaming a field, or changing its meaning, increments `schema_version`.
