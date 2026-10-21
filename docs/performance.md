# Performance

All numbers below were measured on 2026-10-20 with `python3 bench/bench.py` against a release build, after the third review round (screen snapshots, paged reads, child-state reset, kind index). They are a baseline, not a claim about other machines. Re-run the suite before and after any change that touches the PTY path, the recorder or the store.

Machine: Apple M5 Pro, 18 cores, 48 GiB RAM, macOS 27.0.1 (arm64), Python 3.9.6, rustc 1.99.1.

## How to reproduce

```
just bench            # or: python3 bench/bench.py --json bench-results.json
python3 bench/bench.py --quick
```

The script builds `target/release/agentrecorder`, uses a temporary `AGENTRECORDER_HOME` under `/tmp`, and prints JSON. It needs only Python 3 and the Rust toolchain.

## Results

| Measurement | Direct | agentrecorder | Method |
| --- | --- | --- | --- |
| Startup (median / p95) | 2.24 / 3.30 ms to spawn `true` | 12.07 / 20.17 ms until `run -d` returns with a supervisor that answers a status request | 40 runs each |
| Keystroke echo round trip (median / p95) | 0.010 / 0.013 ms | 0.034 / 0.045 ms | 300 single-byte writes to `cat` in raw mode, through a PTY; agentrecorder path is PTY -> client -> socket -> supervisor -> PTY writer -> `cat` and back |
| Output throughput, 100 MiB, recording on | 26.6 MiB/s | 28.6 MiB/s | `head -c 100M /dev/zero \| tr \| fold -w 99` drained from a PTY as fast as possible; the client reads from a PTY, so it is an interactive client |
| Output throughput, 100 MiB, `--no-record` | - | 30.0 MiB/s | same |
| Bytes recorded of 106,975,934 produced | - | 106,975,934, no drop warnings, 12 screen snapshots | `inspect --json` event counts |
| Replay to a pipe (`--instant --raw`) | - | 1450 MiB/s (102 MiB in 0.070 s) | same recording; reads in pages of 256 rows |
| Supervisor RSS idle / peak / after | - | 9.7 / 19.0 / 19.0 MiB | `ps -o rss` sampled every 50 ms while `seq 1 2000000` runs detached |
| Event store writes | - | 95,143 events/s, 92.9 MiB/s, with the kind index | 200,000 events of 1 KiB, batches of 512 (`cargo run --release -p agentrecorder-store --example store_bench`) |
| Event store reads | - | 1043 MiB/s | sequential read of the same events, paged |
| `inspect` of a finished session with 102 MiB recorded | - | 0.047 s (0.199 s before the `data_len` column) | 3 runs; byte counts no longer read the blobs |
| `recover` of the same session | - | 0.077 s (0.86 s before screen snapshots) | 3 runs; starts from the last `terminal.snapshot` instead of parsing 102 MiB |

The four runs on this machine (first baseline and three review rounds) put the keystroke round trip at 0.036, 0.042, 0.033 and 0.034 ms and startup at 11.3, 12.4, 11.6 and 12.1 ms. Differences of that size are within run-to-run noise here. A fifth run on 2026-10-21, after interactive clients moved from detach-on-stall to discard-and-redraw, measured 11.6 ms startup, 0.034 ms round trip, 28.1 MiB/s recorded throughput with no drop warnings, and 9.9 / 19.3 MiB supervisor RSS idle / peak, all within that noise.

The third-round benchmark also caught two regressions before they shipped: startup rose to 26.6 ms while every possible descriptor (up to 65,536) was marked close-on-exec in the child, fixed by marking only the descriptors that are actually open; and recording stopped at 8 MiB because the byte budget subtracted snapshot bytes it never added, fixed and covered by `events_with_data_that_are_not_output_do_not_disturb_the_byte_budget`.

## Throughput ceiling

Measured on 2026-10-21 on the same machine, with `bench_ceiling` in `bench/bench.py`: `cat` of a pre-generated 200 MiB file of 99-byte lines, which writes faster than a PTY drains. The PTY's newline translation adds one byte per line, so 211.8 MB reach the reader.

