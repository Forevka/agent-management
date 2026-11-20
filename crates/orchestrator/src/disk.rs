//! Free disk space on the nodes: what a node's [`DiskReport`] means, telling the owner, and
//! holding new runs on a node that is about to fill up.
//!
//! Claude Code writes every session, subagent and transcript under its config dir. On a full
//! disk it fails mid-run, cuts the session's transcript short (a resume then loses context), and
//! every task placed there afterwards fails the same way. So the orchestrator reads each node's
//! reports and keeps one state per node:
//!
//! - `ok`;
//! - `low` (below `FLEET_DISK_LOW_GB`): the owner is told once, with the biggest session folders;
//! - `full` (below `FLEET_DISK_FLOOR_GB`): new task runs are not started there and wait in the
//!   queue instead, until a report shows space again. Runs already going are left alone, and the
//!   fleet's own sessions (brain, secretary, retrospective) are never held: the owner must still
//!   be able to talk to the fleet while they free space.
//!
//! Only a change of state is told, so a disk that stays low is one message, not one per report.
//! A volume leaves a state only with 10% more than its threshold (judged per volume, from its
//! own previous state), so a disk hovering at the line does not flap between messages. A report
//! older than [`STALE_AFTER_MINUTES`] holds nothing.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use fleet_net::EndpointId;
use fleet_proto::{DiskReport, DiskVolume};

use crate::{
    chat::{From, Notify, esc},
    config::DiskLimits,
    core::Orchestrator,
    model::NodeRow,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiskState {
    Ok,
    Low,
    Full,
}

impl DiskState {
    pub fn as_str(self) -> &'static str {
        match self {
            DiskState::Ok => "ok",
            DiskState::Low => "low",
            DiskState::Full => "full",
        }
    }

    /// The stored state; anything unknown reads as `ok`, which holds nothing.
    pub fn parse(s: &str) -> Self {
        match s {
            "low" => DiskState::Low,
            "full" => DiskState::Full,
            _ => DiskState::Ok,
        }
    }
}

/// How much more than a threshold it takes to leave the state below it.
fn with_margin(bytes: u64) -> u64 {
    bytes.saturating_add(bytes / 10)
}

/// One volume's state, given the node's previous one (for the margin on the way back up).
pub fn level(free: u64, limits: &DiskLimits, prev: DiskState) -> DiskState {
    let bar = |bytes: u64, state: DiskState| {
        if prev >= state {
            with_margin(bytes)
        } else {
            bytes
        }
    };
    if free < bar(limits.floor_bytes, DiskState::Full) {
        DiskState::Full
    } else if free < bar(limits.low_bytes, DiskState::Low) {
        DiskState::Low
    } else {
        DiskState::Ok
    }
}

/// Whether the fleet writes to this volume: Claude Code's sessions or a mapped project are on it.
fn relevant(v: &DiskVolume) -> bool {
    v.claude_dir || !v.projects.is_empty()
}

/// The state of each volume the fleet writes to, keyed by volume.
pub type Levels = HashMap<String, DiskState>;

/// What a report means: the node's state (its worst volume's) and each volume's, every one
/// judged against its own previous state, so a volume that never crossed a line is not held by
/// another's margin. `None` when the report measured no volume the fleet writes to (every probe
/// failed on the node): that says nothing about the disks, so it must not lift a hold either.
pub fn evaluate(
    report: &DiskReport,
    limits: &DiskLimits,
    prev: &HashMap<String, String>,
) -> Option<(DiskState, Levels)> {
    let levels: Levels = report
        .volumes
        .iter()
        .filter(|v| relevant(v))
        .map(|v| {
            let before = prev
                .get(&v.volume)
                .map_or(DiskState::Ok, |s| DiskState::parse(s));
            (v.volume.clone(), level(v.free, limits, before))
        })
        .collect();
    let state = levels.values().copied().max()?;
    Some((state, levels))
}

