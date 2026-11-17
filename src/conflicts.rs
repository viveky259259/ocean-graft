//! Incremental conflict detection across sessions.
//!
//! For every session, a fingerprint is taken from one `git status`: HEAD,
//! the status entries, and the size and modification time of every listed
//! path. Only sessions whose fingerprint changed since the last run get a new
//! snapshot commit (their working tree as it is, untracked files included).
//!
//! Pairs of sessions are compared only if they change a file in common, and
//! the result of `git merge-tree` is cached under the two snapshot ids, so
//! an unchanged pair is never merged twice. `merge-tree` runs in the
//! project's hub repository and reads both sessions' objects in place
//! through GIT_ALTERNATE_OBJECT_DIRECTORIES, so nothing is fetched or copied.
//! Independent work runs on all cores.

use crate::git;
use crate::project::Project;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Default)]
struct SessionEntry {
    fingerprint: String,
    snapshot: String,
    objects: String,
    files: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct PairEntry {
    /// Conflicting paths; empty means the pair merges cleanly.
    conflicts: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    sessions: BTreeMap<String, SessionEntry>,
    pairs: BTreeMap<String, PairEntry>,
}

pub struct Pair {
    pub a: String,
    pub b: String,
    pub shared: Vec<String>,
    pub conflicts: Vec<String>,
}

pub struct Report {
    /// Session name and the files it changed, committed or not.
    pub changed: Vec<(String, usize)>,
    /// Pairs that change at least one file in common.
    pub pairs: Vec<Pair>,
    pub quiet_pairs: usize,
    pub sessions_resnapshotted: usize,
    pub pairs_merged: usize,
}

fn fnv(bytes: &[u8], mut hash: u64) -> u64 {
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// HEAD, the status entries, and the size and mtime of each listed path.
fn fingerprint(dir: &Path) -> Result<String> {
    let out = git::command(dir)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ])
        .output()?;
    if !out.status.success() {
        bail!(
            "git status failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut records = out.stdout.split(|b| *b == 0).peekable();
    while let Some(record) = records.next() {
        hash = fnv(record, hash);
        let text = String::from_utf8_lossy(record);
        // The path is the last field; "2" (rename) records carry the
        // original path in the next record.
        let path = match text.chars().next() {
            Some('1') => text.splitn(9, ' ').nth(8),
            Some('2') => {
                let path = text.splitn(10, ' ').nth(9).map(str::to_string);
                if let Some(orig) = records.next() {
                    hash = fnv(orig, hash);
                }
                if let Some(p) = path {
                    hash = stat_into(dir, &p, hash);
                }
                continue;
            }
            Some('u') => text.splitn(11, ' ').nth(10),
            Some('?') => text.get(2..),
            _ => None,
        };
        if let Some(p) = path {
            hash = stat_into(dir, p, hash);
        }
    }
    Ok(format!("{hash:016x}"))
}

fn stat_into(dir: &Path, rel: &str, hash: u64) -> u64 {
    match std::fs::symlink_metadata(dir.join(rel)) {
        Ok(m) => {
            let stamp = format!("{}:{}:{}", m.len(), m.mtime(), m.mtime_nsec());
            fnv(stamp.as_bytes(), hash)
        }
        Err(_) => fnv(b"missing", hash),
    }
}

fn snapshot(dir: &Path, base: &str) -> Result<SessionEntry> {
    let snapshot = git::snapshot_commit(dir)?;
    // Keep the snapshot reachable so nothing prunes it.
    git::git(dir, &["update-ref", "refs/shoal/snapshot", &snapshot])?;
    let files = git::git(dir, &["diff", "--name-only", base, &snapshot])?
        .lines()
        .map(str::to_string)
        .collect();
    let objects = git::git_trim(dir, &["rev-parse", "--git-path", "objects"])?;
    let objects = if Path::new(&objects).is_absolute() {
        PathBuf::from(objects)
    } else {
        dir.join(objects)
    };
    Ok(SessionEntry {
        fingerprint: String::new(),
        snapshot,
        objects: objects.to_string_lossy().into_owned(),
        files,
    })
}

fn merge_tree(hub: &Path, a: &SessionEntry, b: &SessionEntry) -> Result<Vec<String>> {
    let out = git::command(hub)
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            format!("{}:{}", a.objects, b.objects),
        )
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            &a.snapshot,
            &b.snapshot,
        ])
        .output()?;
    match out.status.code() {
        Some(0) => Ok(Vec::new()),
        Some(1) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()),
        _ => bail!(
            "git merge-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Run `work` over `items` on all cores; results come back in input order.
fn parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= items.len() {
                    break;
                }
                let r = work(&items[i]);
                results.lock().expect("results lock")[i] = Some(r);
            });
        }
    });
    results
        .into_inner()
        .expect("results lock")
        .into_iter()
        .map(|r| r.expect("every item was processed"))
        .collect()
}

