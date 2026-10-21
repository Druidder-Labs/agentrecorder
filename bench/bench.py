#!/usr/bin/env python3
"""Reproducible agentrecorder benchmarks. Usage: python3 bench/bench.py [--quick] [--json out.json]"""

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
BIN = os.path.join(ROOT, "target", "release", "agentrecorder")


def env_for(home):
    e = dict(os.environ)
    e["AGENTRECORDER_HOME"] = home
    e["AGENTRECORDER_RETENTION_DAYS"] = "0"
    e.pop("AGENTRECORDER_SESSION", None)
    return e


def agentrecorder(home, *args, check=True):
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
        agentrecorder(home, "run", "-d", "--", "sleep", "60")
        managed.append(time.perf_counter() - t)
    for s in json.loads(agentrecorder(home, "sessions", "--json").stdout):
        if s["status"] in ("running", "detached"):
            agentrecorder(home, "stop", s["session"], "--signal", "KILL", "--timeout", "1", check=False)
    return {"direct_spawn": summary(direct), "agentrecorder_run_detached_until_ready": summary(managed)}


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
    for label, extra in (("agentrecorder_recording", []), ("agentrecorder_no_record", ["--no-record"])):
        secs, n = drain_pty([BIN, "run", *extra, "--", "sh", "-c", producer], env_for(home))
        row = {"seconds": round(secs, 3), "bytes_seen_by_client": n, "mib_per_s": round(n / secs / 1048576, 1)}
        last = json.loads(agentrecorder(home, "sessions", "--json").stdout)[-1]
        if not extra:
            counts = json.loads(agentrecorder(home, "inspect", last["session"], "--json").stdout)["event_counts"]
            row["recorded_output_bytes"] = sum(c["bytes"] for c in counts if c["kind"] == "terminal.output")
            row["dropped_warnings"] = sum(c["count"] for c in counts if c["kind"] == "runtime.warning")
            out["replay_session"] = last["session"]
        out[label] = row
    tmux = shutil.which("tmux")
    out["tmux"] = "not measured (tmux not installed)" if not tmux else "not measured (no tmux harness yet)"
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
    for s in json.loads(agentrecorder(home, "sessions", "--json").stdout):
        if s["status"] in ("running", "detached"):
            agentrecorder(home, "stop", s["session"], "--signal", "KILL", "--timeout", "1", check=False)
    return {"direct_pty_round_trip": summary(direct), "agentrecorder_round_trip": summary(managed)}


def bench_memory(home):
    sid = agentrecorder(home, "run", "-d", "--", "sh", "-c", "sleep 1; seq 1 2000000; sleep 60").stdout.decode().strip()
    info = json.loads(agentrecorder(home, "inspect", sid, "--json").stdout)
    sup = info["session"]["supervisor_pid"]
    rss = lambda: int(subprocess.run(["ps", "-o", "rss=", "-p", str(sup)], capture_output=True).stdout.decode().strip() or 0)
    idle = rss()
    peak = idle
    t = time.time()
    while time.time() - t < 8:
        peak = max(peak, rss())
        time.sleep(0.05)
    after = rss()
    agentrecorder(home, "stop", sid, "--signal", "KILL", "--timeout", "1", check=False)
    return {"supervisor_rss_idle_kib": idle, "supervisor_rss_peak_kib": peak, "supervisor_rss_after_kib": after}


def bench_store():
    r = subprocess.run(
        ["cargo", "run", "-q", "--release", "-p", "agentrecorder-store", "--example", "store_bench"],
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
    subprocess.run(["cargo", "build", "--release", "-q", "-p", "agentrecorder"], cwd=ROOT, check=True)
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
