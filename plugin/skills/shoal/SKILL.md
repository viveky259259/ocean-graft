---
name: shoal
description: Isolated, instant copy-on-write sessions of the repository. Use when you want to try more than one approach, make a risky change (migration, large refactor, dependency upgrade) you may need to undo, keep work away from the user's checkout until it passes tests, run parallel subagents that must not collide, or show the user a running preview of a change.
---

# Working with shoal sessions

shoal gives you sessions: complete copies of the repository with dependencies
installed, each on its own branch and port, created in milliseconds. Changes
in a session never touch the user's checkout until you land them.

The tools are `mcp__plugin_shoal_shoal__<tool>`. Every tool that works on one
session takes `session` (a name from `status`) or `path` (your working
directory, if it is inside a session).

## Decide

| Situation | Do this |
|---|---|
| Unsure where you are, or about to change anything | `status` |
| Two or more plausible approaches | `fork` with `count` set to the number of approaches, one approach per session; run the tests in each; keep the best and `discard` the rest |
| About to run a migration, a large refactor, a dependency upgrade, or anything you might regret | `checkpoint` with a descriptive `label` first |
| Something went wrong | `rewind` to your checkpoint, or `label: "latest"`. When `status` says `auto_checkpoints: true`, checkpoints named `turn-N` are taken at the end of each turn; otherwise checkpoint yourself before risky steps. A rewind saves what it replaces as `pre-rewind-N` (`saved_as`), so it can be undone |
| Work is done and should reach the user | `land`. Pass `commit_message` if there are uncommitted changes |
| The user should see the change running | `preview`, then give the user the URL |
| Several sessions in progress | `conflicts` before landing any of them |
| Explore from an earlier state without losing the current one | `fork` with `from` set to the session and `at` set to the checkpoint |

## Rules

- Work inside the session's `path`. Run commands there, and edit files there.
- Never edit the user's checkout directly while a session exists for the task.
- After `land`, read `status` in the result:
  - **`conflicts`**: merge the target into the session, resolve, commit, and land again. The session was left unchanged.
  - **`tests_failed`**: fix the failures, or `rewind` to the `pre-land-N` checkpoint named in the result.
  - **`timed_out`**: the tests did not fail; the land ran out of time. Land again, with a larger `timeout_secs` if the tests are slow.
  - **`target_blocked`**: the user has uncommitted edits, or untracked or ignored files (listed in `files`), in the way. Tell them; the work is safe on the branch named in the message.
  - **`nothing_to_land`**: the target already has these changes.
- `discard` keeps work by default: it commits leftovers as WIP and saves the branch (plus any other branches and stashes) in the source repository. Pass `keep_work: false` only when the session's work is worthless.
- If `fork` returns `refresh_error`, the session started from an older template; tell the user why (often a failing setup on the latest commit).
- Subagents started with worktree isolation get their own session automatically. Inside a session, a subagent's session is a fork of yours, including your uncommitted work.
