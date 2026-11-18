# shoal plugin for Claude Code: architecture

Claude is the operator. The person talks to Claude; Claude uses shoal the
way it uses git. This plugin is how Claude Code reaches shoal: MCP tools for
deliberate actions, hooks for automatic ones, and a skill that says when to
use which.

## Layers

```
Claude Code ──MCP stdio──▶ shoal mcp ──────────────┐
            ──hooks (JSON on stdin)─▶ shoal hook <event> ─┤
scripts, CI ──▶ shoal api <tool> '<json>' ─────────┤
                                                    ▼
                                      tools.rs   one dispatcher: JSON in, JSON out
                                                    ▼
                                      ops.rs     every operation, once:
                                                 init, refresh, create/fork,
                                                 checkpoint, rewind, land, remove
                                                    ▼
                                      engine     project (state, lock), volumes
                                                 (overlay / clone), conflicts,
                                                 queue, services, gateway
```

Rules that keep the integration correct:

1. **One implementation per operation.** The CLI, the MCP tools and the hooks
   are adapters over `ops`/`tools`. None of them reimplements an operation.
2. **stdout belongs to the protocol.** In MCP mode stdout carries JSON-RPC;
   for WorktreeCreate its last line is the path. Engine code never prints to
   stdout. Progress goes to stderr. Child processes started by engine code
   (setup, tests) get stdin from /dev/null and their stdout sent to stderr or
   captured.
3. **Never hold the project lock across slow work.** Template refreshes
   (fetch, copy, setup) run under their own refresh lock, tests in `land`
   run under a per-session land lock, and the MCP server's idle time holds
   nothing; each call opens the project, does its work and releases it.
   A full copy (the clone backend on a filesystem without copy-on-write,
   such as ext4) is made without the project lock too: a `pending` entry in
   the state takes the new session's name, gc keeps the template it reads,
   and the session lock of what it copies keeps that from changing hands.
   The session lock's file names its holder, so a command that meets a copy
   waits (giving up the project lock while it does), and one that meets a
   land fails at once. A copy whose process died is cleaned up by the next
   command that opens the project.
4. **Nothing lost.** Any path that removes a session first saves its work:
   uncommitted changes are committed as WIP, the branch is imported into the
   source repository as `shoal/<name>`, and other local branches and stashes
   as `shoal/<name>--<branch>` and `shoal/<name>--stash-N`. Saving never
   moves an existing branch except forward: if `shoal/<name>` holds other
   work (an earlier session of the same name), the save goes to
   `shoal/<name>-2`. A session in the middle of a rebase or merge is not
   removed. `rewind` always checkpoints the state it replaces
   (`pre-rewind-N`; the newest 10 are kept, pruned only after a successful
   rewind and never the checkpoint being restored; a rewind that fails
   without changing anything drops the one it made), and
   `land` never overwrites ignored files in the user's checkout. Ignored
   files (dependencies, build output) are not saved on removal.
5. **Hooks never break Claude.** Stop, SubagentStop, SessionStart and
   UserPromptSubmit handlers always exit 0 and write diagnostics to stderr.
   WorktreeCreate and WorktreeRemove fail loudly (exit 1) because Claude
   Code must not continue with a half-made or wrongly deleted directory.
   Nothing ever exits 2 (which blocks a prompt or a stop): the launcher
   `libexec/shoal-plugin` maps every exit code, so a binary that is missing,
   older than the plugin, or crashes cannot block Claude.
7. **The binary is verified, not trusted.** The launcher uses `$SHOAL_BIN`,
   a binary bundled beside it, or a development build; otherwise it
   downloads the release binary for this platform once into the plugin's
   data directory. The download is checked against the SHA-256 in
   `libexec/release.txt`, which the release workflow commits with the
   plugin, so a tampered or truncated download is never run. On any failure
   it falls back to `shoal` on PATH, and keeps the exit-code rule above.
