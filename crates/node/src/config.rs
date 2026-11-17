//! The node's local configuration file (`node.json` in the data directory).

use std::path::{Path, PathBuf};

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

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        // It may hold a Claude token or API key.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
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
}
