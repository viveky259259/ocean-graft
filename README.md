# shoal

Copy-on-write session volumes for running coding agents in parallel. It is a
standalone CLI with no dependency on Ocean, and it implements the design in
[`docs/specs/session-volumes.md`](../../docs/specs/session-volumes.md).

`shoal init` builds a **template** from a git repository and runs a setup
command in it once, for example `npm ci`. `shoal new` then gives each agent a
**session**: a copy-on-write child of the template. A session is a complete
checkout with its own `.git`, its own branch, installed dependencies and its
own port. It is ready in milliseconds and stores only what it changes.

```bash
cd tools/shoal && cargo build --release    # binary: target/release/shoal

cd ~/code/my-app
shoal init --setup "npm ci"                # once: clone + install into the template
shoal new search                           # ~1.5 ms, warm, on branch shoal/search
shoal new tags
cd "$(shoal path search)"                  # point an agent here
shoal ls                                   # what every session has changed
shoal conflicts                            # overlaps and real conflicts, before anyone commits
shoal import --all                         # commits come back as shoal/<name> branches
```

## Commands

| Command | What it does |
|---|---|
| `init [repo] --setup CMD [--copy .env] [--backend overlay\|clone]` | Clone the repo into a template and run the setup command in it once |
| `new NAME` | Create a session from the latest template, on branch `shoal/NAME` |
| `fork SOURCE NAME` | Copy a session as it is now, including uncommitted and ignored files |
| `ls` / `status NAME` | Commits, uncommitted files, and the bytes each session stores |
| `path NAME` | Print the session directory, mounting it again if needed |
| `diff NAME [--stat]` | Everything changed against the template, including untracked files |
| `exec [--sandbox] NAME -- CMD` | Run a command in the session with `SHOAL_SESSION`, `SHOAL_PORT` and `PORT` set |
| `shell [--sandbox] NAME` | Open a shell in the session |
| `run [NAME] [--slots K] -- CMD` | Run a heavy command (tests, a build) once a machine-wide queue slot is free. Inside a session, NAME can be left out |
| `queue [--slots N]` | Show running and waiting jobs, or set how many may run at once (default: one per CPU) |
| `serve NAME [--as SERVICE] -- CMD` | Start a long-running service, such as a dev server, in the background on the session's port |
| `services` / `stop NAME [SERVICE]` | List services with their memory and idle time; stop them |
| `reap [--idle 15m] [--watch 1m]` | Stop the services of sessions with no recent activity, in every project |
| `wake NAME` / `touch NAME` | Restart services that `reap` stopped; mark a session active (for agent hooks) |
| `checkpoint NAME [LABEL]` / `rewind NAME LABEL` | Save the whole session state and restore it later |
| `conflicts` | Files that two or more sessions changed, and which pairs really conflict (`git merge-tree`). Incremental: only sessions that changed since the last check are looked at again |
| `import [NAME...] [--all]` | Fetch session commits into the source repo as `shoal/NAME` branches |
| `refresh` | Build the next template generation from the source repo; setup re-runs only when a lockfile changed |
| `rm NAME [--force]` | Remove a session; refuses if it would lose uncommitted or unimported work |
| `up` / `down` | Mount or unmount every session, for example after a reboot |
| `gc` / `info` | Delete unused layers; show generations and disk use |

## Backends

| Backend | Platform | How a session is made | Tested |
|---|---|---|---|
| `overlay` | Linux, as root | overlayfs: template layers are read-only lowers, the session is the upper dir. Constant time, stores only changes. | Yes: end-to-end tests and the live run below |
| `clone` | macOS | One `clonefile(2)` call on the template directory (APFS copy-on-write). Grows with the number of entries. | Type-checked for `aarch64-apple-darwin`; **not yet run on a Mac** |
| `clone` | Linux, any user | `cp -a --reflink=auto`: shares blocks on XFS and btrfs, full copy elsewhere | Yes: end-to-end tests |

`--sandbox` uses bubblewrap on Linux. Everything outside the session becomes
read-only, the command gets a private `/tmp`, and other sessions and the
template are hidden. It is not available on macOS yet.

## How it works

