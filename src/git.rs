//! Git helpers. Every session is an ordinary, self-contained git checkout.

use crate::sys::output;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    // GIT_OPTIONAL_LOCKS=0: ocean-graft's own `git status` calls must not take
    // index.lock and make an agent's concurrent `git commit` fail.
    cmd.arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    output(command(dir).args(args))
}

pub fn git_trim(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(git(dir, args)?.trim().to_string())
}

pub fn head(dir: &Path) -> Result<String> {
    git_trim(dir, &["rev-parse", "HEAD"])
}

/// A directory outside any git repository (as opposed to a git failure).
#[derive(Debug)]
pub struct NotARepository(pub PathBuf);

impl std::fmt::Display for NotARepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not inside a git repository", self.0.display())
    }
}

impl std::error::Error for NotARepository {}

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let top = match git_trim(dir, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => top,
        Err(err) => {
            let not_repo = format!("{err:#}").contains("not a git repository");
            return Err(if not_repo {
                anyhow::Error::new(NotARepository(dir.to_path_buf())).context(format!("{err:#}"))
            } else {
                err.context(format!("{} is not inside a git repository", dir.display()))
            });
        }
    };
    Ok(PathBuf::from(top).canonicalize()?)
}

/// `git status --porcelain` lines: uncommitted and untracked changes.
pub fn status_lines(dir: &Path) -> Result<Vec<String>> {
    Ok(git(dir, &["status", "--porcelain"])?
        .lines()
        .map(str::to_string)
        .collect())
}

/// The paths `git status` reports as changed or untracked, unquoted (spaces
/// and non-ASCII names come back as they are on disk). A rename reports its
/// new path.
pub fn status_paths(dir: &Path) -> Result<Vec<String>> {
    let out = output_bytes(command(dir).args(["status", "--porcelain", "-z"]))?;
    let mut paths = Vec::new();
    let mut records = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let (code, path) = (&record[..2], &record[3..]);
        paths.push(String::from_utf8_lossy(path).into_owned());
        if code.contains(&b'R') || code.contains(&b'C') {
            records.next(); // the original path
        }
    }
    Ok(paths)
}

fn output_bytes(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .context("could not run git")?;
    if !out.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

pub fn count_commits(dir: &Path, range: &str) -> Result<u32> {
    Ok(git_trim(dir, &["rev-list", "--count", range])?
        .parse()
        .unwrap_or(0))
}

pub fn short(rev: &str) -> &str {
    &rev[..rev.len().min(8)]
}

/// A commit holding the working tree as it is right now, including untracked
/// (but not ignored) files. HEAD, the branch and the real index are untouched.
pub fn snapshot_commit(dir: &Path) -> Result<String> {
    let index = git_trim(
        dir,
        &["rev-parse", "--git-path", "ocean-graft-snapshot-index"],
    )?;
    let index = if Path::new(&index).is_absolute() {
        PathBuf::from(index)
    } else {
        dir.join(index)
    };
    let _ = std::fs::remove_file(&index);
    let with_index = |args: &[&str]| -> Result<String> {
        output(
            command(dir)
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_AUTHOR_NAME", "ocean-graft")
                .env("GIT_AUTHOR_EMAIL", "ocean-graft@localhost")
                .env("GIT_COMMITTER_NAME", "ocean-graft")
                .env("GIT_COMMITTER_EMAIL", "ocean-graft@localhost"),
        )
    };
    with_index(&["read-tree", "HEAD"])?;
    with_index(&["add", "-A"])?;
    let tree = with_index(&["write-tree"])?.trim().to_string();
    let commit = with_index(&[
        "commit-tree",
        &tree,
        "-p",
        "HEAD",
        "-m",
        "ocean-graft snapshot",
    ])?
    .trim()
    .to_string();
    let _ = std::fs::remove_file(&index);
    Ok(commit)
}

/// Fail unless git is at least `major.minor`.
pub fn require_version(major: u32, minor: u32) -> Result<()> {
    let version = output(Command::new("git").arg("--version"))?;
    let numbers: Vec<u32> = version
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .split('.')
        .filter_map(|n| n.parse().ok())
        .collect();
    let found = (
        numbers.first().copied().unwrap_or(0),
        numbers.get(1).copied().unwrap_or(0),
    );
    if found < (major, minor) {
        bail!(
            "this needs git {major}.{minor} or newer (found {})",
            version.trim()
        );
    }
    Ok(())
}