6. **Crash safety.** state.json is written durably (fsync, rename, fsync
   of the directory). Removal forgets a session before deleting its files.
   A rewind copies the checkpoint aside and swaps it in with renames; the
   next command to open the project repairs one that was killed halfway.
   A session some process works in (Claude Code's shell; the session's own
   services are stopped first and do not count) is instead restored in
   place, file by file, comparing contents, not just size and time, so that
   process is not left in a deleted directory and a busy overlay need not
   be unmounted; if that is interrupted, the `pre-rewind-N` checkpoint is
   the way back. A rewind's safety checkpoint is dropped only when the
   session is provably unchanged. Setup and
   test commands are killed with shoal if shoal itself is killed.
   Service process groups are only signalled while their leader's start
   time matches the one recorded, so a reused pid is never killed.

## MCP tools

Seen by Claude as `mcp__plugin_shoal_shoal__<tool>`. Each takes `session`
(a name) or `path` (any path inside a session) where a session is needed,
and an optional `project` (path to the repository) when the server's working
directory is not the repository.

| Tool | Does | Returns |
|---|---|---|
| `status` | Orientation in one call: project, every session, checkpoints, services, overlaps | JSON report |
| `fork` | New session(s) from the latest template, another session, or a checkpoint; `count` for best-of-N | name, path, port, branch per session |
| `checkpoint` | Save the whole session state, uncommitted and ignored files included | label, HEAD |
| `rewind` | Restore a checkpoint, first saving the current state as `pre-rewind-N`; stops and restarts the session's services | label, `saved_as`, HEAD |
| `conflicts` | Files changed by more than one session and which pairs conflict | pairs |
| `land` | Commit (optional), rebase onto the target branch, run tests through the queue, fast-forward the target branch, all within `timeout_secs` (default 1200, at most 1500, under Claude Code's 30-minute tool limit) | `landed`, `conflicts`, `tests_failed`, `timed_out`, `uncommitted_changes`, `nothing_to_land`, `target_blocked`, `target_moving` |
| `preview` | Start (or stop) the session's dev server behind a wake-on-request gateway | URL |
| `discard` | Remove a session. `keep_work` (default true) saves its work first (see rule 4); `keep_work: false` deletes it outright | what was saved |

Results are JSON in a text content block, so every MCP revision can carry
them. Errors set `isError` and say how to fix the problem.

## Hooks

| Event | Handler | Behaviour |
|---|---|---|
| WorktreeCreate | `shoal hook worktree-create` | Creates a shoal session for `claude --worktree`, subagents with `isolation: worktree`, and background sessions. Inside an existing session it forks that session, so a subagent starts with the parent's uncommitted work; otherwise it starts from the latest template (auto-initialising the project on first use, and falling back to the last good template if a refresh fails). Prints the path as the last stdout line. Claude Code never sends WorktreeRemove for subagent or workflow worktrees (and sends no field that tells them apart), so hook-made sessions are swept by a background `shoal sweep` once idle for 72 hours (`SHOAL_WORKTREE_IDLE_HOURS`, 0 turns it off), their work saved first, unless a main session ran in them: SessionStart, UserPromptSubmit or a main-session Stop (not SubagentStop) firing inside a session marks it interactive for good, through a marker file that needs no lock. Sessions running a service and sessions any process is working in are never swept; if the process list cannot be read, nothing is. The hook waits at most 60 s for another refresh and gives setup 600 s (hook timeout 900 s); past that it starts from the last good template and finishes the refresh in the background. First-time setup that is too slow for a hook goes on in the background too (the hook fails once, saying so). |
| WorktreeRemove | `shoal hook worktree-remove` | Saves the session's work (WIP commit, import as `shoal/<name>`), then removes it. Refuses paths that are not shoal sessions. |
| Stop, SubagentStop | `shoal hook stop` | Checkpoints the session the agent is in, when anything changed (ignored files included) and checkpoints are cheap (overlay, or copy-on-write clones). Keeps the 20 newest automatic checkpoints. `status` reports `auto_checkpoints`. |
| SessionStart | `shoal hook session-start` | Inside a session, tells Claude which session, branch and port it has, whether checkpoints are automatic, and which tools exist. |
| UserPromptSubmit | `shoal hook prompt` | Marks the session active, so idle shutdown leaves it alone. |

## Project file

`shoal.toml` at the repository root, all keys optional. Without it, shoal
detects the setup, test and preview commands from the lockfile and manifest.

```toml
setup = "npm ci"            # makes the template warm
test = "npm test"           # used by land
preview = "npm run dev"     # used by preview
copy = [".env"]             # ignored files to copy into the template
port_base = 4100           # 1..65526; each session reserves a block of ten
                            # that overlaps no other session's block on the
                            # machine, whatever their port_base
backend = "clone"           # or "overlay" (Linux, root)
```

shoal.toml must be a regular file (not a symlink or FIFO). `copy` entries
must resolve inside the repository. A committed change to `setup` takes
effect at the next template refresh. Setup is killed after
`SHOAL_SETUP_TIMEOUT_SECS` (default 1200). A refresh that fails is
remembered for its commit: forks use the last good template (reporting
`refresh_error`) and do not retry the same commit for ten minutes, or until
`shoal refresh`.

## Land, step by step

1. With `commit_message`, commit uncommitted work; otherwise refuse with
   `uncommitted_changes` and the list of files.
2. Fetch the target branch from the source repository into the session.
3. Checkpoint `pre-land-N`, then rebase the session's commits onto the
   target. On conflict: abort the rebase (the session is back where it was)
   and return `conflicts` with the files. Any other rebase failure is an
   error with git's message. If the rebase drops every commit (the target
   already has the changes), return `nothing_to_land`.
