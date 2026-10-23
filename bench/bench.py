#!/usr/bin/env python3
"""Reproducible witnessed benchmarks. Usage: python3 bench/bench.py [--quick] [--json out.json]"""

import argparse
import json
import os
import platform
import pty
import select
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "witnessed")


def env_for(home):
    e = dict(os.environ)
    e["WITNESSED_HOME"] = home
    e["WITNESSED_RETENTION_DAYS"] = "0"
    e.pop("WITNESSED_SESSION", None)
    return e


def witnessed(home, *args, check=True):
    return subprocess.run([BIN, *args], env=env_for(home), capture_output=True, check=check)


def percentile(xs, p):
    xs = sorted(xs)
    k = max(0, min(len(xs) - 1, int(round(p / 100.0 * (len(xs) - 1)))))
    return xs[k]


def summary(xs):
    return {
        "n": len(xs),
        "median_ms": round(statistics.median(xs) * 1000, 3),
        "p95_ms": round(percentile(xs, 95) * 1000, 3),
        "min_ms": round(min(xs) * 1000, 3),
    }


def bench_startup(home, n):
    direct, managed = [], []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run(["true"], check=True)
        direct.append(time.perf_counter() - t)
        t = time.perf_counter()
        witnessed(home, "run", "-d", "--", "sleep", "60")
        managed.append(time.perf_counter() - t)
    for s in json.loads(witnessed(home, "sessions", "--json").stdout):
        if s["status"] in ("running", "detached"):
            witnessed(home, "stop", s["session"], "--signal", "KILL", "--timeout", "1", check=False)
    return {"direct_spawn": summary(direct), "witnessed_run_detached_until_ready": summary(managed)}


def drain_pty(argv, env):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(argv[0], argv, env)
    total = 0
    t = time.perf_counter()
    while True:
        try:
            d = os.read(fd, 65536)
        except OSError:
            break
        if not d:
            break
        total += len(d)
    _, status = os.waitpid(pid, 0)
    return time.perf_counter() - t, total


def bench_throughput(home, mib):
    producer = f"head -c {mib * 1048576} /dev/zero | tr '\\000' 'a' | fold -w 99"
    out = {}
    secs, n = drain_pty(["sh", "-c", producer], dict(os.environ))
    out["direct_pty"] = {"seconds": round(secs, 3), "bytes": n, "mib_per_s": round(n / secs / 1048576, 1)}
    for label, extra in (("witnessed_recording", []), ("witnessed_no_record", ["--no-record"])):
        secs, n = drain_pty([BIN, "run", *extra, "--", "sh", "-c", producer], env_for(home))
        row = {"seconds": round(secs, 3), "bytes_seen_by_client": n, "mib_per_s": round(n / secs / 1048576, 1)}
        last = json.loads(witnessed(home, "sessions", "--json").stdout)[-1]
        if not extra:
            counts = json.loads(witnessed(home, "inspect", last["session"], "--json").stdout)["event_counts"]
            row["recorded_output_bytes"] = sum(c["bytes"] for c in counts if c["kind"] == "terminal.output")
            row["dropped_warnings"] = sum(c["count"] for c in counts if c["kind"] == "runtime.warning")
            out["replay_session"] = last["session"]
        out[label] = row
    tmux = shutil.which("tmux")
    out["tmux"] = "not measured (tmux not installed)" if not tmux else "not measured (no tmux harness yet)"
    return out


def recording_counts(home, session):
    info = json.loads(witnessed(home, "inspect", session, "--json").stdout)
    counts = info["event_counts"]
    warnings = [n["payload"] for n in info["notices"] if n["type"] == "runtime.warning"]
    return {
        "recorded_output_bytes": sum(c["bytes"] for c in counts if c["kind"] == "terminal.output"),
        "events_dropped_warnings": sum(1 for w in warnings if w.get("code") == "events_dropped"),
        "client_discarded_bytes": sum(w.get("bytes", 0) for w in warnings if w.get("code") == "client_output_discarded"),
    }


def drain_pipe(argv, env):
    t = time.perf_counter()
    p = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    total = 0
    while True:
        d = p.stdout.read(1 << 20)
        if not d:
            break
        total += len(d)
    p.wait()
    return time.perf_counter() - t, total


