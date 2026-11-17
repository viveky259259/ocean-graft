//! Edge-case tests for project configuration and initialisation: `shoal.toml`
//! parsing and validation, detection without a file, auto-initialisation on
//! the first fork from main, `refresh` (new template generations) and the
//! rule that sessions never modify the template.
//!
//! Port range 5700-5799. No test here starts anything that listens on a
//! port (ports are only recorded in state.json), so each test takes its own
//! port_base inside the range. The one exception is the port-overflow test,
//! which records ports near 65535 on purpose and still listens on nothing.
//!
//! Every shoal call runs with GIT_CEILING_DIRECTORIES at the test's
//! temporary root: target/tmp lives inside the ocean git repository, and a
//! "non-git directory" would otherwise resolve to that enclosing repository.

mod common;
use common::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

const SETUP: &str = "mkdir -p deps && cat deps.lock > deps/installed.txt";

fn write(path: impl AsRef<Path>, body: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn commit_all(env: &Env, msg: &str) {
    env.git(&env.repo(), &["add", "-A"]);
    env.git(&env.repo(), &["commit", "-q", "-m", msg]);
}

fn set_toml(env: &Env, body: &str) {
    write(env.repo().join("shoal.toml"), body);
}

/// The standard project file: the harness's "install", .env copied, the
/// test's port base and the given backend.
fn standard_toml(env: &Env, backend: &str) -> String {
    format!(
        "setup = \"{SETUP}\"\nsetup_inputs = [\"deps.lock\"]\ncopy = [\".env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
        env.port_base
    )
}

fn shoal_cmd(env: &Env, dir: &Path, args: &[&str], extra: &[(&str, String)]) -> Command {
    let mut cmd = env.command(BIN, dir);
    cmd.args(args)
        .env("GIT_CEILING_DIRECTORIES", env.root.path())
        .stdin(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd
}

fn parse_api(tool: &str, ok: bool, text: &str, err_text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).unwrap_or_else(|e| {
        panic!("api {tool} printed non-JSON ({e}):\n{text}\nstderr:\n{err_text}")
    });
    if ok {
        Ok(value)
    } else {
        Err(value["error"].as_str().unwrap_or_default().to_string())
    }
}

/// `shoal api <tool>` in `dir` with extra environment variables.
fn call_env(
    env: &Env,
    dir: &Path,
    tool: &str,
    args: Value,
    extra: &[(&str, String)],
) -> Result<Value, String> {
    let out = shoal_cmd(env, dir, &["api", tool, &args.to_string()], extra)
        .output()
        .unwrap();
    parse_api(tool, out.status.success(), &stdout(&out), &stderr(&out))
}

fn call(env: &Env, dir: &Path, tool: &str, args: Value) -> Result<Value, String> {
    call_env(env, dir, tool, args, &[])
}

fn fork(env: &Env) -> Result<Value, String> {
    call(env, &env.repo(), "fork", json!({}))
}

fn fork_ok(env: &Env) -> Value {
    fork(env).unwrap_or_else(|e| panic!("fork failed: {e}"))
}

fn status(env: &Env) -> Value {
    call(env, &env.repo(), "status", json!({})).unwrap_or_else(|e| panic!("status failed: {e}"))
}

fn spath(r: &Value) -> PathBuf {
    PathBuf::from(
        r["sessions"][0]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("no session path in {r}")),
    )
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// Run shoal with a deadline. None when it had to be killed. Output goes to
/// files so a chatty child cannot block on a full pipe.
fn run_with_deadline(
    env: &Env,
    dir: &Path,
    args: &[&str],
    secs: u64,
) -> Option<(bool, String, String)> {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let out_path = env.root.path().join(format!("deadline-{n}.out"));
    let err_path = env.root.path().join(format!("deadline-{n}.err"));
    let mut cmd = shoal_cmd(env, dir, args, &[]);
    cmd.stdout(Stdio::from(fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&err_path).unwrap()));
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some((
                status.success(),
                fs::read_to_string(&out_path).unwrap_or_default(),
                fs::read_to_string(&err_path).unwrap_or_default(),
            ));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Directories under SHOAL_HOME/projects (lock files are not projects).
fn project_dirs(env: &Env) -> Vec<PathBuf> {
    match fs::read_dir(env.home().join("projects")) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn assert_no_project(env: &Env, why: &str) {
    let dirs = project_dirs(env);
    assert!(
        dirs.is_empty(),
        "{why}: a project was left behind: {dirs:?}"
    );
    let st = status(env);
    assert_eq!(st["initialized"], false, "{why}: {st}");
}

fn mounts_under(path: &Path) -> Vec<String> {
    let needle = path.to_string_lossy().into_owned();
    fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(&needle))
        .map(str::to_string)
        .collect()
}

fn template_g1(env: &Env, backend: &str) -> PathBuf {
    env.project_dir().join(match backend {
        "overlay" => "layers/g1",
        _ => "templates/g1",
    })
}

fn generations(env: &Env) -> usize {
    env.state()["generations"].as_array().unwrap().len()
}

/// Content snapshot of a tree (files hashed with their mode, symlinks by
/// target), never following symlinks.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let meta = p.symlink_metadata().unwrap();
            let ft = meta.file_type();
            if ft.is_symlink() {
                out.insert(
                    rel,
                    format!("link {}", fs::read_link(&p).unwrap().display()),
                );
            } else if ft.is_dir() {
                out.insert(rel, format!("dir {:o}", meta.permissions().mode()));
                walk(root, &p, out);
            } else {
                let bytes = fs::read(&p).unwrap();
                let mut h = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut h);
                out.insert(
                    rel,
                    format!(
                        "file {:o} {} {:x}",
                        meta.permissions().mode(),
                        bytes.len(),
                        h.finish()
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// A directory holding fake executables, to prepend to PATH.
fn fake_bins(env: &Env, bins: &[(&str, &str)]) -> Vec<(&'static str, String)> {
    let dir = env.root.path().join("fakebin");
    fs::create_dir_all(&dir).unwrap();
    for (name, body) in bins {
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    vec![("PATH", path)]
}

fn lines(path: impl AsRef<Path>) -> usize {
    read(path).lines().count()
}

// ---------------------------------------------------------------------------
// shoal.toml is used on the first fork
// ---------------------------------------------------------------------------

#[test]
fn toml_setup_copy_port_and_backend_are_used_on_first_fork() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5700 + i as u16);
        set_toml(&env, &standard_toml(&env, backend));
        commit_all(&env, "add shoal.toml");
        assert!(project_dirs(&env).is_empty());

        let r = fork_ok(&env);
        let s = &r["sessions"][0];
        assert_eq!(s["port"], env.port_base, "[{backend}] port_base: {r}");
        assert_eq!(s["generation"], 1, "[{backend}] {r}");
        assert!(
            r["refreshed"].is_null(),
            "[{backend}] fresh project needs no refresh: {r}"
        );
        let path = spath(&r);
        assert_eq!(
            read(path.join("deps/installed.txt")),
            "lib 1.0\n",
            "[{backend}] setup ran"
        );
        assert_eq!(
            read(path.join(".env")),
            "SECRET=1\n",
            "[{backend}] .env copied"
        );
        assert_eq!(
            read(path.join("src/app.txt")),
            "line one\nline two\nline three\n"
        );

        let st = status(&env);
        assert_eq!(st["initialized"], true);
        assert_eq!(
            st["backend"], backend,
            "[{backend}] backend respected: {st}"
        );
        assert_eq!(st["branch"], "main");
        assert_eq!(st["commands"]["setup"], SETUP);
        assert_eq!(st["template"]["generation"], 1);
        assert_eq!(st["template"]["behind_source"], false);

        let state = env.state();
        assert_eq!(state["port_base"], env.port_base);
        assert_eq!(state["setup_inputs"], json!(["deps.lock"]));
        assert_eq!(state["backend"], backend);

        let r2 = fork_ok(&env);
        assert_eq!(
            r2["sessions"][0]["port"],
            env.port_base + 10,
            "[{backend}] second block of ten"
        );
        assert_ne!(r2["sessions"][0]["name"], s["name"]);
        assert_eq!(project_dirs(&env).len(), 1);
    }
}

#[test]
fn uncommitted_shoal_toml_in_the_working_tree_is_honoured() {
    let env = Env::new(5703);
    set_toml(&env, &standard_toml(&env, "clone"));
    let r = fork_ok(&env);
    let path = spath(&r);
    assert_eq!(r["sessions"][0]["port"], env.port_base);
    assert_eq!(read(path.join("deps/installed.txt")), "lib 1.0\n");
    assert_eq!(read(path.join(".env")), "SECRET=1\n");
    // The template is built from the commit, so the uncommitted file itself
    // is not in the session.
    assert!(!path.join("shoal.toml").exists());
}

#[test]
fn shoal_toml_from_the_repository_root_is_used_when_forking_from_a_subdirectory() {
    let env = Env::new(5705);
    set_toml(&env, &standard_toml(&env, "clone"));
    commit_all(&env, "toml");
    let r = call(&env, &env.repo().join("src"), "fork", json!({})).unwrap();
    assert_eq!(r["sessions"][0]["port"], env.port_base);
    assert!(spath(&r).join("deps/installed.txt").exists());

    // From an unrelated directory with `project`: the same project.
    let elsewhere = env.root.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let r2 = call(
        &env,
        &elsewhere,
        "fork",
        json!({"project": env.repo().to_string_lossy()}),
    )
    .unwrap();
    assert_eq!(r2["sessions"][0]["port"], env.port_base + 10);
    assert_eq!(project_dirs(&env).len(), 1);
}

#[test]
fn setup_runs_in_the_template_with_shoal_template_set() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5707 + i as u16);
        set_toml(
            &env,
            &format!(
                "setup = 'mkdir -p deps && echo \"$SHOAL_TEMPLATE\" > deps/flag && pwd > deps/where'\nport_base = {}\nbackend = \"{backend}\"\n",
                env.port_base
            ),
        );
        let r = fork_ok(&env);
        let path = spath(&r);
        assert_eq!(read(path.join("deps/flag")), "1\n", "[{backend}]");
        let where_ = read(path.join("deps/where"));
        let want = if backend == "overlay" {
            "/layers/g1\n"
        } else {
            "/templates/g1\n"
        };
        assert!(
            where_.ends_with(want),
            "[{backend}] setup ran in {where_:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Detection without shoal.toml
// ---------------------------------------------------------------------------

#[test]
fn package_lock_without_shoal_toml_detects_npm_ci() {
    let env = Env::new(5710);
    write(env.repo().join("package-lock.json"), "{}\n");
    commit_all(&env, "lockfile");
    let path_env = fake_bins(
        &env,
        &[(
            "npm",
            "mkdir -p deps && echo \"npm $* T=$SHOAL_TEMPLATE\" > deps/npm-ran.txt",
        )],
    );
    let r = call_env(&env, &env.repo(), "fork", json!({}), &path_env).unwrap();
    let path = spath(&r);
    assert_eq!(read(path.join("deps/npm-ran.txt")), "npm ci T=1\n");
    let st = status(&env);
    assert_eq!(st["commands"]["setup"], "npm ci", "{st}");
    assert!(st["commands"]["test"].is_null(), "no test script: {st}");
    let auto = if is_root() && cfg!(target_os = "linux") {
        "overlay"
    } else {
        "clone"
    };
    assert_eq!(st["backend"], auto, "automatic backend: {st}");
    // Without shoal.toml, the default lockfile inputs apply.
    let inputs = env.state()["setup_inputs"].clone();
    assert!(
        inputs
            .as_array()
            .unwrap()
            .contains(&json!("package-lock.json")),
        "{inputs}"
    );
}

#[test]
fn pnpm_lockfile_wins_over_package_lock_and_package_json_scripts_are_detected() {
    let env = Env::new(5712);
    write(env.repo().join("package-lock.json"), "{}\n");
    write(env.repo().join("pnpm-lock.yaml"), "lockfileVersion: 9\n");
    write(
        env.repo().join("package.json"),
        r#"{"scripts":{"test":"vitest","dev":"vite"}}"#,
    );
    commit_all(&env, "js project");
    let path_env = fake_bins(
        &env,
        &[
            ("pnpm", "mkdir -p deps && echo \"pnpm $*\" > deps/ran.txt"),
            ("npm", "echo npm must not run >&2; exit 1"),
        ],
    );
    let r = call_env(&env, &env.repo(), "fork", json!({}), &path_env).unwrap();
    assert_eq!(
        read(spath(&r).join("deps/ran.txt")),
        "pnpm install --frozen-lockfile\n"
    );
    let st = status(&env);
    assert_eq!(st["commands"]["setup"], "pnpm install --frozen-lockfile");
    assert_eq!(st["commands"]["test"], "pnpm test");
    assert_eq!(st["commands"]["preview"], "pnpm dev");
}

#[test]
fn repo_without_lockfile_has_no_setup_and_sessions_still_work() {
    let env = Env::new(5714);
    let r = fork_ok(&env);
    let path = spath(&r);
    assert_eq!(read(path.join("README.md")), "demo\n");
    assert!(!path.join("deps").exists(), "no setup ran");
    assert!(
        !path.join(".env").exists(),
        "ignored files are not copied without copy="
    );
    assert_eq!(r["sessions"][0]["port"], 4100, "default port_base");
    let st = status(&env);
    assert!(st["commands"]["setup"].is_null(), "{st}");
    assert!(st["commands"]["test"].is_null(), "{st}");
    assert!(st["commands"]["preview"].is_null(), "{st}");
    // Sessions are real git checkouts on their own branch.
    assert_eq!(
        env.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim(),
        format!("shoal/{}", r["sessions"][0]["name"].as_str().unwrap())
    );
    let r2 = fork_ok(&env);
    assert_eq!(r2["sessions"][0]["port"], 4110);
}

#[test]
fn partial_toml_keeps_detection_for_the_keys_it_leaves_out() {
    let env = Env::new(5716);
    write(env.repo().join("package-lock.json"), "{}\n");
    set_toml(&env, &format!("port_base = {}\n", env.port_base));
    commit_all(&env, "partial");
    let path_env = fake_bins(
        &env,
        &[("npm", "mkdir -p deps && echo \"$*\" > deps/npm.txt")],
    );
    let r = call_env(&env, &env.repo(), "fork", json!({}), &path_env).unwrap();
    assert_eq!(r["sessions"][0]["port"], env.port_base);
    assert_eq!(read(spath(&r).join("deps/npm.txt")), "ci\n");
    assert_eq!(status(&env)["commands"]["setup"], "npm ci");
}

#[test]
fn empty_setup_string_disables_the_detected_setup() {
    let env = Env::new(5718);
    write(env.repo().join("package-lock.json"), "{}\n");
    set_toml(
        &env,
        &format!("setup = \"\"\nport_base = {}\n", env.port_base),
    );
    commit_all(&env, "no setup");
    let path_env = fake_bins(&env, &[("npm", "echo npm must not run >&2; exit 1")]);
    let r = call_env(&env, &env.repo(), "fork", json!({}), &path_env);
    let r = r.unwrap_or_else(|e| panic!("setup=\"\" must not run npm ci: {e}"));
    assert!(!spath(&r).join("deps").exists());
}

#[test]
fn empty_and_crlf_shoal_toml_files_are_valid() {
    let env = Env::new(5720);
    set_toml(&env, "");
    let r = fork_ok(&env);
    assert_eq!(r["sessions"][0]["port"], 4100, "empty file means defaults");

    let env = Env::new(5721);
    set_toml(
        &env,
        &format!(
            "# comment\r\nsetup = \"{SETUP}\"\r\ncopy = [\r\n  \".env\",\r\n]\r\nport_base = {}\r\n",
            env.port_base
        ),
    );
    let r = fork_ok(&env);
    assert_eq!(r["sessions"][0]["port"], env.port_base);
    assert_eq!(read(spath(&r).join("deps/installed.txt")), "lib 1.0\n");
    assert!(spath(&r).join(".env").exists());
}

/// A UTF-8 byte-order mark (common from Windows editors) is accepted.
#[test]
fn shoal_toml_with_a_byte_order_mark_is_accepted() {
    let env = Env::new(5722);
    set_toml(&env, &format!("\u{feff}port_base = {}\n", env.port_base));
    let r = fork(&env);
    let r = r.unwrap_or_else(|e| panic!("BOM-prefixed shoal.toml rejected: {e}"));
    assert_eq!(r["sessions"][0]["port"], env.port_base);
}

// ---------------------------------------------------------------------------
// Invalid shoal.toml
// ---------------------------------------------------------------------------

#[test]
fn invalid_shoal_toml_is_a_clear_error_naming_the_file_and_creates_nothing() {
    let env = Env::new(5724);
    let cases: &[(&str, &str)] = &[
        ("setp = \"typo\"\n", "setp"),
        ("Setup = \"x\"\n", "Setup"),
        ("port_base = \"high\"\n", "port_base"),
        ("port_base = 70000\n", "port_base"),
        ("port_base = -1\n", "port_base"),
        ("port_base = 4100.5\n", "port_base"),
        ("copy = \".env\"\n", "copy"),
        ("copy = [1]\n", "copy"),
        ("copy = [\"../outside\"]\n", "../outside"),
        ("copy = [\"/etc/passwd\"]\n", "/etc/passwd"),
        ("copy = [\"\"]\n", "copy"),
        ("copy = [\"a/../../x\"]\n", "a/../../x"),
        ("backend = \"zfs\"\n", "zfs"),
        ("backend = \"Clone\"\n", "Clone"),
        ("setup = 5\n", "setup"),
        ("setup_inputs = \"deps.lock\"\n", "setup_inputs"),
        ("setup = \n", "setup"),
        ("[setup]\ncmd = \"x\"\n", "setup"),
        ("setup = \"a\"\nsetup = \"b\"\n", "setup"),
    ];
    for (body, needle) in cases {
        set_toml(&env, body);
        let err = fork(&env).expect_err(&format!("{body:?} must be rejected"));
        assert!(
            err.contains("shoal.toml"),
            "{body:?}: error must name the file: {err}"
        );
        assert!(
            err.contains(needle),
            "{body:?}: error must mention {needle:?}: {err}"
        );
        assert_no_project(&env, body);
    }
    // Once the file is fixed, the next fork works.
    set_toml(&env, &standard_toml(&env, "clone"));
    let r = fork_ok(&env);
    assert_eq!(r["sessions"][0]["port"], env.port_base);
}

#[test]
fn shoal_toml_that_is_a_directory_is_a_clear_error() {
    let env = Env::new(5726);
    fs::create_dir_all(env.repo().join("shoal.toml")).unwrap();
    let err = fork(&env).expect_err("a directory is not a project file");
    assert!(err.contains("shoal.toml"), "{err}");
    assert_no_project(&env, "directory shoal.toml");
}

// Fixed bug: shoal.toml is opened with a plain read, so a FIFO named shoal.toml in
// the repository blocks the first fork forever (the MCP tool call or the
// WorktreeCreate hook never returns) instead of being rejected as not a
// regular file.
#[test]
fn shoal_toml_that_is_a_fifo_does_not_hang_the_first_fork() {
    let env = Env::new(5727);
    let fifo = env.repo().join("shoal.toml");
    let st = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(st.success());
    let result = run_with_deadline(&env, &env.repo(), &["api", "fork", "{}"], 10);
    let _ = fs::remove_file(&fifo);
    let (ok, out, err) = result.expect("fork hung on a FIFO shoal.toml");
    assert!(!ok, "a FIFO is not a project file: {out}");
    assert!(out.contains("shoal.toml"), "{out}\n{err}");
}

#[test]
fn invalid_shoal_toml_through_mcp_is_a_tool_error_and_the_server_survives() {
    let env = Env::new(5728);
    set_toml(&env, "backend = \"zfs\"\nport_base = 1\n");
    let mut mcp = env.mcp();
    mcp.initialize();
    let (is_err, text) = mcp.call(1, "fork", json!({}));
    assert!(is_err, "{text}");
    let text = text.as_str().unwrap().to_string();
    assert!(
        text.contains("shoal.toml") && text.contains("zfs"),
        "{text}"
    );

    set_toml(&env, &standard_toml(&env, "clone"));
    let (is_err, r) = mcp.call(2, "fork", json!({}));
    assert!(!is_err, "{r}");
    assert_eq!(r["sessions"][0]["port"], env.port_base);
    let (is_err, st) = mcp.call(3, "status", json!({}));
    assert!(!is_err, "{st}");
    assert_eq!(st["sessions"].as_array().unwrap().len(), 1);
    assert!(
        mcp.next_message(Duration::from_millis(200)).is_none(),
        "stray output"
    );
    assert!(mcp.close().success());
}

#[test]
fn status_never_initialises_a_project_even_with_a_valid_or_invalid_file() {
    let env = Env::new(5730);
    let st = status(&env);
    assert_eq!(st["initialized"], false, "{st}");
    assert!(
        st["next"].as_str().unwrap_or_default().contains("fork"),
        "{st}"
    );
    set_toml(&env, "nonsense = 1\n");
    let st = status(&env);
    assert_eq!(st["initialized"], false, "{st}");
    assert!(project_dirs(&env).is_empty());

    // After init, a broken file does not break orientation.
    set_toml(&env, &standard_toml(&env, "clone"));
    fork_ok(&env);
    set_toml(&env, "nonsense = 1\n");
    let st = status(&env);
    assert_eq!(st["initialized"], true, "{st}");
    assert_eq!(st["sessions"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// copy
// ---------------------------------------------------------------------------

#[test]
fn copy_of_a_missing_file_is_skipped_and_other_entries_still_copied() {
    let env = Env::new(5732);
    set_toml(
        &env,
        &format!(
            "copy = [\"nope.txt\", \".env\", \"missing/dir\"]\nport_base = {}\n",
            env.port_base
        ),
    );
    let r = fork_ok(&env);
    let path = spath(&r);
    assert_eq!(read(path.join(".env")), "SECRET=1\n");
    assert!(!path.join("nope.txt").exists());
    assert!(!path.join("missing").exists());
}

#[test]
fn copy_of_a_tracked_file_is_a_clear_error_and_leaves_no_project() {
    let env = Env::new(5734);
    set_toml(
        &env,
        &format!("copy = [\"README.md\"]\nport_base = {}\n", env.port_base),
    );
    let err = fork(&env).expect_err("README.md is tracked");
    assert!(
        err.contains("README.md") && err.contains("tracked"),
        "{err}"
    );
    assert_no_project(&env, "tracked copy entry");
}

#[test]
fn copy_handles_directories_nested_paths_and_symlinks_and_nothing_else_leaks() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5736 + i as u16);
        let repo = env.repo();
        write(
            repo.join(".gitignore"),
            "deps/\n.env\nsecrets/\nconfig/local.env\n*.secret\n",
        );
        write(repo.join("config/app.toml"), "tracked = true\n");
        write(repo.join("secrets/top.txt"), "top\n");
        write(repo.join("secrets/a/b/deep.txt"), "deep\n");
        std::os::unix::fs::symlink("top.txt", repo.join("secrets/link")).unwrap();
        write(repo.join("config/local.env"), "LOCAL=1\n");
        write(repo.join("other.secret"), "do not copy\n");
        set_toml(
            &env,
            &format!(
                "copy = [\"secrets\", \"config/local.env\", \"./.env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
                env.port_base
            ),
        );
        commit_all(&env, "layout");
        let r = fork_ok(&env);
        let path = spath(&r);
        assert_eq!(read(path.join("secrets/top.txt")), "top\n", "[{backend}]");
        assert_eq!(
            read(path.join("secrets/a/b/deep.txt")),
            "deep\n",
            "[{backend}]"
        );
        assert_eq!(
            fs::read_link(path.join("secrets/link")).unwrap(),
            PathBuf::from("top.txt"),
            "[{backend}] symlinks are copied as symlinks"
        );
        assert_eq!(
            read(path.join("config/local.env")),
            "LOCAL=1\n",
            "[{backend}]"
        );
        assert_eq!(
            read(path.join("config/app.toml")),
            "tracked = true\n",
            "[{backend}]"
        );
        assert_eq!(read(path.join(".env")), "SECRET=1\n", "[{backend}]");
        assert!(
            !path.join("other.secret").exists(),
            "[{backend}] only listed files are copied"
        );
        // Copied ignored files do not show as changes.
        assert_eq!(
            env.git(&path, &["status", "--porcelain"]).trim(),
            "",
            "[{backend}]"
        );
    }
}

// Fixed bug: copy entries are checked only lexically (no "..", not absolute). An
// entry that goes through an ignored symlink to a directory outside the
// repository ("ext/secret.txt" with ext -> /somewhere/else) copies a file
// from outside the repository into every session, which the
// "copy entries must be paths inside the repository" rule is meant to stop.
#[test]
fn copy_through_a_symlink_that_leaves_the_repository_is_rejected() {
    let env = Env::new(5739);
    let outside = env.root.path().join("outside");
    write(outside.join("secret.txt"), "OUTSIDE-SECRET\n");
    std::os::unix::fs::symlink(&outside, env.repo().join("ext")).unwrap();
    write(env.repo().join(".gitignore"), "deps/\n.env\next\n");
    commit_all(&env, "ignore ext");
    set_toml(
        &env,
        &format!(
            "copy = [\"ext/secret.txt\"]\nport_base = {}\nbackend = \"clone\"\n",
            env.port_base
        ),
    );
    match fork(&env) {
        Err(err) => assert!(err.contains("ext/secret.txt"), "{err}"),
        Ok(r) => {
            let leaked = spath(&r).join("ext/secret.txt");
            assert!(
                !leaked.exists(),
                "a file from outside the repository was copied into the session: {}",
                read(&leaked)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Setup failures
// ---------------------------------------------------------------------------

#[test]
fn failing_setup_is_a_clear_error_leaves_nothing_behind_and_recovers_once_fixed() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5740 + i as u16);
        set_toml(
            &env,
            &format!(
                "setup = 'mkdir -p deps && echo half > deps/partial && echo boom-from-setup >&2 && exit 7'\ncopy = [\".env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
                env.port_base
            ),
        );
        let err = fork(&env).expect_err("setup fails");
        assert!(err.contains("setup"), "[{backend}] {err}");
        assert!(
            err.contains("exit 7"),
            "[{backend}] error names the command: {err}"
        );
        assert!(
            err.contains("7)") || err.contains("status: 7"),
            "[{backend}] exit code: {err}"
        );
        assert_no_project(&env, backend);
        assert!(
            mounts_under(&env.home()).is_empty(),
            "{:?}",
            mounts_under(&env.home())
        );

        // A second failing attempt behaves the same (no "already has a project").
        let err2 = fork(&env).expect_err("still failing");
        assert!(err2.contains("setup"), "{err2}");
        assert_no_project(&env, backend);

        set_toml(&env, &standard_toml(&env, backend));
        let r = fork_ok(&env);
        let path = spath(&r);
        assert_eq!(read(path.join("deps/installed.txt")), "lib 1.0\n");
        assert!(
            !path.join("deps/partial").exists(),
            "[{backend}] the failed build was discarded"
        );
        assert_eq!(r["sessions"][0]["port"], env.port_base);
        assert_eq!(generations(&env), 1);
    }
}

#[test]
fn setup_output_and_stdin_never_touch_the_mcp_protocol() {
    let env = Env::new(5744);
    set_toml(
        &env,
        &format!(
            "setup = 'echo SETUP-NOISE-ON-STDOUT; cat; head -c 300000 /dev/zero | tr \"\\\\0\" x; echo; {SETUP}'\ncopy = [\".env\"]\nport_base = {}\nbackend = \"clone\"\n",
            env.port_base
        ),
    );
    let mut mcp = env.mcp();
    mcp.initialize();
    mcp.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "fork", "arguments": {}}}));
    // If setup inherited the server's stdin, `cat` would block here forever.
    let resp = mcp.response(&json!(1), Duration::from_secs(60));
    let result = &resp["result"];
    assert_ne!(result["isError"], true, "{resp}");
    let text = result["content"][0]["text"].as_str().unwrap();
    let r: Value = serde_json::from_str(text).unwrap();
    assert!(spath(&r).join("deps/installed.txt").exists());
    let (is_err, st) = mcp.call(2, "status", json!({}));
    assert!(!is_err, "{st}");
    assert!(
        mcp.next_message(Duration::from_millis(300)).is_none(),
        "stray output"
    );
    assert!(mcp.close().success());
}