pub fn check(p: &Project) -> Result<Report> {
    git::require_version(2, 38)?;
    let hub = p.dir.join("hub.git");
    let cache_path = p.dir.join("conflicts-cache.json");
    let mut cache: Cache = std::fs::read(&cache_path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default();

    // 1. Fingerprint every session; snapshot only the ones that changed.
    let mut sessions = Vec::new();
    for (name, s) in &p.state.sessions {
        sessions.push((name.clone(), p.ensure_ready(name)?, s.base_commit.clone()));
    }
    let entries = parallel(
        &sessions,
        |(name, path, base)| -> Result<(SessionEntry, bool)> {
            let fp = fingerprint(path)?;
            if let Some(old) = cache.sessions.get(name) {
                if old.fingerprint == fp {
                    return Ok((old.clone(), false));
                }
            }
            let mut entry =
                snapshot(path, base).with_context(|| format!("could not snapshot {name}"))?;
            entry.fingerprint = fp;
            Ok((entry, true))
        },
    );
    let mut current: BTreeMap<String, SessionEntry> = BTreeMap::new();
    let mut resnapshotted = 0;
    for ((name, _, _), entry) in sessions.iter().zip(entries) {
        let (entry, fresh) = entry?;
        resnapshotted += usize::from(fresh);
        current.insert(name.clone(), entry);
    }

    // 2. Only pairs that change a file in common can conflict.
    let mut by_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, entry) in &current {
        for f in &entry.files {
            by_file.entry(f).or_default().push(name);
        }
    }
    let mut candidates: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (file, names) in &by_file {
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                candidates
                    .entry((a.to_string(), b.to_string()))
                    .or_default()
                    .insert(file.to_string());
            }
        }
    }
    let n = current.len();
    let quiet_pairs = n * n.saturating_sub(1) / 2 - candidates.len();

    // 3. Merge the pairs whose snapshots have not been merged before.
    let key = |a: &SessionEntry, b: &SessionEntry| {
        let (x, y) = if a.snapshot <= b.snapshot {
            (&a.snapshot, &b.snapshot)
        } else {
            (&b.snapshot, &a.snapshot)
        };
        format!("{x}:{y}")
    };
    let todo: Vec<(String, String)> = candidates
        .keys()
        .filter(|(a, b)| !cache.pairs.contains_key(&key(&current[a], &current[b])))
        .cloned()
        .collect();
    let merged = parallel(&todo, |(a, b)| merge_tree(&hub, &current[a], &current[b]));
    let mut fresh_pairs = BTreeMap::new();
    for ((a, b), result) in todo.iter().zip(merged) {
        fresh_pairs.insert(
            key(&current[a], &current[b]),
            PairEntry { conflicts: result? },
        );
    }
    cache.pairs.extend(fresh_pairs);

    let mut pairs = Vec::new();
    for ((a, b), shared) in &candidates {
        let conflicts = cache.pairs[&key(&current[a], &current[b])]
            .conflicts
            .clone();
        pairs.push(Pair {
            a: a.clone(),
            b: b.clone(),
            shared: shared.iter().cloned().collect(),
            conflicts,
        });
    }

    // Keep the cache to what still exists.
    let live: BTreeSet<&str> = current.values().map(|e| e.snapshot.as_str()).collect();
    cache.pairs.retain(|k, _| {
        let mut ids = k.split(':');
        ids.all(|id| live.contains(id))
    });
    cache.sessions = current.clone();
    let tmp = p.dir.join("conflicts-cache.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(&tmp, &cache_path)?;

    Ok(Report {
        changed: current
            .iter()
            .map(|(name, e)| (name.clone(), e.files.len()))
            .collect(),
        pairs,
        quiet_pairs,
        sessions_resnapshotted: resnapshotted,
        pairs_merged: todo.len(),
    })
}

/// Used by `shoal rm`: forget a removed session.
pub fn forget(project_dir: &Path, name: &str) {
    let path = project_dir.join("conflicts-cache.json");
    let Ok(raw) = std::fs::read(&path) else {
        return;
    };
    let Ok(mut cache) = serde_json::from_slice::<Cache>(&raw) else {
        return;
    };
    cache.sessions.remove(name);
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(&path, bytes);
    }
}