/// A report older than this holds nothing: the node reports every 5 minutes, so a node that has
/// gone this long without one cannot measure (a hung network drive), and a hold it can never
/// lift would keep its work queued for good.
pub const STALE_AFTER_MINUTES: i64 = 20;

/// Whether a new run of a task in `project` must not start on this node: the node is `full`, its
/// report is recent, and a full volume is one this run would write to (Claude Code's sessions, or
/// the project's own directory). A node that never reported is never held.
pub fn held(node: &NodeRow, project: &str) -> bool {
    if DiskState::parse(&node.disk_state) != DiskState::Full {
        return false;
    }
    let fresh = node
        .disk_at
        .is_some_and(|at| chrono::Utc::now() - at < chrono::Duration::minutes(STALE_AFTER_MINUTES));
    let Some(report) = node.disk.as_ref().filter(|_| fresh) else {
        return false;
    };
    report.0.volumes.iter().any(|v| {
        (v.claude_dir || v.projects.iter().any(|p| p == project))
            && node
                .disk_levels
                .0
                .get(&v.volume)
                .is_some_and(|s| DiskState::parse(s) == DiskState::Full)
    })
}

/// The free space of the node's fullest volume the fleet writes to, for notes ("0.8 GB free").
pub fn least_free(node: &NodeRow) -> Option<u64> {
    node.disk
        .as_ref()?
        .0
        .volumes
        .iter()
        .filter(|v| relevant(v))
        .map(|v| v.free)
        .min()
}

/// A node's disks as the web UI and the brain's `admin show node(s)` see them; `null` before
/// its first report (an older node never sends one).
pub fn json(node: &NodeRow) -> serde_json::Value {
    let Some(report) = &node.disk else {
        return serde_json::Value::Null;
    };
    let r = &report.0;
    serde_json::json!({
        "state": DiskState::parse(&node.disk_state).as_str(),
        "at": node.disk_at,
        "claude_dir": r.claude_dir,
        "least_free": least_free(node),
        "volumes": r.volumes.iter().map(|v| serde_json::json!({
            "volume": v.volume,
            "free": v.free,
            "total": v.total,
            "claude_dir": v.claude_dir,
            "projects": v.projects,
            "state": node.disk_levels.0.get(&v.volume),
        })).collect::<Vec<_>>(),
        "biggest": r.biggest.iter().map(|f| serde_json::json!({"name": f.name, "bytes": f.bytes})).collect::<Vec<_>>(),
    })
}

/// `3.2 GB`, `512 MB`.
pub fn bytes(n: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else {
        format!("{:.0} MB", n / MB)
    }
}

/// A node reported its disks: store the report, and tell the owner when its state changed.
/// While a node is `full`, every report retries the queued work: one of its disks may have been
/// freed (a held project's) while another keeps the node `full`, and a node whose work is all
/// held is idle, so nothing else it sends would trigger a retry.
pub async fn reported(o: &Arc<Orchestrator>, peer: EndpointId, report: DiskReport) -> Result<()> {
    let id = peer.to_string();
    let Some((name, prev, prev_levels)): Option<(
        String,
        String,
        sqlx::types::Json<HashMap<String, String>>,
    )> = sqlx::query_as("select name, disk_state, disk_levels from nodes where id = $1")
        .bind(&id)
        .fetch_optional(&o.db)
        .await?
    else {
        return Ok(());
    };
    let prev = DiskState::parse(&prev);
    let limits = o.cfg.disk;
    let Some((state, levels)) = evaluate(&report, &limits, &prev_levels.0) else {
        // Kept as it was; if this goes on, the last report goes stale and stops holding.
        tracing::warn!(node = %name, "a disk report measured none of the fleet's volumes");
        return Ok(());
    };
    let stored: HashMap<&str, &str> = levels
        .iter()
        .map(|(v, s)| (v.as_str(), s.as_str()))
        .collect();
    sqlx::query(
        "update nodes set disk = $2, disk_at = now(), disk_state = $3, disk_levels = $4 where id = $1",
    )
    .bind(&id)
    .bind(sqlx::types::Json(&report))
    .bind(state.as_str())
    .bind(sqlx::types::Json(&stored))
    .execute(&o.db)
    .await?;
    if state != prev {
        tracing::info!(node = %name, from = prev.as_str(), to = state.as_str(), "disk state changed");
        o.audit(
            "orchestrator",
            "node_disk",
            serde_json::json!({"node": name, "from": prev.as_str(), "to": state.as_str()}),
        )
        .await;
        if let Some((html, notify)) = notice(&name, prev, state, &report, &levels) {
            let notify = quieted(o, &id, state, notify).await;
            // Best effort, like every post: the state is stored either way.
            if let Err(e) = o
                .chat
                .send(From::Orchestrator, None, &html, vec![], None, notify)
                .await
            {
                tracing::warn!("telling the owner about {name}'s disk failed: {e:#}");
            }
        }
    }
    if prev == DiskState::Full {
        crate::tasks::dispatch_queued(o).await;
    }
    Ok(())
}