#[test]
fn worktree_create_hook_auto_initialises_from_shoal_toml_with_only_the_path_on_stdout() {
    let env = Env::new(5746);
    set_toml(
        &env,
        &format!(
            "setup = 'echo HOOK-NOISE; {SETUP}'\ncopy = [\".env\"]\nport_base = {}\nbackend = \"clone\"\n",
            env.port_base
        ),
    );
    let r = env.hook(
        "worktree-create",
        &json!({"name": "wt-one", "cwd": env.repo()}),
        &env.repo(),
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.last_line().ends_with("/sessions/wt-one"), "{}", r.stdout);
    assert!(
        !r.stdout.contains("HOOK-NOISE"),
        "setup output leaked to stdout: {}",
        r.stdout
    );
    assert_eq!(r.stdout.trim().lines().count(), 1, "{}", r.stdout);
    let path = PathBuf::from(r.last_line());
    assert_eq!(read(path.join("deps/installed.txt")), "lib 1.0\n");
    assert_eq!(read(path.join(".env")), "SECRET=1\n");
    assert_eq!(env.state()["sessions"]["wt-one"]["port"], env.port_base);
}

// ---------------------------------------------------------------------------
// Repository shapes
// ---------------------------------------------------------------------------

#[test]
fn non_repository_and_detached_head_are_clear_errors() {
    let env = Env::new(5748);
    let plain = env.root.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let err = call(&env, &plain, "fork", json!({})).expect_err("not a git repository");
    assert!(err.to_lowercase().contains("git"), "{err}");
    assert!(project_dirs(&env).is_empty());

    env.git(&env.repo(), &["checkout", "-q", "--detach"]);
    let err = fork(&env).expect_err("detached HEAD");
    assert!(err.contains("detached HEAD"), "{err}");
    assert!(project_dirs(&env).is_empty());
    env.git(&env.repo(), &["checkout", "-q", "main"]);
    fork_ok(&env);
}

