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
   Known exception: copying a session or checkpoint on a filesystem without
   copy-on-write (ext4) is a full copy made under the lock.
4. **Nothing lost.** Any path that removes a session first saves its work:
   uncommitted changes are committed as WIP, the branch is imported into the
   source repository as `shoal/<name>`, and other local branches and stashes
   as `shoal/<name>--<branch>` and `shoal/<name>--stash-N`. Saving never
   moves an existing branch except forward: if `shoal/<name>` holds other
   work (an earlier session of the same name), the save goes to
   `shoal/<name>-2`. A session in the middle of a rebase or merge is not
   removed. `rewind` checkpoints the state it replaces (`pre-rewind-N`), and
   `land` never overwrites ignored files in the user's checkout. Ignored
   files (dependencies, build output) are not saved on removal.
5. **Hooks never break Claude.** Stop, SubagentStop, SessionStart and
   UserPromptSubmit handlers always exit 0 and write diagnostics to stderr.
   WorktreeCreate and WorktreeRemove fail loudly (exit 1) because Claude
   Code must not continue with a half-made or wrongly deleted directory.
   Nothing ever exits 2 (which blocks a prompt or a stop): the launcher
   `libexec/shoal-plugin` maps every exit code, so a binary that is missing,
   older than the plugin, or crashes cannot block Claude.
6. **Crash safety.** state.json is written durably (fsync, rename, fsync
   of the directory). Removal forgets a session before deleting its files.
   A rewind copies the checkpoint aside and swaps it in with renames; the
   next command to open the project repairs one that was killed halfway.
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
| `land` | Commit (optional), rebase onto the target branch, run tests through the queue, fast-forward the target branch, all within `timeout_secs` (default 1200, at most 1500, under Claude Code's 30-minute tool limit) | `landed`, `conflicts`, `tests_failed`, `uncommitted_changes`, `nothing_to_land`, `target_blocked`, `target_moving` |
| `preview` | Start (or stop) the session's dev server behind a wake-on-request gateway | URL |
| `discard` | Remove a session. `keep_work` (default true) saves its work first (see rule 4); `keep_work: false` deletes it outright | what was saved |

Results are JSON in a text content block, so every MCP revision can carry
them. Errors set `isError` and say how to fix the problem.

## Hooks

| Event | Handler | Behaviour |
|---|---|---|
| WorktreeCreate | `shoal hook worktree-create` | Creates a shoal session for `claude --worktree`, subagents with `isolation: worktree`, and background sessions. Inside an existing session it forks that session, so a subagent starts with the parent's uncommitted work; otherwise it starts from the latest template (auto-initialising the project on first use, and falling back to the last good template if a refresh fails). Prints the path as the last stdout line. Claude Code never sends WorktreeRemove for subagent worktrees, so it also removes worktree sessions idle for over 72 hours (`SHOAL_WORKTREE_IDLE_HOURS`), saving their work first. |
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
port_base = 4100           # 1..65526; ports are unique across all projects
backend = "clone"           # or "overlay" (Linux, root)
```

shoal.toml must be a regular file (not a symlink or FIFO). `copy` entries
must resolve inside the repository. A committed change to `setup` takes
effect at the next template refresh.

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
   checkpoint lets Claude rewind the rebase.
5. Fast-forward the target branch, never anything else, one land at a time
   per project. If the target is checked out, `git merge --ff-only` updates
   the working tree; it is refused (`target_blocked`) if it would overwrite
   the user's uncommitted edits, or any untracked or ignored file.
   Otherwise the branch ref is fetched fast-forward-only. If the target
   moved meanwhile, go back to step 2, at most three times.
6. The work is recorded as `shoal/<name>` in the source repository before
   step 5, without ever moving that branch backwards (see rule 4).

The session is locked for the whole land: `rewind` and `discard` of it fail
with a clear message until the land finishes.
