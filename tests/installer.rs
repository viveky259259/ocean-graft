//! install.sh: the standalone installer for agents other than Claude Code.
//! A fake release is served from a local directory through file:// URLs
//! (SHOAL_RELEASE_BASE), laid out as GitHub serves releases, so nothing
//! touches the network.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FAKE_BINARY: &str = "#!/bin/sh\necho \"fake shoal $*\"\n";

fn platform_asset() -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(
            r#"os=$(uname -s); arch=$(uname -m)
            if [ "$os" = Darwin ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then arch=arm64; fi
            case "$os-$arch" in
              Darwin-*) echo "shoal-darwin-$arch" ;;
              Linux-x86_64|Linux-amd64) echo shoal-linux-x86_64 ;;
              Linux-aarch64|Linux-arm64) echo shoal-linux-aarch64 ;;
            esac"#,
        )
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

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

struct Release {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Release {
    /// A server with `latest` and `v<version>` releases holding the fake
    /// binary, listed in release.txt with `sha` (default: its real SHA-256).
    fn new(version: &str, sha: Option<&str>) -> Release {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path().to_path_buf();
        let asset = platform_asset();
        let sha = sha
            .map(str::to_string)
            .unwrap_or_else(|| sha256(FAKE_BINARY.as_bytes()));
        let release_txt = format!(
            "# test release\nversion {version}\nsha256 {asset} {sha}\nsha256 shoal-other-thing 00\n"
        );
        for served in [
            root.join("server/latest/download"),
            root.join(format!("server/download/v{version}")),
        ] {
            fs::create_dir_all(&served).unwrap();
            fs::write(served.join(&asset), FAKE_BINARY).unwrap();
            fs::write(served.join("release.txt"), &release_txt).unwrap();
        }
        fs::create_dir_all(root.join("home")).unwrap();
        Release { _dir: dir, root }
    }

    fn install(&self, envs: &[(&str, &str)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        let mut cmd = Command::new("sh");
        cmd.arg(script)
            .env("HOME", self.root.join("home"))
            .env(
                "SHOAL_RELEASE_BASE",
                format!("file://{}/server", self.root.display()),
            )
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env_remove("SHOAL_VERSION")
            .env_remove("SHOAL_INSTALL_DIR");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn installed(&self) -> PathBuf {
        self.root.join("home/.local/bin/shoal")
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn installs_the_latest_release_after_checking_it() {
    if platform_asset().is_empty() {
        eprintln!("skipping: no shoal build for this platform");
        return;
    }
    let r = Release::new("9.9.9", None);
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", text(&out));
    let log = text(&out);
    assert!(log.contains("Installed shoal 9.9.9"), "{log}");
    assert!(log.contains("is not on your PATH"), "{log}");
    assert!(log.contains("shoal setup all"), "{log}");
    assert_eq!(fs::read_to_string(r.installed()).unwrap(), FAKE_BINARY);
    let mode = fs::metadata(r.installed()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    // Nothing but the binary is left behind.
    let names: Vec<_> = fs::read_dir(r.installed().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["shoal"]);
}

#[test]
fn a_binary_that_does_not_match_release_txt_is_never_installed() {
    if platform_asset().is_empty() {
        return;
    }
    let r = Release::new("9.9.9", Some(&"0".repeat(64)));
    let out = r.install(&[]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("does not match release.txt"),
        "{}",
        text(&out)
    );
    assert!(!r.installed().exists());
    assert!(!r.root.join("home/.local").exists());
}

#[test]
fn installs_a_named_version_into_a_chosen_directory() {
    if platform_asset().is_empty() {
        return;
    }
    let r = Release::new("1.2.3", None);
    let dir = r.root.join("tools");
    let path = format!("{}:/usr/bin:/bin", dir.display());
    let out = r.install(&[
        ("SHOAL_VERSION", "v1.2.3"),
        ("SHOAL_INSTALL_DIR", dir.to_str().unwrap()),
        ("PATH", &path),
    ]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(dir.join("shoal").exists());
    assert!(
        !text(&out).contains("is not on your PATH"),
        "{}",
        text(&out)
    );
}

#[test]
fn a_version_that_is_not_released_fails_cleanly() {
    if platform_asset().is_empty() {
        return;
    }
    let r = Release::new("1.2.3", None);
    let out = r.install(&[("SHOAL_VERSION", "4.5.6")]);
    assert!(!out.status.success());
    assert!(text(&out).contains("could not download"), "{}", text(&out));
    assert!(!r.installed().exists());
}

#[test]
fn an_existing_shoal_is_replaced_whole() {
    if platform_asset().is_empty() {
        return;
    }
    let r = Release::new("9.9.9", None);
    fs::create_dir_all(r.installed().parent().unwrap()).unwrap();
    fs::write(r.installed(), "old").unwrap();
    let out = r.install(&[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(fs::read_to_string(r.installed()).unwrap(), FAKE_BINARY);
}

#[test]
fn the_script_parses_and_runs_only_from_its_last_line() {
    let script =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")).unwrap();
    assert_eq!(script.trim_end().lines().last(), Some("main \"$@\""));
    let ok = Command::new("sh")
        .args(["-n", "install.sh"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(ok.success());
    // A download cut off before the last line defines functions and runs nothing.
    let cut: String = script
        .lines()
        .take_while(|l| *l != "main \"$@\"")
        .collect::<Vec<_>>()
        .join("\n");
    let out = Command::new("sh").arg("-c").arg(&cut).output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}