// Fixed bug: in a repository with no commits yet (`git init` only), ops::init runs
// `git rev-parse --abbrev-ref HEAD` before its "branch has no commits" check,
// so the first fork fails with raw git output ("fatal: ambiguous argument
// 'HEAD': unknown revision ...") instead of saying the branch has no commits
// and that a first commit is needed.
#[test]
fn repository_without_commits_is_a_clear_error() {
    let env = Env::new(5749);
    let empty = env.root.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    env.git(&empty, &["init", "-q", "-b", "main"]);
    write(empty.join("file.txt"), "x\n");
    let err = call(&env, &empty, "fork", json!({})).expect_err("no commits");
    assert!(project_dirs(&env).is_empty(), "{:?}", project_dirs(&env));
    assert!(err.contains("no commits"), "{err}");
}

#[test]
fn repository_paths_with_spaces_and_unicode_and_same_named_repositories() {
    let env = Env::new(5750);
    let mut seen = Vec::new();
    for (i, (dir, backend)) in [
        ("a b/my repo ü", "clone"),
        ("other/my repo ü", backends()[backends().len() - 1]),
    ]
    .into_iter()
    .enumerate()
    {
        let repo = env.root.path().join(dir);
        write(repo.join("file.txt"), "hello\n");
        write(repo.join(".gitignore"), "deps/\n");
        write(repo.join("deps.lock"), format!("lock {i}\n").as_str());
        write(
            repo.join("shoal.toml"),
            &format!(
                "setup = \"{SETUP}\"\nport_base = {}\nbackend = \"{backend}\"\n",
                env.port_base + i as u16
            ),
        );
        env.git(&repo, &["init", "-q", "-b", "main"]);
        env.git(&repo, &["add", "-A"]);
        env.git(&repo, &["commit", "-q", "-m", "init"]);
        let r = call(&env, &repo, "fork", json!({})).unwrap();
        let path = spath(&r);
        assert_eq!(
            read(path.join("deps/installed.txt")),
            format!("lock {i}\n"),
            "[{dir}]"
        );
        // The second project's port_base is one above the first's, so its
        // first block would overlap the first project's (port_base..+9):
        // it moves up to the next block that does not.
        assert_eq!(r["sessions"][0]["port"], env.port_base + 11 * i as u16);
        let st = call(&env, &repo, "status", json!({})).unwrap();
        assert_eq!(
            PathBuf::from(st["source"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            repo.canonicalize().unwrap()
        );
        seen.push(st["project"].as_str().unwrap().to_string());
    }
    assert_ne!(
        seen[0], seen[1],
        "same basename, different repositories, different projects"
    );
    assert_eq!(project_dirs(&env).len(), 2);
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

// Was a bug: Project::next_port iterated `(0u16..)`, which wraps in release
// builds, so with port_base near the top the second fork hung forever while
// holding the project lock. Now port_base is capped at 65526 (a block of ten
// must fit) and running out of ports is an error.
#[test]
fn port_base_near_the_top_of_the_range_does_not_hang_the_second_fork() {
    let env = Env::new(5752);
    set_toml(&env, "port_base = 65530\nbackend = \"clone\"\n");
    let err = fork(&env).unwrap_err();
    assert!(err.contains("port_base must be between"), "{err}");
    set_toml(&env, "port_base = 65526\nbackend = \"clone\"\n");
    let r = fork_ok(&env);
    assert_eq!(r["sessions"][0]["port"], 65526);
    let result = run_with_deadline(&env, &env.repo(), &["api", "fork", "{}"], 15);
    let (ok, out, err) =
        result.expect("second fork with port_base = 65526 hung (killed after 15s)");
    if ok {
        let v: Value = serde_json::from_str(&out).unwrap();
        let port = v["sessions"][0]["port"].as_u64().unwrap();
        assert_ne!(port, 65526, "two sessions on one port: {out}");
    } else {
        assert!(
            out.contains("port"),
            "error should explain the port problem: {out}\n{err}"
        );
    }
}

// Fixed bug: port_base = 0 is accepted, and the first session is given port 0, so
// $PORT=0 (an OS-chosen port) and a preview URL of http://localhost:0.
#[test]
fn port_base_zero_is_rejected_or_never_hands_out_port_zero() {
    let env = Env::new(5753);
    set_toml(&env, "port_base = 0\nbackend = \"clone\"\n");
    match fork(&env) {
        Err(err) => assert!(err.contains("port_base"), "{err}"),
        Ok(r) => assert_ne!(r["sessions"][0]["port"], 0, "session given port 0: {r}"),
    }
}

// ---------------------------------------------------------------------------
// Concurrency of auto-initialisation and refresh
// ---------------------------------------------------------------------------

#[test]
fn concurrent_first_forks_build_one_template_and_run_setup_once() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5755 + i as u16);
        let counter = env.root.path().join("setup-runs.txt");
        set_toml(
            &env,
            &format!(
                "setup = 'echo run >> \"{}\"; sleep 1; {SETUP}'\ncopy = [\".env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
                counter.display(),
                env.port_base
            ),
        );
        commit_all(&env, "toml");
        let results: Vec<Result<Value, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4).map(|_| s.spawn(|| fork(&env))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut names = Vec::new();
        let mut ports = Vec::new();
        for r in &results {
            let r = r
                .as_ref()
                .unwrap_or_else(|e| panic!("[{backend}] concurrent fork failed: {e}"));
            assert!(spath(r).join("deps/installed.txt").exists());
            assert!(spath(r).join(".env").exists());
            names.push(r["sessions"][0]["name"].as_str().unwrap().to_string());
            ports.push(r["sessions"][0]["port"].as_u64().unwrap());
        }
        names.sort();
        names.dedup();
        ports.sort();
        ports.dedup();
        assert_eq!(names.len(), 4, "[{backend}] distinct names");
        assert_eq!(ports.len(), 4, "[{backend}] distinct ports");
        assert_eq!(lines(&counter), 1, "[{backend}] setup ran once");
        assert_eq!(project_dirs(&env).len(), 1);
        assert_eq!(generations(&env), 1);
        assert_eq!(env.state()["sessions"].as_object().unwrap().len(), 4);
    }
}

