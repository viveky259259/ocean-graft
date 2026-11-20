//! Thin wrappers over the operating system: processes, mounts, tree clones
//! and disk usage.

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Quote a word for a POSIX shell.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// How to invoke this binary from a shell: Claude's Bash tool has no
/// `ocean-graft` on PATH when the plugin's launcher found or downloaded it.
pub fn ocean_graft_command() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(shell_quote))
        .unwrap_or_else(|| "ocean-graft".into())
}

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

static CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_signal(signal: libc::c_int) {
    let pid = CHILD.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, signal) };
    }
}

/// Like `interactive`, but ocean-graft stays in charge of the child until it exits:
/// SIGTERM and SIGHUP sent to ocean-graft are passed on to the child, and Ctrl-C
/// reaches the child directly from the terminal. This keeps resources that
/// ocean-graft holds for the job (such as a queue slot) tied to the job's lifetime.
pub fn supervised(cmd: &mut Command) -> Result<i32> {
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not start `{}`", describe(cmd)))?;
    CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    let handler = forward_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let previous = unsafe {
        [
            (libc::SIGTERM, libc::signal(libc::SIGTERM, handler)),
            (libc::SIGHUP, libc::signal(libc::SIGHUP, handler)),
            (libc::SIGINT, libc::signal(libc::SIGINT, libc::SIG_IGN)),
            (libc::SIGQUIT, libc::signal(libc::SIGQUIT, libc::SIG_IGN)),
        ]
    };
    let status = child.wait();
    for (signal, old) in previous {
        unsafe { libc::signal(signal, old) };
    }
    CHILD.store(0, std::sync::atomic::Ordering::SeqCst);
    let status = status?;
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
        output(Command::new("cp").args(["-RPp", "--"]).arg(src).arg(dst))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        output(
            Command::new("cp")
                .args(["-a", "--reflink=auto", "--"])
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
            Some(libc::EINVAL) => {
                " (OCEAN_GRAFT_HOME may be on a filesystem overlayfs cannot write to)"
            }
            _ => "",
        };
        bail!("mounting {} failed: {err}{hint}", target.display());
    }
    Ok(())
}

