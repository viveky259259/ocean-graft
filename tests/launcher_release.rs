//! The plugin launcher's release download: on first use it fetches the
//! binary for this platform, verifies it against the SHA-256 in
//! release.txt (which ships with the plugin), caches it, and keeps Claude
//! Code's exit-code and stdout rules on every failure.
//!
//! A fake release is served from a local directory through file:// URLs
//! (OCEAN_GRAFT_RELEASE_BASE), so nothing touches the network.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const VERSION: &str = "9.9.9";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn platform_asset() -> String {
    let os = String::from_utf8(Command::new("uname").arg("-s").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_lowercase();
    let arch = String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    format!("ocean-graft-{os}-{arch}")
}

/// A stand-in for the real binary: prints its arguments on stdout (as the
/// MCP server or the worktree path would) and exits 0.
const FAKE_BINARY: &str = "#!/bin/sh\necho \"fake ocean-graft $*\"\nexit 0\n";

fn sha256(bytes: &[u8]) -> String {
    let mut child = Command::new("sh")
        .args([
            "-c",
            "if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

impl Fixture {
    /// A plugin copy (launcher + release.txt) away from any development
    /// build, a release server directory holding the fake binary, and a
    /// private plugin data directory.
    fn new(release_sha: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path().to_path_buf();
        let libexec = root.join("plugin/libexec");
        fs::create_dir_all(&libexec).unwrap();
        let launcher = libexec.join("ocean-graft-plugin");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/libexec/ocean-graft-plugin"),
            &launcher,
        )
        .unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        let asset = platform_asset();
        let served = root.join(format!("server/v{VERSION}"));
        fs::create_dir_all(&served).unwrap();
        fs::write(served.join(&asset), FAKE_BINARY).unwrap();
        let sha = release_sha
            .map(str::to_string)
            .unwrap_or_else(|| sha256(FAKE_BINARY.as_bytes()));
        fs::write(
            libexec.join("release.txt"),
            format!("# test release\nversion {VERSION}\nsha256 {asset} {sha}\nsha256 ocean-graft-other-thing 00\n"),
        )
        .unwrap();
        Fixture { _dir: dir, root }
    }

    fn launcher(&self) -> PathBuf {
        self.root.join("plugin/libexec/ocean-graft-plugin")
    }

    fn cached(&self) -> PathBuf {
        self.root
            .join(format!("data/bin/{VERSION}/{}", platform_asset()))
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(self.launcher())
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("CLAUDE_PLUGIN_DATA", self.root.join("data"))
            .env(
                "OCEAN_GRAFT_RELEASE_BASE",
                format!("file://{}", self.root.join("server").display()),
            )
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn has_curl_or_wget() -> bool {
    ["curl", "wget"].iter().any(|t| {
        Command::new("sh")
            .args(["-c", &format!("command -v {t}")])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

#[test]
fn first_use_downloads_verifies_and_caches_the_release_binary() {
    if !has_curl_or_wget() {
        eprintln!("skipping: needs curl or wget");
        return;
    }
    let f = Fixture::new(None);
    let out = f.run(&["api", "status", "{}"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // stdout carries only the binary's own output.
    assert_eq!(stdout(&out), "fake ocean-graft api status {}\n");
    assert!(
        stderr(&out).contains("downloading ocean-graft 9.9.9"),
        "{}",
        stderr(&out)
    );
    let cached = f.cached();
    assert_eq!(fs::read_to_string(&cached).unwrap(), FAKE_BINARY);
    assert_eq!(
        fs::metadata(&cached).unwrap().permissions().mode() & 0o777,
        0o755
    );

    // Second use: from the cache, no download (the server copy is gone).
    fs::remove_file(
        f.root
            .join(format!("server/v{VERSION}/{}", platform_asset())),
    )
    .unwrap();
    let out = f.run(&["mcp"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "fake ocean-graft mcp\n");
    assert!(!stderr(&out).contains("downloading"), "{}", stderr(&out));
}

#[test]
fn a_binary_that_does_not_match_release_txt_is_never_used() {
    if !has_curl_or_wget() {
        return;
    }
    let f = Fixture::new(Some(&"0".repeat(64)));
    for (args, code) in [
        (&["mcp"][..], 1),
        (&["hook", "worktree-create"][..], 1),
        (&["hook", "worktree-remove"][..], 1),
        (&["hook", "stop"][..], 0),
        (&["hook", "session-start"][..], 0),
        (&["hook", "prompt"][..], 0),
    ] {
        let out = f.run(args);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {}", stderr(&out));
        assert!(
            stdout(&out).is_empty(),
            "{args:?} printed {:?}",
            stdout(&out)
        );
        assert!(
            stderr(&out).contains("does not match release.txt"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    assert!(!f.cached().exists(), "a mismatching binary was cached");
    let leftovers: Vec<_> = fs::read_dir(f.cached().parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn a_failed_download_keeps_the_hook_exit_code_rules() {
    if !has_curl_or_wget() {
        return;
    }
    let f = Fixture::new(None);
    fs::remove_dir_all(f.root.join("server")).unwrap();
    for (args, code) in [
        (&["api", "status", "{}"][..], 1),
        (&["hook", "worktree-create"][..], 1),
        (&["hook", "stop"][..], 0),
        (&["hook", "session-start"][..], 0),
    ] {
        let out = f.run(args);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {}", stderr(&out));
        assert!(
            stdout(&out).is_empty(),
            "{args:?} printed {:?}",
            stdout(&out)
        );
        assert!(
            stderr(&out).contains("download failed"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    assert!(!f.cached().exists());
}

#[test]
fn without_a_release_entry_for_this_platform_nothing_is_downloaded() {
    let f = Fixture::new(None);
    fs::write(
        f.root.join("plugin/libexec/release.txt"),
        format!("version {VERSION}\nsha256 ocean-graft-plan9-mips 00\n"),
    )
    .unwrap();
    let out = f.run(&["hook", "stop"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(!stderr(&out).contains("downloading"), "{}", stderr(&out));
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));
    let out = f.run(&["mcp"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn many_first_runs_at_once_all_succeed_with_one_verified_binary() {
    if !has_curl_or_wget() {
        return;
    }
    let f = Fixture::new(None);
    let results: Vec<Output> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let f = &f;
                s.spawn(move || f.run(&["api", "status", &format!("{{\"n\":{i}}}")]))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, out) in results.iter().enumerate() {
        assert!(out.status.success(), "run {i}: {}", stderr(out));
        assert_eq!(
            stdout(out),
            format!("fake ocean-graft api status {{\"n\":{i}}}\n")
        );
    }
    assert_eq!(fs::read_to_string(f.cached()).unwrap(), FAKE_BINARY);
    let entries: Vec<_> = fs::read_dir(f.cached().parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec![platform_asset()], "stray files: {entries:?}");
}

#[test]
fn ocean_graft_bin_and_a_bundled_binary_take_precedence_over_the_download() {
    let f = Fixture::new(None);
    let own = f.root.join("own-ocean-graft");
    fs::write(&own, "#!/bin/sh\necho own $*\n").unwrap();
    fs::set_permissions(&own, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(f.launcher())
        .arg("mcp")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("OCEAN_GRAFT_BIN", &own)
        .env("CLAUDE_PLUGIN_DATA", f.root.join("data"))
        .env("OCEAN_GRAFT_RELEASE_BASE", "file:///nonexistent")
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "own mcp\n");
    assert!(!stderr(&out).contains("downloading"));
    let bundled = f.root.join("plugin/libexec").join(platform_asset());
    fs::write(&bundled, "#!/bin/sh\necho bundled $*\n").unwrap();
    fs::set_permissions(&bundled, fs::Permissions::from_mode(0o755)).unwrap();
    let out = f.run(&["mcp"]);
    assert_eq!(stdout(&out), "bundled mcp\n");
    assert!(!stderr(&out).contains("downloading"));
}