#[test]
fn concurrent_forks_after_a_commit_build_one_new_generation() {
    let backend = *backends().last().unwrap();
    let env = Env::new(5758);
    let counter = env.root.path().join("setup-runs.txt");
    set_toml(
        &env,
        &format!(
            "setup = 'echo run >> \"{}\"; {SETUP}'\nsetup_inputs = [\"deps.lock\"]\nport_base = {}\nbackend = \"{backend}\"\n",
            counter.display(),
            env.port_base
        ),
    );
    commit_all(&env, "toml");
    fork_ok(&env);
    write(env.repo().join("deps.lock"), "lib 2.0\n");
    commit_all(&env, "bump");
    let results: Vec<Result<Value, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..3).map(|_| s.spawn(|| fork(&env))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut refreshed = 0;
    for r in &results {
        let r = r.as_ref().unwrap_or_else(|e| panic!("fork failed: {e}"));
        assert_eq!(r["sessions"][0]["generation"], 2, "{r}");
        assert_eq!(read(spath(r).join("deps/installed.txt")), "lib 2.0\n");
        if !r["refreshed"].is_null() {
            refreshed += 1;
        }
    }
    assert_eq!(refreshed, 1, "exactly one fork builds the new generation");
    assert_eq!(lines(&counter), 2, "setup: once at init, once on refresh");
    assert_eq!(generations(&env), 2);
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

#[test]
fn refresh_reruns_setup_only_when_a_setup_input_changes() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5760 + i as u16);
        set_toml(
            &env,
            &format!(
                "setup = 'mkdir -p deps && echo run >> deps/runs.txt && cat deps.lock > deps/installed.txt'\nsetup_inputs = [\"deps.lock\"]\ncopy = [\".env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
                env.port_base
            ),
        );
        commit_all(&env, "toml");
        let a = fork_ok(&env);
        let a_path = spath(&a);
        assert_eq!(lines(a_path.join("deps/runs.txt")), 1);

        // A commit that does not touch deps.lock: new template, no setup.
        write(env.repo().join("README.md"), "demo v2\n");
        commit_all(&env, "readme");
        let b = fork_ok(&env);
        let rb = &b["refreshed"];
        assert_eq!(rb["up_to_date"], false, "[{backend}] {b}");
        assert_eq!(rb["setup_ran"], false, "[{backend}] {b}");
        assert_eq!(rb["changed_files"], 1, "[{backend}] {b}");
        assert_eq!(rb["generation"], 2, "[{backend}] {b}");
        assert_eq!(b["sessions"][0]["generation"], 2);
        let b_path = spath(&b);
        assert_eq!(read(b_path.join("README.md")), "demo v2\n");
        assert_eq!(
            lines(b_path.join("deps/runs.txt")),
            1,
            "[{backend}] setup skipped"
        );
        assert_eq!(read(b_path.join("deps/installed.txt")), "lib 1.0\n");
        assert_eq!(
            read(b_path.join(".env")),
            "SECRET=1\n",
            "[{backend}] copied files survive refresh"
        );
        assert_eq!(
            env.git(&b_path, &["status", "--porcelain"]).trim(),
            "",
            "[{backend}]"
        );

        // A commit that changes deps.lock: setup runs again.
        write(env.repo().join("deps.lock"), "lib 2.0\n");
        commit_all(&env, "bump deps");
        let c = fork_ok(&env);
        assert_eq!(c["refreshed"]["setup_ran"], true, "[{backend}] {c}");
        assert_eq!(c["refreshed"]["generation"], 3);
        let c_path = spath(&c);
        assert_eq!(
            read(c_path.join("deps/installed.txt")),
            "lib 2.0\n",
            "[{backend}]"
        );
        assert_eq!(lines(c_path.join("deps/runs.txt")), 2);
        assert_eq!(read(c_path.join("README.md")), "demo v2\n");

        // Older sessions keep what they started with.
        assert_eq!(read(a_path.join("deps/installed.txt")), "lib 1.0\n");
        assert_eq!(read(a_path.join("README.md")), "demo\n");
        assert_eq!(read(b_path.join("deps/installed.txt")), "lib 1.0\n");

        // Nothing changed since: the next fork does not refresh.
        let d = fork_ok(&env);
        assert!(d["refreshed"].is_null(), "[{backend}] {d}");
        assert_eq!(d["sessions"][0]["generation"], 3);
        assert_eq!(generations(&env), 3);
        assert_eq!(status(&env)["sessions"].as_array().unwrap().len(), 4);
    }
}