4. Run the test command through the job queue, with no project lock held.
   On failure return `tests_failed` with the end of the output; the
   checkpoint lets Claude rewind the rebase. If `timeout_secs` runs out
   (waiting for a slot, or while the tests run) return `timed_out`: the
   tests did not fail.
5. Fast-forward the target branch, never anything else, one land at a time
   per project. If the target is checked out, `git merge --ff-only` updates
   the working tree; it is refused (`target_blocked`) if it would overwrite
   the user's uncommitted edits, or any untracked or ignored file.
   Otherwise the branch ref is fetched fast-forward-only. If the target
   moved meanwhile, go back to step 2, at most three times.
6. The work is recorded as `shoal/<name>` in the source repository before
   step 5, without ever moving that branch backwards (see rule 4).

The session is locked for the whole land: `rewind`, `checkpoint`, `fork`
and `discard` of it fail with a clear message until the land finishes. If
`timeout_secs` runs out anywhere (a lock, the queue, the tests) the status
is `timed_out`.

## macOS

shoal ships for macOS first; there it uses the clone backend (APFS
`clonefile`) and runs as the user, not root. Rules that follow from that:

- **Names are lower case.** APFS matches file names, and git matches loose
  refs, without regard to case by default. Session names are lower-cased
  (a mixed-case name is refused), checkpoint labels are compared ignoring
  case, saved `shoal/` branches skip names that differ only in case, and a
  repository with paths that differ only in case is refused on a
  case-insensitive SHOAL_HOME rather than checked out wrongly. A case-only
  rename is landed normally.
- **Trees are removed with `remove_tree`,** which opens read-only
  directories first (Go module caches, `chmod a-w` output): without root,
  `remove_dir_all` cannot empty them.
- **Clones are probed, not assumed.** Automatic checkpoints are on only if
  `clonefile` works where SHOAL_HOME lives (not on HFS+, exFAT or a network
  home); sizes printed on macOS are logical, since clones share blocks.
- **Process start times come from the kernel** (`proc_pidinfo`), not from
  `ps` text, which changes with the time zone and locale.
- **Loopback means both 127.0.0.1 and ::1.** Dev servers bound to
  `localhost` listen on ::1 only with recent Node on macOS; the gateway
  listens and connects on both, and port checks also try to connect,
  because on macOS a test bind can succeed while another process listens on
  the wildcard address.
- **No /proc.** Working directories of processes come from libproc
  (`proc_listallpids` and `proc_pidinfo`, as Activity Monitor uses them),
  not from parsing `lsof`; if that fails, every session counts as in use.

Verified on a real Mac (macOS 27, Apple Silicon, APFS, as a normal user):
the whole suite and `demo/mac-test.sh` pass, including the four-session
demo, the 100-session scale test and the APFS benchmark. CI lints for
`aarch64-apple-darwin` and `x86_64-apple-darwin` and runs the Mac test on
macOS.