/// How long after a sounded warning about a node another one, for the same state or a milder
/// one, comes silently. Agents build in worktrees, and a `target/` or `node_modules` swings the
/// free space by gigabytes; a volume sitting near a threshold would otherwise ring every few
/// minutes as it crosses back and forth.
const SOUND_COOLDOWN_HOURS: i64 = 12;

/// `notify`, unless a warning about this node at least as bad as `state` already sounded in the
/// last [`SOUND_COOLDOWN_HOURS`]: then silent. A sounded warning is remembered (in `kv`).
async fn quieted(o: &Arc<Orchestrator>, node_id: &str, state: DiskState, notify: Notify) -> Notify {
    if notify != Notify::Sound {
        return notify;
    }
    let key = format!("disk_notice:{node_id}");
    let now = chrono::Utc::now();
    let recent = match o.kv_get(&key).await {
        Ok(Some(v)) => {
            let worse_or_same = v["state"]
                .as_str()
                .is_some_and(|s| DiskState::parse(s) >= state);
            let at = v["at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            worse_or_same
                && at.is_some_and(|at| {
                    now - at.with_timezone(&chrono::Utc)
                        < chrono::Duration::hours(SOUND_COOLDOWN_HOURS)
                })
        }
        _ => false,
    };
    if recent {
        return Notify::Silent;
    }
    if let Err(e) = o
        .kv_set(
            &key,
            serde_json::json!({"state": state.as_str(), "at": now.to_rfc3339()}),
        )
        .await
    {
        tracing::warn!("remembering a disk warning failed: {e:#}");
    }
    Notify::Sound
}

/// Called from housekeeping. A hold lasts only as long as its report is fresh ([`held`]), but
/// queued work is otherwise only looked at again when some node reports: a node whose reports
/// stopped (a hung drive, a node gone quiet) would keep its tasks waiting after the hold itself
/// had lapsed. So when any `full` node's report has gone stale, the queue is looked at again.
pub async fn release_stale_holds(o: &Arc<Orchestrator>) -> Result<()> {
    let stale: bool = sqlx::query_scalar(
        "select exists (select 1 from nodes where disk_state = 'full'
            and (disk_at is null or disk_at < now() - make_interval(mins => $1::int)))",
    )
    .bind(STALE_AFTER_MINUTES as i32)
    .fetch_one(&o.db)
    .await?;
    if stale {
        crate::tasks::dispatch_queued(o).await;
    }
    Ok(())
}