#[test]
fn refresh_when_nothing_changed_reports_up_to_date_and_ignores_uncommitted_source_edits() {
    let env = Env::new(5764);
    set_toml(&env, &standard_toml(&env, "clone"));
    commit_all(&env, "toml");
    fork_ok(&env);
    let out = env.shoal(&["refresh"]);
    assert!(out.contains("already at"), "{out}");
    assert_eq!(generations(&env), 1);

    // Uncommitted edits in the source are not part of the template.
    write(env.repo().join("README.md"), "uncommitted\n");
    let r = fork_ok(&env);
    assert!(r["refreshed"].is_null(), "{r}");
    assert_eq!(read(spath(&r).join("README.md")), "demo\n");
    assert_eq!(status(&env)["template"]["behind_source"], false);
    env.git(&env.repo(), &["checkout", "--", "README.md"]);

    // A commit makes status report the template behind, and refresh builds g2.
    write(env.repo().join("src/new.txt"), "new\n");
    commit_all(&env, "new file");
    assert_eq!(status(&env)["template"]["behind_source"], true);
    let out = env.shoal(&["refresh"]);
    assert!(out.contains("g2"), "{out}");
    assert_eq!(generations(&env), 2);
    assert_eq!(status(&env)["template"]["behind_source"], false);
    let out = env.shoal(&["refresh"]);
    assert!(out.contains("already at"), "{out}");
    let r = fork_ok(&env);
    assert!(r["refreshed"].is_null(), "{r}");
    assert_eq!(read(spath(&r).join("src/new.txt")), "new\n");
}

