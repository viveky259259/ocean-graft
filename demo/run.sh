#!/usr/bin/env bash
# Sets up the four-session demo: a base repository made from notes-app, a warm
# template, and four sessions that are each checked with the test suite.
#
#   demo/run.sh [WORK_DIR]
#
# WORK_DIR defaults to ~/ocean-graft-demo. The base repository is created at
# WORK_DIR/notes-app; ocean-graft keeps its data under OCEAN_GRAFT_HOME (~/.ocean-graft).
# OCEAN_GRAFT_BACKEND=clone or overlay picks the backend (default: overlay as root
# on Linux, clone otherwise).
# Needs: ocean-graft on PATH (cargo build --release in this repository), git, node 22.12+.
set -euo pipefail
# Plain test output, whatever the caller's terminal settings (a Mac shell
# often sets FORCE_COLOR or CLICOLOR_FORCE): results are matched as text.
export NO_COLOR=1
unset FORCE_COLOR CLICOLOR_FORCE
strip_colors() { sed -e $'s/\x1b\\[[0-9;]*m//g'; }

here="$(cd "$(dirname "$0")" && pwd)"
work="${1:-$HOME/ocean-graft-demo}"
repo="$work/notes-app"
sessions=(search tags fix-limit validation)

command -v ocean-graft >/dev/null || { echo "ocean-graft is not on PATH" >&2; exit 1; }
if [ -e "$repo" ]; then
  echo "$repo already exists; remove it (and its ocean-graft project) first" >&2
  exit 1
fi

mkdir -p "$repo"
(cd "$here/notes-app" && tar cf - --exclude=node_modules .) | tar xf - -C "$repo"
cd "$repo"
git init -q -b main
git add -A
git -c user.name="Demo" -c user.email=demo@example.invalid commit -q -m "Initial notes app"

echo "\$ ocean-graft init --setup \"npm ci\""
ocean-graft init ${OCEAN_GRAFT_BACKEND:+--backend "$OCEAN_GRAFT_BACKEND"} --setup "npm ci"

for s in "${sessions[@]}"; do
  echo
  echo "\$ ocean-graft new $s"
  ocean-graft new "$s"
done

echo
echo "Running the test suite in every session (no install step):"
for s in "${sessions[@]}"; do
  printf '  %-11s ' "$s"
  ocean-graft exec "$s" -- npx vitest run 2>&1 | strip_colors | grep -E "Tests " | tr -s ' '
  ocean-graft checkpoint "$s" start >/dev/null
done

echo
ocean-graft ls
cat <<EOF

Each session is a full checkout on its own branch with node_modules installed.
Point one agent at each directory (see \`ocean-graft path <name>\`), then:

  ocean-graft ls               what each session has changed
  ocean-graft conflicts        files touched by more than one session, and real conflicts
  ocean-graft import --all     bring committed work back to $repo as ocean-graft/<name> branches
  ocean-graft exec <name> -- npm start    run a session's server on its own port
EOF
