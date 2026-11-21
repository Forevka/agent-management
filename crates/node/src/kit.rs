//! The kit on a node: report what this machine's Claude Code has (the import), and install the
//! orchestrator's kit for the owner's own Claude Code and the agents (both run as this user).
//!
//! Where things are, as Claude Code itself decides it (checked against the real CLI, 2.1.268):
//!
//! - the config dir is `CLAUDE_CONFIG_DIR` when set, else `~/.claude`; skills are its
//!   `skills/<name>/` directories, and plugins are recorded in its `plugins/installed_plugins.json`
//!   (by `plugin@marketplace`, with a scope) and `plugins/known_marketplaces.json` (sources);
//! - user-scope MCP servers are the top-level `mcpServers` of `.claude.json`, which is
//!   `$CLAUDE_CONFIG_DIR/.claude.json` when that is set, else `~/.claude.json`. Nothing else in
//!   that file is read: it also holds the account's tokens.
//!
//! The node never touches what it did not install. A skill it writes carries a marker
//! ([`SKILL_MARKER`]) naming the skill and the files it wrote, with their hash; plugins and MCP
//! servers it added are listed in `kit-state.json` in the node's data dir. An item of the same
//! name the owner has on the machine is a `conflict` and is left alone (or `installed`, when it is
//! identical); one the kit installed and the owner changed since is theirs from then on; and only
//! what the kit installed, unchanged, is ever replaced or removed. Plugins and MCP servers are
//! changed with the `claude` CLI (`claude plugin install`, `claude mcp add-json --scope user`),
//! which keeps its own files consistent; every call has a timeout and takes its whole process
//! tree down when it runs out, and a failure is a status, never a crash.

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
        MAX_SKILL_BYTES, MAX_SKILL_FILE_BYTES, MAX_SKILL_FILES, SKILL_MARKER, clean_source,
        safe_skill_path, scrub_values, secret_file, valid_name,
    },
    redact::redact_secrets,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// The start of the name an unreadable `kit-state.json` is set aside under.
const UNREADABLE: &str = "kit-state.json.unreadable-";
/// How long one `claude` CLI call may take. Adding a marketplace clones a repository, which the
/// CLI itself gives two minutes.
const CLI_TIMEOUT: Duration = Duration::from_secs(180);
/// How long asking the CLI for its `--help` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Directories inside a skill that are tooling, not the skill.
const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "__pycache__"];
/// Files an OS or a tool leaves in any directory: never part of a skill, never the owner's work.
const JUNK_FILES: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];
/// At most this many things left out are reported per skill, and in all: a capture must fit its
/// messages even for a skill directory with thousands of files the kit does not take.
const MAX_SKIPPED_PER_SKILL: usize = 50;
const MAX_SKIPPED: usize = 500;
/// The longest MCP server config the kit takes (the orchestrator's limit too).
const MAX_MCP_CONFIG_BYTES: usize = 64 * 1024;

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

// ---------------------------------------------------------------- reading what is there

/// A JSON file the CLI keeps: `Ok(None)` when it does not exist yet, an error when it exists
/// and cannot be read or parsed (it may be half written). An unreadable file is never taken for
/// an empty one: that would make the kit forget, or redo, what it installed.
fn read_json(path: &Path) -> std::result::Result<Option<Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} cannot be read as JSON ({e})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} cannot be read ({e})", path.display())),
    }
}

/// Plugin ids (`plugin@marketplace`) installed at user scope.
fn user_plugins(plugins_dir: &Path) -> std::result::Result<BTreeSet<String>, String> {
    let Some(v) = read_json(&plugins_dir.join("installed_plugins.json"))? else {
        return Ok(BTreeSet::new());
    };
    Ok(v["plugins"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, installs)| {
            installs
                .as_array()
                .is_some_and(|a| a.iter().any(|i| i["scope"] == "user"))
        })
        .map(|(id, _)| id.clone())
        .collect())
}

/// Known marketplaces: name -> what `claude plugin marketplace add` takes to add it again.
fn marketplaces(
    plugins_dir: &Path,
) -> std::result::Result<BTreeMap<String, Option<String>>, String> {
    let Some(v) = read_json(&plugins_dir.join("known_marketplaces.json"))? else {
        return Ok(BTreeMap::new());
    };
    Ok(v.as_object()
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
        .collect())
}

/// User-scope MCP servers: `mcpServers` of `.claude.json`, and nothing else from that file.
fn user_mcp(claude_json: &Path) -> std::result::Result<serde_json::Map<String, Value>, String> {
    Ok(match read_json(claude_json)? {
        Some(mut v) => match v.get_mut("mcpServers").map(Value::take) {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        },
        None => serde_json::Map::new(),
    })
}

// ---------------------------------------------------------------- capture