#[test]
fn default_setup_inputs_match_lockfiles_at_any_depth_but_not_lookalikes() {
    let env = Env::new(5766);
    set_toml(
        &env,
        &format!(
            "setup = 'mkdir -p deps && echo run >> deps/runs.txt'\nport_base = {}\nbackend = \"clone\"\n",
            env.port_base
        ),
    );
    commit_all(&env, "toml");
    fork_ok(&env);
    let cases: &[(&str, bool)] = &[
        ("deps.lock", false),             // not a default input
        ("my-package-lock.json", false),  // a lookalike name
        ("package-lock.json.bak", false), // a lookalike name
        ("web/package-lock.json", true),  // nested lockfile
        ("Cargo.lock", true),
        ("go.sum", true),
    ];
    let mut runs = 1;
    for (file, expect) in cases {
        write(env.repo().join(file), &format!("changed {file}\n"));
        commit_all(&env, file);
        let r = fork_ok(&env);
        assert_eq!(r["refreshed"]["setup_ran"], *expect, "{file}: {r}");
        if *expect {
            runs += 1;
        }
        assert_eq!(lines(spath(&r).join("deps/runs.txt")), runs, "{file}");
    }
}

#[test]
fn failed_refresh_keeps_the_previous_generation_and_recovers_after_a_fix() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5768 + i as u16);
        set_toml(
            &env,
            &format!(
                "setup = 'test \"$(cat deps.lock)\" != broken && {SETUP}'\nsetup_inputs = [\"deps.lock\"]\ncopy = [\".env\"]\nport_base = {}\nbackend = \"{backend}\"\n",
                env.port_base
            ),
        );
        commit_all(&env, "toml");
        let a = fork_ok(&env);
        let a_name = a["sessions"][0]["name"].as_str().unwrap().to_string();
        write(spath(&a).join("work.txt"), "in progress\n");

        write(env.repo().join("deps.lock"), "broken\n");
        commit_all(&env, "break deps");
        // A failing refresh does not block work: the fork starts from the
        // last good template and says why.
        let fallback = fork_ok(&env);
        let why = fallback["refresh_error"].as_str().unwrap_or_default();
        assert!(why.contains("setup"), "[{backend}] {fallback}");
        assert_eq!(fallback["sessions"][0]["generation"], 1, "[{backend}]");
        let fallback_name = fallback["sessions"][0]["name"]
            .as_str()
            .unwrap()
            .to_string();
        let pd = env.project_dir();
        assert_eq!(generations(&env), 1, "[{backend}]");
        assert!(
            !pd.join("templates/g2").exists(),
            "[{backend}] half-built clone left"
        );
        assert!(
            !pd.join("layers/g2").exists(),
            "[{backend}] half-built layer left"
        );
        assert!(
            !pd.join("build").exists() && !pd.join("build-work").exists(),
            "[{backend}]"
        );
        assert!(mounts_under(&pd.join("build")).is_empty());
        let st = status(&env);
        assert_eq!(
            st["sessions"].as_array().unwrap().len(),
            2,
            "[{backend}] the earlier session and the fallback: {st}"
        );
        assert_eq!(st["template"]["behind_source"], true);
        call(
            &env,
            &env.repo(),
            "discard",
            json!({"session": fallback_name, "keep_work": false}),
        )
        .unwrap();
        assert_eq!(
            read(spath(&a).join("work.txt")),
            "in progress\n",
            "[{backend}] work kept"
        );
        // Forking an existing session does not need the template.
        let f = call(&env, &env.repo(), "fork", json!({"from": a_name})).unwrap();
        assert_eq!(read(spath(&f).join("work.txt")), "in progress\n");

        write(env.repo().join("deps.lock"), "lib 3.0\n");
        commit_all(&env, "fix deps");
        let c = fork_ok(&env);
        assert_eq!(c["refreshed"]["generation"], 2, "[{backend}] {c}");
        assert_eq!(c["refreshed"]["setup_ran"], true);
        assert_eq!(read(spath(&c).join("deps/installed.txt")), "lib 3.0\n");
        assert_eq!(generations(&env), 2);
    }
}

