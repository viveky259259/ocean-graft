#!/usr/bin/env bash
# The whole ocean-graft test on a Mac, in one command:
#   1. build ocean-graft and run its end-to-end tests (clone backend, clonefile on APFS)
#   2. the four-session demo, replaying the agents' recorded commits, with
#      tests, conflicts, all four servers at once, and import
#   3. a 100-session scale test
#   4. the APFS session benchmark (cp -Rc vs one directory clonefile vs a
#      cloned disk image, plus several cloned images attached at once) on the
#      demo app with its node_modules
#
#   demo/mac-test.sh [--keep]
#
# Everything is created in a new folder in your home directory and deleted at
# the end unless you pass --keep. Your checkout and ~/.ocean-graft are not modified.
# The full output is saved to ~/ocean-graft-mac-test-<time>.log.
#
# Needs Rust (cargo), Node 22.12 or newer, git and python3. Ports 4100-4130
# must be free. For the page-cache measurement, run `sudo -v` first (the
# script uses `sudo -n purge` and skips that step if it cannot).
set -euo pipefail

if [ "$(uname -s)" != Darwin ]; then
  echo "This script is for macOS. On Linux use run.sh, replay.sh and scale.py." >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
keep=0
[ "${1:-}" = "--keep" ] && keep=1
stamp="$(date +%Y%m%d-%H%M%S)"
work="$HOME/ocean-graft-mac-test-$stamp"
log="$HOME/ocean-graft-mac-test-$stamp.log"
mkdir -p "$work"
export OCEAN_GRAFT_HOME="$work/ocean-graft-home"
exec > >(tee "$log") 2>&1

finish() {
  status=$?
  if [ "$keep" = 0 ]; then rm -rf "$work"; fi
  echo
  if [ "$status" = 0 ]; then echo "mac-test: all steps passed"; else echo "mac-test: FAILED (exit $status)"; fi
  echo "log: $log"
}
trap finish EXIT

section() { printf '\n=== %s\n' "$*"; }

section "Machine"
sw_vers
echo "arch $(uname -m), $(sysctl -n hw.ncpu) cpus, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB memory"
sysctl -n machdep.cpu.brand_string
diskutil info / | grep "File System Personality" || true
git --version
node --version
cargo --version

section "Build and end-to-end tests"
(cd "$root" && cargo build --release && cargo test --release --no-fail-fast -- --nocapture)
export PATH="$root/target/release:$PATH"

section "Four sessions, replaying the agents' work"
"$root/demo/run.sh" "$work/demo"
"$root/demo/replay.sh" "$work/demo/notes-app"

section "Scale test: 100 sessions"
(cd "$work/demo/notes-app" && python3 "$root/demo/scale.py" --sessions 100)

section "APFS benchmark on the demo app with its node_modules"
# A real tree with dependencies installed: the demo app after npm ci.
cp -R "$root/demo/notes-app" "$work/bench-src"
(cd "$work/bench-src" && rm -rf node_modules && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error)
python3 "$root/bench/bench_macos.py" "$work/bench-src" \
  --runs 3 --work-parent "$work"
