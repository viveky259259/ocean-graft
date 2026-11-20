//! ocean-graft for coding agents other than Claude Code: Codex, Cursor, Gemini CLI
//! and OpenCode.
//!
//! Everything ocean-graft does is reachable over its MCP server (`ocean-graft mcp`), which
//! needs nothing from Claude Code. Two things are Claude-specific: the plugin
//! that registers that server, and the skill that tells the agent when to use
//! it. `ocean-graft setup` does the first for each agent, in the agent's own config
//! file, and writes the second as a section of the instructions file the
//! agent reads (AGENTS.md, or GEMINI.md for Gemini CLI). The instructions are
//! derived from the plugin's SKILL.md when ocean-graft runs, so the two cannot drift.
//!
//! Setup only adds: an agent that already has a `ocean-graft` server keeps it as it
//! is, the rest of every config file keeps its text and key order, and the
//! instructions live between two markers that a later run replaces.

use crate::git;
use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde_json::value::RawValue;
use serde_json::{json, Value};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const SKILL: &str = include_str!("../plugin/skills/ocean-graft/SKILL.md");

/// The skill's one sentence about Claude Code's tool names, and what other
/// agents are told instead.
const CLAUDE_TOOLS: &str = "The tools are `mcp__plugin_ocean-graft_ocean-graft__<tool>`.";
const NEUTRAL_TOOLS: &str = "The tools come from the `ocean-graft` MCP server, which may not have
started in your repository: pass `project` (the repository's path) with every
call that names no `session` or `path`. Where the tools are not available,
each runs from a shell as `ocean-graft api <tool> '<JSON arguments>'` and prints the
same JSON result.";
/// A rule about Claude Code's worktree-isolated subagents, which no other
/// agent has.
const CLAUDE_ONLY_RULE: &str = "- Subagents started with worktree isolation";

const START: &str =
    "<!-- ocean-graft:start (written by `ocean-graft setup`; run it again to update) -->";
const START_PREFIX: &str = "<!-- ocean-graft:start";
const END: &str = "<!-- ocean-graft:end -->";

/// ocean-graft's instructions for an agent other than Claude Code: the plugin's
/// skill without its front matter and Claude-only parts.
pub fn instructions() -> String {
    let body = strip_front_matter(SKILL);
    let mut out = String::new();
    let mut skipping = false;
    for line in body.lines() {
        if line.starts_with(CLAUDE_ONLY_RULE) {
            skipping = true;
            continue;
        }
        // A skipped bullet's continuation lines are indented.
        if skipping && line.starts_with("  ") {
            continue;
        }
        skipping = false;
        out.push_str(&line.replace(CLAUDE_TOOLS, NEUTRAL_TOOLS));
        out.push('\n');
    }
    format!("{}\n", out.trim())
}

fn strip_front_matter(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return &rest[end + 5..];
        }
    }
    text
}

/// The instructions as a section of someone else's file: headings one level
/// down, so they sit under the file's own title, between the markers.
fn section() -> String {
    let mut out = format!("{START}\n");
    let mut fenced = false;
    for line in instructions().lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        }
        if !fenced && line.starts_with('#') {
            out.push('#');
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(END);
    out.push('\n');
    out
}