#[test]
fn refresh_follows_rewritten_and_rewound_history() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5772 + i as u16);
        set_toml(&env, &standard_toml(&env, backend));
        commit_all(&env, "toml");
        let first = env
            .git(&env.repo(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        fork_ok(&env);
        write(env.repo().join("README.md"), "v2\n");
        commit_all(&env, "v2");
        let b = fork_ok(&env);
        assert_eq!(read(spath(&b).join("README.md")), "v2\n");

        // Non-fast-forward: the commit is replaced.
        write(env.repo().join("README.md"), "v2-alt\n");
        env.git(
            &env.repo(),
            &["commit", "-q", "-a", "--amend", "-m", "v2 alt"],
        );
        let c = fork_ok(&env);
        assert_eq!(c["refreshed"]["generation"], 3, "[{backend}] {c}");
        assert_eq!(read(spath(&c).join("README.md")), "v2-alt\n", "[{backend}]");

        // Rewound to an older commit.
        env.git(&env.repo(), &["reset", "-q", "--hard", &first]);
        let d = fork_ok(&env);
        assert_eq!(d["refreshed"]["generation"], 4, "[{backend}] {d}");
        assert_eq!(read(spath(&d).join("README.md")), "demo\n", "[{backend}]");
        assert_eq!(
            env.git(&spath(&d), &["rev-parse", "HEAD"]).trim(),
            first,
            "[{backend}] the session starts at the rewound commit"
        );
        assert_eq!(read(spath(&d).join("deps/installed.txt")), "lib 1.0\n");
    }
}

#[test]
fn tracked_branch_that_disappears_is_a_clear_error_and_existing_sessions_survive() {
    let env = Env::new(5776);
    set_toml(&env, &standard_toml(&env, "clone"));
    commit_all(&env, "toml");
    let a = fork_ok(&env);
    env.git(&env.repo(), &["branch", "-m", "main", "trunk"]);
    // The refresh fails and says why; the fork still works from the last
    // good template.
    let r = fork_ok(&env);
    let why = r["refresh_error"].as_str().unwrap_or_default();
    assert!(why.contains("main"), "{r}");
    let fallback = r["sessions"][0]["name"].as_str().unwrap().to_string();
    let st = status(&env);
    assert_eq!(st["initialized"], true);
    assert_eq!(st["sessions"].as_array().unwrap().len(), 2, "{st}");
    call(
        &env,
        &env.repo(),
        "discard",
        json!({"session": fallback, "keep_work": false}),
    )
    .unwrap();
    assert!(spath(&a).join("deps/installed.txt").exists());
    env.git(&env.repo(), &["branch", "-m", "trunk", "main"]);
    let r = fork_ok(&env);
    assert!(r["refreshed"].is_null(), "{r}");
}

// Fixed bug: the setup command and setup_inputs are copied into state.json when
// the project is first created and never read from shoal.toml again. A
// committed change to `setup` in shoal.toml is ignored by every later
// refresh (status keeps reporting the old command), while `test` and
// `preview` are read live from the same file. The only way to pick up a new
// setup command is to delete the project.
#[test]
fn a_committed_change_to_the_setup_command_is_used_by_the_next_refresh() {
    let env = Env::new(5778);
    set_toml(&env, &standard_toml(&env, "clone"));
    commit_all(&env, "toml");
    fork_ok(&env);
    let new_setup = "mkdir -p deps && echo second > deps/second.txt";
    set_toml(
        &env,
        &format!(
            "setup = \"{new_setup}\"\nsetup_inputs = [\"deps.lock\", \"shoal.toml\"]\ncopy = [\".env\"]\nport_base = {}\nbackend = \"clone\"\n",
            env.port_base
        ),
    );
    write(env.repo().join("deps.lock"), "lib 2.0\n");
    commit_all(&env, "new setup and new deps");
    let r = fork_ok(&env);
    assert_eq!(r["refreshed"]["setup_ran"], true, "{r}");
    assert_eq!(
        status(&env)["commands"]["setup"],
        new_setup,
        "status shows the stale setup"
    );
    assert!(
        spath(&r).join("deps/second.txt").exists(),
        "the new setup command did not run"
    );
}

