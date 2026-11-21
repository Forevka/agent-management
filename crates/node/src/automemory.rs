//! Claude Code's own memory ("auto memory") in fleet sessions.
//!
//! Claude keeps notes for itself between sessions — the owner's rules, a project's gotchas —
//! as Markdown files in a folder per project, and loads their index (`MEMORY.md`) at every
//! start. Left alone, that folder is `<config>/projects/<the working directory>/memory/`, which
//! has two problems for a fleet:
//! - it is the **same folder the owner's own Claude Code uses** in that repository, so the
//!   agents' rules load into the owner's sessions and the owner's notes into the agents';
//! - it never leaves the machine, so an agent of the same project on another node, the web UI
//!   and the fleet's long-term memory (Hindsight) never see what was learned.
//!
//! So each session is pointed at a folder of the node's own (`autoMemoryDirectory`, through a
//! `--settings` file; verified with claude 2.1.268): one per project, and one for the brain. It
//! starts as a copy of what Claude's default folder held for that directory, so nothing a
//! session already relied on is lost. After every turn, and when the run ends, the notes that
//! changed are sent to the orchestrator ([`NOTE_TOOL`]), which keeps each one as a document in
//! Hindsight, replaced when the note changes and removed when it is deleted.
//!
//! What is copied in is not all sent on. A project's default folder is the owner's own, shared
//! with their interactive sessions in that repository: those notes stay on the node, where the
//! sessions keep reading them, and only what the fleet's sessions write from then on goes out.
//! The brain's default folder belongs to the fleet alone (it works in a directory of its own),
//! so what it holds is sent too. The secretary and the retrospective get none of this: their
//! sessions are sealed (`--setting-sources ""`, no `--settings`) and cannot write files anyway.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use fleet_proto::SpawnAgent;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::Node;
use crate::tools::BRAIN_ROLE;

/// The orchestrator's tool that keeps one note (see `memory_note` there). Only a node sends it,
/// for one of its runs, through the same path as an agent's own tool calls; the IPC server
/// refuses it from an agent. An orchestrator that does not know it answers with an error, and
/// the node tries again after the next turn.
pub const NOTE_TOOL: &str = "memory_note";

/// Claude's index of the notes, loaded at every start; it only points at the notes, which are
/// what is kept.
const INDEX: &str = "MEMORY.md";

/// The most of one note that is sent. Claude keeps them short; anything longer is not a note.
const NOTE_MAX: usize = 16_000;

/// Whose notes a run keeps: its project's, or the brain's own. `None` for the sealed sessions
/// (the secretary and the retrospective), which are left as they are.
pub fn key(spawn: &SpawnAgent) -> Option<String> {
    match spawn.role.as_str() {
        crate::skill::SECRETARY_ROLE | crate::skill::RETRO_ROLE => None,
        BRAIN_ROLE => Some(format!(
            "agent-{}",
            crate::runner::sanitize(&spawn.agent_id)
        )),
        _ => Some(format!(
            "project-{}",
            crate::runner::sanitize(&spawn.project)
        )),
    }
}

/// Whether the notes a run's folder starts with are sent on: only when the folder they came
/// from was the fleet's own, never the owner's (see the module's comment).
pub fn sends_seeded(spawn: &SpawnAgent) -> bool {
    spawn.role == BRAIN_ROLE
}

/// The folder a run's notes live in.
pub fn dir(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join("memory").join(key)
}

/// What has been sent from a folder: each note's name and the hash of what was sent.
fn state_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join("memory-sent").join(format!("{key}.json"))
}

/// The folder Claude would use for `cwd` on its own: every character of the path that is not a
/// letter or a digit becomes `-` (`C:\repos\app` is `C--repos-app`).
pub fn claude_default(config_dir: &Path, cwd: &Path) -> PathBuf {
    let name: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config_dir.join("projects").join(name).join("memory")
}

/// A file name the orchestrator keeps as a note: plain ASCII, Markdown, not the index. The same
/// rule as its `note_document`, so nothing is sent only to be refused after every turn.
pub fn is_note_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    name != INDEX
        && !stem.is_empty()
        && stem.len() <= 120
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A note's text as it is sent and hashed.
fn note_text(raw: String) -> String {
    raw.chars().take(NOTE_MAX).collect()
}

fn hash(text: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
}