/// What the owner is told when a node's disk state changes from `prev` to `state`. Getting worse
/// needs them (sound); getting better is news (silent).
pub fn notice(
    node: &str,
    prev: DiskState,
    state: DiskState,
    report: &DiskReport,
    levels: &Levels,
) -> Option<(String, Notify)> {
    let node = esc(node);
    let free = report
        .volumes
        .iter()
        .filter(|v| relevant(v))
        .map(|v| v.free)
        .min()
        .map(bytes)
        .unwrap_or_else(|| "?".into());
    if state < prev {
        let html = match state {
            DiskState::Ok => format!("✅ <b>{node}</b> has disk space again ({free} free)."),
            _ => format!("💾 <b>{node}</b> has {free} free, still low."),
        };
        let html = if prev == DiskState::Full {
            format!("{html} New runs start there again.")
        } else {
            html
        };
        return Some((html, Notify::Silent));
    }
    let at_least = |v: &DiskVolume, s: DiskState| levels.get(&v.volume).is_some_and(|l| *l >= s);
    let head = match state {
        DiskState::Ok => return None,
        DiskState::Low => format!("💾 <b>{node}</b> is running low on disk space."),
        DiskState::Full => {
            let full: Vec<&DiskVolume> = report
                .volumes
                .iter()
                .filter(|v| at_least(v, DiskState::Full))
                .collect();
            // Every run writes to Claude Code's dir; a full project disk holds that project only.
            let which = if full.iter().any(|v| v.claude_dir) {
                "New task runs there".to_owned()
            } else {
                let projects: Vec<&str> = full
                    .iter()
                    .flat_map(|v| v.projects.iter().map(String::as_str))
                    .collect();
                format!("New runs of {} there", esc(&projects.join(", ")))
            };
            format!(
                "🛑 <b>{node}</b> is almost out of disk space. {which} are held and queued until space is freed."
            )
        }
    };
    let mut lines = vec![head];
    for v in report
        .volumes
        .iter()
        .filter(|v| at_least(v, DiskState::Low))
    {
        lines.push(format!(
            "• {} free of {} on <code>{}</code> ({})",
            bytes(v.free),
            bytes(v.total),
            esc(&v.volume),
            esc(&what_is_on(v, report))
        ));
    }
    if !report.biggest.is_empty() {
        lines.push(format!(
            "Biggest under <code>{}</code>:",
            esc(&format!(
                "{}{}projects",
                report.claude_dir,
                separator(&report.claude_dir)
            ))
        ));
        for f in &report.biggest {
            lines.push(format!(
                "• <code>{}</code> {}",
                esc(&f.name),
                bytes(f.bytes)
            ));
        }
    }
    Some((lines.join("\n"), Notify::Sound))
}

/// `~/.claude, fleet-src, bioins`: what the fleet keeps on a volume.
fn what_is_on(v: &DiskVolume, report: &DiskReport) -> String {
    let mut on = Vec::new();
    if v.claude_dir {
        on.push(claude_label(&report.claude_dir));
    }
    on.extend(v.projects.iter().cloned());
    on.join(", ")
}

/// `~/.claude` for the usual config dir, its path when `CLAUDE_CONFIG_DIR` moved it.
fn claude_label(dir: &str) -> String {
    let last = dir
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(dir);
    if last == ".claude" {
        "~/.claude".into()
    } else {
        dir.to_owned()
    }
}

/// The node's path separator, judged by the path itself (the orchestrator runs on Linux, nodes
/// on either).
fn separator(path: &str) -> &'static str {
    if path.contains('\\') { "\\" } else { "/" }
}

#[cfg(test)]
mod tests {
    use fleet_proto::FolderSize;

    use super::*;

    const GB: u64 = 1 << 30;

    fn limits() -> DiskLimits {
        DiskLimits {
            low_bytes: 5 * GB,
            floor_bytes: GB,
        }
    }

