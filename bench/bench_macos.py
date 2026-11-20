#!/usr/bin/env python3
"""APFS session benchmark.

On APFS, what does it cost to give a session a warm copy of a project tree,
and how fast is a disk-image volume for small-file work? A run on macOS 27
(Apple Silicon) is in results/macos-27-arm64-demo-app.json: one directory
clonefile took 9.9 ms, a per-file cp -Rc 186 ms, and a cloned disk image
224 ms plus attach. On a 42,243-file web app
(results/macos-27-arm64-large-app.json): 476 ms, 5.3 s and 248 ms
including attach, after an 18 s image build.

Strategies
  cp-Rc          per-file clonefile driven from user space (a plain `cp -Rc`)
  clonefile-dir  one clonefile(2) call on the directory (in-kernel walk)
  image-clone    clonefile of a sparse APFS disk image, then attach it

Usage
  python3 bench_macos.py /path/to/checkout-with-deps-installed [--runs 5]

Everything is created inside a fresh directory under --work-parent (default:
your home directory, so it is on the same APFS volume as most checkouts) and
removed afterwards. The source tree is only read. hdiutil is deprecated from
macOS 27 and prints a warning; a shipping implementation should use
`diskutil image` or DiskImageKit there.
"""
import argparse
import ctypes
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

LIBC = ctypes.CDLL(None, use_errno=True)


def run(*cmd, cwd=None, check=True):
    proc = subprocess.run([str(c) for c in cmd], cwd=cwd, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and proc.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))} failed: {proc.stderr.strip()}")
    return proc.stdout


def timed(fn, *args):
    start = time.perf_counter()
    result = fn(*args)
    return (time.perf_counter() - start) * 1000.0, result


def clonefile(src, dst):
    if LIBC.clonefile(os.fsencode(str(src)), os.fsencode(str(dst)), 0) != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err), str(src))


def attach(image, mountpoint):
    Path(mountpoint).mkdir(parents=True, exist_ok=True)
    run("hdiutil", "attach", image, "-mountpoint", mountpoint,
        "-nobrowse", "-noverify", "-noautofsck", "-quiet")


def detach(mountpoint):
    run("hdiutil", "detach", mountpoint, "-quiet", check=False)


def walk(path):
    """Stat every entry: a proxy for small-file metadata speed on the volume."""
    count = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            os.lstat(os.path.join(root, name))
            count += 1
    return count


def s_cp_rc(ctx, dst):
    dst.mkdir()
    run("cp", "-Rc", "-P", f"{ctx['src']}/", dst)
    return lambda: shutil.rmtree(dst)


def s_clonefile_dir(ctx, dst):
    clonefile(ctx["src"], dst)
    return lambda: shutil.rmtree(dst)


def s_image_clone(ctx, dst):
    image = ctx["work"] / f"{dst.name}.sparseimage"
    clonefile(ctx["tpl_image"], image)
    attach(image, dst)

    def cleanup():
        detach(dst)
        image.unlink()
        dst.rmdir()
    return cleanup


STRATEGIES = {"cp-Rc": s_cp_rc, "clonefile-dir": s_clonefile_dir, "image-clone": s_image_clone}