fn load_state(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Written whole or not at all: a half-written record would send every note again.
fn save_state(path: &Path, sent: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(sent).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

/// Creates the run's folder the first time, as a copy of the notes in `seed` (Claude's default
/// folder for the run's directory), so a session keeps what it had before the fleet moved it.
/// Unless `send_seeded`, what was copied is recorded as already sent: it stays on the node.
pub fn prepare(data_dir: &Path, key: &str, seed: Option<&Path>, send_seeded: bool) -> Result<()> {
    let dir = dir(data_dir, key);
    if dir.is_dir() {
        return Ok(());
    }
    // Copied beside it and moved into place, so a crash mid-copy leaves no folder that looks
    // done.
    let parent = dir.parent().context("memory folder has no parent")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{key}.seeding-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut seeded = BTreeMap::new();
    for entry in seed
        .and_then(|s| std::fs::read_dir(s).ok())
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_file() || !name.ends_with(".md") {
            continue;
        }
        std::fs::copy(&path, tmp.join(&name))
            .with_context(|| format!("copying {}", path.display()))?;
        if let Ok(text) = std::fs::read_to_string(&path) {
            seeded.insert(name, hash(&note_text(text)));
        }
    }
    if !send_seeded {
        seeded.retain(|name, _| is_note_name(name));
        save_state(&state_path(data_dir, key), &seeded)?;
    }
    std::fs::rename(&tmp, &dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(())
}

/// The settings file that points Claude's memory at `dir`. A file rather than JSON on the
/// command line: quoted arguments do not survive Windows batch wrappers such as `claude.cmd`.
pub fn settings_file(agent_dir: &Path, dir: &Path) -> Result<PathBuf> {
    let path = agent_dir.join("settings.json");
    let text = serde_json::to_string_pretty(&json!({
        "autoMemoryDirectory": dir.display().to_string(),
    }))?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// One note to send: its file name, and its text, or `None` once it is deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub content: Option<String>,
    hash: String,
}

/// The notes in `dir` that differ from what `sent` says went out: new or changed ones with their
/// text, and deleted ones.
pub fn changes(dir: &Path, sent: &BTreeMap<String, String>) -> Vec<Change> {
    let mut now = BTreeMap::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !is_note_name(&name) || !path.is_file() {
            continue;
        }
        // A note being written as we look is read again after the next turn.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        now.insert(name, note_text(text));
    }
    let mut out: Vec<Change> = now
        .into_iter()
        .filter_map(|(name, text)| {
            let h = hash(&text);
            (sent.get(&name) != Some(&h)).then_some(Change {
                name,
                content: Some(text),
                hash: h,
            })
        })
        .collect();
    for name in sent.keys() {
        if !dir.join(name).is_file() {
            out.push(Change {
                name: name.clone(),
                content: None,
                hash: String::new(),
            });
        }
    }
    out
}

/// One sync at a time per folder (the agents of a project share one, and its record of what was
/// sent); different folders do not wait on each other.
fn folder_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);
    LOCKS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(key.to_owned())
        .or_default()
        .clone()
}

