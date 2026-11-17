#!/usr/bin/env python3
"""Scale test for shoal: what grows when one machine runs many sessions?

Run from the base repository of an initialised project (see run.sh), as root
on Linux with the overlay backend:

    python3 scale.py --sessions 100 --out scale.json

It creates N extra sessions named sNNN, gives each a small edit (every fifth
session edits the same tracked file, so there are real conflicts), and
measures:
  - session creation time as the count grows
  - disk used by the whole project
  - `shoal ls` and `shoal conflicts` wall time at N sessions
  - page cache: reading node_modules in 10 sessions vs in 1
  - throughput of the test suite run in 1, 4, 8 and 16 sessions at once
  - memory of one running dev server
Everything it creates is removed at the end unless --keep is given.
"""
import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time

EDIT_TARGETS = ["README.md", "src/app.ts", "src/store.ts", "src/views.ts", "test/app.test.ts"]


def sh(*cmd, cwd=None, check=True):
    p = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True)
    if check and p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed:\n{p.stdout}{p.stderr}")
    return p.stdout


def timed(fn, *a, **kw):
    t = time.perf_counter()
    r = fn(*a, **kw)
    return (time.perf_counter() - t) * 1000.0, r


def cached_kb():
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("Cached:"):
                return int(line.split()[1])
    return 0


def drop_caches():
    os.sync()
    with open("/proc/sys/vm/drop_caches", "w") as f:
        f.write("3\n")


def read_tree(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            p = os.path.join(root, name)
            if os.path.islink(p):
                continue
            with open(p, "rb") as fh:
                while chunk := fh.read(1 << 20):
                    total += len(chunk)
    return total


def tree_rss_kb(pid):
    """Resident memory of a process and all its descendants."""
    children = {}
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            try:
                with open(f"/proc/{entry}/stat") as f:
                    ppid = int(f.read().rsplit(")", 1)[1].split()[1])
                children.setdefault(ppid, []).append(int(entry))
            except (OSError, ValueError, IndexError):
                pass
    total, stack = 0, [pid]
    while stack:
        p = stack.pop()
        try:
            with open(f"/proc/{p}/status") as f:
                m = re.search(r"VmRSS:\s+(\d+)", f.read())
                total += int(m.group(1)) if m else 0
        except OSError:
            pass
        stack.extend(children.get(p, []))
    return total


def wait_until_idle(path, timeout=15):
    """Wait until no process has its working directory inside `path`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        busy = False
        for entry in os.listdir("/proc"):
            if entry.isdigit():
                try:
                    if os.readlink(f"/proc/{entry}/cwd").startswith(path):
                        busy = True
                        break
                except OSError:
                    pass
        if not busy:
            return
        time.sleep(0.2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sessions", type=int, default=100)
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--out")
    args = ap.parse_args()
    n = args.sessions
    names = [f"s{i:03d}" for i in range(1, n + 1)]
    results = {"sessions": n}

    # 1. Creation time as the count grows.
    create = []
    for name in names:
        out = sh("shoal", "new", name)
        m = re.search(r"in ([\d.]+) (ms|s) \(branch \S+ ([\d.]+) (ms|s)\)", out)
        to_ms = lambda v, u: float(v) * (1000 if u == "s" else 1)
        create.append((to_ms(m.group(1), m.group(2)), to_ms(m.group(3), m.group(4))))
    vol = [c[0] for c in create]
    br = [c[1] for c in create]
    results["create_volume_ms"] = {
        "first10_median": statistics.median(vol[:10]),
        "last10_median": statistics.median(vol[-10:]),
        "max": max(vol),
    }
    results["create_branch_ms_median"] = statistics.median(br)

    # 2. Give every session a small edit; every fifth one edits the same file.
    paths = {}
    for i, name in enumerate(names):
        p = sh("shoal", "path", name).strip()
        paths[name] = p
        target = EDIT_TARGETS[i % len(EDIT_TARGETS)]
        with open(os.path.join(p, target), "a") as f:
            f.write(f"\n// edit from {name}\n")
        with open(os.path.join(p, f"NOTES-{name}.md"), "w") as f:
            f.write(f"scratch notes for {name}\n")

    info = sh("shoal", "info")
    results["project_disk"] = re.search(r"whole project: (.+)", info).group(1)

    # 3. Commands whose cost grows with the number of sessions.
    ms, out = timed(sh, "shoal", "ls")
    results["ls_ms"] = round(ms)
    ms, out = timed(sh, "shoal", "conflicts")
    results["conflicts_ms"] = round(ms)
    results["conflict_pairs"] = out.count("CONFLICT in")
    results["clean_overlap_pairs"] = out.count("merge cleanly")

    # 4. Page cache: one copy of node_modules in memory however many sessions read it.
    drop_caches()
    base = cached_kb()
    read_tree(os.path.join(paths[names[0]], "node_modules"))
    one = cached_kb() - base
    for name in names[1:10]:
        read_tree(os.path.join(paths[name], "node_modules"))
    ten = cached_kb() - base
    results["page_cache_mb"] = {"after_1_session": round(one / 1024), "after_10_sessions": round(ten / 1024)}

    # 5. Throughput: the test suite in k sessions at once (CPU-bound).
    tput = {}
    for k in (1, 4, 8, 16):
        group = names[:k]
        t = time.perf_counter()
        procs = [subprocess.Popen(["shoal", "exec", g, "--", "npx", "vitest", "run"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) for g in group]
        codes = [p.wait() for p in procs]
        wall = time.perf_counter() - t
        tput[k] = {"wall_s": round(wall, 2), "runs_per_min": round(k / wall * 60, 1),
                   "all_passed": all(c == 0 for c in codes)}
    results["test_runs_in_parallel"] = tput
    results["cpus"] = os.cpu_count()

    # 6. Memory of one running dev server.
    server = subprocess.Popen(["shoal", "exec", names[0], "--", "npx", "tsx", "src/server.ts"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                              env={**os.environ, "PORT": "4999"}, start_new_session=True)
    time.sleep(4)
    results["dev_server_rss_mb"] = round(tree_rss_kb(server.pid) / 1024)
    os.killpg(server.pid, 15)
    server.wait(timeout=10)
    wait_until_idle(paths[names[0]])

    print(json.dumps(results, indent=2))
    if args.out:
        with open(args.out, "w") as f:
            json.dump(results, f, indent=2)

    if not args.keep:
        t = time.perf_counter()
        for name in names:
            sh("shoal", "rm", "--force", name)
        print(f"removed {n} sessions in {time.perf_counter() - t:.2f} s")


if __name__ == "__main__":
    main()
