//! Git helpers. Every session is an ordinary, self-contained git checkout.

use crate::sys::output;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
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

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let top = git_trim(dir, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    Ok(PathBuf::from(top).canonicalize()?)
}

/// `git status --porcelain` lines: uncommitted and untracked changes.
pub fn status_lines(dir: &Path) -> Result<Vec<String>> {
    Ok(git(dir, &["status", "--porcelain"])?
        .lines()
        .map(str::to_string)
        .collect())
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
    let index = git_trim(dir, &["rev-parse", "--git-path", "shoal-snapshot-index"])?;
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
                .env("GIT_AUTHOR_NAME", "shoal")
                .env("GIT_AUTHOR_EMAIL", "shoal@localhost")
                .env("GIT_COMMITTER_NAME", "shoal")
                .env("GIT_COMMITTER_EMAIL", "shoal@localhost"),
        )
    };
    with_index(&["read-tree", "HEAD"])?;
    with_index(&["add", "-A"])?;
    let tree = with_index(&["write-tree"])?.trim().to_string();
    let commit = with_index(&["commit-tree", &tree, "-p", "HEAD", "-m", "shoal snapshot"])?
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
