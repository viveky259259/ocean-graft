//! Project configuration: `ocean-graft.toml` at the repository root, with every
//! key optional. Missing keys are detected from the repository's lockfiles
//! and manifests, so most projects need no file at all.

use crate::project::Backend;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub setup: Option<String>,
    pub test: Option<String>,
    pub preview: Option<String>,
    #[serde(default)]
    pub copy: Vec<String>,
    #[serde(default)]
    pub setup_inputs: Vec<String>,
    pub port_base: Option<u16>,
    pub backend: Option<Backend>,
}

/// Configuration after detection fills the gaps.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub setup: Option<String>,
    pub test: Option<String>,
    pub preview: Option<String>,
    pub copy: Vec<String>,
    pub setup_inputs: Vec<String>,
    pub port_base: Option<u16>,
    pub backend: Option<Backend>,
}

pub const FILE_NAME: &str = "ocean-graft.toml";
const MAX_FILE: u64 = 256 * 1024;
/// Each session takes a block of ten ports, so the last base must leave room.
pub const PORT_BASE_MAX: u16 = u16::MAX - 9;

/// Read ocean-graft.toml without following a symlink out of the repository and
/// without blocking on a FIFO or device someone named ocean-graft.toml.
fn read_config_text(path: &Path) -> Result<Option<String>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    if !meta.file_type().is_file() {
        bail!(
            "{} must be a regular file (not a symlink, directory or FIFO)",
            path.display()
        );
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    if !file.metadata()?.file_type().is_file() {
        bail!("{} must be a regular file", path.display());
    }
    let mut text = String::new();
    file.take(MAX_FILE + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("could not read {} as UTF-8 text", path.display()))?;
    if text.len() as u64 > MAX_FILE {
        bail!("{} is larger than {} KiB", path.display(), MAX_FILE / 1024);
    }
    Ok(Some(text))
}

pub fn read_file(repo: &Path) -> Result<FileConfig> {
    let path = repo.join(FILE_NAME);
    let Some(text) = read_config_text(&path)? else {
        return Ok(FileConfig::default());
    };
    let parsed: FileConfig =
        toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    for item in &parsed.copy {
        let rel = Path::new(item);
        if item.is_empty() || rel.is_absolute() || rel.components().any(|c| c.as_os_str() == "..") {
            bail!(
                "{}: copy entries must be paths inside the repository, got {item:?}",
                path.display()
            );
        }
    }
    if let Some(port) = parsed.port_base {
        if port == 0 || port > PORT_BASE_MAX {
            bail!(
                "{}: port_base must be between 1 and {PORT_BASE_MAX}, got {port}",
                path.display()
            );
        }
    }
    Ok(parsed)
}

/// The JavaScript package manager the repository uses, from its lockfile.
fn js_runner(repo: &Path) -> Option<&'static str> {
    if repo.join("pnpm-lock.yaml").exists() {
        Some("pnpm")
    } else if repo.join("yarn.lock").exists() {
        Some("yarn")
    } else if repo.join("bun.lock").exists() || repo.join("bun.lockb").exists() {
        Some("bun")
    } else if repo.join("package-lock.json").exists() || repo.join("package.json").exists() {
        Some("npm")
    } else {
        None
    }
}