/// What this machine has at user scope, and what was left out.
pub fn capture(paths: &KitPaths) -> (KitContent, Vec<KitSkipped>) {
    let mut skipped = Vec::new();
    let skills = capture_skills(&paths.skills(), &mut skipped);
    let plugins = capture_plugins(&paths.plugins(), &mut skipped);
    let mcp = capture_mcp(&paths.claude_json, &mut skipped);
    if skipped.len() > MAX_SKIPPED {
        let more = skipped.len() - MAX_SKIPPED;
        skipped.truncate(MAX_SKIPPED);
        skip(
            &mut skipped,
            "capture",
            "",
            format!("…and {more} more left out"),
        );
    }
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
    let clip = |s: &str| s.chars().take(300).collect::<String>();
    out.push(KitSkipped {
        kind: kind.into(),
        name: clip(name),
        reason: clip(&reason.into()),
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
        let mut own = Vec::new();
        match read_skill(&path, &name, &mut own) {
            Ok(s) => out.push(s),
            Err(reason) => skip(&mut own, "skill", &name, reason),
        }
        if own.len() > MAX_SKIPPED_PER_SKILL {
            let more = own.len() - MAX_SKIPPED_PER_SKILL;
            own.truncate(MAX_SKIPPED_PER_SKILL);
            skip(
                &mut own,
                "skill",
                &name,
                format!("…and {more} more left out"),
            );
        }
        skipped.extend(own);
    }
    out
}

/// A file that is never part of a skill and never the owner's work: an OS's or a tool's.
fn junk(file_name: &str) -> bool {
    JUNK_FILES.contains(&file_name) || file_name.ends_with(".pyc")
}

/// A skill directory's text files. Files the kit cannot carry (too large, binary, an unusable
/// path, a secret) are reported in `skipped` and left out; a skill that is too large as a whole,
/// or has no `SKILL.md`, is refused with the reason.
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
            let here = format!("{name}/{rel}");
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_symlink() {
                skip(skipped, "skill", &here, "a link (not followed)");
                continue;
            }
            if meta.is_dir() {
                if SKIP_DIRS.contains(&file_name.as_str()) {
                    skip(skipped, "skill", &here, "tooling directory");
                } else {
                    stack.push((e.path(), format!("{rel}/")));
                }
                continue;
            }
            if rel == SKILL_MARKER || junk(&file_name) {
                continue;
            }
            if secret_file(&rel) {
                skip(skipped, "skill", &here, "looks like a secret file");
                continue;
            }
            if !safe_skill_path(&rel) {
                skip(skipped, "skill", &here, "unusable file name");
                continue;
            }
            if meta.len() as usize > MAX_SKILL_FILE_BYTES {
                skip(
                    skipped,
                    "skill",
                    &here,
                    format!("larger than {} KB", MAX_SKILL_FILE_BYTES / 1024),
                );
                continue;
            }
            let bytes = std::fs::read(e.path()).map_err(|e| format!("cannot read {rel}: {e}"))?;
            let text = match String::from_utf8(bytes) {
                Ok(t) if !t.contains('\0') => t,
                _ => {
                    skip(skipped, "skill", &here, "binary file");
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

fn capture_plugins(dir: &Path, skipped: &mut Vec<KitSkipped>) -> Vec<KitPlugin> {
    let (installed, known) = match (user_plugins(dir), marketplaces(dir)) {
        (Ok(i), Ok(k)) => (i, k),
        (Err(e), _) | (_, Err(e)) => {
            skip(skipped, "plugin", "", e);
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for id in installed {
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
            // A password in the source would go into the database and to every machine.
            Some(Some(source)) if !clean_source(source) => skip(
                skipped,
                "plugin",
                &id,
                "its marketplace's source carries a user, password, token or query",
            ),
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

fn capture_mcp(claude_json: &Path, skipped: &mut Vec<KitSkipped>) -> Vec<KitMcp> {
    let servers = match user_mcp(claude_json) {
        Ok(s) => s,
        Err(e) => {
            skip(skipped, "mcp", "", e);
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (name, config) in servers {
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
        let config = config.to_string();
        if config.len() > MAX_MCP_CONFIG_BYTES {
            skip(
                skipped,
                "mcp",
                &name,
                format!(
                    "its config is larger than {} KB",
                    MAX_MCP_CONFIG_BYTES / 1024
                ),
            );
            continue;
        }
        out.push(KitMcp { name, config });
    }
    out
}

// ---------------------------------------------------------------- install

/// What the kit added of one MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct McpRecord {
    /// Hash of the config the kit asked for: it changes only when the kit's does.
    desired: String,
    /// Hash of the config as the CLI stored it (it normalises: adds `args: []`, drops keys it
    /// does not know). A different one on disk means the owner changed it since.
    stored: String,
}

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
    #[serde(default)]
    mcp: BTreeMap<String, McpRecord>,
}

impl State {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("kit-state.json")
    }

    /// The record, or an empty one when there is none yet. One that exists and cannot be read
    /// is set aside (never overwritten: a person may want it) and said, loudly; the kit then
    /// knows of nothing it installed, so it removes nothing, and [`State::trouble`] keeps saying
    /// so in the node's kit status until the set-aside file is deleted.
    fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::error!("the kit's record {} cannot be read: {e}", path.display());
                return Self::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let aside = data_dir.join(format!("{UNREADABLE}{stamp}"));
                let _ = std::fs::rename(&path, &aside);
                tracing::error!(
                    "the kit's record of what it installed is unreadable ({e}); set aside as {}: until it is sorted out, nothing the kit installed earlier is updated or removed",
                    aside.display()
                );
                Self::default()
            }
        }
    }

    /// What to tell the owner about the record: a set-aside unreadable one, if any.
    fn trouble(data_dir: &Path) -> Option<String> {
        let aside = std::fs::read_dir(data_dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with(UNREADABLE))?;
        Some(format!(
            "this node's record of what the kit installed was unreadable and is kept as {aside} in its data dir: skills, plugins and MCP servers the kit installed before are no longer updated or removed by it; delete that file once they are sorted out"
        ))
    }

    /// Written after every change the CLI made, so a crash in the middle of an apply never
    /// leaves the kit not knowing what it installed.
    fn save(&self, data_dir: &Path) {
        let save = || -> Result<()> {
            let path = Self::path(data_dir);
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        };
        if let Err(e) = save() {
            tracing::warn!("could not save the kit state: {e:#}");
        }
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
        detail: detail.into().chars().take(500).collect(),
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

/// What the kit records in a skill directory it wrote: whose it is and exactly which files, so
/// that what tools or the owner add beside them is never taken for a change, nor deleted.
#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    name: String,
    hash: String,
    files: Vec<String>,
}

fn marker_text(skill: &KitSkill, hash: &str) -> String {
    serde_json::to_string(&Marker {
        name: skill.name.clone(),
        hash: hash.to_owned(),
        files: skill.files.iter().map(|f| f.path.clone()).collect(),
    })
    .expect("a marker serialises")
}

/// Who a skill directory belongs to, as it stands on disk.
#[derive(Debug, PartialEq)]
enum Found {
    Absent,
    /// The kit wrote it (the marker names this directory) and its files are still exactly that.
    Kit(String),
    /// The kit wrote it, and one of its files has changed or gone since.
    KitEdited,
    /// The owner's, with its content hash when the kit could read all of it.
    Own(Option<String>),
}

fn read_marker(dir: &Path, name: &str) -> Option<Marker> {
    let text = std::fs::read_to_string(dir.join(SKILL_MARKER)).ok()?;
    serde_json::from_str::<Marker>(&text)
        .ok()
        .filter(|m| m.name == name)
}

fn inspect(dir: &Path, name: &str) -> Found {
    if !dir.exists() {
        return Found::Absent;
    }
    if let Some(m) = read_marker(dir, name) {
        // Only the files the kit wrote count: what was added beside them is someone else's.
        let mut files = Vec::new();
        for path in &m.files {
            if !safe_skill_path(path) {
                return Found::KitEdited;
            }
            match std::fs::read_to_string(dir.join(path)) {
                Ok(text) => files.push(KitFile {
                    path: path.clone(),
                    text,
                }),
                Err(_) => return Found::KitEdited,
            }
        }
        return if skill_hash(&files) == m.hash {
            Found::Kit(m.hash)
        } else {
            Found::KitEdited
        };
    }
    // Only what the kit itself could have written counts: a file it would have left out (a
    // binary, a secret, a link) means the directory is not the same as a kit skill.
    let mut skipped = Vec::new();
    let content = read_skill(dir, name, &mut skipped)
        .ok()
        .filter(|_| skipped.iter().all(|s| s.reason == "tooling directory"))
        .map(|s| skill_hash(&s.files));
    Found::Own(content)
}

/// Decides for one skill: the marker says whether the directory is the kit's, and its files
/// whether they still are exactly what the kit wrote.
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

/// What the owner added to a kit skill: files under `dir` that are neither the kit's (`listed`,
/// the marker) nor an OS's junk, and whole tooling directories (`.git`, `node_modules`,
/// `__pycache__`), which the kit never writes. They are kept when the kit updates or removes it.
fn extras(dir: &Path, listed: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for e in entries.flatten() {
            let file_name = e.file_name().to_string_lossy().into_owned();
            let rel = format!("{prefix}{file_name}");
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() && !meta.is_symlink() {
                if SKIP_DIRS.contains(&file_name.as_str()) {
                    out.push(rel);
                } else {
                    stack.push((e.path(), format!("{rel}/")));
                }
            } else if rel != SKILL_MARKER && !junk(&file_name) && !listed.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

/// Where the kit stages a skill's directory: beside `skills/`, on the same volume (so a rename
/// moves it in one step), and never where Claude Code looks for skills.
fn staging(paths: &KitPaths) -> PathBuf {
    paths.config_dir.join(".fleet-kit-staging")
}

/// A place in staging that is this apply's alone (`old~<skill>~<random>`): staging is never
/// cleared wholesale, so a copy an earlier apply could not put back is never deleted by a later
/// one.
fn staged(paths: &KitPaths, what: &str, name: &str) -> PathBuf {
    staging(paths).join(format!(
        "{what}~{name}~{}",
        &crate::runner::random_token()[..8]
    ))
}

/// A copy of skill `name` an earlier apply set aside and could not put back (`old~…`,
/// `failed~…`): until a person looks at it, the kit leaves that skill alone.
fn leftover(paths: &KitPaths, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(staging(paths))
        .ok()?
        .flatten()
        .find(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&format!("old~{name}~")) || n.starts_with(&format!("failed~{name}~"))
        })
        .map(|e| e.path())
}

/// Deletes something this apply set aside in staging. It is renamed to `trash~…` first, so if
/// the deletion fails (a file held open on Windows) it is no longer taken for a copy a person
/// must look at (`leftover`), and the skill is not held up by it.
fn discard(paths: &KitPaths, ours: &Path) {
    let trash = staged(paths, "trash", "x");
    let target = if std::fs::rename(ours, &trash).is_ok() {
        trash
    } else {
        ours.to_path_buf()
    };
    if let Err(e) = std::fs::remove_dir_all(&target) {
        tracing::warn!("could not delete {}: {e}", target.display());
    }
}

/// Moves `rel` from under `from` to under `to`.
fn carry(from: &Path, to: &Path, rel: &str) -> std::io::Result<()> {
    let dest = to.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(from.join(rel), dest)
}

/// Takes the kit's skill off this machine. Without anything of the owner's in it, the directory
/// is moved out of `skills/` and deleted there, so a removal that fails half way (a file held open
/// on Windows) never leaves a broken skill where Claude Code loads it. With the owner's own files
/// in it, only the kit's files and its marker go.
fn take_away(paths: &KitPaths, dir: &Path, name: &str, marker: &Marker) -> Result<()> {
    let listed: BTreeSet<String> = marker.files.iter().cloned().collect();
    let own = extras(dir, &listed);
    if own.is_empty() {
        let old = staged(paths, "old", name);
        std::fs::create_dir_all(staging(paths))?;
        std::fs::rename(dir, &old).with_context(|| format!("moving {} aside", dir.display()))?;
        discard(paths, &old);
        return Ok(());
    }
    // SKILL.md first, so a half-finished removal leaves no skill behind.
    let mut order: Vec<&String> = marker.files.iter().collect();
    order.sort_by_key(|p| *p != "SKILL.md");
    for path in order {
        match std::fs::remove_file(dir.join(path)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {path}"));
            }
            _ => {}
        }
    }
    std::fs::remove_file(dir.join(SKILL_MARKER))?;
    Ok(())
}

/// What became of a skill the kit meant to write.
enum Written {
    Done,
    /// A file the owner added to the kit's older copy is where the new version has one of its
    /// own: nothing was written, so the owner's file is never overwritten.
    Collides(String),
}

/// Whether the owner's `extra` (a file or a whole directory) and a kit file `path` would land on
/// each other.
fn collides(extra: &str, path: &str) -> bool {
    extra == path
        || path.starts_with(&format!("{extra}/"))
        || extra.starts_with(&format!("{path}/"))
}

/// Writes a skill's directory: staged first, then swapped in, so a failure half way never
/// leaves a broken skill where Claude Code would load it. Files the owner added to the kit's
/// older copy are carried over; if one cannot be, the old copy goes back whole.
fn write_skill(paths: &KitPaths, skill: &KitSkill, hash: &str) -> Result<Written> {
    let dir = paths.skills().join(&skill.name);
    let had_old = dir.exists();
    let keep = match read_marker(&dir, &skill.name) {
        Some(m) if had_old => {
            let listed: BTreeSet<String> = m.files.into_iter().collect();
            extras(&dir, &listed)
        }
        _ => Vec::new(),
    };
    if let Some(p) = keep
        .iter()
        .find(|e| skill.files.iter().any(|f| collides(e, &f.path)))
    {
        return Ok(Written::Collides(p.clone()));
    }
    let tmp = staged(paths, "new", &skill.name);
    let staged_new = (|| -> Result<()> {
        std::fs::create_dir_all(&tmp)?;
        for f in &skill.files {
            let path = tmp.join(&f.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &f.text).with_context(|| format!("writing {}", f.path))?;
        }
        std::fs::write(tmp.join(SKILL_MARKER), marker_text(skill, hash))?;
        Ok(())
    })();
    if let Err(e) = staged_new {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let old = staged(paths, "old", &skill.name);
    if had_old && let Err(e) = std::fs::rename(&dir, &old) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e).with_context(|| format!("moving {} aside", dir.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, &dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        if had_old && std::fs::rename(&old, &dir).is_err() {
            return Err(e).with_context(|| {
                format!(
                    "putting {} in place failed, and so did putting the old copy back: it is at {}",
                    dir.display(),
                    old.display()
                )
            });
        }
        return Err(e).with_context(|| format!("putting {} in place", dir.display()));
    }
    for (i, rel) in keep.iter().enumerate() {
        if let Err(e) = carry(&old, &dir, rel) {
            // Put everything back as it was: what was carried returns to the old copy, and the old
            // copy to its place. Nothing of the owner's is ever deleted.
            for back in &keep[..i] {
                let _ = carry(&dir, &old, back);
            }
            let failed = staged(paths, "failed", &skill.name);
            let restored =
                std::fs::rename(&dir, &failed).is_ok() && std::fs::rename(&old, &dir).is_ok();
            if !restored {
                // Both copies stay where they are for a person to sort out; the next apply
                // sees them (`leftover`) and leaves the skill alone until then.
                return Err(e).with_context(|| {
                    format!(
                        "could not keep the owner's {rel}, nor put the old copy back: it is at {}",
                        old.display()
                    )
                });
            }
            // Only the new files the kit just wrote are in `failed` now: this apply's own.
            discard(paths, &failed);
            return Err(e).with_context(|| {
                format!("could not keep the owner's {rel}; the old copy of the skill stays")
            });
        }
    }
    if had_old {
        discard(paths, &old);
    }
    Ok(Written::Done)
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
        if let Some(at) = leftover(paths, &s.name) {
            out.push(status(
                "skill",
                &s.name,
                "failed",
                format!(
                    "an earlier copy of this skill was set aside and not put back: it is at {}; put it back or delete it, and the kit tries again",
                    at.display()
                ),
            ));
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
                Ok(Written::Done) => {
                    state.skills.insert(s.name.clone(), hash);
                    status("skill", &s.name, "installed", "")
                }
                Ok(Written::Collides(p)) => {
                    state.skills.remove(&s.name);
                    status(
                        "skill",
                        &s.name,
                        "conflict",
                        format!(
                            "the owner's own {p} is where the kit's new version has a file; the kit leaves the skill alone"
                        ),
                    )
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
        if let Some(at) = leftover(paths, &name) {
            out.push(status(
                "skill",
                &name,
                "failed",
                format!(
                    "an earlier copy of this skill was set aside and not put back: it is at {}; put it back or delete it, and the kit tries again",
                    at.display()
                ),
            ));
            continue;
        }
        let target = dir.join(&name);
        match (inspect(&target, &name), read_marker(&target, &name)) {
            (Found::Kit(_), Some(marker)) => match take_away(paths, &target, &name, &marker) {
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
            (Found::KitEdited, _) => out.push(status("skill", &name, "conflict", EDITED)),
            _ => {}
        }
        state.skills.remove(&name);
    }
}

/// What to do with an MCP server of a name the machine may already have at user scope.
#[derive(Debug, PartialEq)]
pub enum McpPlan {
    Add,
    /// The kit's own, untouched since, and the kit's config for it changed: remove and add again.
    Replace,
    UpToDate,
    OwnIdentical,
    Conflict,
    /// The kit's own, changed on this machine since: the owner's now, left alone.
    Edited,
}

/// The hash a config is recorded by: of its JSON, keys in order.
fn config_hash(config: &Value) -> String {
    hex_sha256(&[config.to_string().as_bytes()])
}

/// A server's config as the CLI keeps it, for telling the owner's own copy from the kit's: only
/// the keys Claude Code knows, with empty lists and maps (which the CLI adds or drops) and the
/// default `stdio` type left out.
fn normalized(config: &Value) -> Value {
    const KNOWN: [&str; 7] = ["type", "command", "args", "env", "url", "headers", "oauth"];
    let Value::Object(m) = config else {
        return config.clone();
    };
    Value::Object(
        m.iter()
            .filter(|(k, _)| KNOWN.contains(&k.as_str()))
            .filter(|(k, v)| {
                !(matches!(v, Value::Array(a) if a.is_empty())
                    || matches!(v, Value::Object(o) if o.is_empty())
                    || (*k == "type" && *v == "stdio"))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

fn plan_mcp(current: Option<&Value>, record: Option<&McpRecord>, desired: &Value) -> McpPlan {
    match (current, record) {
        (None, _) => McpPlan::Add,
        (Some(cur), Some(r)) if config_hash(cur) != r.stored => McpPlan::Edited,
        (Some(_), Some(r)) if r.desired == config_hash(desired) => McpPlan::UpToDate,
        (Some(_), Some(_)) => McpPlan::Replace,
        (Some(cur), None) if normalized(cur) == normalized(desired) => McpPlan::OwnIdentical,
        (Some(_), None) => McpPlan::Conflict,
    }
}

/// The `claude` CLI, as the node's configured command runs it.
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

    /// Runs one call: its output, or why it failed. The call runs in a process group (a job
    /// object on Windows) that a timeout takes down whole, so nothing a `claude.cmd` shim started
    /// is left running.
    async fn call(
        &self,
        args: &[&str],
        limit: Duration,
    ) -> std::result::Result<(bool, String), String> {
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
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let what = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not run `claude {what}`: {e}"))?;
        let tree = crate::runner::ProcessTree::adopt(&child);
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        // Both pipes at once: a call that fills one while the other is being read to its end
        // would stall until the timeout.
        let read_out = async {
            let mut a = Vec::new();
            if let Some(o) = stdout.as_mut() {
                let _ = o.read_to_end(&mut a).await;
            }
            a
        };
        let read_err = async {
            let mut b = Vec::new();
            if let Some(e) = stderr.as_mut() {
                let _ = e.read_to_end(&mut b).await;
            }
            b
        };
        let ran = tokio::time::timeout(limit, async {
            let (out, err, status) = tokio::join!(read_out, read_err, child.wait());
            ((out, err), status)
        })
        .await;
        let ((out, err), status) = match ran {
            Ok(done) => done,
            Err(_) => {
                tree.kill(&mut child).await;
                return Err(format!("`claude {what}` took over {}s", limit.as_secs()));
            }
        };
        let mut text = String::from_utf8_lossy(&out).into_owned();
        text.push_str(&String::from_utf8_lossy(&err));
        redact_secrets(&mut text, &self.secrets);
        let ok = status.map(|s| s.success()).unwrap_or(false);
        Ok((ok, text))
    }

    async fn run(&self, args: &[&str]) -> std::result::Result<String, String> {
        let what = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let (ok, text) = self.call(args, CLI_TIMEOUT).await?;
        if ok {
            return Ok(text);
        }
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

/// The flags of the node's `claude` whose absence would fail every run, from its `--help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliFlags {
    /// `--strict-mcp-config` (added in 2.0; verified with 2.1.268).
    pub strict_mcp: bool,
    /// `--include-partial-messages` (verified with 2.1.268).
    pub partial_messages: bool,
}

/// What the node's `claude` says it takes, from its `--help`. `None` when the CLI cannot be
/// asked at all.
pub async fn probe_cli_flags(command: &[String], paths: Option<&KitPaths>) -> Option<CliFlags> {
    let fallback;
    let paths = match paths {
        Some(p) => p,
        None => {
            fallback = KitPaths {
                config_dir: PathBuf::new(),
                claude_json: PathBuf::new(),
                set_env: false,
            };
            &fallback
        }
    };
    let cli = Cli::new(command, paths, Vec::new()).ok()?;
    let (ok, text) = cli.call(&["--help"], PROBE_TIMEOUT).await.ok()?;
    (ok || text.contains("Usage")).then(|| CliFlags {
        strict_mcp: text.contains(crate::runner::STRICT_MCP),
        partial_messages: text.contains(crate::runner::PARTIAL_MESSAGES),
    })
}

async fn apply_plugins(
    paths: &KitPaths,
    data_dir: &Path,
    cli: &Cli,
    state: &mut State,
    plugins: &[KitPlugin],
    out: &mut Vec<KitItemStatus>,
) {
    let (installed, mut known) = match (
        user_plugins(&paths.plugins()),
        marketplaces(&paths.plugins()),
    ) {
        (Ok(i), Ok(k)) => (i, k),
        (Err(e), _) | (_, Err(e)) => {
            // Taken for empty, it would reinstall everything and forget what the kit did.
            for p in plugins {
                out.push(status(
                    "plugin",
                    &p.id(),
                    "failed",
                    format!("{e}; trying again next time"),
                ));
            }
            return;
        }
    };
    for p in plugins {
        let id = p.id();
        if !valid_name(&p.plugin)
            || !valid_name(&p.marketplace)
            || !clean_source(&p.marketplace_source)
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
                state.save(data_dir);
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
                state.save(data_dir);
                out.push(status("plugin", &id, "removed", ""));
            }
            Err(e) => out.push(status("plugin", &id, "failed", e)),
        }
    }
}

async fn apply_mcp(
    paths: &KitPaths,
    data_dir: &Path,
    cli: &Cli,
    state: &mut State,
    servers: &[KitMcp],
    out: &mut Vec<KitItemStatus>,
) {
    let current = match user_mcp(&paths.claude_json) {
        Ok(c) => c,
        Err(e) => {
            // Half written by a running Claude Code, perhaps: never taken for "no servers".
            for m in servers {
                out.push(status(
                    "mcp",
                    &m.name,
                    "failed",
                    format!("{e}; trying again next time"),
                ));
            }
            return;
        }
    };
    // What the CLI stored for `name` after a change, read back from its file.
    let stored = |name: &str| -> Option<String> {
        user_mcp(&paths.claude_json)
            .ok()?
            .get(name)
            .map(config_hash)
    };
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
        let record = |stored: Option<String>| McpRecord {
            desired: config_hash(&desired),
            stored: stored.unwrap_or_else(|| config_hash(&desired)),
        };
        let plan = plan_mcp(current.get(&m.name), state.mcp.get(&m.name), &desired);
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
                state.save(data_dir);
                status("mcp", &m.name, "conflict", EDITED)
            }
            McpPlan::Add => match cli.run(&add_args).await {
                Ok(_) => {
                    state.mcp.insert(m.name.clone(), record(stored(&m.name)));
                    state.save(data_dir);
                    status("mcp", &m.name, "installed", "")
                }
                Err(e) => status("mcp", &m.name, "failed", e),
            },
            McpPlan::Replace => {
                let removed = cli
                    .run(&["mcp", "remove", "--scope", "user", &m.name])
                    .await;
                if removed.is_ok() {
                    // Gone now: until it is added again the kit has nothing of it here.
                    state.mcp.remove(&m.name);
                    state.save(data_dir);
                }
                match removed {
                    Ok(_) => match cli.run(&add_args).await {
                        Ok(_) => {
                            state.mcp.insert(m.name.clone(), record(stored(&m.name)));
                            state.save(data_dir);
                            status("mcp", &m.name, "installed", "")
                        }
                        Err(e) => status("mcp", &m.name, "failed", e),
                    },
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
        if state.mcp.get(&name).map(|r| r.stored.as_str()) != Some(config_hash(cur).as_str()) {
            // Changed on this machine since the kit added it: the owner's now.
            state.mcp.remove(&name);
            out.push(status("mcp", &name, "conflict", EDITED));
            continue;
        }
        match cli.run(&["mcp", "remove", "--scope", "user", &name]).await {
            Ok(_) => {
                state.mcp.remove(&name);
                state.save(data_dir);
                out.push(status("mcp", &name, "removed", ""));
            }
            Err(e) => out.push(status("mcp", &name, "failed", e)),
        }
    }
}

/// The secrets of each server in a JSON object of MCP servers, which a transcript or a CLI
/// message must never show: every header value, every secret found in a URL or the args, and an
/// env value that is named like a secret or long enough to be one. Short, plainly named env
/// values (`DEBUG=1`, `MODE=production`) are left readable.
pub fn mcp_secret_values(servers: &serde_json::Map<String, Value>) -> Vec<String> {
    servers.values().flat_map(scrub_values).collect()
}

/// Brings this machine to the kit: installs what is missing, updates what the kit installed
/// before, removes what the kit installed and no longer has. One status per item it looked at,
/// and one for the node itself: whether its CLI holds agent sessions to the kit's MCP servers.
pub async fn apply(
    paths: &KitPaths,
    data_dir: &Path,
    claude_command: &[String],
    strict_mcp: Option<bool>,
    kit: &KitContent,
) -> Vec<KitItemStatus> {
    let skilled = tokio::task::spawn_blocking({
        let (paths, data_dir, skills) = (paths.clone(), data_dir.to_owned(), kit.skills.clone());
        move || {
            let mut state = State::load(&data_dir);
            let mut out = Vec::new();
            apply_skills(&paths, &mut state, &skills, &mut out);
            state.save(&data_dir);
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
            apply_plugins(paths, data_dir, &cli, &mut state, &kit.plugins, &mut out).await;
            apply_mcp(paths, data_dir, &cli, &mut state, &kit.mcp, &mut out).await;
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
    state.save(data_dir);
    out.push(match State::trouble(data_dir) {
        Some(why) => status("node", "state", "failed", why),
        None => status("node", "state", "installed", ""),
    });
    out.push(match strict_mcp {
        Some(false) => status(
            "node",
            "allowlist",
            "failed",
            "allowlist not enforced: this node's claude CLI has no --strict-mcp-config, so agent sessions also load MCP servers configured outside the kit; update the claude CLI (2.1.268 is known to work)",
        ),
        _ => status("node", "allowlist", "installed", ""),
    });
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
        std::fs::write(skills.join("pdf/.DS_Store"), "junk").unwrap();
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
        // Thousands of files left out: the report says how many, not each.
        std::fs::create_dir_all(skills.join("blobs")).unwrap();
        std::fs::write(skills.join("blobs/SKILL.md"), "blobs").unwrap();
        for i in 0..120 {
            std::fs::write(skills.join(format!("blobs/b{i}.bin")), [0u8, 1, 2]).unwrap();
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
            r#"{"version":2,"plugins":{"mermaid@omm":[{"scope":"user"}],"local@omm":[{"scope":"project","projectPath":"C:/x"}],"lost@gone":[{"scope":"user"}],"priv@corp":[{"scope":"user"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            paths.plugins().join("known_marketplaces.json"),
            r#"{"omm":{"source":{"source":"github","repo":"o/omm"}},"corp":{"source":{"source":"git","url":"https://bot:ghp_secret@git.corp/m.git"}}}"#,
        )
        .unwrap();

        let (content, skipped) = capture(&paths);
        let names: Vec<&str> = content.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["blobs", "pdf"], "{skipped:?}");
        let pdf = &content.skills[1];
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
        assert!(
            !skipped.iter().any(|s| s.name.contains(".DS_Store")),
            "junk is not news"
        );
        assert_eq!(reason("notes"), "no SKILL.md");
        assert!(reason("big").starts_with("larger than"));
        assert!(reason("lost@gone").contains("marketplace"));
        assert!(reason("priv@corp").contains("password"));
        assert!(!format!("{content:?}{skipped:?}").contains("ghp_secret"));
        let blobs = skipped
            .iter()
            .filter(|s| s.name.starts_with("blobs"))
            .count();
        assert_eq!(
            blobs,
            MAX_SKIPPED_PER_SKILL + 1,
            "capped, with a count of the rest"
        );
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
        // The capture fits its messages.
        let parts = content.split(fleet_proto::kit::PART_BUDGET);
        let n = parts.len();
        for (i, p) in parts.into_iter().enumerate() {
            let body = fleet_proto::Body::KitCapture(fleet_proto::KitCapture {
                id: "c".into(),
                part: i as u32,
                last: i + 1 == n,
                content: p,
                skipped: if i == 0 { skipped.clone() } else { vec![] },
            });
            assert!(fleet_proto::kit::fits(&body));
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_skill_is_written_only_where_the_kit_owns_the_directory() {
        let root = temp("skills");
        let paths = KitPaths::at(root.join("cfg"));
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
        let marker = read_marker(&paths.skills().join("mine"), "mine").unwrap();
        assert_eq!(marker.hash, skill_hash(&mine.files));
        assert_eq!(marker.files, ["SKILL.md"]);
        assert_eq!(
            std::fs::read_dir(staging(&paths))
                .map(|d| d.count())
                .unwrap_or(0),
            0,
            "nothing staged is left behind"
        );
        assert!(
            !paths.skills().join("same").join(SKILL_MARKER).exists(),
            "not taken over"
        );

        // Tools leave things beside a kit skill (bytecode, an OS's index), and the owner may add
        // a note of their own: none of it is a change to the skill.
        let mine_dir = paths.skills().join("mine");
        std::fs::create_dir_all(mine_dir.join("__pycache__")).unwrap();
        std::fs::write(mine_dir.join("__pycache__/x.pyc"), [0u8, 1]).unwrap();
        std::fs::write(mine_dir.join(".DS_Store"), "junk").unwrap();
        std::fs::write(mine_dir.join("my-notes.md"), "the owner's note").unwrap();
        assert_eq!(
            plan_skill(&mine_dir, &skill_hash(&mine.files), "mine"),
            SkillPlan::UpToDate
        );

        // An update reaches the kit's own copy, and keeps the owner's note.
        let mine2 = KitSkill {
            name: "mine".into(),
            files: vec![file("SKILL.md", "kit version 2")],
        };
        assert_eq!(
            plan_skill(&mine_dir, &skill_hash(&mine2.files), "mine"),
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
            std::fs::read_to_string(mine_dir.join("SKILL.md")).unwrap(),
            "kit version 2"
        );
        assert_eq!(
            std::fs::read_to_string(mine_dir.join("my-notes.md")).unwrap(),
            "the owner's note"
        );

        // A copy of the kit's skill under another name is the owner's, not the kit's.
        let copy = paths.skills().join("copied");
        std::fs::create_dir_all(&copy).unwrap();
        for f in ["SKILL.md", SKILL_MARKER] {
            std::fs::copy(mine_dir.join(f), copy.join(f)).unwrap();
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

        // Dropped from the kit: the kit's files go; the owner's note, the owner's skills, the copy
        // and the edited one stay.
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[], &mut out);
        let s = by_item(&out);
        assert_eq!(s[&("skill".into(), "mine".into())].0, "removed");
        assert_eq!(s[&("skill".into(), "edited".into())].0, "conflict");
        assert!(!mine_dir.join("SKILL.md").exists(), "no longer a skill");
        assert!(
            mine_dir.join("my-notes.md").exists(),
            "the owner's note stays"
        );
        assert!(paths.skills().join("theirs/SKILL.md").exists());
        assert!(paths.skills().join("same/SKILL.md").exists());
        assert!(paths.skills().join("copied/SKILL.md").exists());
        assert!(paths.skills().join("edited/SKILL.md").exists());
        assert_eq!(
            out.len(),
            2,
            "only the kit's own skills are looked at: {out:?}"
        );

        // A kit skill nobody added to goes whole.
        let plain = KitSkill {
            name: "plain".into(),
            files: vec![file("SKILL.md", "plain")],
        };
        apply_skills(&paths, &mut state, &[plain], &mut Vec::new());
        apply_skills(&paths, &mut state, &[], &mut Vec::new());
        assert!(!paths.skills().join("plain").exists());

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
    fn an_mcp_server_is_replaced_only_when_the_kit_changed_it() {
        let desired = serde_json::json!({"type": "stdio", "command": "npx", "env": {"T": "v"}});
        // What `claude mcp add-json` stores for it: `args: []` added.
        let stored =
            serde_json::json!({"type": "stdio", "command": "npx", "args": [], "env": {"T": "v"}});
        let edited = serde_json::json!({"command": "mine"});
        let rec = McpRecord {
            desired: config_hash(&desired),
            stored: config_hash(&stored),
        };
        assert_eq!(plan_mcp(None, None, &desired), McpPlan::Add);
        assert_eq!(
            plan_mcp(Some(&stored), Some(&rec), &desired),
            McpPlan::UpToDate,
            "what the CLI normalised is not a change"
        );
        let newer = serde_json::json!({"type": "stdio", "command": "npx", "env": {"T": "w"}});
        assert_eq!(
            plan_mcp(Some(&stored), Some(&rec), &newer),
            McpPlan::Replace,
            "the kit's own config changed"
        );
        assert_eq!(
            plan_mcp(Some(&edited), Some(&rec), &desired),
            McpPlan::Edited,
            "a config changed on the machine since the kit added it is the owner's"
        );
        assert_eq!(
            plan_mcp(Some(&stored), None, &desired),
            McpPlan::OwnIdentical,
            "the owner's own copy, as the CLI keeps it, is identical"
        );
        assert_eq!(plan_mcp(Some(&edited), None, &desired), McpPlan::Conflict);
    }

    #[test]
    fn only_secrets_are_scrubbed_from_transcripts() {
        let servers: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"a":{"command":"x","env":{"API_KEY":"secret-1","MODE":"fast","LONG":"abcdefghijkl"}},"b":{"url":"u","headers":{"Authorization":"Bearer abcdef"}},"c":{"command":"y","args":["--api-key","k-123456","postgres://u:pw-123@db/x","--mode","production"]}}"#,
        )
        .unwrap();
        let v = mcp_secret_values(&servers);
        for secret in ["secret-1", "Bearer abcdef", "k-123456", "pw-123"] {
            assert!(
                v.iter().any(|s| s == secret),
                "{secret} not scrubbed: {v:?}"
            );
        }
        // Plain words and flags stay readable in a transcript.
        for plain in ["production", "--api-key"] {
            assert!(!v.iter().any(|s| s == plain), "{plain} scrubbed: {v:?}");
        }
    }

    #[test]
    fn a_copy_left_in_staging_is_never_deleted_and_holds_its_skill() {
        let root = temp("leftover");
        let paths = KitPaths::at(root.join("cfg"));
        let mut state = State::default();
        let v1 = KitSkill {
            name: "held".into(),
            files: vec![file("SKILL.md", "v1")],
        };
        apply_skills(&paths, &mut state, &[v1], &mut Vec::new());
        // An earlier apply could not put a copy back: it sits in staging.
        let left = staging(&paths).join("old~held~abcd1234");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::write(left.join("SKILL.md"), "the only copy").unwrap();
        let v2 = KitSkill {
            name: "held".into(),
            files: vec![file("SKILL.md", "v2")],
        };
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[v2], &mut out);
        assert_eq!(out[0].state, "failed", "{out:?}");
        assert!(
            out[0].detail.contains("old~held~abcd1234"),
            "says where it is: {out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(left.join("SKILL.md")).unwrap(),
            "the only copy"
        );
        // Removing the skill from the kit leaves it alone too.
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[], &mut out);
        assert_eq!(out[0].state, "failed", "{out:?}");
        assert!(left.join("SKILL.md").exists() && paths.skills().join("held/SKILL.md").exists());
        // Once a person deletes it, the kit carries on.
        std::fs::remove_dir_all(&left).unwrap();
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[], &mut out);
        assert_eq!(out[0].state, "removed", "{out:?}");
        // What an apply could not delete of its own is trash, never a leftover.
        std::fs::create_dir_all(staging(&paths).join("trash~x~abcd1234")).unwrap();
        let again = KitSkill {
            name: "held".into(),
            files: vec![file("SKILL.md", "v3")],
        };
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[again], &mut out);
        assert_eq!(out[0].state, "installed", "{out:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unreadable_record_is_set_aside_and_said() {
        let root = temp("state");
        std::fs::write(State::path(&root), "{ half written").unwrap();
        let state = State::load(&root);
        assert!(
            state.plugins.is_empty() && state.mcp.is_empty(),
            "nothing is taken from it"
        );
        assert!(
            !State::path(&root).exists(),
            "set aside, not overwritten later"
        );
        let said = State::trouble(&root).expect("the node's status says so");
        assert!(said.contains(UNREADABLE), "{said}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_owners_files_in_a_kit_skill_are_never_overwritten_or_deleted() {
        let root = temp("owners-files");
        let paths = KitPaths::at(root.join("cfg"));
        let mut state = State::default();
        let v1 = KitSkill {
            name: "col".into(),
            files: vec![file("SKILL.md", "v1")],
        };
        apply_skills(&paths, &mut state, &[v1], &mut Vec::new());
        let dir = paths.skills().join("col");
        // The owner adds a note and a tooling directory of their own.
        std::fs::create_dir_all(dir.join("ref")).unwrap();
        std::fs::write(dir.join("ref/notes.md"), "the owner's note").unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "x").unwrap();
        // The kit's next version has a file exactly where the owner's note is: the skill is left
        // alone rather than the note overwritten.
        let v2 = KitSkill {
            name: "col".into(),
            files: vec![file("SKILL.md", "v2"), file("ref/notes.md", "the kit's")],
        };
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[v2], &mut out);
        assert_eq!(out[0].state, "conflict", "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("ref/notes.md")).unwrap(),
            "the owner's note"
        );
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), "v1");
        // A version that does not collide is written, and keeps both of the owner's things.
        let v3 = KitSkill {
            name: "col".into(),
            files: vec![file("SKILL.md", "v3")],
        };
        let mut out = Vec::new();
        apply_skills(&paths, &mut state, &[v3], &mut out);
        assert_eq!(out[0].state, "installed", "{out:?}");
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), "v3");
        assert!(
            dir.join("ref/notes.md").exists() && dir.join("node_modules/pkg/index.js").exists()
        );
        // Removed from the kit: its files go, the owner's stay.
        apply_skills(&paths, &mut state, &[], &mut Vec::new());
        assert!(!dir.join("SKILL.md").exists());
        assert!(
            dir.join("ref/notes.md").exists() && dir.join("node_modules/pkg/index.js").exists()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unreadable_config_is_never_taken_for_an_empty_one() {
        let root = temp("unreadable");
        let paths = KitPaths::at(root.join("cfg"));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        assert!(
            user_mcp(&paths.claude_json).unwrap().is_empty(),
            "absent: empty"
        );
        std::fs::write(&paths.claude_json, r#"{"mcpServers": {"a": "#).unwrap();
        assert!(
            user_mcp(&paths.claude_json).is_err(),
            "half written: an error"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
