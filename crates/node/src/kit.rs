//! The kit on a node: report what this machine's Claude Code has (the import), and install the
//! orchestrator's kit for the owner's own Claude Code and the agents (both run as this user).
//!
//! Where things are, as Claude Code itself decides it (checked against the real CLI):
//!
//! - the config dir is `CLAUDE_CONFIG_DIR` when set, else `~/.claude`; skills are its
//!   `skills/<name>/` directories, and plugins are recorded in its `plugins/installed_plugins.json`
//!   (by `plugin@marketplace`, with a scope) and `plugins/known_marketplaces.json` (sources);
//! - user-scope MCP servers are the top-level `mcpServers` of `.claude.json`, which is
//!   `$CLAUDE_CONFIG_DIR/.claude.json` when that is set, else `~/.claude.json`. Nothing else in
//!   that file is read: it also holds the account's tokens.
//!
//! The node never touches what it did not install. A skill it writes carries a marker
//! ([`SKILL_MARKER`]) with the content's hash; plugins and MCP servers it added are listed in
//! `kit-state.json` in the node's data dir. An item of the same name the owner has on the
//! machine is a `conflict` and is left alone (or `installed`, when it is identical), and only
//! what the kit installed is ever removed. Plugins and MCP servers are changed with the `claude`
//! CLI (`claude plugin install`, `claude mcp add-json --scope user`), which keeps its own files
//! consistent; every call has a timeout, and a failure is a status, never a crash.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use fleet_proto::{
    KitContent, KitFile, KitItemStatus, KitMcp, KitPlugin, KitSkill, KitSkipped,
    kit::{
        MAX_SKILL_BYTES, MAX_SKILL_FILE_BYTES, MAX_SKILL_FILES, SKILL_MARKER, safe_skill_path,
        secret_file, valid_name,
    },
    redact::redact_secrets,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How long one `claude` CLI call may take. Adding a marketplace clones a repository, which the
/// CLI itself gives two minutes.
const CLI_TIMEOUT: Duration = Duration::from_secs(180);
/// Directories inside a skill that are tooling, not the skill.
const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "__pycache__"];

/// Where this user's Claude Code keeps its configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct KitPaths {
    pub config_dir: PathBuf,
    /// The file holding user-scope MCP servers.
    pub claude_json: PathBuf,
    /// Set `CLAUDE_CONFIG_DIR` for the CLI and the agents: the node was pointed at a config dir
    /// other than its environment's.
    pub set_env: bool,
}

impl KitPaths {
    /// As Claude Code finds them from this process's environment.
    pub fn from_env() -> Option<Self> {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
            let dir = PathBuf::from(dir);
            return Some(Self {
                claude_json: dir.join(".claude.json"),
                config_dir: dir,
                set_env: false,
            });
        }
        let home = if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            std::env::var_os("HOME")
        };
        let home = PathBuf::from(home.filter(|h| !h.is_empty())?);
        Some(Self {
            config_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            set_env: false,
        })
    }

    /// A given config dir, as if `CLAUDE_CONFIG_DIR` pointed there.
    pub fn at(config_dir: PathBuf) -> Self {
        Self {
            claude_json: config_dir.join(".claude.json"),
            config_dir,
            set_env: true,
        }
    }

    fn skills(&self) -> PathBuf {
        self.config_dir.join("skills")
    }

    fn plugins(&self) -> PathBuf {
        self.config_dir.join("plugins")
    }
}

// ---------------------------------------------------------------- capture

/// What this machine has at user scope, and what was left out.
pub fn capture(paths: &KitPaths) -> (KitContent, Vec<KitSkipped>) {
    let mut skipped = Vec::new();
    let skills = capture_skills(&paths.skills(), &mut skipped);
    let plugins = capture_plugins(&paths.plugins(), &mut skipped);
    let mcp = capture_mcp(&paths.claude_json, &mut skipped);
    (
        KitContent {
            skills,
            plugins,
            mcp,
        },
        skipped,
    )
}

fn skip(out: &mut Vec<KitSkipped>, kind: &str, name: &str, reason: impl Into<String>) {
    out.push(KitSkipped {
        kind: kind.into(),
        name: name.into(),
        reason: reason.into(),
    });
}

fn capture_skills(dir: &Path, skipped: &mut Vec<KitSkipped>) -> Vec<KitSkill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut names: Vec<(String, PathBuf)> = entries
        .flatten()
        // `is_dir` follows a link: a skill linked in from elsewhere is still the owner's skill.
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    names.sort();
    for (name, path) in names {
        if name.starts_with('.') {
            continue;
        }
        if !valid_name(&name) {
            skip(
                skipped,
                "skill",
                &name,
                "the name has characters the kit does not take",
            );
            continue;
        }
        match read_skill(&path, &name, skipped) {
            Ok(s) => out.push(s),
            Err(reason) => skip(skipped, "skill", &name, reason),
        }
    }
    out
}

