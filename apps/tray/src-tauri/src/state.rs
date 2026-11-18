//! Reading and changing the node's project mappings, kept free of Tauri so it can be unit tested.
//!
//! The mapping rules (slug validation, canonical paths, re-map replaces) and the file formats
//! belong to `fleet_node_config`, shared with the node. The tray touches only the `projects` key
//! of `node.json` and writes every other key back verbatim, so a tray older than the node cannot
//! drop settings it does not know. Every change reloads the file first because the node
//! rewrites it too.

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
pub use fleet_node_config::status::Status;
use fleet_node_config::{NodeConfig, write_private};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::process::{self, Liveness};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mapping {
    pub slug: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrayState {
    pub data_dir: String,
    pub mappings: Vec<Mapping>,
    pub status: Option<Status>,
    /// The process named by `status.pid` is not running, so the status is left over from a
    /// node that has stopped.
    pub status_stale: bool,
}

/// The status file is optional and advisory; `Status::read` is lenient (see there).
pub fn read_status(data_dir: &Path) -> Option<Status> {
    Status::read(data_dir)
}

pub fn get_state(data_dir: &Path) -> Result<TrayState> {
    let cfg = projects_of(&load_doc(data_dir)?, data_dir)?;
    // `NodeConfig` never writes a slug twice, but a hand-edited file might; the node uses the
    // first entry, so the tray shows that one.
    let mut seen = HashSet::new();
    let mappings = cfg
        .projects
        .into_iter()
        .filter(|p| seen.insert(p.project.clone()))
        .map(|p| Mapping {
            slug: p.project,
            path: p.path,
        })
        .collect();
    let status = read_status(data_dir);
    let status_stale = status.as_ref().is_some_and(is_stale);
    Ok(TrayState {
        data_dir: data_dir.display().to_string(),
        mappings,
        status,
        status_stale,
    })
}

pub fn map_project(data_dir: &Path, slug: &str, path: &Path) -> Result<()> {
    // A relative path would be resolved against the tray's working directory, which means
    // nothing to the person who picked it.
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    edit_projects(data_dir, |cfg| cfg.map_project(slug, path))
}

pub fn unmap_project(data_dir: &Path, slug: &str) -> Result<()> {
    edit_projects(data_dir, |cfg| {
        if !cfg.unmap_project(slug) {
            bail!("{slug} is not mapped");
        }
        Ok(())
    })
}

/// Returns `path` only if it is a currently mapped project and still a folder. This keeps the
/// window to opening folders the owner mapped, rather than any path it is handed.
pub fn mapped_dir(data_dir: &Path, path: &str) -> Result<PathBuf> {
    let cfg = projects_of(&load_doc(data_dir)?, data_dir)?;
    if !cfg.projects.iter().any(|p| p.path == path) {
        bail!("{path} is not a mapped project folder");
    }
    let path = PathBuf::from(path);
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Ok(path),
        Ok(_) => bail!("{} is not a folder", path.display()),
        Err(e) => Err(e).with_context(|| format!("opening {}", path.display())),
    }
}

/// Appends a line to `<data_dir>/logs/tray.log`. The tray has no console, so this is where
/// problems that are not shown in the window end up.
pub fn log(data_dir: &Path, msg: &str) -> Result<()> {
    let dir = data_dir.join("logs");
    std::fs::create_dir_all(&dir)?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tray.log"))?;
    writeln!(f, "{secs} {msg}")?;
    Ok(())
}