// ---------------------------------------------------------------------------
// The template is never modified by sessions
// ---------------------------------------------------------------------------

#[test]
fn sessions_never_modify_the_template() {
    for (i, backend) in backends().into_iter().enumerate() {
        let env = Env::new(5780 + i as u16);
        set_toml(&env, &standard_toml(&env, backend));
        commit_all(&env, "toml");
        let a = fork_ok(&env);
        let template = template_g1(&env, backend);
        assert!(
            template.join("deps/installed.txt").exists(),
            "[{backend}] {}",
            template.display()
        );
        let before = snapshot(&template);
        let head = env.state()["generations"][0]["commit"]
            .as_str()
            .unwrap()
            .to_string();

        let p = spath(&a);
        let name = a["sessions"][0]["name"].as_str().unwrap().to_string();
        write(p.join("src/app.txt"), "rewritten by the session\n");
        fs::remove_file(p.join("README.md")).unwrap();
        write(p.join("new/deep/file.txt"), "new\n");
        write(p.join("deps/installed.txt"), "tampered\n");
        write(p.join(".env"), "SECRET=changed\n");
        fs::set_permissions(p.join("deps.lock"), fs::Permissions::from_mode(0o600)).unwrap();
        env.git(&p, &["add", "-A"]);
        env.git(&p, &["commit", "-q", "-m", "session work"]);
        env.git(&p, &["branch", "side"]);
        env.git(&p, &["tag", "v-session"]);
        write(p.join("uncommitted.txt"), "dirty\n");

        // Fork the session, checkpoint it, fork from main, discard the session.
        let f = call(&env, &env.repo(), "fork", json!({"from": name})).unwrap();
        write(spath(&f).join("src/app.txt"), "fork edit\n");
        call(
            &env,
            &env.repo(),
            "checkpoint",
            json!({"session": name, "label": "c1"}),
        )
        .unwrap();
        let b = fork_ok(&env);
        let bp = spath(&b);
        assert_eq!(
            read(bp.join("src/app.txt")),
            "line one\nline two\nline three\n",
            "[{backend}]"
        );
        assert_eq!(read(bp.join("README.md")), "demo\n", "[{backend}]");
        assert_eq!(
            read(bp.join("deps/installed.txt")),
            "lib 1.0\n",
            "[{backend}]"
        );
        assert_eq!(read(bp.join(".env")), "SECRET=1\n", "[{backend}]");
        assert!(
            !bp.join("new").exists() && !bp.join("uncommitted.txt").exists(),
            "[{backend}]"
        );
        assert_eq!(
            env.git(&bp, &["rev-parse", "HEAD"]).trim(),
            head,
            "[{backend}]"
        );
        call(
            &env,
            &env.repo(),
            "discard",
            json!({"session": name, "keep_work": false}),
        )
        .unwrap();

        let after = snapshot(&template);
        let changed: Vec<_> = before
            .iter()
            .filter(|(k, v)| after.get(*k) != Some(v))
            .map(|(k, _)| k.clone())
            .chain(after.keys().filter(|k| !before.contains_key(*k)).cloned())
            .collect();
        assert!(
            changed.is_empty(),
            "[{backend}] template changed: {changed:?}"
        );
        assert!(
            env.git(&env.repo(), &["status", "--porcelain"])
                .trim()
                .is_empty(),
            "[{backend}] the source repository was touched"
        );
    }
}

// ---------------------------------------------------------------------------
// Explicit init and more setup failures
// ---------------------------------------------------------------------------

#[test]
fn explicit_init_twice_is_refused_and_keeps_the_existing_project() {
    let env = Env::new(5790);
    env.init("clone");
    let a = fork_ok(&env);
    let before = env.state();
    let out = shoal_cmd(
        &env,
        &env.repo(),
        &["init", "--backend", "clone", "--setup", "exit 1"],
        &[],
    )
    .output()
    .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("already has a shoal project"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.state(), before, "state untouched");
    assert!(spath(&a).join("deps/installed.txt").exists());
    // shoal.toml written after init does not re-initialise anything.
    set_toml(&env, "port_base = 5999\n");
    let b = fork_ok(&env);
    assert_eq!(b["sessions"][0]["port"], env.port_base + 10);
}

#[test]
fn explicit_init_rejects_copy_paths_outside_the_repository() {
    let env = Env::new(5791);
    for bad in ["../outside", "/etc/passwd", "", "a/../../b"] {
        let out = shoal_cmd(
            &env,
            &env.repo(),
            &["init", "--backend", "clone", "--copy", bad],
            &[],
        )
        .output()
        .unwrap();
        assert!(!out.status.success(), "{bad:?} accepted");
        assert!(stderr(&out).contains("copy"), "{bad:?}: {}", stderr(&out));
        assert_no_project(&env, bad);
    }
}

#[test]
fn setup_command_that_does_not_exist_is_a_clear_error() {
    let env = Env::new(5792);
    set_toml(
        &env,
        &format!(
            "setup = \"definitely-not-a-command-xyz --flag\"\nport_base = {}\n",
            env.port_base
        ),
    );
    let err = fork(&env).expect_err("command not found");
    assert!(err.contains("definitely-not-a-command-xyz"), "{err}");
    assert!(err.contains("127"), "{err}");
    assert_no_project(&env, "missing setup command");
}

#[test]
fn first_fork_with_count_builds_the_template_once_for_every_session() {
    let env = Env::new(5793);
    let counter = env.root.path().join("runs.txt");
    set_toml(
        &env,
        &format!(
            "setup = 'echo run >> \"{}\"; {SETUP}'\ncopy = [\".env\"]\nport_base = {}\n",
            counter.display(),
            env.port_base
        ),
    );
    let r = call(
        &env,
        &env.repo(),
        "fork",
        json!({"count": 3, "name": "try"}),
    )
    .unwrap();
    let sessions = r["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3, "{r}");
    for (i, s) in sessions.iter().enumerate() {
        assert_eq!(s["name"], format!("try-{}", i + 1));
        assert_eq!(s["port"], env.port_base + 10 * i as u16);
        let p = PathBuf::from(s["path"].as_str().unwrap());
        assert_eq!(read(p.join("deps/installed.txt")), "lib 1.0\n");
        assert_eq!(read(p.join(".env")), "SECRET=1\n");
    }
    assert_eq!(lines(&counter), 1);
    // count out of range does not initialise anything in a fresh repository.
    let env2 = Env::new(5794);
    set_toml(&env2, &format!("port_base = {}\n", env2.port_base));
    let err = call(&env2, &env2.repo(), "fork", json!({"count": 0})).expect_err("count 0");
    assert!(err.contains("count"), "{err}");
    let err = call(&env2, &env2.repo(), "fork", json!({"count": 9})).expect_err("count 9");
    assert!(err.contains("count"), "{err}");
    assert_no_project(&env2, "bad count");
}
