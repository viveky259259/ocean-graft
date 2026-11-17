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
3. **Never hold the project lock across slow work.** Tests in `land` and the
   MCP server's idle time run without the lock; each call opens the project,
   does its work and releases it.
4. **Nothing lost.** Any path that removes a session first saves its work:
   uncommitted changes are committed as WIP and the branch is imported into
   the source repository as `shoal/<name>`.
5. **Hooks never break Claude.** Stop, SubagentStop, SessionStart and
   UserPromptSubmit handlers always exit 0 and write diagnostics to stderr.
   WorktreeCreate and WorktreeRemove fail loudly (non-zero) because Claude
   Code must not continue with a half-made or wrongly deleted directory.

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
| `rewind` | Restore a checkpoint; stops and restarts the session's services | label, HEAD |
| `conflicts` | Files changed by more than one session and which pairs conflict | pairs |
| `land` | Commit (optional), rebase onto the target branch, run tests through the queue, fast-forward the target branch | `landed`, `conflicts`, `tests_failed`, `uncommitted_changes`, `nothing_to_land`, `target_blocked` |
| `preview` | Start (or stop) the session's dev server behind a wake-on-request gateway | URL |
| `discard` | Remove a session; refuses if it holds work that is not in the source repository unless `force` | what was saved |

Results are JSON in a text content block, so every MCP revision can carry
them. Errors set `isError` and say how to fix the problem.

## Hooks

| Event | Handler | Behaviour |
|---|---|---|
| WorktreeCreate | `shoal hook worktree-create` | Creates a shoal session for `claude --worktree`, subagents with `isolation: worktree`, and background sessions. Inside an existing session it forks that session, so a subagent starts with the parent's uncommitted work; otherwise it starts from the latest template (auto-initialising the project on first use). Prints the path as the last stdout line. |
| WorktreeRemove | `shoal hook worktree-remove` | Saves the session's work (WIP commit, import as `shoal/<name>`), then removes it. Refuses paths that are not shoal sessions. |
| Stop, SubagentStop | `shoal hook stop` | Checkpoints the session the agent is in, when anything changed and checkpoints are cheap (overlay, or copy-on-write clones). Keeps the 20 newest automatic checkpoints. |
| SessionStart | `shoal hook session-start` | Inside a session, tells Claude which session, branch and port it has, and which tools exist. |
| UserPromptSubmit | `shoal hook prompt` | Marks the session active, so idle shutdown leaves it alone. |

## Project file

`shoal.toml` at the repository root, all keys optional. Without it, shoal
detects the setup, test and preview commands from the lockfile and manifest.

```toml
setup = "npm ci"            # makes the template warm
test = "npm test"           # used by land
preview = "npm run dev"     # used by preview
copy = [".env"]             # ignored files to copy into the template
port_base = 4100
backend = "clone"           # or "overlay" (Linux, root)
```

## Land, step by step

1. With `commit_message`, commit uncommitted work; otherwise refuse with
   `uncommitted_changes` and the list of files.
2. Fetch the target branch from the source repository into the session.
3. Checkpoint `pre-land-N`, then rebase the session's commits onto the
   target. On conflict: abort the rebase (the session is back where it was)
   and return `conflicts` with the files.
4. Run the test command through the job queue, with no project lock held.
   On failure return `tests_failed` with the end of the output; the
   checkpoint lets Claude rewind the rebase.
5. Fast-forward the target branch, never anything else. If the target is
   checked out, `git merge --ff-only` updates the working tree and refuses if
   it would overwrite the user's uncommitted edits (`target_blocked`).
   Otherwise the branch ref is fetched fast-forward-only. If the target
   moved meanwhile, go back to step 2, at most three times.
6. Record the branch as `shoal/<name>` in the source repository.
