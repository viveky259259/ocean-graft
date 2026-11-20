#!/usr/bin/env python3
"""Measures what the job queue and idle shutdown save, on a real project.

Run from the base repository of an initialised notes-app project (run.sh):

    python3 resources.py [--jobs 16] [--servers 8]

A. Job queue: start the test suite in N sessions at the same moment, first
   with `ocean-graft exec` (no queue), then with `ocean-graft run` (queued, one slot per
   CPU). Samples every 100 ms how many test runners are alive and how much
   memory they hold.
B. Idle shutdown: start a dev server in M sessions with `ocean-graft serve`, keep
   two sessions busy, let the rest go idle, run `ocean-graft reap`, and measure
   the memory freed. Then `ocean-graft wake` one session and time how long its
   server takes to answer again.
Everything it creates is removed at the end.
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request


def sh(*cmd, check=True):
    p = subprocess.run(cmd, text=True, capture_output=True)
    if check and p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed:\n{p.stdout}{p.stderr}")
    return p.stdout


def processes():
    """(pid, pgid, rss_kb, command) for every process."""
    out = sh("ps", "axo", "pid=,pgid=,rss=,command=")
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) == 4:
            rows.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3]))
    return rows


class Sampler:
    """Samples test-runner processes every 100 ms."""

    def __init__(self, marker):
        self.marker = marker
        self.peak_procs = 0
        self.peak_rss_kb = 0
        self.stop = threading.Event()

    def run(self):
        while not self.stop.is_set():
            rows = [r for r in processes() if self.marker in r[3]]
            self.peak_procs = max(self.peak_procs, len(rows))
            self.peak_rss_kb = max(self.peak_rss_kb, sum(r[2] for r in rows))
            time.sleep(0.1)


def burst(names, verb):
    sampler = Sampler("vitest")
    thread = threading.Thread(target=sampler.run)
    thread.start()
    start = time.perf_counter()
    procs = [subprocess.Popen(["ocean-graft", verb, n, "--", "npx", "vitest", "run"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
             for n in names]
    codes = [p.wait() for p in procs]
    wall = time.perf_counter() - start
    sampler.stop.set()
    thread.join()
    return {
        "wall_s": round(wall, 1),
        "all_passed": all(c == 0 for c in codes),
        "peak_runner_processes": sampler.peak_procs,
        "peak_runner_memory_mb": round(sampler.peak_rss_kb / 1024),
    }


def wait_http(port, timeout=30):
    start = time.perf_counter()
    while time.perf_counter() - start < timeout:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1):
                return time.perf_counter() - start
        except OSError:
            time.sleep(0.05)
    raise RuntimeError(f"nothing answered on port {port}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--jobs", type=int, default=16)
    ap.add_argument("--servers", type=int, default=8)
    args = ap.parse_args()
    count = max(args.jobs, args.servers)
    names = [f"r{i:02d}" for i in range(1, count + 1)]
    results = {"cpus": os.cpu_count(), "queue_slots": None}
    for n in names:
        sh("ocean-graft", "new", n)
    try:
        # A. Job queue.
        results["queue_slots"] = int(sh("ocean-graft", "queue").split(" of ")[1].split()[0])
        results["no_queue"] = burst(names[: args.jobs], "exec")
        results["queued"] = burst(names[: args.jobs], "run")

        # B. Idle shutdown.
        servers = names[: args.servers]
        for n in servers:
            sh("ocean-graft", "serve", n, "--", "npx", "tsx", "src/server.ts")
        ports = {}
        for line in sh("ocean-graft", "services").splitlines()[1:]:
            parts = line.split()
            if parts and parts[0] in servers:
                ports[parts[0]] = int(parts[2])
        for n in servers:
            wait_http(ports[n])
        time.sleep(1)
        listing = sh("ocean-graft", "services")
        server_kb = 0
        for line in listing.splitlines()[1:]:
            parts = line.split()
            if parts and parts[0] in servers and parts[3] == "running":
                pgid = int(parts[4])
                server_kb += sum(r[2] for r in processes() if r[1] == pgid)
        results["servers_running"] = len(servers)
        results["servers_memory_mb"] = round(server_kb / 1024)

        time.sleep(4)
        busy = servers[:2]
        for n in busy:  # someone is still working in these two
            path = sh("ocean-graft", "path", n).strip()
            with open(os.path.join(path, "src", "notes-draft.ts"), "w") as f:
                f.write("// work in progress\n")
        start = time.perf_counter()
        reaped = sh("ocean-graft", "reap", "--idle", "3s")
        results["reap_s"] = round(time.perf_counter() - start, 2)
        freed_mb = 0.0
        for line in reaped.splitlines():
            amount, unit = line.rsplit("about ", 1)[1].split()
            freed_mb += float(amount) * {"KB": 1e-3, "MB": 1, "GB": 1e3}[unit]
        results["reap_output"] = reaped.strip().splitlines()
        results["servers_stopped"] = len(reaped.strip().splitlines())
        results["memory_freed_mb"] = round(freed_mb)
        still = [l.split()[0] for l in sh("ocean-graft", "services").splitlines()[1:] if " running " in l]
        results["still_running"] = still

        woken = servers[-1]
        start = time.perf_counter()
        sh("ocean-graft", "wake", woken)
        wait_http(ports[woken])
        results["wake_to_first_response_s"] = round(time.perf_counter() - start, 2)
    finally:
        for n in names:
            sh("ocean-graft", "rm", "--force", n, check=False)
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
