# Performance

Baseline on an Apple M5 Pro (18 cores, 48 GiB, macOS 27), release build, `python3 bench/bench.py`. Numbers are for this machine; re-run before and after changes to the PTY path, recorder or store.

```
just bench                       # full suite, JSON to bench-results.json
python3 bench/bench.py --quick   # what just check-ci runs
```

## Results

| Measurement | Direct | agentrecorder |
| --- | --- | --- |
| Startup, median / p95 | 2.2 / 3.3 ms (spawn `true`) | 12.1 / 20.2 ms (`run -d` until status answers) |
| Keystroke round trip, median / p95 | 0.010 / 0.013 ms | 0.034 / 0.045 ms |
| Output throughput, 100 MiB, recording on | 26.6 MiB/s | 28.6 MiB/s (producer bound) |
| Bytes recorded of 106,975,934 produced | - | all, no drops, 12 snapshots |
| Replay to a pipe, `--instant --raw` | - | 1450 MiB/s |
| Supervisor RSS idle / peak | - | 9.7 / 19.0 MiB (2 million lines) |
| Store writes, 1 KiB events, batches of 512 | - | 95,143 events/s (92.9 MiB/s) |
| Store reads, sequential, paged | - | 1043 MiB/s |
| `inspect` of a 102 MiB session | - | 0.047 s |
| `recover` of the same session | - | 0.077 s (from the last snapshot) |

Five runs put the round trip between 0.033 and 0.042 ms and startup between 11.3 and 12.4 ms; differences of that size are noise.

## Throughput ceiling

`cat` of a 200 MiB file of 99-byte lines, faster than a PTY drains (`bench_ceiling`):

| Path | MiB/s | Recorded bytes | Drops |
| --- | --- | --- | --- |
| Bare PTY | 191.9 | - | - |
| agentrecorder, interactive client | 70.7 | 211,833,591 | 0 |
| agentrecorder, pipe client | 71.6 | 211,833,636 | 0 |
| agentrecorder, `--no-record` | 76.8 | 0 | 0 |

The ceiling is about 70 MiB/s with recording and 77 without: most of the cost is the second PTY hop and the socket, not the recorder. A tmux comparison is open (tmux not installed on the machine).

`crates/agentrecorder/tests/conservation.rs` checks produced == recorded == delivered by content: pipe clients with up to 12 MiB produced before attach, late attach to a detached session, producers that exit at once, a 24 MiB burst to an interactive client, 60,000 single-line writes.

## Events table layout

Measured with `layout_bench`: the current `WITHOUT ROWID` table keyed by `(session_id, seq)` inserts 38 percent slower and reads 76 percent slower than a rowid table with 64 KiB blobs, and is within 5 percent with 200-byte events. Its slowest rate is about 325 MiB/s of inserts against a 70 MiB/s supervisor ceiling, so the layout stays. Revisit when the ceiling approaches the store's insert rate.

`rm` of a 64 MB session leaves `state.db` at 24 KiB with no free pages (`incremental_vacuum` runs to completion).

## Bounded resources

| Resource | Bound | When full |
| --- | --- | --- |
| Recorder queue | 4096 events, 16 MiB; last 512 slots reserved for lifecycle | Output and input dropped and counted, one `events_dropped` warning per batch |
| Recorder batch | 512 events, 4 MiB or 20 ms | Written, new batch |
| Attached client queue | 256 frames | Interactive: output discarded, screen redrawn at half queue, `client_output_discarded`; detached as `client_too_slow` after 5 s without reads. Pipe: backpressure |
| Input queue | 1024 chunks | Dropped, `input_dropped` |
| Recording size | `--max-record-mb`, default 256 MiB | Recording stops, `recording_limit_reached`; session continues |
| Screen history | 5000 lines; 1000 sent on attach | Older lines leave the live model only |
| Store write failure | Busy or locked: retry every 100 ms for 10 s (60 s for exit metadata) | Batch counted as dropped, error in `logs/ID.log`; new output dropped while retrying |

The managed process is never blocked by telemetry. An interactive client can block it for at most 1 s per chunk; a pipe client for as long as the reader takes.