fn load_doc(data_dir: &Path) -> Result<Map<String, Value>> {
    let path = NodeConfig::path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))? {
        Value::Object(doc) => Ok(doc),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

/// Deserializes only `projects`, into a `NodeConfig` so its mapping rules can be reused.
fn projects_of(doc: &Map<String, Value>, data_dir: &Path) -> Result<NodeConfig> {
    let projects = match doc.get("projects") {
        None | Some(Value::Null) => Value::Array(Vec::new()),
        Some(p) => p.clone(),
    };
    serde_json::from_value(serde_json::json!({ "projects": projects })).with_context(|| {
        format!(
            "parsing projects in {}",
            NodeConfig::path(data_dir).display()
        )
    })
}

/// Left over from a node that has stopped: its pid is gone, or now belongs to another process.
/// Without a pid, or when that cannot be decided, the status is taken at its word.
pub fn is_stale(status: &Status) -> bool {
    status.pid.is_some_and(|pid| {
        process::liveness(pid) == Liveness::Gone || status.same_process() == Some(false)
    })
}

fn edit_projects(data_dir: &Path, edit: impl FnOnce(&mut NodeConfig) -> Result<()>) -> Result<()> {
    // The lock the node and `fleet project map` hold for their own load, change and save.
    let _lock = NodeConfig::lock(data_dir)?;
    let mut doc = load_doc(data_dir)?;
    let mut cfg = projects_of(&doc, data_dir)?;
    edit(&mut cfg)?;
    // Entries the edit left alone are written back as they were, fields we do not model included.
    let originals: Vec<Value> = match doc.get("projects") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let projects = cfg
        .projects
        .iter()
        .map(|p| {
            let same = |v: &&Value| {
                v.get("project").and_then(Value::as_str) == Some(p.project.as_str())
                    && v.get("path").and_then(Value::as_str) == Some(p.path.as_str())
            };
            match originals.iter().find(same) {
                Some(v) => Ok(v.clone()),
                None => serde_json::to_value(p),
            }
        })
        .collect::<serde_json::Result<Vec<_>>>()?;
    doc.insert("projects".into(), Value::Array(projects));
    save_doc(data_dir, &doc)
}

fn save_doc(data_dir: &Path, doc: &Map<String, Value>) -> Result<()> {
    let path = NodeConfig::path(data_dir);
    // The same atomic, owner-only write the node uses, with a temp name unique to this process.
    write_private(&path, serde_json::to_string_pretty(doc)?.as_bytes())
        .with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fleet-tray-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn map_unmap_round_trip_through_node_json() {
        let root = temp_dir("map");
        let data = root.join("data");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let state = get_state(&data).unwrap();
        assert!(state.mappings.is_empty());
        assert_eq!(state.status, None);

        map_project(&data, "demo", &project).unwrap();
        assert!(NodeConfig::path(&data).exists(), "node.json is written");
        // The node must still be able to read what the tray wrote.
        assert_eq!(NodeConfig::load(&data).unwrap().projects.len(), 1);
        let state = get_state(&data).unwrap();
        assert_eq!(state.mappings.len(), 1);
        assert_eq!(state.mappings[0].slug, "demo");

        // Re-mapping replaces rather than duplicating.
        map_project(&data, "demo", &project).unwrap();
        assert_eq!(get_state(&data).unwrap().mappings.len(), 1);

        let mapped = state.mappings[0].path.clone();
        assert!(mapped_dir(&data, &mapped).is_ok());
        assert!(mapped_dir(&data, &root.display().to_string()).is_err());

        unmap_project(&data, "demo").unwrap();
        assert!(get_state(&data).unwrap().mappings.is_empty());
        assert!(unmap_project(&data, "demo").is_err());

        let leftovers: Vec<_> = std::fs::read_dir(&data)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files are left behind");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn edits_only_projects_and_keeps_other_keys_verbatim() {
        let root = temp_dir("keys");
        let project = root.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let settings = serde_json::json!({
            "claude_bin": "claude",
            "a_field_from_a_newer_node": { "nested": [1, 2, 3] }
        });
        let original = serde_json::json!({
            "projects": [{ "project": "old", "path": "C:/somewhere", "extra": true }],
            "settings": settings,
            "future_key": 42
        });
        std::fs::write(NodeConfig::path(&root), original.to_string()).unwrap();

        map_project(&root, "demo", &project).unwrap();
        let doc = read_json(&NodeConfig::path(&root));
        assert_eq!(doc["settings"], settings);
        assert_eq!(doc["future_key"], 42);
        let slugs: Vec<_> = doc["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["project"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(slugs, ["demo", "old"]);
        assert_eq!(
            doc["projects"][1]["extra"], true,
            "untouched entries keep unknown fields"
        );

        unmap_project(&root, "old").unwrap();
        let doc = read_json(&NodeConfig::path(&root));
        assert_eq!(doc["settings"], settings);
        assert_eq!(doc["future_key"], 42);
        assert_eq!(doc["projects"].as_array().unwrap().len(), 1);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn map_rejects_bad_input() {
        let root = temp_dir("bad");
        let err = map_project(&root, "Bad Slug", &root).unwrap_err();
        assert!(err.to_string().contains("project slug"), "{err}");
        assert!(map_project(&root, "ok", &root.join("missing")).is_err());
        let err = map_project(&root, "ok", Path::new("relative/dir")).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
        assert!(!NodeConfig::path(&root).exists(), "nothing was written");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_node_json_names_the_file() {
        let root = temp_dir("broken");
        std::fs::write(NodeConfig::path(&root), "{not json").unwrap();
        let err = format!("{:#}", get_state(&root).unwrap_err());
        assert!(err.contains("node.json"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn duplicate_mappings_show_once() {
        let root = temp_dir("dupmap");
        std::fs::write(
            NodeConfig::path(&root),
            r#"{"projects":[{"project":"a","path":"C:/one"},{"project":"a","path":"C:/two"},
                {"project":"b","path":"C:/three"}]}"#,
        )
        .unwrap();
        let state = get_state(&root).unwrap();
        let got: Vec<_> = state
            .mappings
            .iter()
            .map(|m| (m.slug.as_str(), m.path.as_str()))
            .collect();
        assert_eq!(got, [("a", "C:/one"), ("b", "C:/three")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn status_from_a_stopped_node_is_stale() {
        let root = temp_dir("stale");
        let write = |pid: u32| {
            std::fs::write(
                Status::path(&root),
                format!(r#"{{"connected":true,"pid":{pid},"projects":[]}}"#),
            )
            .unwrap()
        };
        write(std::process::id());
        assert!(!get_state(&root).unwrap().status_stale);
        write(crate::process::tests::dead_pid());
        assert!(get_state(&root).unwrap().status_stale);
        std::fs::write(Status::path(&root), r#"{"connected":true}"#).unwrap();
        assert!(
            !get_state(&root).unwrap().status_stale,
            "no pid, no verdict"
        );

        // A live pid with the start stamp of another process: the pid was reused.
        let me = std::process::id();
        let started = fleet_node_config::process::start_time(me).unwrap();
        let write_started = |started: u64| {
            std::fs::write(
                Status::path(&root),
                format!(r#"{{"connected":true,"pid":{me},"pid_started":{started}}}"#),
            )
            .unwrap()
        };
        write_started(started);
        assert!(!get_state(&root).unwrap().status_stale, "the very process");
        write_started(started + 1);
        assert!(get_state(&root).unwrap().status_stale, "a reused pid");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn edits_wait_for_the_config_lock() {
        let root = temp_dir("lock");
        let project = root.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let held = NodeConfig::lock(&root).unwrap();
        let worker = {
            let (root, project) = (root.clone(), project.clone());
            std::thread::spawn(move || map_project(&root, "demo", &project))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !worker.is_finished(),
            "blocked while another writer holds the lock"
        );
        assert!(!NodeConfig::path(&root).exists());
        drop(held);
        worker.join().unwrap().unwrap();
        assert_eq!(get_state(&root).unwrap().mappings.len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