def build_template_image(ctx):
    kb = int(run("du", "-sk", ctx["src"]).split()[0])
    size_gb = max(2, (kb * 2) // (1024 * 1024) + 1)
    base = ctx["work"] / "template"
    run("hdiutil", "create", "-size", f"{size_gb}g", "-type", "SPARSE", "-fs", "APFS",
        "-volname", "OceanGraftTemplate", "-quiet", base)
    image = Path(str(base) + ".sparseimage")
    mnt = ctx["work"] / "template-mnt"
    attach(image, mnt)
    (mnt / ".metadata_never_index").touch()  # keep Spotlight off session volumes
    ms, _ = timed(run, "cp", "-R", "-P", f"{ctx['src']}/", mnt)
    if (mnt / ".git").exists():
        run("git", "status", cwd=mnt, check=False)  # refresh index stat data
    detach(mnt)
    ctx["tpl_image"] = image
    return round(ms, 1)


def concurrent_images(ctx, count):
    """Real sessions need several images attached at once. Clones of one image
    share its APFS volume UUID, so check that macOS mounts them side by side
    and keeps their writes apart."""
    mounts, attach_ms, error = [], [], None
    try:
        for i in range(count):
            image = ctx["work"] / f"concurrent-{i}.sparseimage"
            clonefile(ctx["tpl_image"], image)
            dst = ctx["work"] / f"concurrent-{i}"
            ms, _ = timed(attach, image, dst)
            attach_ms.append(round(ms, 1))
            if not os.path.ismount(dst) or not any(dst.iterdir()):
                raise RuntimeError(f"clone {i} attached but {dst} is not a populated mount")
            mounts.append(dst)
        (mounts[0] / "OCEAN_GRAFT_PROBE").write_text("only in the first\n")
        isolated = all(not (m / "OCEAN_GRAFT_PROBE").exists() for m in mounts[1:])
    except Exception as exc:  # report, do not abort the whole benchmark
        error, isolated = str(exc)[:400], None
    finally:
        for dst in mounts:
            detach(dst)
    return {"requested": count, "mounted": len(mounts), "attach_ms": attach_ms,
            "writes_isolated": isolated, "error": error}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    parser.add_argument("source")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--work-parent", default=str(Path.home()))
    parser.add_argument("--out")
    args = parser.parse_args()

    if platform.system() != "Darwin":
        sys.exit("macOS only; use svb.py on Linux")
    if os.geteuid() == 0:
        sys.exit("run as your normal user: the point is that no root is needed")
    src = Path(args.source).resolve()
    if not src.is_dir():
        sys.exit(f"not a directory: {src}")
    if os.stat(src).st_dev != os.stat(args.work_parent).st_dev:
        sys.exit("--work-parent must be on the same APFS volume as the source (clonefile cannot cross volumes)")

    work = Path(tempfile.mkdtemp(prefix="ocean-graft-bench.", dir=args.work_parent))
    ctx = {"src": src, "work": work}
    results = {"macos": platform.mac_ver()[0], "machine": platform.machine(), "rows": []}
    try:
        ms_native, files = timed(walk, src)
        results["files"] = files
        results["native_walk_ms"] = round(ms_native, 1)
        results["template_build_ms"] = build_template_image(ctx)
        is_git = (src / ".git").exists()
        for name, strategy in STRATEGIES.items():
            create, walks, status, discard = [], [], [], []
            for i in range(args.runs):
                dst = work / f"{name}-{i}"
                ms, cleanup = timed(strategy, ctx, dst)
                create.append(ms)
                walks.append(timed(walk, dst)[0])
                if is_git:
                    status.append(timed(lambda: run("git", "status", "--porcelain", cwd=dst))[0])
                discard.append(timed(cleanup)[0])
            row = {
                "strategy": name,
                "create_ms": round(statistics.median(create), 1),
                "create_ms_all": [round(x, 1) for x in create],
                "walk_ms": round(statistics.median(walks), 1),
                "discard_ms": round(statistics.median(discard), 1),
            }
            if status:
                row["first_status_ms"] = round(statistics.median(status), 1)
            print(json.dumps(row), flush=True)
            results["rows"].append(row)
        results["concurrent_images"] = concurrent_images(ctx, 4)
        print(json.dumps({"concurrent_images": results["concurrent_images"]}), flush=True)
    finally:
        for entry in work.iterdir():
            if entry.is_dir() and os.path.ismount(entry):
                detach(entry)
        shutil.rmtree(work, ignore_errors=True)
    print(json.dumps(results, indent=2))
    if args.out:
        Path(args.out).write_text(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
