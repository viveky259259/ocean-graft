#!/usr/bin/env bash
# Replays the four agents' commits from the live test (demo/agent-work/*.patch)
# into the sessions run.sh created, then checks the result end to end:
# tests in every session, conflicts, all four servers at once, and import.
#
#   demo/replay.sh [BASE_REPO]
#
# BASE_REPO defaults to ~/ocean-graft-demo/notes-app. Exits non-zero if any check fails.
set -euo pipefail
# Plain test output, whatever the caller's terminal settings (a Mac shell
# often sets FORCE_COLOR or CLICOLOR_FORCE): results are matched as text.
export NO_COLOR=1
unset FORCE_COLOR CLICOLOR_FORCE
strip_colors() { sed -e $'s/\x1b\\[[0-9;]*m//g'; }
set -m # each background job gets its own process group, so servers can be stopped cleanly

here="$(cd "$(dirname "$0")" && pwd)"
repo="${1:-$HOME/ocean-graft-demo/notes-app}"
sessions=(search tags fix-limit validation)
cd "$repo"

fail() { echo "FAIL: $*" >&2; exit 1; }
json() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{const d=JSON.parse(s);console.log($1)})"; }

echo "== Applying each agent's commits in its own session"
for s in "${sessions[@]}"; do
  path="$(ocean-graft path "$s")"
  git -C "$path" -c user.name="ocean-graft demo" -c user.email=demo@example.invalid \
    am -q "$here/agent-work/$s.patch"
  printf '  %-11s %s\n' "$s" "$(git -C "$path" log --oneline -1)"
done
# The validation agent installed zod; install it in that session only.
ocean-graft exec validation -- npm install --no-audit --no-fund --loglevel=error >/dev/null

echo
echo "== Tests and typecheck in every session"
expected=(7 8 6 11)
for i in "${!sessions[@]}"; do
  s="${sessions[$i]}"
  result="$(ocean-graft exec "$s" -- npx vitest run 2>&1 | strip_colors | grep -E 'Tests ' | tr -s ' ')"
  ocean-graft exec "$s" -- npx tsc --noEmit || fail "$s: typecheck"
  printf '  %-11s %s, typecheck ok\n' "$s" "$result"
  [[ "$result" == *"${expected[$i]} passed (${expected[$i]})"* ]] || fail "$s: expected ${expected[$i]} passing tests"
done
for s in search tags fix-limit; do
  [ ! -d "$(ocean-graft path "$s")/node_modules/zod" ] || fail "zod leaked into $s"
done
echo "  zod is installed only in validation"

echo
echo "== Conflicts"
report="$(ocean-graft conflicts)"
echo "$report"
[ "$(grep -c 'CONFLICT in' <<<"$report")" = 5 ] || fail "expected 5 conflicting pairs"

echo
echo "== All four servers at once"
pids=()
for s in "${sessions[@]}"; do
  ocean-graft exec "$s" -- npx tsx src/server.ts >/dev/null 2>&1 &
  pids+=($!)
  disown # no "Terminated" job notices when the servers are stopped
done
stop_servers() { for pid in "${pids[@]}"; do kill -- "-$pid" 2>/dev/null || true; done; }
trap stop_servers EXIT
for _ in $(seq 1 80); do
  up=0
  for port in 4100 4110 4120 4130; do curl -sf "localhost:$port/health" >/dev/null && up=$((up + 1)); done
  [ "$up" = 4 ] && break
  sleep 0.25
done
[ "$up" = 4 ] || fail "only $up of 4 servers came up"

n="$(curl -s 'localhost:4100/notes?q=MILK' | json 'd.total')"
echo "  search      GET /notes?q=MILK            -> $n match"
[ "$n" = 1 ] || fail "search"
n="$(curl -s 'localhost:4110/notes?tag=ideas' | json 'd.items.map(n=>n.title).join()')"
echo "  tags        GET /notes?tag=ideas         -> $n"
[ "$n" = Ideas ] || fail "tags"
a="$(curl -s 'localhost:4120/notes?limit=2' | json 'd.items.length')"
b="$(curl -s 'localhost:4130/notes?limit=2' | json 'd.items.length')"
echo "  fix-limit   GET /notes?limit=2           -> $a items (validation, without the fix: $b)"
[ "$a" = 2 ] && [ "$b" = 1 ] || fail "fix-limit"
a="$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:4130/notes -H 'content-type: application/json' -d '{"title":"  "}')"
b="$(curl -s -o /dev/null -w '%{http_code}' -X POST localhost:4100/notes -H 'content-type: application/json' -d '{"title":"  "}')"
echo "  validation  POST /notes blank title      -> HTTP $a (search, without validation: $b)"
[ "$a" = 400 ] && [ "$b" = 201 ] || fail "validation"
stop_servers
trap - EXIT
sleep 1

echo
echo "== Import into the base repository"
ocean-graft import --all
git branch --list 'ocean-graft/*' -v
[ -z "$(git status --porcelain)" ] || fail "base working tree changed"
echo "  base working tree untouched"
echo
echo "All checks passed."