    fn report(claude_free: u64, project_free: u64) -> DiskReport {
        DiskReport {
            claude_dir: "C:\\Users\\me\\.claude".into(),
            volumes: vec![
                DiskVolume {
                    volume: "C:\\".into(),
                    free: claude_free,
                    total: 500 * GB,
                    claude_dir: true,
                    projects: vec!["fleet-src".into()],
                },
                DiskVolume {
                    volume: "D:\\".into(),
                    free: project_free,
                    total: 1000 * GB,
                    claude_dir: false,
                    projects: vec!["bioins".into()],
                },
                // Nothing of the fleet's on it: never counts.
                DiskVolume {
                    volume: "E:\\".into(),
                    free: 0,
                    total: GB,
                    claude_dir: false,
                    projects: vec![],
                },
            ],
            biggest: vec![FolderSize {
                name: "C--repos-fleet-src".into(),
                bytes: 7 * GB,
            }],
        }
    }

    fn stored(levels: &Levels) -> HashMap<String, String> {
        levels
            .iter()
            .map(|(v, s)| (v.clone(), s.as_str().to_owned()))
            .collect()
    }

    /// A node as the orchestrator stores it after evaluating `r` from scratch, reported `age` ago.
    fn node(r: Option<DiskReport>, age_minutes: i64) -> NodeRow {
        let (state, levels) = r
            .as_ref()
            .and_then(|r| evaluate(r, &limits(), &HashMap::new()))
            .unwrap_or((DiskState::Ok, Levels::new()));
        NodeRow {
            id: "n".into(),
            name: "pc".into(),
            os: "windows".into(),
            app_version: String::new(),
            online: true,
            last_seen: None,
            claude_command: sqlx::types::Json(vec![]),
            auth_mode: "default".into(),
            auth_secret: None,
            max_agents: 4,
            projects: sqlx::types::Json(vec![]),
            running: sqlx::types::Json(vec![]),
            incarnation: None,
            disk: r.map(sqlx::types::Json),
            disk_at: Some(chrono::Utc::now() - chrono::Duration::minutes(age_minutes)),
            disk_state: state.as_str().into(),
            disk_levels: sqlx::types::Json(stored(&levels)),
        }
    }

    fn state_of(r: &DiskReport, prev: &HashMap<String, String>) -> DiskState {
        evaluate(r, &limits(), prev).unwrap().0
    }

    #[test]
    fn the_worst_volume_the_fleet_uses_decides() {
        let none = HashMap::new();
        assert_eq!(state_of(&report(100 * GB, 100 * GB), &none), DiskState::Ok);
        assert_eq!(state_of(&report(4 * GB, 100 * GB), &none), DiskState::Low);
        assert_eq!(state_of(&report(100 * GB, GB / 2), &none), DiskState::Full);
    }

    #[test]
    fn leaving_a_state_takes_a_margin() {
        let l = limits();
        // Just above the floor: not full when coming from above, still full when coming from it.
        let just_above = GB + GB / 20;
        assert_eq!(level(just_above, &l, DiskState::Ok), DiskState::Low);
        assert_eq!(level(just_above, &l, DiskState::Full), DiskState::Full);
        assert_eq!(level(GB + GB / 5, &l, DiskState::Full), DiskState::Low);
        // The same for low.
        let just_above_low = 5 * GB + GB / 4;
        assert_eq!(level(just_above_low, &l, DiskState::Ok), DiskState::Ok);
        assert_eq!(level(just_above_low, &l, DiskState::Low), DiskState::Low);
        assert_eq!(level(6 * GB, &l, DiskState::Low), DiskState::Ok);
    }

    #[test]
    fn the_margin_is_each_volumes_own() {
        // D filled up and the node is full; C sits just above the floor and never crossed it.
        let (state, levels) =
            evaluate(&report(GB + GB / 20, GB / 2), &limits(), &HashMap::new()).unwrap();
        assert_eq!(state, DiskState::Full);
        assert_eq!(levels["C:\\"], DiskState::Low, "C never crossed the floor");
        // So only D's project is held, not everything that writes to C.
        let mut n = node(Some(report(GB + GB / 20, GB / 2)), 0);
        n.disk_levels = sqlx::types::Json(stored(&levels));
        assert!(held(&n, "bioins"));
        assert!(!held(&n, "fleet-src"));
        // D is cleared while C stays where it was: the node is no longer full.
        let after = state_of(&report(GB + GB / 20, 100 * GB), &stored(&levels));
        assert_eq!(after, DiskState::Low);
    }