```text
~/.shoal/projects/<repo>-<hash>/
  state.json        what exists; every command takes a lock while it reads or writes it
  layers/g1/        overlay: generation 1 template (clone + installed dependencies)
  layers/g2/        overlay: what changed in generation 2, on top of g1
  templates/gN/     clone backend: one full template per generation
  sessions/NAME/    the session directory: an overlay mount, or a clone
  volumes/NAME/     overlay: the session's upper and work dirs, the only per-session storage
  checkpoints/      saved session states
  hub.git           bare repo where `conflicts` compares sessions
```

- **Git.** Each session has its own `.git`, so a checkpoint or fork captures
  HEAD, the index and any rebase in progress along with the files. `import`
  fetches `HEAD` from the session into the source repo.
- **Conflicts.** `conflicts` fingerprints each session from one `git status`
  (HEAD, the entries, and the size and modification time of each listed
  file). Only a session whose fingerprint changed gets a new snapshot: its
  working tree committed to a throwaway object, with HEAD and the index left
  alone. Only pairs that change a file in common are merged with
  `git merge-tree`. It runs in `hub.git` and reads both sessions' objects in
  place, so nothing is copied. Results are cached by the two snapshot ids,
  and the work runs on all cores.
- **Templates are never modified.** Sessions set `gc.auto=0` so they never
  repack shared objects. `refresh` builds a new generation on top of the old
  one; running sessions keep theirs.
- **Ports.** Session *n* gets `4100 + 10n` as `SHOAL_PORT`, and as `PORT`
  unless `PORT` is already set.

## Live test: four agents on one repo

Run on Linux 6.18 (4 vCPU VM) with the overlay backend, using
[`demo/notes-app`](demo/notes-app): a small Hono and TypeScript service with
Vitest tests and 1,506 files in `node_modules`.
[`demo/run.sh`](demo/run.sh) reproduces the setup.

1. **Template.** `shoal init --setup "npm ci"` took 1.7 s and used 128 MB.
2. **Sessions.** `shoal new` created `search`, `tags`, `fix-limit` and
   `validation` in 1.2 to 1.5 ms each, plus 7 to 55 ms for the branch. The
   test suite passed in every session straight away, with no install step.
3. **Agents.** Four agents worked at the same time, one per session:
   - `search`: add text search.
   - `tags`: add tags.
   - `fix-limit`: fix a paging bug planted in the base.
   - `validation`: add zod validation, which installs a package.

   Each finished in 17 to 21 s with one commit. Their tests pass (7, 8, 6 and
   11 tests) and so does `tsc`, re-run independently afterwards.
4. **Isolation.**
   - The package `validation` installed exists only in that session: 848 new
     files, 8.8 MB stored. The template and the other three sessions do not
     have it.
   - Each of the other sessions stores about 0.5 MB.
   - The whole project uses 141 MB on disk, against about 510 MB for four
     full copies.
5. **Conflicts before merge.** Run while the agents were still editing, it
   found nothing yet. Run once they had finished:

   ```text
   fix-limit + search       CONFLICT in: src/store.ts
   fix-limit + tags         CONFLICT in: src/store.ts
   fix-limit + validation   merge cleanly, both change: src/app.ts, test/app.test.ts
   search + tags            CONFLICT in: src/app.ts, src/store.ts, test/app.test.ts
   search + validation      CONFLICT in: test/app.test.ts
   tags + validation        CONFLICT in: src/app.ts, test/app.test.ts
   ```

   `search` and `tags` both fixed the planted paging bug on the same line as
   `fix-limit`, without being asked to.
6. **Running side by side.** All four servers ran at once on ports
   4100–4130, each serving only its own change. See
   [`demo/four-sessions.png`](demo/four-sessions.png).
7. **Import.** `shoal import --all` added four `shoal/*` branches to the
   source repo and left its working tree untouched.
8. **Fork and rewind.**
   - A fork of `search` was ready in 11 ms.
   - After `rm -rf src node_modules` in the fork, its tests failed while
     `search` still passed.
   - `shoal rewind` restored the fork in 90 ms, and its tests passed again.

## Scale test: 100 sessions on one machine

[`demo/scale.py`](demo/scale.py) adds 100 sessions to the live-test project,
for 104 in total, and gives each a small edit. Every fifth session edits the
same tracked file, so real conflicts exist. Measured on a 4 vCPU VM with the
overlay backend.

