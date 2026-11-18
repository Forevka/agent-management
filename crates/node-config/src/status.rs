//! `status.json`: what a running node tells the tray app about itself.
//!
//! The node rewrites it on start, on every connect and disconnect, and whenever the
//! orchestrator sends its project list. Readers treat it as advisory: it can be missing, left
//! over from a node that has stopped (check `pid`), or written by another version.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use fleet_proto::ProjectSummary;
use serde::Serialize;
use serde_json::Value;

use crate::{process, validate_slug, write_private_unsynced};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// Whether the node's link to the orchestrator is up.
    pub connected: bool,
    pub node_name: String,
    /// The node process, so a reader can tell a live status from a stale one.
    pub pid: Option<u32>,
    /// [`crate::process::start_time`] of `pid`, so a later process that reused the pid is not
    /// taken for the node. `None` where the platform cannot tell (or from an older node).
    pub pid_started: Option<u64>,
    /// RFC 3339, UTC.
    pub updated_at: String,
    /// The orchestrator's projects. `None` until the orchestrator has sent them, which is
    /// different from it having none.
    pub projects: Option<Vec<ProjectSummary>>,
}

impl Status {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("status.json")
    }

    pub fn write(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        // Advisory and rewritten often, so not flushed to disk; still atomic and owner-only.
        write_private_unsynced(&path, serde_json::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Reads the file field by field: a missing or half-written file is `None`, and a bad field
    /// or project entry is skipped rather than discarding everything else. Project entries
    /// with an invalid slug, and repeated slugs, are dropped.
    pub fn read(data_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(data_dir)).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let v = v.as_object()?;
        let str_field = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let projects = v.get("projects").and_then(Value::as_array).map(|items| {
            let mut seen = HashSet::new();
            items
                .iter()
                .filter_map(|p| {
                    let slug = p.get("slug")?.as_str()?;
                    validate_slug(slug).ok()?;
                    let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
                    Some(ProjectSummary {
                        slug: slug.to_owned(),
                        name: name.to_owned(),
                    })
                })
                .filter(|p| seen.insert(p.slug.clone()))
                .collect()
        });
        Some(Self {
            connected: v.get("connected").and_then(Value::as_bool).unwrap_or(false),
            node_name: str_field("node_name"),
            pid: v
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok()),
            pid_started: v.get("pid_started").and_then(Value::as_u64),
            updated_at: str_field("updated_at"),
            projects,
        })
    }

    /// Whether `pid` is checked to be the very process that wrote this status: `Some(false)`
    /// when the pid is now another process, `None` when that cannot be decided (no pid, no
    /// start stamp, or the process cannot be queried or is gone).
    pub fn same_process(&self) -> Option<bool> {
        process::is_same(self.pid?, self.pid_started)
    }
}

/// The current time as RFC 3339 in UTC, e.g. `2026-11-18T09:30:00Z`.
pub fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format_utc(secs)
}

fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fleet-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn formats_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_794_987_737), "2026-11-18T07:42:17Z");
    }

    #[test]
    fn write_then_read() {
        let dir = temp_dir("rw");
        assert_eq!(Status::read(&dir), None);
        let status = Status {
            connected: true,
            node_name: "pc".into(),
            pid: Some(42),
            pid_started: Some(7),
            updated_at: now_utc(),
            projects: Some(vec![ProjectSummary {
                slug: "demo".into(),
                name: "Demo".into(),
            }]),
        };
        status.write(&dir).unwrap();
        assert_eq!(Status::read(&dir), Some(status));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_is_lenient() {
        let dir = temp_dir("lenient");
        std::fs::write(Status::path(&dir), "{not json").unwrap();
        assert_eq!(Status::read(&dir), None);

        // Bad entries, invalid slugs and duplicates are dropped; the rest is kept.
        std::fs::write(
            Status::path(&dir),
            r#"{"connected":true,"node_name":"pc","pid":42,"updated_at":"2026-01-01T00:00:00Z",
                "projects":[{"slug":"demo","name":"Demo"},{"slug":"demo","name":"Again"},
                            {"slug":"Bad Slug"},{"name":"no slug"},7,{"slug":"web"}]}"#,
        )
        .unwrap();
        let status = Status::read(&dir).unwrap();
        assert!(status.connected);
        assert_eq!(status.pid, Some(42));
        let got: Vec<_> = status
            .projects
            .unwrap()
            .into_iter()
            .map(|p| (p.slug, p.name))
            .collect();
        assert_eq!(
            got,
            [
                ("demo".into(), "Demo".into()),
                ("web".into(), String::new())
            ]
        );

        std::fs::write(Status::path(&dir), r#"{"connected":"yes","pid":-1}"#).unwrap();
        let status = Status::read(&dir).unwrap();
        assert!(!status.connected);
        assert_eq!(status.pid, None);
        assert_eq!(status.projects, None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
