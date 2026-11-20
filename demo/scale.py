#!/usr/bin/env python3
"""Scale test for ocean-graft: what grows when one machine runs many sessions?

Run from the base repository of an initialised project (see run.sh). Works on
Linux (overlay or clone backend) and macOS (clone backend):

    python3 scale.py --sessions 100 --out scale.json

It creates N extra sessions named sNNN, gives each a small edit (every fifth
session edits the same tracked file, so there are real conflicts), and
measures:
  - session creation time as the count grows
  - disk the filesystem allocated for the sessions
  - `ocean-graft ls` and `ocean-graft conflicts` wall time at N sessions
  - page cache: reading node_modules in 10 sessions vs in 1
  - throughput of the test suite run in 1, 4, 8 and 16 sessions at once
  - memory of one running dev server
Everything it creates is removed at the end unless --keep is given.
"""
import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time

IS_MAC = sys.platform == "darwin"
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
    """Memory holding file contents (Linux page cache, macOS file-backed pages)."""
    if IS_MAC:
        out = subprocess.run(["vm_stat"], capture_output=True, text=True).stdout
        page = int(re.search(r"page size of (\d+) bytes", out).group(1))
        m = re.search(r"File-backed pages:\s+(\d+)", out)
        return int(m.group(1)) * page // 1024 if m else 0
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("Cached:"):
                return int(line.split()[1])
    return 0


def drop_caches():
    """Empty the file cache. Returns False when that is not permitted."""
    os.sync()
    if IS_MAC:
        return subprocess.run(["sudo", "-n", "purge"], capture_output=True).returncode == 0
    try:
        with open("/proc/sys/vm/drop_caches", "w") as f:
            f.write("3\n")
        return True
    except OSError:
        return False


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
    out = subprocess.run(["ps", "axo", "pid=,ppid=,rss="], capture_output=True, text=True).stdout
    children, rss = {}, {}
    for line in out.splitlines():
        try:
            p, pp, r = (int(x) for x in line.split())
        except ValueError:
            continue
        children.setdefault(pp, []).append(p)
        rss[p] = r
    total, stack = 0, [pid]
    while stack:
        p = stack.pop()
        total += rss.get(p, 0)
        stack.extend(children.get(p, []))
    return total


def wait_until_idle(path, timeout=15):
    """Wait until no process has its working directory inside `path`."""
    if not os.path.isdir("/proc"):
        return  # macOS: the clone backend never unmounts, so nothing can be busy
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
    results = {"sessions": n, "platform": sys.platform, "cpus": os.cpu_count()}
    home = os.environ.get("OCEAN_GRAFT_HOME") or os.path.expanduser("~/.ocean-graft")
    used_before = shutil.disk_usage(home).used

    # 1. Creation time as the count grows.
    create = []
    for name in names:
        out = sh("ocean-graft", "new", name)
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
        p = sh("ocean-graft", "path", name).strip()
        paths[name] = p
        target = EDIT_TARGETS[i % len(EDIT_TARGETS)]
        with open(os.path.join(p, target), "a") as f:
            f.write(f"\n// edit from {name}\n")
        with open(os.path.join(p, f"NOTES-{name}.md"), "w") as f:
            f.write(f"scratch notes for {name}\n")

    info = sh("ocean-graft", "info")
    # du-style total; on APFS it counts cloned blocks as if they were copies.
    results["project_disk_du"] = re.search(r"whole project: (.+)", info).group(1)
    # What the filesystem actually allocated for the N sessions and their edits.
    os.sync()
    results["volume_used_by_sessions_mb"] = round((shutil.disk_usage(home).used - used_before) / 1e6, 1)

    # 3. Commands whose cost grows with the number of sessions.
    ms, out = timed(sh, "ocean-graft", "ls")
    results["ls_ms"] = round(ms)
    ms, out = timed(sh, "ocean-graft", "conflicts")
    results["conflicts_ms"] = round(ms)
    results["conflict_pairs"] = out.count("CONFLICT in")
    results["clean_overlap_pairs"] = out.count("merge cleanly")
    # The check is incremental: nothing changed, then one session changes.
    ms, _ = timed(sh, "ocean-graft", "conflicts")
    results["conflicts_unchanged_ms"] = round(ms)
    with open(os.path.join(paths[names[0]], EDIT_TARGETS[0]), "a") as f:
        f.write("// one more edit\n")
    ms, _ = timed(sh, "ocean-graft", "conflicts")
    results["conflicts_after_one_change_ms"] = round(ms)

    # 4. Page cache: one copy of node_modules in memory however many sessions read it.
    if drop_caches():
        base = cached_kb()
        read_tree(os.path.join(paths[names[0]], "node_modules"))
        one = cached_kb() - base
        readers = names[:10]
        for name in readers[1:]:
            read_tree(os.path.join(paths[name], "node_modules"))
        many = cached_kb() - base
        results["page_cache_mb"] = {"after_1_session": round(one / 1024),
                                    f"after_{len(readers)}_sessions": round(many / 1024)}
    else:
        results["page_cache_mb"] = "skipped: cannot drop caches"

    # 5. Throughput: the test suite in k sessions at once (CPU-bound).
    tput = {}
    cpus = os.cpu_count() or 1
    for k in (1, cpus, 2 * cpus, 4 * cpus):
        group = names[:k]
        t = time.perf_counter()
        procs = [subprocess.Popen(["ocean-graft", "exec", g, "--", "npx", "vitest", "run"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) for g in group]
        codes = [p.wait() for p in procs]
        wall = time.perf_counter() - t
        tput[k] = {"wall_s": round(wall, 2), "runs_per_min": round(k / wall * 60, 1),
                   "all_passed": all(c == 0 for c in codes)}
    results["test_runs_in_parallel"] = tput

    # 6. Memory of one running dev server.
    server = subprocess.Popen(["ocean-graft", "exec", names[0], "--", "npx", "tsx", "src/server.ts"],
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
            sh("ocean-graft", "rm", "--force", name)
        print(f"removed {n} sessions in {time.perf_counter() - t:.2f} s")


if __name__ == "__main__":
    main()