/// A read-only overlay of `lowers` (top first) at `target`: how the session
/// would look with that stack of layers, without changing any of them.
#[cfg(target_os = "linux")]
pub fn mount_overlay_readonly(lowers: &[&Path], target: &Path) -> Result<()> {
    for path in lowers.iter().copied().chain([target]) {
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
    let fstype = CString::new("overlay")?;
    let target_c = cstring(target)?;
    let options_c = CString::new(format!("lowerdir={lowerdir}"))?;
    let rc = unsafe {
        libc::mount(
            fstype.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            libc::MS_RDONLY,
            options_c.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        bail!(
            "mounting {} failed: {}",
            target.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mount_overlay_readonly(_: &[&Path], _: &Path) -> Result<()> {
    bail!("the overlay backend is only available on Linux")
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
/// What sizes from `disk_usage` mean: on macOS clones share blocks the
/// size does not see, so it is an upper bound, not space owned.
pub const SIZE_NOTE: &str = if cfg!(target_os = "macos") {
    " (logical size: APFS clones share most of it)"
} else {
    ""
};

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

/// When process `pid` started, in a form only meant for comparison, or None
/// if there is no such process.
pub fn process_start(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        // Field 22 (starttime) comes after the command name, which is in
        // parentheses and may itself contain spaces or parentheses.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(19).map(str::to_string)
    }
    #[cfg(target_os = "macos")]
    {
        // From the kernel, in microseconds since the epoch: unlike `ps -o
        // lstart`, independent of the time zone and locale, so a trip or a
        // different LANG never makes ocean-graft disown its own services.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then(|| format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let out = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .env("TZ", "UTC0")
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

/// Whether something already listens on `port` on this machine: a test
/// bind on 127.0.0.1 and 0.0.0.0, and a connection attempt on 127.0.0.1
/// and ::1. The connection catches listeners a bind misses (on macOS a
/// bind to 127.0.0.1 succeeds while another process listens on the
/// wildcard address, and servers often listen on ::1 only).
pub fn port_in_use(port: u16) -> bool {
    use std::net::{SocketAddr, TcpListener, TcpStream};
    let bound = TcpListener::bind(("127.0.0.1", port)).is_err()
        || TcpListener::bind(("0.0.0.0", port)).is_err();
    if bound {
        return true;
    }
    let answers =
        |addr: SocketAddr| TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok();
    answers(SocketAddr::from(([127, 0, 0, 1], port)))
        || answers(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)))
}

/// Whether file names in `dir` are matched without regard to case (the
/// default on macOS APFS and HFS+).
pub fn case_insensitive(dir: &Path) -> bool {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = dir.join(format!(
        ".ocean-graft-case-probe-{}-{n}",
        std::process::id()
    ));
    if std::fs::write(&probe, b"").is_err() {
        return cfg!(target_os = "macos");
    }
    let upper = dir.join(format!(
        ".OCEAN-GRAFT-CASE-PROBE-{}-{n}",
        std::process::id()
    ));
    let insensitive = upper.symlink_metadata().is_ok();
    let _ = std::fs::remove_file(&probe);
    insensitive
}

/// Whether the filesystem holding `dir` can clone files (APFS clonefile).
#[cfg(target_os = "macos")]
pub fn clones_supported(dir: &Path) -> bool {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let a = dir.join(format!(
        ".ocean-graft-clone-probe-a-{}-{n}",
        std::process::id()
    ));
    let b = dir.join(format!(
        ".ocean-graft-clone-probe-b-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&b);
    if std::fs::write(&a, b"probe").is_err() {
        return false;
    }
    let ok = match (cstring(&a), cstring(&b)) {
        (Ok(sa), Ok(sb)) => {
            let rc = unsafe { libc::clonefile(sa.as_ptr(), sb.as_ptr(), 0) };
            rc == 0
        }
        _ => false,
    };
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    ok
}

/// The working directory of every process this user can see: on Linux
/// from /proc, on macOS from libproc, elsewhere from one `lsof` run. None when that cannot be
/// determined; callers must then treat every directory as in use.
pub fn process_cwds() -> Option<Vec<std::path::PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        let procs = std::fs::read_dir("/proc").ok()?;
        Some(
            procs
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                })
                .filter_map(|e| std::fs::read_link(e.path().join("cwd")).ok())
                .collect(),
        )
    }
    #[cfg(target_os = "macos")]
    {
        // libproc, as Activity Monitor and lsof use it: every pid, then each
        // one's current directory. Processes of other users are skipped
        // (EPERM), as on Linux.
        let mut pids: Vec<libc::c_int> = vec![0; 4096];
        let n = loop {
            let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
            let n =
                unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes) };
            if n < 0 {
                return None;
            }
            if (n as usize) < pids.len() {
                break n as usize;
            }
            pids.resize(pids.len() * 2, 0); // more processes than room
        };
        let mut cwds = Vec::new();
        for &pid in &pids[..n] {
            if pid <= 0 {
                continue;
            }
            let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDVNODEPATHINFO,
                    0,
                    &mut info as *mut _ as *mut libc::c_void,
                    size,
                )
            };
            if got != size {
                continue;
            }
            let raw: Vec<u8> = info
                .pvi_cdir
                .vip_path
                .iter()
                .flatten()
                .map(|c| *c as u8)
                .take_while(|b| *b != 0)
                .collect();
            if !raw.is_empty() {
                use std::os::unix::ffi::OsStringExt;
                cwds.push(std::path::PathBuf::from(std::ffi::OsString::from_vec(raw)));
            }
        }
        Some(cwds)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let out = Command::new("lsof")
            .args(["-w", "-a", "-d", "cwd", "-Fn"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() && out.stdout.is_empty() {
            return None;
        }
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix('n'))
                .map(std::path::PathBuf::from)
                .collect(),
        )
    }
}

/// Whether any process has its working directory at or under `dir`. When
/// that cannot be determined, the answer is yes (fail safe).
pub fn is_cwd_of_a_process(dir: &Path) -> bool {
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    match process_cwds() {
        Some(cwds) => cwds.iter().any(|cwd| cwd.starts_with(&dir)),
        None => true,
    }
}

