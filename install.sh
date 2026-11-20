#!/bin/sh
# Install the ocean-graft command-line tool, for any coding agent.
#
#   curl -fsSL https://raw.githubusercontent.com/viveky259259/ocean-graft/main/install.sh | sh
#
# Downloads the ocean-graft binary for this machine (macOS on Apple Silicon or
# Intel, Linux on x86_64 or arm64) from a GitHub release of
# viveky259259/ocean-graft, checks it against the SHA-256 in that release's
# release.txt, and puts it in ~/.local/bin. A binary that does not match is
# never installed. It changes nothing else; `ocean-graft setup` then registers ocean-graft
# with your agents.
#
# Settings, all optional:
#   OCEAN_GRAFT_VERSION       a release such as 0.2.0 (default: the latest)
#   OCEAN_GRAFT_INSTALL_DIR   where to put ocean-graft (default: ~/.local/bin)
#   OCEAN_GRAFT_RELEASE_BASE  where releases are served (default: GitHub)
#
# Everything is in functions and runs from the last line, so a download cut
# off part way runs nothing.

set -eu

say() { printf '%s\n' "$*"; }
fail() {
  printf 'ocean-graft install: %s\n' "$*" >&2
  exit 1
}

fetch() { # url destination
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --connect-timeout 15 --retry 2 -o "$2" "$1" || fail "could not download $1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -T 60 -O "$2" "$1" || fail "could not download $1"
  else
    fail "neither curl nor wget is installed"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "neither sha256sum nor shasum is installed, so the download cannot be checked"
  fi
}

# The release asset for this machine: ocean-graft-<os>-<arch>.
asset_name() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Darwin)
      # A shell under Rosetta reports x86_64 on Apple Silicon: ask the hardware.
      if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ]; then
        arch=arm64
      fi
      case "$arch" in
        arm64 | x86_64) echo "ocean-graft-darwin-$arch" ;;
        *) fail "no ocean-graft build for macOS on $arch" ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64 | amd64) echo "ocean-graft-linux-x86_64" ;;
        aarch64 | arm64) echo "ocean-graft-linux-aarch64" ;;
        *) fail "no ocean-graft build for Linux on $arch" ;;
      esac
      ;;
    *) fail "no ocean-graft build for $os; ocean-graft runs on macOS and Linux" ;;
  esac
}

main() {
  base="${OCEAN_GRAFT_RELEASE_BASE:-https://github.com/viveky259259/ocean-graft/releases}"
  dir="${OCEAN_GRAFT_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"
  asset=$(asset_name)
  if [ -n "${OCEAN_GRAFT_VERSION:-}" ]; then
    from="$base/download/v${OCEAN_GRAFT_VERSION#v}"
  else
    from="$base/latest/download"
  fi

  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT INT TERM

  fetch "$from/release.txt" "$work/release.txt"
  version=$(awk '$1 == "version" {print $2}' "$work/release.txt")
  want=$(awk -v a="$asset" '$1 == "sha256" && $2 == a {print $3}' "$work/release.txt")
  [ -n "$version" ] || fail "release.txt names no version"
  if [ -n "${OCEAN_GRAFT_VERSION:-}" ] && [ "$version" != "${OCEAN_GRAFT_VERSION#v}" ]; then
    fail "asked for $OCEAN_GRAFT_VERSION, but that release's release.txt says $version"
  fi
  [ -n "$want" ] || fail "ocean-graft $version has no $asset"

  say "Downloading ocean-graft $version ($asset)"
  fetch "$from/$asset" "$work/ocean-graft"
  got=$(sha256_of "$work/ocean-graft")
  [ "$got" = "$want" ] || fail "the download does not match release.txt (got $got, expected $want); nothing was installed"
  chmod 755 "$work/ocean-graft"

  mkdir -p "$dir" || fail "cannot create $dir"
  # Copy beside the target, then rename over it: a running ocean-graft keeps its
  # file, and nobody sees a half-written one.
  cp "$work/ocean-graft" "$dir/.ocean-graft.$$" || fail "cannot write to $dir"
  mv -f "$dir/.ocean-graft.$$" "$dir/ocean-graft" || { rm -f "$dir/.ocean-graft.$$"; fail "cannot write to $dir"; }
  say "Installed ocean-graft $version at $dir/ocean-graft (SHA-256 $got)"

  case ":${PATH:-}:" in
    *":$dir:"*) ;;
    *)
      say ""
      say "$dir is not on your PATH. Add it, for example in ~/.profile or ~/.zshrc:"
      say "  export PATH=\"$dir:\$PATH\""
      ;;
  esac
  say ""
  say "Next, register ocean-graft with your agents. Run this inside a repository to also"
  say "add ocean-graft's instructions to its AGENTS.md (GEMINI.md for Gemini CLI):"
  say "  $dir/ocean-graft setup all        # Codex, Cursor, Gemini CLI, OpenCode: whichever are installed"
  say "For Claude Code, install the plugin instead:"
  say "  claude plugin install ocean-graft --marketplace viveky259259/ocean-graft"
}

main "$@"