/// A skill directory's text files. Files the kit cannot carry (too large, binary, an unusable
/// path) are reported in `skipped` and left out; a skill that is too large as a whole, or has no
/// `SKILL.md`, is refused with the reason.
fn read_skill(
    dir: &Path,
    name: &str,
    skipped: &mut Vec<KitSkipped>,
) -> std::result::Result<KitSkill, String> {
    let mut files = Vec::new();
    let mut total = 0usize;
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&at).map_err(|e| format!("cannot read it: {e}"))?;
        for e in entries.flatten() {
            let file_name = e.file_name().to_string_lossy().into_owned();
            let rel = format!("{prefix}{file_name}");
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_symlink() {
                skip(
                    skipped,
                    "skill",
                    &format!("{name}/{rel}"),
                    "a link (not followed)",
                );
                continue;
            }
            if meta.is_dir() {
                if SKIP_DIRS.contains(&file_name.as_str()) {
                    skip(
                        skipped,
                        "skill",
                        &format!("{name}/{rel}"),
                        "tooling directory",
                    );
                } else {
                    stack.push((e.path(), format!("{rel}/")));
                }
                continue;
            }
            if rel == SKILL_MARKER {
                continue;
            }
            if secret_file(&rel) {
                skip(
                    skipped,
                    "skill",
                    &format!("{name}/{rel}"),
                    "looks like a secret file",
                );
                continue;
            }
            if !safe_skill_path(&rel) {
                skip(
                    skipped,
                    "skill",
                    &format!("{name}/{rel}"),
                    "unusable file name",
                );
                continue;
            }
            if meta.len() as usize > MAX_SKILL_FILE_BYTES {
                skip(
                    skipped,
                    "skill",
                    &format!("{name}/{rel}"),
                    format!("larger than {} KB", MAX_SKILL_FILE_BYTES / 1024),
                );
                continue;
            }
            let bytes = std::fs::read(e.path()).map_err(|e| format!("cannot read {rel}: {e}"))?;
            let text = match String::from_utf8(bytes) {
                Ok(t) if !t.contains('\0') => t,
                _ => {
                    skip(skipped, "skill", &format!("{name}/{rel}"), "binary file");
                    continue;
                }
            };
            total += text.len();
            files.push(KitFile { path: rel, text });
            if files.len() > MAX_SKILL_FILES {
                return Err(format!("more than {MAX_SKILL_FILES} files"));
            }
            if total > MAX_SKILL_BYTES {
                return Err(format!(
                    "larger than {} MB",
                    MAX_SKILL_BYTES / (1024 * 1024)
                ));
            }
        }
    }
    if !files.iter().any(|f| f.path == "SKILL.md") {
        return Err("no SKILL.md".into());
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(KitSkill {
        name: name.to_owned(),
        files,
    })
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Plugin ids (`plugin@marketplace`) installed at user scope.
fn user_plugins(plugins_dir: &Path) -> BTreeSet<String> {
    let Some(v) = read_json(&plugins_dir.join("installed_plugins.json")) else {
        return BTreeSet::new();
    };
    v["plugins"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, installs)| {
            installs
                .as_array()
                .is_some_and(|a| a.iter().any(|i| i["scope"] == "user"))
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Known marketplaces: name -> what `claude plugin marketplace add` takes to add it again.
fn marketplaces(plugins_dir: &Path) -> BTreeMap<String, Option<String>> {
    let Some(v) = read_json(&plugins_dir.join("known_marketplaces.json")) else {
        return BTreeMap::new();
    };
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(name, m)| {
            let s = &m["source"];
            let source = match s["source"].as_str() {
                Some("github") => s["repo"].as_str(),
                Some("git" | "url") => s["url"].as_str(),
                Some("directory" | "file") => s["path"].as_str(),
                _ => None,
            };
            (name.clone(), source.map(str::to_owned))
        })
        .collect()
}

fn capture_plugins(dir: &Path, skipped: &mut Vec<KitSkipped>) -> Vec<KitPlugin> {
    let known = marketplaces(dir);
    let mut out = Vec::new();
    for id in user_plugins(dir) {
        let Some((plugin, marketplace)) = id.rsplit_once('@') else {
            skip(skipped, "plugin", &id, "not a plugin@marketplace id");
            continue;
        };
        if !valid_name(plugin) || !valid_name(marketplace) {
            skip(
                skipped,
                "plugin",
                &id,
                "the name has characters the kit does not take",
            );
            continue;
        }
        match known.get(marketplace) {
            Some(Some(source)) => out.push(KitPlugin {
                plugin: plugin.into(),
                marketplace: marketplace.into(),
                marketplace_source: source.clone(),
            }),
            Some(None) => skip(
                skipped,
                "plugin",
                &id,
                "its marketplace's source is of a kind the kit cannot add",
            ),
            None => skip(
                skipped,
                "plugin",
                &id,
                "its marketplace is not known on this machine",
            ),
        }
    }
    out
}

/// User-scope MCP servers: `mcpServers` of `.claude.json`, and nothing else from that file.
fn user_mcp(claude_json: &Path) -> serde_json::Map<String, Value> {
    read_json(claude_json)
        .and_then(|mut v| match v.get_mut("mcpServers").map(Value::take) {
            Some(Value::Object(m)) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn capture_mcp(claude_json: &Path, skipped: &mut Vec<KitSkipped>) -> Vec<KitMcp> {
    let mut out = Vec::new();
    for (name, config) in user_mcp(claude_json) {
        if !valid_name(&name) {
            skip(
                skipped,
                "mcp",
                &name,
                "the name has characters the kit does not take",
            );
            continue;
        }
        if !config.is_object() {
            skip(skipped, "mcp", &name, "its config is not an object");
            continue;
        }
        out.push(KitMcp {
            name,
            config: config.to_string(),
        });
    }
    out
}

// ---------------------------------------------------------------- install

/// What the kit installed on this machine, so that only that is ever removed.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Skill name -> content hash. The marker in the directory is what counts; this is the list
    /// of names to look at when the kit drops one.
    #[serde(default)]
    skills: BTreeMap<String, String>,
    /// `plugin@marketplace` ids the kit installed.
    #[serde(default)]
    plugins: BTreeSet<String>,
    /// MCP server name -> hash of the config the kit added.
    #[serde(default)]
    mcp: BTreeMap<String, String>,
}

impl State {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("kit-state.json")
    }

    fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(Self::path(data_dir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn hex_sha256(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    data_encoding::HEXLOWER.encode(&h.finalize())
}

/// A skill's content hash: its files by path, so the order they arrived in does not matter.
pub fn skill_hash(files: &[KitFile]) -> String {
    let mut sorted: Vec<&KitFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let parts: Vec<&[u8]> = sorted
        .iter()
        .flat_map(|f| [f.path.as_bytes(), f.text.as_bytes()])
        .collect();
    hex_sha256(&parts)
}

fn status(kind: &str, name: &str, state: &str, detail: impl Into<String>) -> KitItemStatus {
    KitItemStatus {
        kind: kind.into(),
        name: name.into(),
        state: state.into(),
        detail: detail.into(),
    }
}

/// What to do with a skill whose directory may already exist.
#[derive(Debug, PartialEq)]
pub enum SkillPlan {
    /// Absent, or the kit's own older copy, untouched since: write it.
    Write,
    /// The kit's copy, already this content.
    UpToDate,
    /// The owner's own directory with exactly this content: nothing to do, and the kit does not
    /// take it over (so dropping it from the kit never deletes it).
    OwnIdentical,
    /// The owner's own directory, different: left alone.
    Conflict,
    /// The kit's copy, changed on this machine since the kit wrote it: the owner's now, left
    /// alone.
    Edited,
}

/// Who a skill directory belongs to, as it stands on disk.
#[derive(Debug, PartialEq)]
enum Found {
    Absent,
    /// The kit wrote it (the marker names this directory) and it is still exactly that.
    Kit(String),
    /// The kit wrote it, and it has been changed since.
    KitEdited,
    /// The owner's, with its content hash when the kit could read all of it.
    Own(Option<String>),
}

/// The marker's text: the skill's name and the content's hash. The name is in it so a copy of
/// a kit skill under another name (`cp -r a b`) is never taken for the kit's own.
fn marker_text(name: &str, hash: &str) -> String {
    format!("{name} {hash}")
}

fn inspect(dir: &Path, name: &str) -> Found {
    if !dir.exists() {
        return Found::Absent;
    }
    // Only what the kit itself could have written counts: a file it would have left out (a
    // binary, a secret, a link) means someone else put it there.
    let mut skipped = Vec::new();
    let content = read_skill(dir, name, &mut skipped)
        .ok()
        .filter(|_| skipped.is_empty())
        .map(|s| skill_hash(&s.files));
    let marker = std::fs::read_to_string(dir.join(SKILL_MARKER)).ok();
    match marker.as_deref().and_then(|m| m.trim().split_once(' ')) {
        Some((n, h)) if n == name => match content {
            Some(c) if c == h => Found::Kit(c),
            _ => Found::KitEdited,
        },
        _ => Found::Own(content),
    }
}

/// Decides for one skill: the marker says whether the directory is the kit's, and the files say
/// whether it still is exactly what the kit wrote.
pub fn plan_skill(dir: &Path, hash: &str, name: &str) -> SkillPlan {
    match inspect(dir, name) {
        Found::Absent => SkillPlan::Write,
        Found::Kit(h) if h == hash => SkillPlan::UpToDate,
        Found::Kit(_) => SkillPlan::Write,
        Found::KitEdited => SkillPlan::Edited,
        Found::Own(Some(h)) if h == hash => SkillPlan::OwnIdentical,
        Found::Own(_) => SkillPlan::Conflict,
    }
}

/// What to do with an MCP server of a name the machine may already have at user scope.
#[derive(Debug, PartialEq)]
pub enum McpPlan {
    Add,
    /// The kit's own, untouched since, with a different config: remove and add again.
    Replace,
    UpToDate,
    OwnIdentical,
    Conflict,
    /// The kit's own, changed on this machine since: the owner's now, left alone.
    Edited,
}

/// The hash a user-scope server's config is recorded by: of the JSON as Claude Code keeps it.
fn config_hash(config: &Value) -> String {
    hex_sha256(&[config.to_string().as_bytes()])
}

pub fn plan_mcp(current: Option<&Value>, managed_hash: Option<&str>, desired: &Value) -> McpPlan {
    match (current, managed_hash) {
        (None, _) => McpPlan::Add,
        (Some(cur), Some(m)) if config_hash(cur) != m => McpPlan::Edited,
        (Some(cur), Some(_)) if cur == desired => McpPlan::UpToDate,
        (Some(_), Some(_)) => McpPlan::Replace,
        (Some(cur), None) if cur == desired => McpPlan::OwnIdentical,
        (Some(_), None) => McpPlan::Conflict,
    }
}

/// Where the kit stages a skill's directory: beside `skills/`, on the same volume (so a rename
/// moves it in one step), and never where Claude Code looks for skills.
fn staging(paths: &KitPaths) -> PathBuf {
    paths.config_dir.join(".fleet-kit-staging")
}

/// Moves a skill directory out of `skills/` and deletes it there: a removal that fails half way
/// (a file held open on Windows) then never leaves a broken skill where Claude Code loads it.
fn take_away(paths: &KitPaths, dir: &Path, name: &str) -> Result<()> {
    let old = staging(paths).join(format!("old-{name}"));
    let _ = std::fs::remove_dir_all(&old);
    std::fs::create_dir_all(staging(paths))?;
    std::fs::rename(dir, &old).with_context(|| format!("moving {} aside", dir.display()))?;
    if let Err(e) = std::fs::remove_dir_all(&old) {
        tracing::warn!("could not delete {}: {e}", old.display());
    }
    Ok(())
}

/// Writes a skill's directory: staged first, then swapped in, so a failure half way never
/// leaves a broken skill where Claude Code would load it; an old copy goes back if the swap
/// fails.
fn write_skill(paths: &KitPaths, skill: &KitSkill, hash: &str) -> Result<()> {
    let tmp = staging(paths).join(format!("new-{}", skill.name));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    for f in &skill.files {
        let path = tmp.join(&f.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &f.text).with_context(|| format!("writing {}", f.path))?;
    }
    std::fs::write(tmp.join(SKILL_MARKER), marker_text(&skill.name, hash))?;
    let dir = paths.skills().join(&skill.name);
    let old = staging(paths).join(format!("old-{}", skill.name));
    let _ = std::fs::remove_dir_all(&old);
    let had_old = dir.exists();
    if had_old {
        std::fs::rename(&dir, &old).with_context(|| format!("moving {} aside", dir.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &dir) {
        if had_old {
            let _ = std::fs::rename(&old, &dir);
        }
        return Err(e).with_context(|| format!("putting {} in place", dir.display()));
    }
    if had_old && let Err(e) = std::fs::remove_dir_all(&old) {
        tracing::warn!("could not delete {}: {e}", old.display());
    }
    Ok(())
}

fn check_skill(s: &KitSkill) -> std::result::Result<(), String> {
    if !valid_name(&s.name) {
        return Err("the name is not usable as a directory".into());
    }
    if let Some(f) = s.files.iter().find(|f| !safe_skill_path(&f.path)) {
        return Err(format!("unsafe file path {:?}", f.path));
    }
    if let Some(f) = s.files.iter().find(|f| secret_file(&f.path)) {
        return Err(format!("{} looks like a secret file", f.path));
    }
    if !s.files.iter().any(|f| f.path == "SKILL.md") {
        return Err("no SKILL.md".into());
    }
    Ok(())
}

const EDITED: &str =
    "changed on this machine since the kit installed it; the kit leaves it to the owner";

fn apply_skills(
    paths: &KitPaths,
    state: &mut State,
    skills: &[KitSkill],
    out: &mut Vec<KitItemStatus>,
) {
    let dir = paths.skills();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        for s in skills {
            out.push(status(
                "skill",
                &s.name,
                "failed",
                format!("cannot create {}: {e}", dir.display()),
            ));
        }
        return;
    }
    for s in skills {
        if let Err(why) = check_skill(s) {
            out.push(status("skill", &s.name, "failed", why));
            continue;
        }
        let hash = skill_hash(&s.files);
        let target = dir.join(&s.name);
        let st = match plan_skill(&target, &hash, &s.name) {
            SkillPlan::UpToDate => {
                state.skills.insert(s.name.clone(), hash);
                status("skill", &s.name, "installed", "")
            }
            SkillPlan::Write => match write_skill(paths, s, &hash) {
                Ok(()) => {
                    state.skills.insert(s.name.clone(), hash);
                    status("skill", &s.name, "installed", "")
                }
                Err(e) => status("skill", &s.name, "failed", format!("{e:#}")),
            },
            SkillPlan::OwnIdentical => {
                state.skills.remove(&s.name);
                status(
                    "skill",
                    &s.name,
                    "installed",
                    "this machine's own copy is identical; the kit leaves it to the owner",
                )
            }
            SkillPlan::Conflict => {
                state.skills.remove(&s.name);
                status(
                    "skill",
                    &s.name,
                    "conflict",
                    format!(
                        "this machine has its own skill {} that differs; the kit leaves it alone",
                        s.name
                    ),
                )
            }
            SkillPlan::Edited => {
                state.skills.remove(&s.name);
                status("skill", &s.name, "conflict", EDITED)
            }
        };
        out.push(st);
    }
    // Gone from the kit: the kit's own directories go too. Any directory whose marker names it
    // counts, so a lost state file does not leave the kit's copies behind; one without it is
    // the owner's, and one changed since the kit wrote it has become the owner's.
    let wanted: BTreeSet<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    let mut gone: BTreeSet<String> = state.skills.keys().cloned().collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().join(SKILL_MARKER).is_file() && !name.starts_with('.') {
                gone.insert(name);
            }
        }
    }
    for name in gone.into_iter().filter(|n| !wanted.contains(n.as_str())) {
        let target = dir.join(&name);
        match inspect(&target, &name) {
            Found::Kit(_) => match take_away(paths, &target, &name) {
                Ok(()) => out.push(status("skill", &name, "removed", "")),
                Err(e) => {
                    out.push(status(
                        "skill",
                        &name,
                        "failed",
                        format!("could not remove it: {e:#}"),
                    ));
                    continue;
                }
            },
            Found::KitEdited => out.push(status("skill", &name, "conflict", EDITED)),
            Found::Absent | Found::Own(_) => {}
        }
        state.skills.remove(&name);
    }
}

/// Runs the `claude` CLI for kit changes.
struct Cli {
    program: PathBuf,
    /// The configured command's leading arguments up to its first long flag (a script for an
    /// interpreter, `-y @anthropic-ai/claude-code` for npx): flags such as `--model` belong to
    /// sessions, not to `claude plugin`.
    lead: Vec<String>,
    env: Option<PathBuf>,
    /// Values to scrub from whatever the CLI prints before it goes into a status.
    secrets: Vec<String>,
}

impl Cli {
    fn new(command: &[String], paths: &KitPaths, secrets: Vec<String>) -> Result<Self> {
        let (program, rest) = command
            .split_first()
            .context("the claude command is empty")?;
        let program = which::which(program).with_context(|| {
            format!("{program:?} not found; set the node's Claude command to a full path")
        })?;
        Ok(Self {
            program,
            lead: rest
                .iter()
                .take_while(|a| !a.starts_with("--"))
                .cloned()
                .collect(),
            env: paths.set_env.then(|| paths.config_dir.clone()),
            secrets,
        })
    }

    async fn run(&self, args: &[&str]) -> std::result::Result<String, String> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.lead)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.env {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let what = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let out = match tokio::time::timeout(CLI_TIMEOUT, cmd.output()).await {
            Err(_) => {
                return Err(format!(
                    "`claude {what}` took over {}s",
                    CLI_TIMEOUT.as_secs()
                ));
            }
            Ok(Err(e)) => return Err(format!("could not run `claude {what}`: {e}")),
            Ok(Ok(o)) => o,
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        redact_secrets(&mut text, &self.secrets);
        if out.status.success() {
            Ok(text)
        } else {
            let tail: String = text
                .trim()
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            Err(format!("`claude {what}` failed: {tail}"))
        }
    }
}

async fn apply_plugins(
    paths: &KitPaths,
    cli: &Cli,
    state: &mut State,
    plugins: &[KitPlugin],
    out: &mut Vec<KitItemStatus>,
) {
    let installed = user_plugins(&paths.plugins());
    let mut known = marketplaces(&paths.plugins());
    for p in plugins {
        let id = p.id();
        if !valid_name(&p.plugin)
            || !valid_name(&p.marketplace)
            || p.marketplace_source.starts_with('-')
        {
            out.push(status(
                "plugin",
                &id,
                "failed",
                "not a usable plugin, marketplace or source",
            ));
            continue;
        }
        if installed.contains(&id) {
            let detail = if state.plugins.contains(&id) {
                ""
            } else {
                "already installed on this machine; the kit leaves it to the owner"
            };
            out.push(status("plugin", &id, "installed", detail));
            continue;
        }
        if !known.contains_key(&p.marketplace) {
            if let Err(e) = cli
                .run(&["plugin", "marketplace", "add", &p.marketplace_source])
                .await
            {
                out.push(status("plugin", &id, "failed", e));
                continue;
            }
            known.insert(p.marketplace.clone(), Some(p.marketplace_source.clone()));
        }
        // No `-y`: a plugin that installs by running a command of its marketplace's is shown to a
        // person first, and fails here instead of running unseen.
        match cli
            .run(&["plugin", "install", &id, "--scope", "user"])
            .await
        {
            Ok(_) => {
                state.plugins.insert(id.clone());
                out.push(status("plugin", &id, "installed", ""));
            }
            Err(e) => out.push(status("plugin", &id, "failed", e)),
        }
    }
    // Marketplaces the kit added stay: they are only a list of what could be installed.
    let wanted: BTreeSet<String> = plugins.iter().map(KitPlugin::id).collect();
    let gone: Vec<String> = state.plugins.difference(&wanted).cloned().collect();
    for id in gone {
        if !installed.contains(&id) {
            state.plugins.remove(&id);
            continue;
        }
        match cli
            .run(&["plugin", "uninstall", &id, "--scope", "user"])
            .await
        {
            Ok(_) => {
                state.plugins.remove(&id);
                out.push(status("plugin", &id, "removed", ""));
            }
            Err(e) => out.push(status("plugin", &id, "failed", e)),
        }
    }
}

async fn apply_mcp(
    paths: &KitPaths,
    cli: &Cli,
    state: &mut State,
    servers: &[KitMcp],
    out: &mut Vec<KitItemStatus>,
) {
    let current = user_mcp(&paths.claude_json);
    for m in servers {
        if !valid_name(&m.name) {
            out.push(status("mcp", &m.name, "failed", "the name is not usable"));
            continue;
        }
        let desired: Value = match serde_json::from_str(&m.config) {
            Ok(v @ Value::Object(_)) => v,
            _ => {
                out.push(status(
                    "mcp",
                    &m.name,
                    "failed",
                    "its config is not a JSON object",
                ));
                continue;
            }
        };
        let json = desired.to_string();
        let plan = plan_mcp(
            current.get(&m.name),
            state.mcp.get(&m.name).map(String::as_str),
            &desired,
        );
        // What the CLI stored, which is what an edit is later told apart from.
        let stored = || {
            user_mcp(&paths.claude_json)
                .get(&m.name)
                .map(config_hash)
                .unwrap_or_else(|| config_hash(&desired))
        };
        let add_args = [
            "mcp",
            "add-json",
            "--scope",
            "user",
            m.name.as_str(),
            json.as_str(),
        ];
        let st = match plan {
            McpPlan::UpToDate => status("mcp", &m.name, "installed", ""),
            McpPlan::Edited => {
                state.mcp.remove(&m.name);
                status("mcp", &m.name, "conflict", EDITED)
            }
            McpPlan::Add => match cli.run(&add_args).await {
                Ok(_) => {
                    state.mcp.insert(m.name.clone(), stored());
                    status("mcp", &m.name, "installed", "")
                }
                Err(e) => status("mcp", &m.name, "failed", e),
            },
            McpPlan::Replace => {
                let replaced = match cli
                    .run(&["mcp", "remove", "--scope", "user", &m.name])
                    .await
                {
                    Ok(_) => cli.run(&add_args).await,
                    Err(e) => Err(e),
                };
                match replaced {
                    Ok(_) => {
                        state.mcp.insert(m.name.clone(), stored());
                        status("mcp", &m.name, "installed", "")
                    }
                    Err(e) => status("mcp", &m.name, "failed", e),
                }
            }
            McpPlan::OwnIdentical => status(
                "mcp",
                &m.name,
                "installed",
                "this machine's own server is identical; the kit leaves it to the owner",
            ),
            McpPlan::Conflict => status(
                "mcp",
                &m.name,
                "conflict",
                format!(
                    "this machine has its own user-scope server {} that differs; the kit leaves it alone",
                    m.name
                ),
            ),
        };
        out.push(st);
    }
    let wanted: BTreeSet<&str> = servers.iter().map(|m| m.name.as_str()).collect();
    let gone: Vec<String> = state
        .mcp
        .keys()
        .filter(|n| !wanted.contains(n.as_str()))
        .cloned()
        .collect();
    for name in gone {
        let Some(cur) = current.get(&name) else {
            state.mcp.remove(&name);
            continue;
        };
        if state.mcp.get(&name) != Some(&config_hash(cur)) {
            // Changed on this machine since the kit added it: the owner's now.
            state.mcp.remove(&name);
            out.push(status("mcp", &name, "conflict", EDITED));
            continue;
        }
        match cli.run(&["mcp", "remove", "--scope", "user", &name]).await {
            Ok(_) => {
                state.mcp.remove(&name);
                out.push(status("mcp", &name, "removed", ""));
            }
            Err(e) => out.push(status("mcp", &name, "failed", e)),
        }
    }
}

/// The secrets of each server in a JSON object of MCP servers (env and header values, and
/// secret-named URL parameters and flags: [`fleet_proto::kit::secret_spots`]): what a transcript
/// or a CLI message must never show.
pub fn mcp_secret_values(servers: &serde_json::Map<String, Value>) -> Vec<String> {
    servers
        .values()
        .flat_map(fleet_proto::kit::secret_spots)
        .map(|s| s.value)
        .collect()
}

/// Brings this machine to the kit: installs what is missing, updates what the kit installed
/// before, removes what the kit installed and no longer has. One status per item it looked at.
pub async fn apply(
    paths: &KitPaths,
    data_dir: &Path,
    claude_command: &[String],
    kit: &KitContent,
) -> Vec<KitItemStatus> {
    let skilled = tokio::task::spawn_blocking({
        let (paths, data_dir, skills) = (paths.clone(), data_dir.to_owned(), kit.skills.clone());
        move || {
            let mut state = State::load(&data_dir);
            let mut out = Vec::new();
            apply_skills(&paths, &mut state, &skills, &mut out);
            (state, out)
        }
    })
    .await;
    let (mut state, mut out) = match skilled {
        Ok(done) => done,
        Err(e) => {
            let why = format!("installing skills failed: {e}");
            let out = kit
                .skills
                .iter()
                .map(|s| status("skill", &s.name, "failed", why.clone()))
                .collect();
            (State::load(data_dir), out)
        }
    };
    let secrets = {
        let servers: serde_json::Map<String, Value> = kit
            .mcp
            .iter()
            .filter_map(|m| Some((m.name.clone(), serde_json::from_str(&m.config).ok()?)))
            .collect();
        mcp_secret_values(&servers)
    };
    match Cli::new(claude_command, paths, secrets) {
        Ok(cli) => {
            apply_plugins(paths, &cli, &mut state, &kit.plugins, &mut out).await;
            apply_mcp(paths, &cli, &mut state, &kit.mcp, &mut out).await;
        }
        Err(e) => {
            let why = format!("{e:#}");
            for p in &kit.plugins {
                out.push(status("plugin", &p.id(), "failed", why.clone()));
            }
            for m in &kit.mcp {
                out.push(status("mcp", &m.name, "failed", why.clone()));
            }
        }
    }
    if let Err(e) = state.save(data_dir) {
        tracing::warn!("could not save the kit state: {e:#}");
    }
    out
}

/// Statuses by item, for tests and logs.
pub fn by_item(statuses: &[KitItemStatus]) -> HashMap<(String, String), (String, String)> {
    statuses
        .iter()
        .map(|s| {
            (
                (s.kind.clone(), s.name.clone()),
                (s.state.clone(), s.detail.clone()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "fleet-kit-{name}-{}-{}",
            std::process::id(),
            crate::runner::random_token()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(path: &str, text: &str) -> KitFile {
        KitFile {
            path: path.into(),
            text: text.into(),
        }
    }

    #[test]
    fn capture_takes_text_files_and_says_what_it_left_out() {
        let root = temp("capture");
        let paths = KitPaths::at(root.join("cfg"));
        let skills = paths.skills();
        std::fs::create_dir_all(skills.join("pdf/scripts")).unwrap();
        std::fs::write(skills.join("pdf/SKILL.md"), "---\nname: pdf\n---\nUse it.").unwrap();
        std::fs::write(skills.join("pdf/scripts/run.py"), "print(1)").unwrap();
        std::fs::write(
            skills.join("pdf/logo.png"),
            [0x89, b'P', b'N', b'G', 0, 1, 2],
        )
        .unwrap();
        std::fs::write(
            skills.join("pdf/huge.txt"),
            "x".repeat(MAX_SKILL_FILE_BYTES + 1),
        )
        .unwrap();
        std::fs::create_dir_all(skills.join("pdf/node_modules/x")).unwrap();
        std::fs::write(skills.join("pdf/.env"), "TOKEN=never-captured").unwrap();
        // No SKILL.md: not a skill.
        std::fs::create_dir_all(skills.join("notes")).unwrap();
        std::fs::write(skills.join("notes/readme.md"), "hi").unwrap();
        // Too large as a whole: refused, not cut.
        std::fs::create_dir_all(skills.join("big")).unwrap();
        std::fs::write(skills.join("big/SKILL.md"), "big").unwrap();
        for i in 0..3 {
            std::fs::write(
                skills.join(format!("big/part{i}.md")),
                "y".repeat(MAX_SKILL_FILE_BYTES - 10),
            )
            .unwrap();
        }
        // User-scope MCP servers, and only those, from .claude.json.
        std::fs::write(
            &paths.claude_json,
            r#"{"oauthAccount":{"token":"never"},"mcpServers":{"slack":{"command":"npx","env":{"SLACK_TOKEN":"xoxp-1"}},"bad name":{}},"projects":{}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(paths.plugins()).unwrap();
        std::fs::write(
            paths.plugins().join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"mermaid@omm":[{"scope":"user"}],"local@omm":[{"scope":"project","projectPath":"C:/x"}],"lost@gone":[{"scope":"user"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            paths.plugins().join("known_marketplaces.json"),
            r#"{"omm":{"source":{"source":"github","repo":"o/omm"}}}"#,
        )
        .unwrap();

        let (content, skipped) = capture(&paths);
        assert_eq!(content.skills.len(), 1, "{skipped:?}");
        let pdf = &content.skills[0];
        assert_eq!(pdf.name, "pdf");
        let paths_of: Vec<&str> = pdf.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths_of, ["SKILL.md", "scripts/run.py"]);
        let reason = |name: &str| {
            skipped
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.reason.clone())
                .unwrap_or_else(|| panic!("{name} not reported: {skipped:?}"))
        };
        assert_eq!(reason("pdf/logo.png"), "binary file");
        assert!(reason("pdf/huge.txt").starts_with("larger than"));
        assert_eq!(reason("pdf/node_modules"), "tooling directory");
        assert_eq!(reason("pdf/.env"), "looks like a secret file");
        assert_eq!(reason("notes"), "no SKILL.md");
        assert!(reason("big").starts_with("larger than"));
        assert!(reason("lost@gone").contains("marketplace"));
        assert_eq!(
            content.plugins,
            vec![KitPlugin {
                plugin: "mermaid".into(),
                marketplace: "omm".into(),
                marketplace_source: "o/omm".into()
            }]
        );
        assert_eq!(content.mcp.len(), 1);
        assert_eq!(content.mcp[0].name, "slack");
        assert!(
            !content.mcp[0].config.contains("never"),
            "nothing else from .claude.json"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_skill_is_written_only_where_the_kit_owns_the_directory() {
        let root = temp("skills");
        let paths = KitPaths::at(root.join("cfg"));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let mine = KitSkill {
            name: "mine".into(),
            files: vec![file("SKILL.md", "kit version")],
        };
        let theirs = KitSkill {
            name: "theirs".into(),
            files: vec![file("SKILL.md", "kit version")],
        };
        let same = KitSkill {
            name: "same".into(),
            files: vec![file("SKILL.md", "identical"), file("ref/a.md", "a")],
        };
        // The owner's own directories: one that differs, one identical to the kit's.
        std::fs::create_dir_all(paths.skills().join("theirs")).unwrap();
        std::fs::write(paths.skills().join("theirs/SKILL.md"), "the owner's").unwrap();
        std::fs::create_dir_all(paths.skills().join("same/ref")).unwrap();
        std::fs::write(paths.skills().join("same/SKILL.md"), "identical").unwrap();
        std::fs::write(paths.skills().join("same/ref/a.md"), "a").unwrap();

        let mut state = State::default();
        let mut out = Vec::new();
        apply_skills(
            &paths,
            &mut state,
            &[mine.clone(), theirs.clone(), same.clone()],
            &mut out,
        );
        let s = by_item(&out);
        assert_eq!(s[&("skill".into(), "mine".into())].0, "installed");
        assert_eq!(s[&("skill".into(), "theirs".into())].0, "conflict");
        assert_eq!(s[&("skill".into(), "same".into())].0, "installed");
        assert_eq!(
            std::fs::read_to_string(paths.skills().join("theirs/SKILL.md")).unwrap(),
            "the owner's",
            "a conflict leaves the owner's directory alone"
        );
        assert_eq!(
            std::fs::read_to_string(paths.skills().join("mine").join(SKILL_MARKER)).unwrap(),
            marker_text("mine", &skill_hash(&mine.files))
        );
        assert!(
            !paths
                .config_dir
                .join(".fleet-kit-staging/new-mine")
                .exists(),
            "nothing staged is left behind"
        );
        assert!(
            !paths.skills().join("same").join(SKILL_MARKER).exists(),
            "not taken over"
        );

        // An edit reaches the kit's own copy; nothing else changes.
        let mine2 = KitSkill {
            name: "mine".into(),
            files: vec![file("SKILL.md", "kit version 2")],
        };
        assert_eq!(
            plan_skill(
                &paths.skills().join("mine"),
                &skill_hash(&mine2.files),
                "mine"
            ),
            SkillPlan::Write
        );
        let mut out = Vec::new();
        apply_skills(
            &paths,
            &mut state,
            &[mine2.clone(), theirs.clone(), same.clone()],
            &mut out,
        );
        assert_eq!(
            std::fs::read_to_string(paths.skills().join("mine/SKILL.md")).unwrap(),
            "kit version 2"
        );

        // A copy of the kit's skill under another name is the owner's, not the kit's.
        let copy = paths.skills().join("copied");
        std::fs::create_dir_all(&copy).unwrap();
        for f in ["SKILL.md", SKILL_MARKER] {
            std::fs::copy(paths.skills().join("mine").join(f), copy.join(f)).unwrap();
        }
        // A kit skill the owner then changed on this machine is theirs from then on.
        let edited = KitSkill {
            name: "edited".into(),
            files: vec![file("SKILL.md", "kit text")],
        };
        let mut out = Vec::new();
        apply_skills(
            &paths,
            &mut state,
            &[mine2.clone(), edited.clone()],
            &mut out,
        );
        std::fs::write(paths.skills().join("edited/SKILL.md"), "my own text").unwrap();
        let edited2 = KitSkill {
            name: "edited".into(),
            files: vec![file("SKILL.md", "kit text 2")],
        };
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[mine2.clone(), edited2], &mut out);
        assert_eq!(
            by_item(&out)[&("skill".into(), "edited".into())].0,
            "conflict"
        );
        assert_eq!(
            std::fs::read_to_string(paths.skills().join("edited/SKILL.md")).unwrap(),
            "my own text",
            "an update never overwrites a local edit"
        );

        // Dropped from the kit: the kit's copy goes; the owner's, the copy and the edited one
        // stay.
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[], &mut out);
        let s = by_item(&out);
        assert_eq!(s[&("skill".into(), "mine".into())].0, "removed");
        assert_eq!(s[&("skill".into(), "edited".into())].0, "conflict");
        assert!(!paths.skills().join("mine").exists());
        assert!(paths.skills().join("theirs/SKILL.md").exists());
        assert!(paths.skills().join("same/SKILL.md").exists());
        assert!(paths.skills().join("copied/SKILL.md").exists());
        assert!(paths.skills().join("edited/SKILL.md").exists());
        assert_eq!(
            out.len(),
            2,
            "only the kit's own skills are looked at: {out:?}"
        );

        // An unsafe path never reaches the disk.
        let evil = KitSkill {
            name: "evil".into(),
            files: vec![file("SKILL.md", "x"), file("../../escape.md", "x")],
        };
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[evil], &mut out);
        assert_eq!(out[0].state, "failed");
        assert!(!paths.config_dir.join("escape.md").exists());
        assert!(!root.join("escape.md").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_mcp_server_of_the_owners_is_never_replaced() {
        let desired = serde_json::json!({"command": "npx", "env": {"T": "v"}});
        let older = serde_json::json!({"command": "npx", "env": {"T": "old"}});
        let edited = serde_json::json!({"command": "mine"});
        let (h_desired, h_older) = (config_hash(&desired), config_hash(&older));
        assert_eq!(plan_mcp(None, None, &desired), McpPlan::Add);
        assert_eq!(
            plan_mcp(Some(&desired), Some(&h_desired), &desired),
            McpPlan::UpToDate
        );
        assert_eq!(
            plan_mcp(Some(&older), Some(&h_older), &desired),
            McpPlan::Replace,
            "the kit's own older config is replaced"
        );
        assert_eq!(
            plan_mcp(Some(&edited), Some(&h_older), &desired),
            McpPlan::Edited,
            "a config changed on the machine since the kit added it is the owner's"
        );
        assert_eq!(
            plan_mcp(Some(&desired), None, &desired),
            McpPlan::OwnIdentical
        );
        assert_eq!(plan_mcp(Some(&edited), None, &desired), McpPlan::Conflict);
    }

    #[test]
    fn mcp_secrets_are_found_for_redaction() {
        let servers: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"a":{"command":"x","env":{"K":"secret-1"}},"b":{"url":"u","headers":{"Authorization":"Bearer abcdef"}},"c":{"command":"y"}}"#,
        )
        .unwrap();
        let mut v = mcp_secret_values(&servers);
        v.sort();
        assert_eq!(v, ["Bearer abcdef", "secret-1"]);
        let flags: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"a":{"command":"x","args":["--api-key","k-123456"]}}"#)
                .unwrap();
        assert_eq!(mcp_secret_values(&flags), ["k-123456"]);
    }
}