def bench_ceiling(home, mib):
    """Producer faster than the PTY: cat of a pre-generated file of 99-byte lines."""
    path = os.path.join(home, "lines.txt")
    line = b"a" * 98 + b"\n"
    with open(path, "wb") as f:
        block = line * (1048576 // len(line) + 1)
        for _ in range(mib):
            f.write(block[:1048576])
    size = os.path.getsize(path)
    out = {"file_bytes": size}
    secs, n = drain_pty(["cat", path], dict(os.environ))
    out["direct_pty"] = {"seconds": round(secs, 3), "bytes": n, "mib_per_s": round(n / secs / 1048576, 1)}
    cases = (
        ("witnessed_interactive_client", drain_pty, []),
        ("witnessed_pipe_client", drain_pipe, []),
        ("witnessed_interactive_no_record", drain_pty, ["--no-record"]),
    )
    for label, drain, extra in cases:
        secs, n = drain([BIN, "run", *extra, "--", "cat", path], env_for(home))
        row = {"seconds": round(secs, 3), "bytes_seen_by_client": n, "mib_per_s": round(n / secs / 1048576, 1)}
        last = json.loads(witnessed(home, "sessions", "--json").stdout)[-1]
        row.update(recording_counts(home, last["session"]))
        out[label] = row
    os.unlink(path)
    return out


def bench_replay(home, session):
    t = time.perf_counter()
    r = subprocess.run([BIN, "replay", session, "--instant", "--raw"], env=env_for(home), stdout=subprocess.PIPE, check=True)
    secs = time.perf_counter() - t
    return {"seconds": round(secs, 3), "bytes": len(r.stdout), "mib_per_s": round(len(r.stdout) / secs / 1048576, 1)}


def echo_latency(argv, env, n):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(argv[0], argv, env)
    buf = b""
    deadline = time.time() + 10
    while b"READY" not in buf and time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.5)
        if r:
            buf += os.read(fd, 65536)
    samples = []
    for i in range(n):
        token = b"x"
        t = time.perf_counter()
        os.write(fd, token)
        got = b""
        while token not in got:
            r, _, _ = select.select([fd], [], [], 2)
            if not r:
                break
            got += os.read(fd, 4096)
        samples.append(time.perf_counter() - t)
    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass
    os.close(fd)
    os.waitpid(pid, 0)
    return samples


def bench_latency(home, n):
    inner = "stty raw -echo; echo READY; exec cat"
    direct = echo_latency(["sh", "-c", inner], dict(os.environ), n)
    managed = echo_latency([BIN, "run", "--detach-key", "none", "--", "sh", "-c", inner], env_for(home), n)
    for s in json.loads(witnessed(home, "sessions", "--json").stdout):
        if s["status"] in ("running", "detached"):
            witnessed(home, "stop", s["session"], "--signal", "KILL", "--timeout", "1", check=False)
    return {"direct_pty_round_trip": summary(direct), "witnessed_round_trip": summary(managed)}


def bench_memory(home):
    sid = witnessed(home, "run", "-d", "--", "sh", "-c", "sleep 1; seq 1 2000000; sleep 60").stdout.decode().strip()
    info = json.loads(witnessed(home, "inspect", sid, "--json").stdout)
    sup = info["session"]["supervisor_pid"]
    rss = lambda: int(subprocess.run(["ps", "-o", "rss=", "-p", str(sup)], capture_output=True).stdout.decode().strip() or 0)
    idle = rss()
    peak = idle
    t = time.time()
    while time.time() - t < 8:
        peak = max(peak, rss())
        time.sleep(0.05)
    after = rss()
    witnessed(home, "stop", sid, "--signal", "KILL", "--timeout", "1", check=False)
    return {"supervisor_rss_idle_kib": idle, "supervisor_rss_peak_kib": peak, "supervisor_rss_after_kib": after}


def bench_store():
    r = subprocess.run(
        ["cargo", "run", "-q", "--release", "-p", "witnessed-store", "--example", "store_bench"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    )
    return json.loads(r.stdout.decode().strip().splitlines()[-1])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--json")
    a = ap.parse_args()
    subprocess.run(["cargo", "build", "--release", "-q", "-p", "witnessed-cli"], cwd=ROOT, check=True)
    home = tempfile.mkdtemp(prefix="agtb", dir="/tmp")
    results = {
        "machine": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpus": os.cpu_count(),
            "python": platform.python_version(),
        },
        "binary": BIN,
    }
    log = lambda m: print(f"[bench] {m}", file=sys.stderr, flush=True)
    log("startup")
    results["startup"] = bench_startup(home, 10 if a.quick else 40)
    log("latency")
    results["latency"] = bench_latency(home, 50 if a.quick else 300)
    log("throughput")
    tp = bench_throughput(home, 20 if a.quick else 100)
    results["replay"] = bench_replay(home, tp.pop("replay_session"))
    results["throughput"] = tp
    log("ceiling")
    results["ceiling"] = bench_ceiling(home, 20 if a.quick else 200)
    log("memory")
    results["memory"] = bench_memory(home)
    log("store")
    results["store"] = bench_store()
    shutil.rmtree(home, ignore_errors=True)
    text = json.dumps(results, indent=2)
    print(text)
    if a.json:
        with open(a.json, "w") as f:
            f.write(text + "\n")


if __name__ == "__main__":
    sys.exit(main())