| What | Result |
|---|---|
| Create a session | 0.2 ms median for the first 10, 0.5 ms for the last 10, 1.2 ms worst; plus 4 ms to create the branch |
| Disk for 104 sessions | 167 MB, of which the template is 128 MB |
| Memory for files | `node_modules` read in 10 sessions occupied 109 MB of page cache, the same as in 1. Overlay sessions share the template's cache. Reflink copies on XFS do not: each extra copy added about 110 MB. |
| `shoal ls` | 678 ms (about 6.5 ms per session) |
| `shoal conflicts` | 5,356 pairs, of which 1,216 share a file and 955 conflict. The first version took 7.5 s. The incremental check takes 3.8 s the first time, 0.19 s when nothing changed, and 0.23 s after one session changes. |
| Test suite in parallel | 53 runs/min with 1 session, 218 with 4, 231 with 8, 234 with 16. It saturates at the core count. |
| One running dev server | 209 MB resident |
| Remove 100 sessions | 1.65 s in one run and 12.4 s in another. The time is the kernel unmounting each overlay (16 to 220 ms), which shoal waits for so it never removes a session someone is still using. |

On one machine, storage stops being the limit. CPU and memory for running
processes limit how many sessions can be active.

## Job queue and idle shutdown

On one machine, the limits are CPU and memory for running processes, not
sessions. Two features keep many sessions practical.

**Job queue.** `shoal run -- npm test` waits for one of N slots before it
starts. There is one slot per CPU by default, shared by every project on the
machine.
- Each slot is an exclusive lock on a file, so a job that crashes frees its
  slot at once. No daemon is needed.
- Waiters line up in arrival order.
- A job can ask for several slots (`--slots`).
- `shoal queue` shows what is running and what is waiting.

**Idle shutdown.** `shoal serve` starts a session's dev server in its own
process group, on the session's port, with a log under `logs/` in the
project. `shoal reap --idle 15m` stops the servers of sessions that have
been idle that long, and frees their memory. Run it with `--watch 1m` to
keep checking. `shoal wake` starts them again.

A session counts as active when any of these happen:
- shoal runs something in it (`exec`, `run`, `serve`, `touch`)
- a file in it changes, apart from dependency, build and cache directories
- its git history moves

`rm`, `down` and `rewind` stop a session's services first; `rewind` starts
them again afterwards.

Measured with [`demo/resources.py`](demo/resources.py) on the notes app
(4 vCPU, overlay backend, two runs):

| | Result |
|---|---|
| 16 test runs started at once, no queue | 4.4–5.1 s, up to 80 test-runner processes, 3.9–4.3 GB peak |
| The same 16 through the queue (4 slots) | 4.4–4.8 s, up to 32 processes, 1.05–1.12 GB peak |
| 8 dev servers | 1.6–1.9 GB in total, about 210–240 MB each |
| `reap --idle 3s`, with 2 sessions still being edited | stopped the 6 idle servers in 1.35 s, freed about 1.3 GB, kept the 2 busy ones |
| `wake` | the server answered again 0.5–1.3 s later |

The queue does not make this workload faster. It cuts peak memory by about
four times, which is what lets many sessions share a laptop.

## Tests

```bash
cargo test --release
```

The tests drive the real binary against a throwaway repository. They cover
the full lifecycle on both backends: isolation, status, diff, ports,
conflicts, checkpoint and rewind, fork, import, refresh, rm safety, remount
and gc. They also cover sandbox confinement, the queue never running more
jobs than it has slots, and idle shutdown, which stops the idle session's
server, keeps the busy one, then wakes, stops and removes them. The overlay
test needs root and is skipped otherwise.

## Limitations

- The overlay backend needs root. As an ordinary user on Linux, `clone` is
  used instead; it copies files unless the filesystem supports reflinks.
- macOS support has been type-checked but not run.
- A checkpoint or fork of a session that is being written to is not atomic.
  Take them between agent turns.
- Rewinding an overlay session unmounts it, so nothing may be running inside
  it at the time.
- Ports are handed out, not reserved; `shoal` does not check whether a port
  is free.
- The queue only governs commands started with `shoal run`. An agent that
  runs `npm test` directly bypasses it, so agents have to be told to use it
  (Ocean can wrap their commands).
- A stopped server comes back with `shoal wake`, not on the first request to
  its port.