/// Sends the notes of `key`'s folder that changed since the last sync, as the run whose IPC
/// token this is. What the orchestrator accepted is recorded; the rest is tried again after the
/// next turn. Nothing is tried while the link is down, and a pass stops at the first note that
/// fails: each call would wait out the same timeout, and queue a retain for when the link is
/// back.
pub async fn sync(node: &Node, data_dir: &Path, key: &str, ipc_token: &str) {
    if !node.is_connected() {
        return;
    }
    let lock = folder_lock(key);
    let _one = lock.lock().await;
    let (dir, state) = (dir(data_dir, key), state_path(data_dir, key));
    let mut sent = load_state(&state);
    let pending = changes(&dir, &sent);
    if pending.is_empty() {
        return;
    }
    let mut changed = false;
    for c in pending {
        let args = match &c.content {
            Some(text) => json!({"name": c.name, "content": text}),
            None => json!({"name": c.name, "deleted": true}),
        };
        let r = node
            .call_tool(ipc_token, NOTE_TOOL.into(), args.to_string())
            .await;
        if !r.ok {
            tracing::debug!(note = %c.name, "memory note not kept, trying after the next turn: {}", r.result);
            break;
        }
        changed = true;
        match c.content {
            Some(_) => sent.insert(c.name, c.hash),
            None => sent.remove(&c.name),
        };
    }
    if changed && let Err(e) = save_state(&state, &sent) {
        tracing::warn!("recording the memory notes sent failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_names_a_projects_folder_after_its_path() {
        let cfg = Path::new("cfg");
        assert_eq!(
            claude_default(
                cfg,
                Path::new("/home/fleet/.local/share/fleet/agents/retro/work")
            ),
            cfg.join("projects")
                .join("-home-fleet--local-share-fleet-agents-retro-work")
                .join("memory")
        );
        assert_eq!(
            claude_default(cfg, Path::new(r"C:\repos\bioinsight\ai-workspace")),
            cfg.join("projects")
                .join("C--repos-bioinsight-ai-workspace")
                .join("memory")
        );
    }

    fn temp(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("fleet-automemory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn seed(root: &Path) -> PathBuf {
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::write(seed.join("MEMORY.md"), "- [Rule](rule.md)").unwrap();
        std::fs::write(seed.join("rule.md"), "Address the owner informally.").unwrap();
        std::fs::write(seed.join("notes.txt"), "not a note").unwrap();
        seed
    }

    #[test]
    fn a_folder_starts_as_a_copy_of_claudes_and_only_what_changed_is_sent() {
        let root = temp("brain");
        let seed = seed(&root);
        // The brain's: what was copied in is the fleet's, and goes out.
        prepare(&root, "agent-brain", Some(&seed), true).unwrap();
        let mine = dir(&root, "agent-brain");
        assert!(mine.join("rule.md").is_file() && mine.join("MEMORY.md").is_file());
        assert!(!mine.join("notes.txt").exists());
        // Only once: a folder that exists is the session's own from then on.
        std::fs::write(seed.join("later.md"), "x").unwrap();
        prepare(&root, "agent-brain", Some(&seed), true).unwrap();
        assert!(!mine.join("later.md").exists());

        // The note goes out, the index never does.
        let mut sent = load_state(&state_path(&root, "agent-brain"));
        let c = changes(&mine, &sent);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "rule.md");
        assert_eq!(
            c[0].content.as_deref(),
            Some("Address the owner informally.")
        );
        sent.insert(c[0].name.clone(), c[0].hash.clone());
        assert!(changes(&mine, &sent).is_empty(), "nothing changed");

        // A change, a new note, a deletion, and a name the orchestrator would refuse.
        std::fs::write(mine.join("rule.md"), "Address the owner as ти.").unwrap();
        std::fs::write(mine.join("jira.md"), "Keep statuses current.").unwrap();
        std::fs::write(mine.join("with space.md"), "refused").unwrap();
        sent.insert("gone.md".into(), "abc".into());
        let mut c = changes(&mine, &sent);
        c.sort_by(|a, b| a.name.cmp(&b.name));
        let got: Vec<(&str, Option<&str>)> = c
            .iter()
            .map(|c| (c.name.as_str(), c.content.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("gone.md", None),
                ("jira.md", Some("Keep statuses current.")),
                ("rule.md", Some("Address the owner as ти.")),
            ]
        );
        save_state(&state_path(&root, "agent-brain"), &sent).unwrap();
        assert_eq!(load_state(&state_path(&root, "agent-brain")), sent);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn notes_copied_from_the_owners_folder_stay_on_the_node() {
        let root = temp("project");
        let seed = seed(&root);
        prepare(&root, "project-bioins", Some(&seed), false).unwrap();
        let mine = dir(&root, "project-bioins");
        // The session still reads them; none is sent on, until a session changes one.
        assert!(mine.join("rule.md").is_file());
        let sent = load_state(&state_path(&root, "project-bioins"));
        assert!(changes(&mine, &sent).is_empty());
        std::fs::write(mine.join("rule.md"), "Changed by an agent.").unwrap();
        std::fs::write(mine.join("new.md"), "Written by an agent.").unwrap();
        let names: Vec<String> = changes(&mine, &sent).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["new.md", "rule.md"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_brain_and_each_project_keep_a_folder_and_sealed_sessions_none() {
        let mut s = SpawnAgent {
            agent_id: "brain".into(),
            task_id: "brain".into(),
            project: "fleet".into(),
            role: "orchestrator".into(),
            prompt: String::new(),
            append_system_prompt: None,
            resume_session_id: None,
            permission_mode: "plan".into(),
            after_plan_mode: None,
            model: None,
            extra_args: vec![],
            extra_mcp_servers: None,
            secret_files: vec![],
            run_id: "r".into(),
        };
        assert_eq!(key(&s).as_deref(), Some("agent-brain"));
        assert!(sends_seeded(&s));
        for sealed in ["secretary", "retro"] {
            s.role = sealed.into();
            assert_eq!(key(&s), None, "{sealed}");
        }
        s.role = "implementor".into();
        s.agent_id = "alice-2".into();
        s.project = "bioins".into();
        assert_eq!(key(&s).as_deref(), Some("project-bioins"));
        assert!(!sends_seeded(&s));
        assert!(is_note_name("owner-rule_v2.md"));
        for bad in ["MEMORY.md", "x.txt", ".md", ".hidden.md", "a b.md", "é.md"] {
            assert!(!is_note_name(bad), "{bad}");
        }
    }
}