fn package_scripts(repo: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(repo.join("package.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    json.get("scripts")
        .and_then(|s| s.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn detect_setup(repo: &Path) -> Option<String> {
    let has = |f: &str| repo.join(f).exists();
    if has("pnpm-lock.yaml") {
        Some("pnpm install --frozen-lockfile".into())
    } else if has("yarn.lock") {
        Some("yarn install --frozen-lockfile".into())
    } else if has("bun.lock") || has("bun.lockb") {
        Some("bun install --frozen-lockfile".into())
    } else if has("package-lock.json") {
        Some("npm ci".into())
    } else if has("uv.lock") {
        Some("uv sync --frozen".into())
    } else if has("poetry.lock") {
        Some("poetry install --no-root".into())
    } else if has("Cargo.lock") {
        Some("cargo fetch".into())
    } else if has("go.sum") {
        Some("go mod download".into())
    } else {
        None
    }
}

pub fn detect_test(repo: &Path) -> Option<String> {
    let scripts = package_scripts(repo);
    if scripts.iter().any(|s| s == "test") {
        return js_runner(repo).map(|r| format!("{r} test"));
    }
    if repo.join("Cargo.toml").exists() {
        return Some("cargo test".into());
    }
    if repo.join("go.mod").exists() {
        return Some("go test ./...".into());
    }
    if repo.join("pyproject.toml").exists() || repo.join("pytest.ini").exists() {
        return Some("pytest".into());
    }
    None
}

pub fn detect_preview(repo: &Path) -> Option<String> {
    let scripts = package_scripts(repo);
    let runner = js_runner(repo)?;
    for script in ["dev", "start", "serve"] {
        if scripts.iter().any(|s| s == script) {
            return Some(match (runner, script) {
                ("npm", "start") => "npm start".to_string(),
                ("npm", s) => format!("npm run {s}"),
                (r, s) => format!("{r} {s}"),
            });
        }
    }
    None
}

/// The file's values, with detection for anything it leaves out.
pub fn load(repo: &Path) -> Result<Config> {
    let file = read_file(repo)?;
    Ok(Config {
        setup: file.setup.or_else(|| detect_setup(repo)),
        test: file.test.or_else(|| detect_test(repo)),
        preview: file.preview.or_else(|| detect_preview(repo)),
        copy: file.copy,
        setup_inputs: file.setup_inputs,
        port_base: file.port_base,
        backend: file.backend,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    #[test]
    fn empty_repo_detects_nothing() {
        let r = repo(&[]);
        assert_eq!(load(r.path()).unwrap(), Config::default());
    }

    #[test]
    fn npm_project_is_detected() {
        let r = repo(&[
            (
                "package.json",
                r#"{"scripts":{"test":"vitest run","dev":"vite"}}"#,
            ),
            ("package-lock.json", "{}"),
        ]);
        let c = load(r.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("npm ci"));
        assert_eq!(c.test.as_deref(), Some("npm test"));
        assert_eq!(c.preview.as_deref(), Some("npm run dev"));
    }

    #[test]
    fn package_manager_follows_the_lockfile() {
        let r = repo(&[
            ("package.json", r#"{"scripts":{"test":"x","start":"y"}}"#),
            ("pnpm-lock.yaml", ""),
        ]);
        let c = load(r.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("pnpm install --frozen-lockfile"));
        assert_eq!(c.test.as_deref(), Some("pnpm test"));
        assert_eq!(c.preview.as_deref(), Some("pnpm start"));
        let r = repo(&[("package.json", r#"{"scripts":{"start":"y"}}"#)]);
        assert_eq!(
            load(r.path()).unwrap().preview.as_deref(),
            Some("npm start")
        );
    }

    #[test]
    fn other_ecosystems_are_detected() {
        let rust = repo(&[("Cargo.toml", "[package]"), ("Cargo.lock", "")]);
        let c = load(rust.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("cargo fetch"));
        assert_eq!(c.test.as_deref(), Some("cargo test"));
        let go = repo(&[("go.mod", "module x"), ("go.sum", "")]);
        let c = load(go.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("go mod download"));
        assert_eq!(c.test.as_deref(), Some("go test ./..."));
        let py = repo(&[("pyproject.toml", ""), ("uv.lock", "")]);
        let c = load(py.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("uv sync --frozen"));
        assert_eq!(c.test.as_deref(), Some("pytest"));
    }

    #[test]
    fn file_values_win_over_detection() {
        let r = repo(&[
            ("package-lock.json", "{}"),
            ("package.json", r#"{"scripts":{"test":"x"}}"#),
            (
                "ocean-graft.toml",
                "setup = \"make deps\"\ntest = \"make check\"\ncopy = [\".env\"]\nport_base = 5000\nbackend = \"clone\"\n",
            ),
        ]);
        let c = load(r.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("make deps"));
        assert_eq!(c.test.as_deref(), Some("make check"));
        assert_eq!(c.copy, vec![".env".to_string()]);
        assert_eq!(c.port_base, Some(5000));
        assert_eq!(c.backend, Some(Backend::Clone));
    }

    #[test]
    fn bad_files_are_rejected_with_the_reason() {
        let r = repo(&[("ocean-graft.toml", "setp = \"typo\"\n")]);
        let err = format!("{:#}", load(r.path()).unwrap_err());
        assert!(err.contains("ocean-graft.toml is not valid"), "{err}");
        let r = repo(&[("ocean-graft.toml", "copy = [\"../secrets\"]\n")]);
        assert!(load(r.path()).is_err());
        let r = repo(&[("ocean-graft.toml", "copy = [\"/etc/passwd\"]\n")]);
        assert!(load(r.path()).is_err());
        let r = repo(&[("ocean-graft.toml", "port_base = 99999\n")]);
        assert!(load(r.path()).is_err());
        let r = repo(&[("ocean-graft.toml", "backend = \"zfs\"\n")]);
        assert!(load(r.path()).is_err());
    }

    #[test]
    fn broken_package_json_does_not_break_detection() {
        let r = repo(&[("package.json", "{not json"), ("package-lock.json", "")]);
        let c = load(r.path()).unwrap();
        assert_eq!(c.setup.as_deref(), Some("npm ci"));
        assert_eq!(c.test, None);
    }
}