/// Process groups ocean-graft started for setup and tests. If ocean-graft is killed
/// (Ctrl-C, a hook timeout, the MCP server shutting down), they are killed
/// with it instead of running on, orphaned and unbounded.
static GROUPS: [std::sync::atomic::AtomicI32; 64] =
    [const { std::sync::atomic::AtomicI32::new(0) }; 64];
static GROUP_HANDLER: std::sync::Once = std::sync::Once::new();

extern "C" fn kill_groups_and_die(signal: libc::c_int) {
    for slot in &GROUPS {
        let pgid = slot.load(std::sync::atomic::Ordering::SeqCst);
        if pgid > 0 {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Kill `pgid` if ocean-graft dies, until the returned guard is dropped.
pub fn own_group(pgid: i32) -> OwnedGroup {
    GROUP_HANDLER.call_once(|| {
        let handler = kill_groups_and_die as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let old = unsafe { libc::signal(signal, handler) };
            if old == libc::SIG_IGN {
                unsafe { libc::signal(signal, libc::SIG_IGN) }; // keep ignoring
            }
        }
    });
    let slot = GROUPS.iter().position(|s| {
        s.compare_exchange(
            0,
            pgid,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    });
    OwnedGroup { slot }
}

pub struct OwnedGroup {
    slot: Option<usize>,
}

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        if let Some(i) = self.slot {
            GROUPS[i].store(0, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// Delete a tree even where it holds read-only directories (which a
/// normal user cannot empty without first making them writable).
pub fn remove_tree(path: impl AsRef<Path>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = path.as_ref();
    let meta = match path.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return std::fs::remove_file(path);
    }
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    // Make every directory writable and searchable, then try again.
    fn open_up(dir: &Path) {
        if let Ok(m) = dir.symlink_metadata() {
            if m.is_dir() {
                let _ = std::fs::set_permissions(
                    dir,
                    std::fs::Permissions::from_mode(m.permissions().mode() | 0o700),
                );
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for e in entries.flatten() {
                        open_up(&e.path());
                    }
                }
            }
        }
    }
    open_up(path);
    std::fs::remove_dir_all(path)
}

/// Whether two regular files hold the same bytes.
fn same_contents(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let na = fa.read(&mut ba)?;
        let mut filled = 0;
        while filled < na {
            let n = fb.read(&mut bb[filled..na])?;
            if n == 0 {
                return Ok(false);
            }
            filled += n;
        }
        if na == 0 {
            return Ok(fb.read(&mut bb[..1])? == 0);
        }
        if ba[..na] != bb[..na] {
            return Ok(false);
        }
    }
}

/// Make the tree at `dst` the same as the one at `src`, in place: entries
/// only in `dst` are removed, files whose contents or metadata differ are
/// copied, symlinks recreated, and modes set where they differ. Used to
/// restore a session that a process is working in, where swapping the
/// directory would leave that process in a deleted one.
///
/// `identical(rel)` may report that the entry at relative path `rel` is
/// known to be the same on both sides (for overlay sessions: neither layer
/// on top of the shared lowers touches it); such entries, and whole
/// directories, are skipped without reading them.
///
/// `unchanged_before`: a file in `dst` whose size, mtime and mode match and
/// whose ctime is earlier than this has not been written since `src` was
/// copied from it (any write or chmod moves ctime on), so its bytes are not
/// compared. Pass the time the copy in `src` was started, or None.
pub fn sync_tree(
    src: &Path,
    dst: &Path,
    identical: &dyn Fn(&Path) -> bool,
    unchanged_before: Option<std::time::SystemTime>,
) -> Result<()> {
    sync_dir(src, dst, Path::new(""), identical, unchanged_before)
}

fn sync_dir(
    src: &Path,
    dst: &Path,
    rel: &Path,
    identical: &dyn Fn(&Path) -> bool,
    unchanged_before: Option<std::time::SystemTime>,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let set_mode = |p: &Path, mode: u32| -> std::io::Result<()> {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    };
    // A read-only or unreadable directory cannot be synced: open it up for the
    // sync (read + write + search) and give it its final mode at the end.
    let dst_mode = dst.symlink_metadata()?.mode() & 0o7777;
    if dst_mode & 0o700 != 0o700 {
        set_mode(dst, dst_mode | 0o700)?;
    }
    let remove = |p: &Path| -> Result<()> {
        remove_tree(p).with_context(|| format!("could not remove {}", p.display()))
    };
    // Compare exact directory entry names (not path probes) so case-only
    // renames (Foo.txt <-> foo.txt) are restored properly on case-insensitive
    // filesystems such as macOS APFS.
    let src_entries: Vec<_> = std::fs::read_dir(src)
        .with_context(|| format!("could not read {}", src.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    let src_names: std::collections::HashSet<std::ffi::OsString> =
        src_entries.iter().map(|e| e.file_name()).collect();
    let mut dst_names: std::collections::HashSet<std::ffi::OsString> =
        std::collections::HashSet::new();
    for entry in
        std::fs::read_dir(dst).with_context(|| format!("could not read {}", dst.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if !src_names.contains(&name) {
            remove(&entry.path())?;
        } else {
            dst_names.insert(name);
        }
    }
    for entry in src_entries {
        let name = entry.file_name();
        let from = entry.path();
        let to = dst.join(&name);
        let child_rel = rel.join(&name);
        let meta = from.symlink_metadata()?;
        let existing = dst_names
            .contains(&name)
            .then(|| to.symlink_metadata().ok())
            .flatten();
        let ft = meta.file_type();
        if existing.is_some() && identical(&child_rel) {
            continue;
        }
        if ft.is_dir() {
            if !existing.as_ref().is_some_and(|m| m.is_dir()) {
                remove(&to)?;
                std::fs::create_dir(&to)?;
            }
            sync_dir(&from, &to, &child_rel, identical, unchanged_before)?;
            let want = meta.mode() & 0o7777;
            if to.symlink_metadata()?.mode() & 0o7777 != want {
                set_mode(&to, want)?;
            }
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            let same = existing
                .as_ref()
                .is_some_and(|m| m.file_type().is_symlink())
                && std::fs::read_link(&to).ok().as_deref() == Some(target.as_path());
            if !same {
                remove(&to)?;
                std::os::unix::fs::symlink(&target, &to)?;
            }
        } else if ft.is_file() {
            // Same size, time and mode is not proof: package managers stamp
            // every file with one fixed time. Compare the bytes too.
            let meta_same = existing.as_ref().is_some_and(|m| {
                m.is_file()
                    && m.len() == meta.len()
                    && m.mtime() == meta.mtime()
                    && m.mtime_nsec() == meta.mtime_nsec()
                    && m.mode() == meta.mode()
            });
            let untouched = || {
                let (Some(limit), Some(m)) = (unchanged_before, existing.as_ref()) else {
                    return false;
                };
                let ctime = std::time::UNIX_EPOCH
                    + std::time::Duration::new(
                        m.ctime().max(0) as u64,
                        m.ctime_nsec().max(0) as u32,
                    );
                ctime < limit
            };
            let same = meta_same && (untouched() || same_contents(&from, &to).unwrap_or(false));
            if !same {
                remove(&to)?;
                std::fs::copy(&from, &to)
                    .with_context(|| format!("could not copy {}", from.display()))?;
                set_mode(&to, meta.mode() & 0o7777)?;
                set_mtime(&to, meta.mtime(), meta.mtime_nsec())?;
            }
        }
        // Sockets, FIFOs and devices are not part of a working tree.
    }
    if dst_mode & 0o700 != 0o700 {
        set_mode(dst, dst_mode)?;
    }
    Ok(())
}

/// Set a file's modification time without opening it (works on read-only
/// files too).
fn set_mtime(path: &Path, secs: i64, nsecs: i64) -> Result<()> {
    let c = cstring(path)?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: secs as _,
            tv_nsec: nsecs as _,
        },
    ];
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("could not set the time of {}", path.display()));
    }
    Ok(())
}