| Path | Seconds | MiB/s | Recorded bytes | Drop warnings | Client discards |
| --- | --- | --- | --- | --- | --- |
| Direct PTY, drained by the benchmark | 1.05 | 191.9 | - | - | - |
| agentrecorder, interactive client (reads from a PTY) | 2.85 | 70.7 | 211,833,591 | 0 | 0 |
| agentrecorder, pipe client (stdout is a pipe) | 2.82 | 71.6 | 211,833,636 | 0 | 0 |
| agentrecorder, interactive client, `--no-record` | 2.63 | 76.8 | 0 | 0 | 0 |

The supervisor's ceiling on this machine is about 70 MiB/s with recording and 77 MiB/s without, 37 to 40 percent of a bare PTY. Turning recording off gains 9 percent, so most of the cost is outside the recorder; the second PTY hop to the client and the socket are the remaining suspects, not measured separately. The recorder kept every byte at that rate. tmux is not installed on this machine, so the tmux comparison is still open (`brew install tmux`, then extend `bench_throughput`).

### Output produced before the client attaches

The ceiling run showed the pipe client receiving 49 to 141 KB fewer bytes than were recorded, in three runs of a 50 MiB file. `run` starts the process before its own client attaches (about 10 ms), and the attach sends a screen snapshot with at most 1000 lines of history, so output beyond that, produced in the gap, reaches the recording but not the client. The loss happens with v0.1.0 too. With `sleep 0.2` before `cat`, the client received the file byte for byte. It affects only programs that print more than about 1000 lines within the first milliseconds; the recording is complete either way. This is an open defect against AC-19.

## Interpretation

- The throughput test is bound by the producer pipeline, not by the PTY or by `agentrecorder`: direct and managed runs finish in the same time within noise. The ceiling is measured separately (see Throughput ceiling).
- The recorder kept up with 28 MiB/s of output without dropping events. The store's sequential write rate (about 95 MiB/s at 1 KiB per event, higher with the recorder's 64 KiB coalescing) is the expected ceiling before the queue fills and output events are dropped.
- Interactive overhead is about 25 to 30 microseconds per keystroke round trip at the median. This is below what a person can perceive; terminal rendering dominates.
- Startup overhead is about 10 ms at the median, mostly the supervisor exec, SQLite open and readiness polling, which waits for a full status reply.
- Supervisor memory is bounded by the vt100 scrollback (5000 lines), the client queue (256 frames), the input queue (1024 chunks) and the recorder queue (16 MiB of output, 4 MiB per batch). RSS rose from 9.6 MiB to 18.2 MiB under 2 million lines and stayed there.
- tmux was not installed on the benchmark machine; the script records `not measured` for it. Zellij is not measured by the script.

## Bounded resources and event loss

| Resource | Bound | When full |
| --- | --- | --- |
| Recorder queue | 4096 events and 16 MiB of queued output or input; the last 512 event slots are reserved for lifecycle events | Output and input events are dropped once either limit is reached, counted, and reported as one `runtime.warning` with code `events_dropped` per batch (none after `session.terminated`). Lifecycle events never wait; they are dropped and counted only when all 4096 slots are full. |
| Recorder batch | 512 events, 4 MiB or 20 ms | The batch is written and a new one starts. |
| Attached client queue | 256 frames | Interactive client: the PTY reader never waits; further output for that client is discarded and counted, and when the queue is down to half the client gets a redraw of the current screen and a `client_output_discarded` warning is recorded. A client that stops reading is detached with reason `client_too_slow` when its socket's 5 s write timeout expires. Non-interactive client (stdout not a terminal): the reader waits, so the managed process runs at the reader's pace, as with a pipe. |
| Input queue | 1024 chunks | Further input is dropped and recorded as `runtime.warning` `input_dropped`; connection threads never block on it. |
| Recording size | `--max-record-mb`, default 256 MiB | Output recording stops; one `runtime.warning` with code `recording_limit_reached` is written. The session keeps running. |
| Screen history | 5000 lines of vt100 scrollback; 1000 lines sent on attach | Older lines are discarded from the live screen model only; the recording keeps them. |
| Store write failure | busy or locked: retried every 100 ms for up to 10 s (60 s for the exit metadata); other errors: not retried | The batch or metadata update is counted as dropped and the error is written to `logs/ID.log`. While the recorder retries, new output events are dropped and counted instead of queuing without bound. |

The managed process is never blocked by telemetry. An interactive client can block it for at most 1 second per chunk; a non-interactive client blocks it for as long as the reader takes, by design.
