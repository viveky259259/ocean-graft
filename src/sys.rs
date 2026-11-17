//! Thin wrappers over the operating system: processes, mounts, tree clones
//! and disk usage.

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn describe(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Run a command, capture stdout, and fail with its stderr on a non-zero exit.
pub fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not start `{}`", describe(cmd)))?;
    if !out.status.success() {
        bail!(
            "`{}` failed: {}",
            describe(cmd),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command with inherited stdio and return its exit code.
pub fn interactive(cmd: &mut Command) -> Result<i32> {
    let status = cmd
        .status()
        .with_context(|| format!("could not start `{}`", describe(cmd)))?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

fn cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains a NUL byte: {}", path.display()))
}

/// Flush dirty data before taking a point-in-time copy.
pub fn sync() {
    unsafe { libc::sync() };
}

/// Make `dst` a copy-on-write clone of `src` (a file or a whole directory).
///
/// macOS: one `clonefile(2)` call; the kernel clones the hierarchy without a
/// per-file round trip. Elsewhere: `cp -a --reflink=auto`, which shares
/// blocks on btrfs and XFS and falls back to copying on other filesystems.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.symlink_metadata().is_ok() {
        bail!("{} already exists", dst.display());
    }
    #[cfg(target_os = "macos")]
    {
        let s = cstring(src)?;
        let d = cstring(dst)?;
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if !matches!(err.raw_os_error(), Some(libc::ENOTSUP) | Some(libc::EXDEV)) {
            return Err(err)
                .with_context(|| format!("clonefile {} -> {}", src.display(), dst.display()));
        }
        // Not APFS, or a different volume: fall back to a plain copy.
        output(Command::new("cp").arg("-RPp").arg(src).arg(dst))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        output(
            Command::new("cp")
                .args(["-a", "--reflink=auto"])
                .arg(src)
                .arg(dst),
        )?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub fn mount_overlay(lowers: &[&Path], upper: &Path, work: &Path, target: &Path) -> Result<()> {
    for path in lowers.iter().copied().chain([upper, work, target]) {
        if path.to_string_lossy().contains([':', ',', '\\']) {
            bail!(
                "overlayfs cannot use a path containing ':', ',' or '\\': {}",
                path.display()
            );
        }
    }
    let lowerdir = lowers
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(":");
    let options = format!(
        "lowerdir={lowerdir},upperdir={},workdir={},index=off",
        upper.display(),
        work.display()
    );
    if options.len() >= 4000 {
        bail!(
            "too many layers for one overlay mount ({} bytes of mount options)",
            options.len()
        );
    }
    let fstype = CString::new("overlay")?;
    let target_c = cstring(target)?;
    let options_c = CString::new(options)?;
    let rc = unsafe {
        libc::mount(
            fstype.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            0,
            options_c.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let hint = match err.raw_os_error() {
            Some(libc::EPERM) => " (the overlay backend needs root; use --backend clone)",
            Some(libc::EINVAL) => " (SHOAL_HOME may be on a filesystem overlayfs cannot write to)",
            _ => "",
        };
        bail!("mounting {} failed: {err}{hint}", target.display());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mount_overlay(_: &[&Path], _: &Path, _: &Path, _: &Path) -> Result<()> {
    bail!("the overlay backend is only available on Linux")
}

#[cfg(target_os = "linux")]
pub fn unmount(target: &Path) -> Result<()> {
    let target_c = cstring(target)?;
    if unsafe { libc::umount2(target_c.as_ptr(), 0) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EBUSY) {
            bail!(
                "{} is in use: stop the processes running in it first",
                target.display()
            );
        }
        bail!("unmounting {} failed: {err}", target.display());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn unmount(_: &Path) -> Result<()> {
    Ok(())
}

/// Undo the octal escapes /proc/self/mountinfo uses for spaces and the like.
#[cfg(target_os = "linux")]
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            let code = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 8).ok());
            if let Some(code) = code {
                out.push(code);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "linux")]
pub fn is_mounted(target: &Path) -> bool {
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    let target = target.to_string_lossy();
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    info.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .any(|mount_point| unescape_mountinfo(mount_point) == target)
}

#[cfg(not(target_os = "linux"))]
pub fn is_mounted(_: &Path) -> bool {
    false
}

/// Bytes allocated on disk under `path`. Symlinks are not followed and mount
/// points are not crossed, so mounted sessions are not counted twice.
pub fn disk_usage(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fn walk(path: &Path, dev: u64, total: &mut u64) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.dev() != dev {
            return;
        }
        *total += meta.blocks() * 512;
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), dev, total);
                }
            }
        }
    }
    let Ok(root) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = 0;
    walk(path, root.dev(), &mut total);
    total
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

pub fn human_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

pub fn human_age(created_at: u64) -> String {
    let secs = now().saturating_sub(created_at);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}