/// `existing` with ocean-graft's section added, or replaced where it already is.
fn upsert_section(existing: &str, section: &str) -> Result<String> {
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    let start = lines.iter().position(|l| l.starts_with(START_PREFIX));
    let end = lines.iter().position(|l| l.trim_end() == END);
    match (start, end) {
        (Some(s), Some(e)) if s < e => {
            let mut out: String = lines[..s].concat();
            out.push_str(section);
            out.push_str(&lines[e + 1..].concat());
            Ok(out)
        }
        (None, None) => {
            let mut out = existing.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(section);
            Ok(out)
        }
        _ => bail!("its ocean-graft section has lost a marker ({START_PREFIX} … {END}); mend or remove it, then run setup again"),
    }
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum AgentArg {
    Codex,
    Cursor,
    Gemini,
    Opencode,
    /// Every one of the above that is installed here
    All,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Agent {
    Codex,
    Cursor,
    Gemini,
    Opencode,
}

const AGENTS: [Agent; 4] = [Agent::Codex, Agent::Cursor, Agent::Gemini, Agent::Opencode];

struct Places {
    /// The agent's own directory: its presence means the agent is installed.
    home: PathBuf,
    /// Where its MCP servers are configured.
    config: PathBuf,
    /// Its instructions file in a repository.
    repo_file: &'static str,
    /// Its global instructions file, where it has one.
    global_file: Option<PathBuf>,
}

impl Agent {
    fn name(self) -> &'static str {
        match self {
            Agent::Codex => "codex",
            Agent::Cursor => "cursor",
            Agent::Gemini => "gemini",
            Agent::Opencode => "opencode",
        }
    }

    fn places(self, home: &Path) -> Places {
        match self {
            Agent::Codex => {
                let dir = env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
                Places {
                    config: dir.join("config.toml"),
                    global_file: Some(dir.join("AGENTS.md")),
                    home: dir,
                    repo_file: "AGENTS.md",
                }
            }
            Agent::Cursor => {
                let dir = home.join(".cursor");
                Places {
                    config: dir.join("mcp.json"),
                    global_file: None,
                    home: dir,
                    repo_file: "AGENTS.md",
                }
            }
            Agent::Gemini => {
                let dir = home.join(".gemini");
                Places {
                    config: dir.join("settings.json"),
                    global_file: Some(dir.join("GEMINI.md")),
                    home: dir,
                    repo_file: "GEMINI.md",
                }
            }
            Agent::Opencode => {
                let dir = env_dir("XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config"))
                    .join("opencode");
                Places {
                    config: dir.join("opencode.json"),
                    global_file: Some(dir.join("AGENTS.md")),
                    home: dir,
                    repo_file: "AGENTS.md",
                }
            }
        }
    }
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[derive(clap::Args)]
pub struct SetupArgs {
    /// Agents to set up
    #[arg(required = true, value_enum)]
    agents: Vec<AgentArg>,
    /// Write the instructions to each agent's own global file (Codex, Gemini
    /// CLI, OpenCode) instead of this repository
    #[arg(long)]
    global: bool,
    /// Register the MCP server only; write no instructions
    #[arg(long, conflicts_with = "global")]
    no_instructions: bool,
    /// Show what would change, and change nothing
    #[arg(long)]
    dry_run: bool,
    /// The ocean-graft binary the agents should start (default: this one)
    #[arg(long, value_name = "PATH")]
    command: Option<PathBuf>,
}

/// What happened to one file.
enum Outcome {
    Added,
    Updated,
    Unchanged,
}

impl Outcome {
    fn word(&self, dry_run: bool) -> &'static str {
        match (self, dry_run) {
            (Outcome::Added, false) => "added",
            (Outcome::Added, true) => "would add",
            (Outcome::Updated, false) => "updated",
            (Outcome::Updated, true) => "would update",
            (Outcome::Unchanged, _) => "already there",
        }
    }
}