    #[test]
    fn a_report_that_measured_nothing_changes_nothing() {
        let empty = DiskReport {
            claude_dir: "/home/me/.claude".into(),
            volumes: vec![],
            biggest: vec![],
        };
        assert!(evaluate(&empty, &limits(), &HashMap::new()).is_none());
    }

    #[test]
    fn a_zero_floor_never_holds() {
        let l = DiskLimits {
            low_bytes: 0,
            floor_bytes: 0,
        };
        assert_eq!(level(0, &l, DiskState::Full), DiskState::Ok);
    }

    #[test]
    fn only_runs_that_write_to_the_full_disk_are_held() {
        // The project disk is full, Claude Code's is fine.
        let n = node(Some(report(100 * GB, GB / 2)), 0);
        assert!(held(&n, "bioins"));
        assert!(!held(&n, "fleet-src"));
        // Claude Code's disk is full: everything is held.
        let n = node(Some(report(GB / 2, 100 * GB)), 0);
        assert!(held(&n, "bioins"));
        assert!(held(&n, "fleet-src"));
        // Low is only a warning; no report is no hold.
        assert!(!held(&node(Some(report(3 * GB, 3 * GB)), 0), "bioins"));
        assert!(!held(&node(None, 0), "bioins"));
    }

    #[test]
    fn a_stale_report_holds_nothing() {
        let n = node(Some(report(GB / 2, GB / 2)), STALE_AFTER_MINUTES + 1);
        assert_eq!(n.disk_state, "full");
        assert!(
            !held(&n, "bioins"),
            "a node that stopped reporting is not held for ever"
        );
    }

    #[test]
    fn the_owner_hears_each_change_once_with_where_to_look() {
        let r = report(GB / 2, 100 * GB);
        let (_, levels) = evaluate(&r, &limits(), &HashMap::new()).unwrap();
        let (html, notify) = notice("pc <1>", DiskState::Ok, DiskState::Full, &r, &levels).unwrap();
        assert_eq!(notify, Notify::Sound);
        assert!(html.contains("<b>pc &lt;1&gt;</b>"), "{html}");
        assert!(html.contains("New task runs there are held"), "{html}");
        assert!(
            html.contains("<code>C:\\</code> (~/.claude, fleet-src)"),
            "{html}"
        );
        // The healthy volume is not listed.
        assert!(!html.contains("D:\\"), "{html}");
        assert!(html.contains("C:\\Users\\me\\.claude\\projects"), "{html}");
        assert!(html.contains("C--repos-fleet-src</code> 7.0 GB"), "{html}");

        // Only a project's disk is full: the notice says whose runs wait.
        let r2 = report(100 * GB, GB / 2);
        let (_, levels2) = evaluate(&r2, &limits(), &HashMap::new()).unwrap();
        let (html, _) = notice("pc", DiskState::Ok, DiskState::Full, &r2, &levels2).unwrap();
        assert!(html.contains("New runs of bioins there are held"), "{html}");

        let (html, notify) = notice("pc", DiskState::Full, DiskState::Ok, &r, &levels).unwrap();
        assert_eq!(notify, Notify::Silent);
        assert!(html.contains("start there again"), "{html}");
        let (html, _) = notice("pc", DiskState::Low, DiskState::Ok, &r, &levels).unwrap();
        assert!(!html.contains("start there again"), "{html}");
    }

    #[test]
    fn sizes_and_labels_read_well() {
        assert_eq!(bytes(3 * GB + GB / 5), "3.2 GB");
        assert_eq!(bytes(512 << 20), "512 MB");
        assert_eq!(claude_label("/home/me/.claude/"), "~/.claude");
        assert_eq!(claude_label("/srv/claude-cfg"), "/srv/claude-cfg");
    }
}
