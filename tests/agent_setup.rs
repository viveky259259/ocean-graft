//! `ocean-graft setup` and `ocean-graft instructions`: ocean-graft for Codex, Cursor, Gemini CLI
//! and OpenCode. Every test runs the binary with HOME (and the agents' own
//! directory variables) pointed at a throwaway directory.
//!
//! Port range 6400-6499.
mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Setup {
    _dir: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(ok.success());
        Setup {
            _dir: dir,
            home,
            repo,
        }
    }

    fn install(&self, agent_dir: &str) -> PathBuf {
        let d = self.home.join(agent_dir);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("setup")
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env_remove("CODEX_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.repo, args)
    }
}

fn ocean_graft_path() -> String {
    Path::new(BIN)
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .into()
}

fn json_file(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

#[test]
fn instructions_are_the_skill_without_claude_only_parts() {
    let out = Command::new(BIN).arg("instructions").output().unwrap();
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(
        text.starts_with("# Working with ocean-graft sessions"),
        "{text}"
    );
    assert!(text.contains("pass `project`"));
    assert!(text.contains("ocean-graft api <tool>"));
    assert!(!text.contains("mcp__plugin"));
    assert!(!text.contains("worktree isolation"));
}

#[test]
fn all_sets_up_every_installed_agent_and_nothing_else() {
    let s = Setup::new();
    s.install(".codex");
    s.install(".gemini");
    let out = s.run(&["all"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("cursor    not installed here"), "{text}");
    assert!(text.contains("opencode  not installed here"), "{text}");
    assert!(!s.home.join(".cursor").exists());
    assert!(!s.home.join(".config/opencode").exists());

    let toml: toml::Table = read(s.home.join(".codex/config.toml")).parse().unwrap();
    assert_eq!(
        toml["mcp_servers"]["ocean-graft"]["command"].as_str(),
        Some(ocean_graft_path().as_str())
    );
    let gemini = json_file(&s.home.join(".gemini/settings.json"));
    assert_eq!(
        gemini["mcpServers"]["ocean-graft"],
        json!({"command": ocean_graft_path(), "args": ["mcp"]})
    );
    // Codex reads AGENTS.md; Gemini CLI reads GEMINI.md.
    assert!(read(s.repo.join("AGENTS.md")).contains("<!-- ocean-graft:start"));
    assert!(read(s.repo.join("GEMINI.md")).contains("<!-- ocean-graft:start"));
}

#[test]
fn all_with_no_agent_installed_fails_and_writes_nothing() {
    let s = Setup::new();
    let out = s.run(&["all"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("none of codex, cursor, gemini or opencode is installed"));
    assert!(!s.repo.join("AGENTS.md").exists());
    assert_eq!(fs::read_dir(&s.home).unwrap().count(), 0);
}

#[test]
fn a_named_agent_is_set_up_even_before_it_is_installed() {
    let s = Setup::new();
    let out = s.run(&["opencode"]);
    assert!(out.status.success(), "{}", stdout(&out));
    let config = json_file(&s.home.join(".config/opencode/opencode.json"));
    assert_eq!(config["$schema"], "https://opencode.ai/config.json");
    assert_eq!(
        config["mcp"]["ocean-graft"],
        json!({"type": "local", "command": [ocean_graft_path(), "mcp"], "enabled": true})
    );
}

#[test]
fn a_second_run_changes_nothing() {
    let s = Setup::new();
    s.install(".cursor");
    fs::write(
        s.home.join(".cursor/mcp.json"),
        "{\n  \"mcpServers\": {\n    \"github\": {\"command\": \"gh\"}\n  }\n}\n",
    )
    .unwrap();
    fs::write(s.repo.join("AGENTS.md"), "# Ours\n").unwrap();
    assert!(s.run(&["cursor"]).status.success());
    let config = read(s.home.join(".cursor/mcp.json"));
    let agents = read(s.repo.join("AGENTS.md"));
    assert!(agents.starts_with("# Ours\n\n<!-- ocean-graft:start"));
    let again = s.run(&["cursor"]);
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("already there"),
        "{}",
        stdout(&again)
    );
    assert_eq!(read(s.home.join(".cursor/mcp.json")), config);
    assert_eq!(read(s.repo.join("AGENTS.md")), agents);
    // The user's own server is untouched.
    assert_eq!(
        json_file(&s.home.join(".cursor/mcp.json"))["mcpServers"]["github"],
        json!({"command": "gh"})
    );
}

#[test]
fn an_edited_section_is_replaced_and_the_rest_of_the_file_kept() {
    let s = Setup::new();
    assert!(s.run(&["codex"]).status.success());
    let first = read(s.repo.join("AGENTS.md"));
    let edited = format!(
        "# Mine\n\n{}\n## After\n",
        first.replace("### Decide", "### Something stale")
    );
    fs::write(s.repo.join("AGENTS.md"), &edited).unwrap();
    let out = s.run(&["codex"]);
    assert!(
        stdout(&out).contains("instructions  updated"),
        "{}",
        stdout(&out)
    );
    let now = read(s.repo.join("AGENTS.md"));
    assert!(now.starts_with("# Mine\n\n"));
    assert!(now.ends_with("\n## After\n"));
    assert!(now.contains("### Decide") && !now.contains("Something stale"));
}

#[test]
fn a_config_ocean_graft_cannot_safely_edit_is_left_alone_and_the_rest_goes_on() {
    let s = Setup::new();
    s.install(".cursor");
    s.install(".gemini");
    let commented = "{\n  // my servers\n  \"mcpServers\": {}\n}\n";
    fs::write(s.home.join(".cursor/mcp.json"), commented).unwrap();
    let out = s.run(&["all"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(
        text.contains("cursor    MCP server    not changed"),
        "{text}"
    );
    assert!(text.contains("add this by hand: \"mcpServers\""), "{text}");
    assert_eq!(read(s.home.join(".cursor/mcp.json")), commented);
    // Gemini CLI was still set up.
    assert!(
        json_file(&s.home.join(".gemini/settings.json"))["mcpServers"]["ocean-graft"].is_object()
    );
}

#[test]
fn opencode_configured_in_jsonc_is_not_rewritten() {
    let s = Setup::new();
    let dir = s.install(".config/opencode");
    fs::write(dir.join("opencode.jsonc"), "{ // mine\n}\n").unwrap();
    let out = s.run(&["opencode"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("opencode.jsonc"), "{}", stdout(&out));
    assert!(!dir.join("opencode.json").exists());
}

#[test]
fn dry_run_changes_nothing() {
    let s = Setup::new();
    s.install(".codex");
    let out = s.run(&["codex", "--dry-run"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("would add"), "{text}");
    assert!(!s.home.join(".codex/config.toml").exists());
    assert!(!s.repo.join("AGENTS.md").exists());
}

#[test]
fn global_writes_each_agents_own_instructions_file() {
    let s = Setup::new();
    let elsewhere = s.home.join("not-a-repo");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = s.run_in(&elsewhere, &["codex", "gemini", "cursor", "--global"]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(read(s.home.join(".codex/AGENTS.md")).contains("<!-- ocean-graft:start"));
    assert!(read(s.home.join(".gemini/GEMINI.md")).contains("<!-- ocean-graft:start"));
    assert!(stdout(&out).contains("Cursor has no global instructions file"));
    assert!(!s.repo.join("AGENTS.md").exists());
}

#[test]
fn outside_a_repository_only_the_server_is_registered() {
    let s = Setup::new();
    let elsewhere = s.home.join("not-a-repo");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = s.run_in(&elsewhere, &["codex"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("not in a git repository"),
        "{}",
        stdout(&out)
    );
    assert!(s.home.join(".codex/config.toml").exists());
    assert!(!elsewhere.join("AGENTS.md").exists());
}

#[test]
fn a_linked_dotfile_is_written_through_and_stays_a_link() {
    let s = Setup::new();
    let dotfiles = s.home.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    fs::write(dotfiles.join("config.toml"), "model = \"o4\"\n").unwrap();
    s.install(".codex");
    std::os::unix::fs::symlink(
        dotfiles.join("config.toml"),
        s.home.join(".codex/config.toml"),
    )
    .unwrap();
    assert!(s.run(&["codex", "--no-instructions"]).status.success());
    let link = fs::symlink_metadata(s.home.join(".codex/config.toml")).unwrap();
    assert!(link.file_type().is_symlink());
    let text = read(dotfiles.join("config.toml"));
    assert!(text.starts_with("model = \"o4\"\n"));
    assert!(text.contains("[mcp_servers.ocean-graft]"));
    assert!(!s.repo.join("AGENTS.md").exists());
}

#[test]
fn a_server_started_outside_the_repository_works_with_project() {
    // What an agent other than Claude Code sees: no CLAUDE_PROJECT_DIR, and
    // the server's working directory is not the repository.
    let env = Env::new(6400);
    env.init("clone");
    let elsewhere = env.home();
    let mut mcp = env.mcp_in(&elsewhere);
    mcp.initialize();
    let repo = env.repo().to_str().unwrap().to_string();
    let (failed, result) = mcp.call(2, "fork", json!({"project": repo, "name": "a"}));
    assert!(!failed, "{result}");
    let (failed, result) = mcp.call(3, "status", json!({"project": repo}));
    assert!(!failed, "{result}");
    assert!(result.to_string().contains("\"a\""), "{result}");
    mcp.close();
}
