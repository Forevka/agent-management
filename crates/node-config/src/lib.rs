//! A node's local files in its data directory: `node.json` (its configuration) and
//! `status.json` (what it tells the tray app). Shared by the node and `apps/tray`, which is why
//! this crate does not depend on the network stack.

pub mod process;
pub mod status;

use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use fleet_proto::{NodeSettings, ProjectMapping};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Server-side project slugs mapped to directories on this host.
    #[serde(default)]
    pub projects: Vec<ProjectMapping>,
    /// Last settings pushed by the orchestrator (how to start Claude).
    #[serde(default)]
    pub settings: Option<NodeSettings>,
}

impl NodeConfig {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("node.json")
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes the file atomically. It does not take the lock: a load, change and save sequence
    /// goes through [`NodeConfig::update`] (or holds [`NodeConfig::lock`]) instead.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        // It may hold a Claude token or API key.
        write_private(&path, serde_json::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("saving {}", path.display()))
    }

    /// Takes the lock that every writer of `node.json` holds from loading the file to saving it,
    /// so that the node, `fleet project map` and the tray app never save over a change one of
    /// the others made in between. Held until the guard is dropped; blocks while another
    /// process (or another guard in this one) holds it. Plain reads do not need it: saves are
    /// atomic renames.
    pub fn lock(data_dir: &Path) -> Result<ConfigLock> {
        let path = data_dir.join("node.json.lock");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(ConfigLock { _file: file })
    }

    /// Loads the file, applies `change` and saves the result, all under [`NodeConfig::lock`].
    /// Nothing is saved when `change` fails. Returns the saved config and `change`'s result.
    pub fn update<T>(
        data_dir: &Path,
        change: impl FnOnce(&mut NodeConfig) -> Result<T>,
    ) -> Result<(NodeConfig, T)> {
        let _lock = Self::lock(data_dir)?;
        let mut cfg = Self::load(data_dir)?;
        let out = change(&mut cfg)?;
        cfg.save(data_dir)?;
        Ok((cfg, out))
    }

    /// Maps (or re-maps) a project slug to a local directory.
    pub fn map_project(&mut self, project: &str, path: &Path) -> Result<()> {
        validate_slug(project)?;
        let path = path
            .canonicalize()
            .with_context(|| format!("{} does not exist", path.display()))?;
        if !path.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        let path = display_path(&path);
        self.projects.retain(|p| p.project != project);
        self.projects.push(ProjectMapping {
            project: project.into(),
            path,
        });
        self.projects.sort_by(|a, b| a.project.cmp(&b.project));
        Ok(())
    }

    pub fn unmap_project(&mut self, project: &str) -> bool {
        let before = self.projects.len();
        self.projects.retain(|p| p.project != project);
        before != self.projects.len()
    }

    pub fn project_dir(&self, project: &str) -> Option<PathBuf> {
        self.projects
            .iter()
            .find(|p| p.project == project)
            .map(|p| PathBuf::from(&p.path))
    }
}

pub fn validate_slug(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 40
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-');
    if !ok {
        bail!("project slug must be 1-40 chars of a-z, 0-9 and '-' (got {s:?})");
    }
    Ok(())
}

/// The guard returned by [`NodeConfig::lock`]. Closing the file releases the lock.
pub struct ConfigLock {
    _file: File,
}

/// Replaces `path` atomically with `bytes`, readable only by the owner on unix, and flushed to
/// disk before the rename.
///
/// Several processes write files in the data directory (the node, `fleet project map`, the
/// tray app), so the temporary file is named after this process and a counter: two writers
/// never share one, and none can rename away another's half-written file.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic(path, bytes, true)
}

/// [`write_private`] without the flush, for files that are only advisory (`status.json`): the
/// next write replaces them anyway, so they need not survive a power cut.
pub fn write_private_unsynced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic(path, bytes, false)
}

fn write_atomic(path: &Path, bytes: &[u8], sync: bool) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(
        "{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let open = || {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&tmp)
    };
    let write = || -> std::io::Result<()> {
        let mut f = match open() {
            // Left behind by an earlier process that had this pid and crashed mid-write: the
            // name is ours now, so it can go.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&tmp)?;
                open()?
            }
            other => other?,
        };
        f.write_all(bytes)?;
        if sync {
            f.sync_all()?;
        }
        drop(f);
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `canonicalize` on Windows returns `\\?\C:\...`; strip the verbatim prefix so the path is
/// readable in the UI and usable by tools that do not understand it.
fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_save_load_unmap() {
        let dir = std::env::temp_dir().join(format!("fleet-node-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = NodeConfig::load(&dir).unwrap();
        assert!(cfg.projects.is_empty());
        cfg.map_project("demo", &dir).unwrap();
        cfg.map_project("demo", &dir).unwrap();
        assert_eq!(cfg.projects.len(), 1, "re-mapping replaces");
        cfg.save(&dir).unwrap();
        let loaded = NodeConfig::load(&dir).unwrap();
        assert_eq!(loaded, cfg);
        assert!(loaded.project_dir("demo").is_some());
        assert!(cfg.unmap_project("demo"));
        assert!(cfg.map_project("Bad Slug", &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let dir = std::env::temp_dir().join(format!("fleet-node-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Each writer appends its own entries; without the lock, writers that loaded the same
        // file would overwrite each other's additions.
        let writers: Vec<_> = (0..6)
            .map(|w| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        NodeConfig::update(&dir, |cfg| {
                            cfg.projects.push(ProjectMapping {
                                project: format!("w{w}-{i}"),
                                path: String::new(),
                            });
                            Ok(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(NodeConfig::load(&dir).unwrap().projects.len(), 60);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_update_saves_nothing() {
        let dir = std::env::temp_dir().join(format!("fleet-node-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = NodeConfig::update(&dir, |cfg| -> Result<()> {
            cfg.projects.clear();
            bail!("no")
        });
        assert!(r.is_err());
        assert!(!NodeConfig::path(&dir).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_leave_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("fleet-node-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.json");
        write_private(&path, b"1").unwrap();
        write_private(&path, b"2").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"2");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["x.json"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