pub fn setup(args: SetupArgs) -> Result<i32> {
    let home = env_dir("HOME").context("HOME is not set")?;
    let command = match &args.command {
        Some(c) => c
            .canonicalize()
            .with_context(|| format!("{} does not exist", c.display()))?,
        None => std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("could not find this ocean-graft binary; pass --command")?,
    };
    let command = command
        .to_str()
        .context("the ocean-graft binary's path is not valid UTF-8")?
        .to_string();

    let all = args.agents.contains(&AgentArg::All);
    let mut chosen = Vec::new();
    let mut lines = Vec::new();
    for agent in AGENTS {
        let named = args.agents.iter().any(|a| a.name_matches(agent));
        if !(named || all) {
            continue;
        }
        if !named && !agent.places(&home).home.is_dir() {
            lines.push(format!(
                "{:<9} not installed here (no {}); name it to set it up anyway",
                agent.name(),
                agent.places(&home).home.display()
            ));
            continue;
        }
        chosen.push(agent);
    }
    if chosen.is_empty() {
        for line in &lines {
            println!("{line}");
        }
        bail!("none of codex, cursor, gemini or opencode is installed here; name the agent to set it up anyway");
    }

    let repo = if args.no_instructions || args.global {
        None
    } else {
        let cwd = std::env::current_dir()?;
        git::toplevel(&cwd).ok()
    };

    let mut failed = false;
    let mut written_instructions: Vec<PathBuf> = Vec::new();
    for agent in chosen {
        let places = agent.places(&home);
        let name = agent.name();
        match register(agent, &places.config, &command, args.dry_run) {
            Ok(outcome) => lines.push(format!(
                "{name:<9} MCP server    {:<13} {}",
                outcome.word(args.dry_run),
                places.config.display()
            )),
            Err(err) => {
                failed = true;
                lines.push(format!("{name:<9} MCP server    not changed:  {err:#}"));
                lines.push(format!(
                    "{:<9} add this by hand: {}",
                    "",
                    manual_entry(agent, &command)
                ));
            }
        }
        if args.no_instructions {
            continue;
        }
        let target = if args.global {
            match &places.global_file {
                Some(file) => file.clone(),
                None => {
                    lines.push(format!(
                        "{name:<9} instructions  none written: Cursor has no global instructions file; run setup in a repository to add AGENTS.md there"
                    ));
                    continue;
                }
            }
        } else {
            match &repo {
                Some(root) => root.join(places.repo_file),
                None => {
                    lines.push(format!(
                        "{name:<9} instructions  none written: not in a git repository; run setup inside one, or pass --global"
                    ));
                    continue;
                }
            }
        };
        // Several agents read the same AGENTS.md: write it once.
        if written_instructions.contains(&target) {
            lines.push(format!(
                "{name:<9} instructions  {:<13} {}",
                "same file",
                target.display()
            ));
            continue;
        }
        written_instructions.push(target.clone());
        match write_instructions(&target, args.dry_run) {
            Ok(outcome) => lines.push(format!(
                "{name:<9} instructions  {:<13} {}",
                outcome.word(args.dry_run),
                target.display()
            )),
            Err(err) => {
                failed = true;
                lines.push(format!("{name:<9} instructions  not changed:  {err:#}"));
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    if !args.dry_run && !failed {
        println!("Restart the agents you set up so they start ocean-graft's MCP server.");
    }
    Ok(if failed { 1 } else { 0 })
}

impl AgentArg {
    fn name_matches(self, agent: Agent) -> bool {
        matches!(
            (self, agent),
            (AgentArg::Codex, Agent::Codex)
                | (AgentArg::Cursor, Agent::Cursor)
                | (AgentArg::Gemini, Agent::Gemini)
                | (AgentArg::Opencode, Agent::Opencode)
        )
    }
}

fn write_instructions(path: &Path, dry_run: bool) -> Result<Outcome> {
    let existing = read_if_exists(path)?;
    let updated = upsert_section(existing.as_deref().unwrap_or(""), &section())?;
    let outcome = match &existing {
        None => Outcome::Added,
        Some(text) if *text == updated => return Ok(Outcome::Unchanged),
        Some(text) if text.contains(START_PREFIX) => Outcome::Updated,
        Some(_) => Outcome::Added,
    };
    if !dry_run {
        write_file(path, &updated)?;
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// MCP server entries
// ---------------------------------------------------------------------------

/// The server entry each agent's config takes.
fn entry(agent: Agent, command: &str) -> Value {
    serde_json::to_value(entry_fields(agent, command)).expect("plain JSON")
}

/// The entry's fields in the order people write them (`json!` would sort them).
fn entry_fields(agent: Agent, command: &str) -> IndexMap<&'static str, Value> {
    let mut fields = IndexMap::new();
    match agent {
        Agent::Opencode => {
            fields.insert("type", json!("local"));
            fields.insert("command", json!([command, "mcp"]));
            fields.insert("enabled", json!(true));
        }
        _ => {
            fields.insert("command", json!(command));
            fields.insert("args", json!(["mcp"]));
        }
    }
    fields
}

fn manual_entry(agent: Agent, command: &str) -> String {
    match agent {
        Agent::Codex => format!(
            "[mcp_servers.ocean-graft] command = {} args = [\"mcp\"]",
            toml_string(command)
        ),
        Agent::Opencode => format!(
            "\"mcp\": {{\"ocean-graft\": {}}}",
            serde_json::to_string(&entry_fields(agent, command)).expect("plain JSON")
        ),
        _ => format!(
            "\"mcpServers\": {{\"ocean-graft\": {}}}",
            serde_json::to_string(&entry_fields(agent, command)).expect("plain JSON")
        ),
    }
}

fn register(agent: Agent, config: &Path, command: &str, dry_run: bool) -> Result<Outcome> {
    if agent == Agent::Opencode && !config.exists() {
        let jsonc = config.with_extension("jsonc");
        if jsonc.exists() {
            bail!(
                "OpenCode is configured in {}, which may hold comments ocean-graft will not rewrite",
                jsonc.display()
            );
        }
    }
    let existing = read_if_exists(config)?;
    let updated = match agent {
        Agent::Codex => codex_config(existing.as_deref(), command)?,
        Agent::Opencode => json_config(existing.as_deref(), "mcp", agent, command, true)?,
        _ => json_config(existing.as_deref(), "mcpServers", agent, command, false)?,
    };
    let Some(updated) = updated else {
        return Ok(Outcome::Unchanged);
    };
    if !dry_run {
        write_file(config, &updated)?;
    }
    Ok(if existing.is_some() {
        Outcome::Updated
    } else {
        Outcome::Added
    })
}

fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Codex's config.toml with a `[mcp_servers.ocean-graft]` table appended, or None
/// when it already has one. Appended as text, so the file's comments and
/// layout stay as they are.
fn codex_config(existing: Option<&str>, command: &str) -> Result<Option<String>> {
    let text = existing.unwrap_or("");
    let parsed: toml::Table = text
        .parse()
        .context("it is not valid TOML, so ocean-graft will not edit it")?;
    if parsed
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .is_some_and(|t| t.contains_key("ocean-graft"))
    {
        return Ok(None);
    }
    let mut out = text.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "# ocean-graft's tools over MCP (added by `ocean-graft setup`)\n[mcp_servers.ocean-graft]\ncommand = {}\nargs = [\"mcp\"]\n",
        toml_string(command)
    ));
    // What was appended must leave the file valid and say what was meant.
    let check: toml::Table = out
        .parse()
        .context("the edited config would not be valid TOML")?;
    if check
        .get("mcp_servers")
        .and_then(|v| v.get("ocean-graft"))
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
        != Some(command)
    {
        bail!("the edited config would not name ocean-graft's server; it may define mcp_servers in a way ocean-graft does not edit");
    }
    Ok(Some(out))
}

type RawObject = IndexMap<String, Box<RawValue>>;

/// A JSON config with `servers_key.ocean-graft` added, or None when it is already
/// there. Only the top level and the servers object are rebuilt; every other
/// value is copied as its original text, in its original order.
fn json_config(
    existing: Option<&str>,
    servers_key: &str,
    agent: Agent,
    command: &str,
    with_opencode_schema: bool,
) -> Result<Option<String>> {
    let entry = &entry(agent, command);
    let text = existing.unwrap_or("").trim();
    let mut top: RawObject = if text.is_empty() {
        IndexMap::new()
    } else {
        serde_json::from_str(text)
            .context("it is not a plain JSON object (comments or trailing commas?), so ocean-graft will not edit it")?
    };
    let mut servers: RawObject = match top.get(servers_key) {
        Some(raw) => serde_json::from_str(raw.get())
            .with_context(|| format!("its {servers_key:?} is not an object"))?,
        None => IndexMap::new(),
    };
    if servers.contains_key("ocean-graft") {
        return Ok(None);
    }
    if top.is_empty() && with_opencode_schema {
        top.insert(
            "$schema".into(),
            RawValue::from_string("\"https://opencode.ai/config.json\"".into())?,
        );
    }
    servers.insert(
        "ocean-graft".into(),
        RawValue::from_string(indent(
            &serde_json::to_string_pretty(&entry_fields(agent, command))?,
            4,
        ))?,
    );
    let servers_text = object_text(&servers, 2);
    top.insert(servers_key.into(), RawValue::from_string(servers_text)?);
    let out = format!("{}\n", object_text(&top, 0));
    // The result must parse, keep every key it had, and hold the entry.
    let check: Value =
        serde_json::from_str(&out).context("the edited config would not be valid JSON")?;
    if check.get(servers_key).and_then(|s| s.get("ocean-graft")) != Some(entry) {
        bail!("the edited config would not hold ocean-graft's server");
    }
    Ok(Some(out))
}

/// An object's text with one key per line at `depth` spaces; values keep
/// their own text.
fn object_text(object: &RawObject, depth: usize) -> String {
    if object.is_empty() {
        return "{}".into();
    }
    let pad = " ".repeat(depth + 2);
    let items: Vec<String> = object
        .iter()
        .map(|(k, v)| format!("{pad}{}: {}", serde_json::to_string(k).unwrap(), v.get()))
        .collect();
    format!("{{\n{}\n{}}}", items.join(",\n"), " ".repeat(depth))
}

/// Every line after the first moved right by `n` spaces.
fn indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                l.to_string()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn read_if_exists(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        Ok(_) => {
            // A link (a dotfiles checkout, say) is read and written through.
            let meta = fs::metadata(path)
                .with_context(|| format!("{} is a broken link", path.display()))?;
            if !meta.is_file() {
                bail!("{} is not a regular file", path.display());
            }
            fs::read_to_string(path)
                .map(Some)
                .with_context(|| format!("cannot read {}", path.display()))
        }
    }
}

/// Replace a file's contents in one step: a temporary file beside it, then a
/// rename, keeping its permissions. Through a link, the link's target is
/// replaced and the link kept.
fn write_file(path: &Path, contents: &str) -> Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => path
            .canonicalize()
            .with_context(|| format!("{} is a broken link", path.display()))?,
        _ => path.to_path_buf(),
    };
    let dir = target.parent().context("no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let name = target
        .file_name()
        .context("no file name")?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.ocean-graft-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot write {}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_drop_claude_only_parts() {
        // The anchors must exist in SKILL.md, or the replacements silently stop.
        assert!(
            SKILL.contains(CLAUDE_TOOLS),
            "SKILL.md no longer names Claude's tools as expected"
        );
        assert!(SKILL.lines().any(|l| l.starts_with(CLAUDE_ONLY_RULE)));
        let text = instructions();
        assert!(!text.contains("mcp__plugin"), "{text}");
        assert!(!text.contains("worktree isolation"), "{text}");
        assert!(!text.starts_with("---"));
        assert!(!text.contains("name: ocean-graft"));
        assert!(text.contains("ocean-graft api <tool>"));
        assert!(text.contains("## Decide") && text.contains("## Rules"));
        // Everything else in the skill is still there.
        for line in strip_front_matter(SKILL).lines() {
            if line.contains(CLAUDE_TOOLS) || line.starts_with(CLAUDE_ONLY_RULE) {
                continue;
            }
            assert!(text.contains(line.trim()), "lost: {line}");
        }
    }

    #[test]
    fn the_section_is_added_once_and_replaced_in_place() {
        let s = section();
        assert!(s.starts_with(START) && s.trim_end().ends_with(END));
        assert!(s.contains("\n## Working with ocean-graft sessions") && s.contains("\n### Decide"));
        let base = "# Project\n\nOur rules.";
        let once = upsert_section(base, &s).unwrap();
        assert!(once.starts_with("# Project\n\nOur rules.\n\n<!-- ocean-graft:start"));
        assert_eq!(upsert_section(&once, &s).unwrap(), once);
        let stale = once.replace("### Decide", "### Old heading");
        assert_eq!(upsert_section(&stale, &s).unwrap(), once);
        let after = format!("{once}\n## Later\n");
        assert_eq!(upsert_section(&after, &s).unwrap(), after);
        let broken = once.replace(END, "");
        assert!(upsert_section(&broken, &s).is_err());
        assert_eq!(upsert_section("", &s).unwrap(), s);
    }

    #[test]
    fn codex_gets_a_table_appended_and_keeps_its_text() {
        let base = "# mine\nmodel = \"o4\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let out = codex_config(Some(base), "/opt/sh\"oal").unwrap().unwrap();
        assert!(out.starts_with(base));
        let t: toml::Table = out.parse().unwrap();
        assert_eq!(
            t["mcp_servers"]["ocean-graft"]["command"].as_str(),
            Some("/opt/sh\"oal")
        );
        assert_eq!(t["mcp_servers"]["other"]["command"].as_str(), Some("x"));
        assert!(codex_config(Some(&out), "/elsewhere").unwrap().is_none());
        let inline = "mcp_servers = { ocean-graft = { command = \"y\" } }\n";
        assert!(codex_config(Some(inline), "/z").unwrap().is_none());
        assert!(codex_config(Some("not [valid"), "/z").is_err());
        assert!(codex_config(None, "/z")
            .unwrap()
            .unwrap()
            .starts_with("# ocean-graft"));
    }

    #[test]
    fn json_configs_keep_their_keys_order_and_text() {
        let base = "{\n  \"zeta\": 1,\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\"}\n  },\n  \"alpha\": [1,2]\n}\n";
        let e = entry(Agent::Cursor, "/bin/ocean-graft");
        let out = json_config(
            Some(base),
            "mcpServers",
            Agent::Cursor,
            "/bin/ocean-graft",
            false,
        )
        .unwrap()
        .unwrap();
        let zeta = out.find("\"zeta\"").unwrap();
        let servers = out.find("\"mcpServers\"").unwrap();
        let alpha = out.find("\"alpha\"").unwrap();
        assert!(zeta < servers && servers < alpha, "{out}");
        assert!(out.contains("\"other\": {\"command\": \"x\"}"), "{out}");
        assert!(out.contains("\"alpha\": [1,2]"), "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["ocean-graft"], e);
        assert!(
            out.find("\"command\"").unwrap() < out.rfind("\"args\"").unwrap(),
            "{out}"
        );
        let cursor = |text: &str| json_config(Some(text), "mcpServers", Agent::Cursor, "/x", false);
        assert!(cursor(&out).unwrap().is_none());
        assert!(cursor("{// c\n}").is_err());
        assert!(cursor("[1]").is_err());
        assert!(cursor("{\"mcpServers\": 3}").is_err());
        let fresh = json_config(None, "mcp", Agent::Opencode, "/b", true)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&fresh).unwrap();
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["mcp"]["ocean-graft"]["command"], json!(["/b", "mcp"]));
    }
}
