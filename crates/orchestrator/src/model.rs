//! Database rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub os: String,
    pub app_version: String,
    pub online: bool,
    pub last_seen: Option<DateTime<Utc>>,
    pub claude_command: sqlx::types::Json<Vec<String>>,
    pub auth_mode: String,
    pub auth_secret: Option<Vec<u8>>,
    pub max_agents: i32,
    pub projects: sqlx::types::Json<Vec<fleet_proto::ProjectMapping>>,
    pub running: sqlx::types::Json<Vec<String>>,
    pub incarnation: Option<String>,
    /// The node's last disk report, if it has sent one (see `crate::disk`).
    pub disk: Option<sqlx::types::Json<fleet_proto::DiskReport>>,
    pub disk_at: Option<DateTime<Utc>>,
    /// `ok`, `low` or `full`: what the orchestrator made of the last report.
    pub disk_state: String,
    /// The same per volume (`{volume: state}`), which is what holds a run (see `crate::disk`).
    pub disk_levels: sqlx::types::Json<std::collections::HashMap<String, String>>,
    /// Where the node clones assigned projects (null: its default, `~/fleet-projects`).
    pub projects_root: Option<String>,
    /// Projects assigned to this node that have a repository URL, so it would clone them for a
    /// task (see `crate::node_projects`). Filled only by `tasks::node_load`, for placement;
    /// empty wherever else a node is read.
    #[sqlx(skip)]
    pub clonable: Vec<String>,
}

impl NodeRow {
    /// Whether the project is checked out and mapped on this node.
    pub fn has_project(&self, slug: &str) -> bool {
        self.projects.0.iter().any(|p| p.project == slug)
    }

    /// Whether a task of `slug` may be placed here: the project is mapped, or the node would
    /// clone it first (assigned with a repository URL, and the node reads its assignments). The
    /// one rule for every placement decision; [`NodeRow::has_project`] is the stronger "already
    /// there", which placement prefers.
    ///
    /// For a project it would clone, `disk::held` sees only the volume of the node's Claude
    /// config dir: the orchestrator learns about a volume through the mapped projects on it, so
    /// the projects root is not measured until something is cloned there. The node checks that
    /// one itself and refuses to clone with less than 1 GB free (`fleet_node::checkout`).
    pub fn can_run(&self, slug: &str) -> bool {
        self.has_project(slug)
            || (self.clonable.iter().any(|p| p == slug)
                && fleet_proto::node_is_current(&self.app_version))
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct ProjectRow {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub next_ticket: i32,
    pub needs_you_topic: Option<i64>,
    pub fyi_topic: Option<i64>,
    /// Who approves plans of its tasks that are not workflows: `owner` or `auto` (`plan_gate`).
    pub plan_gate: String,
    /// Where its code lives, for nodes it is assigned to that have to clone it (null: none set).
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub role: String,
    pub project: Option<String>,
    pub bot_id: Option<i64>,
    pub preferred_node: Option<String>,
    /// Its default Claude model and effort (null: the fleet's).
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TaskRow {
    pub id: String,
    pub project: String,
    pub title: String,
    pub brief: String,
    pub agent_id: Option<String>,
    pub node_id: Option<String>,
    pub status: String,
    pub topic_id: Option<i64>,
    pub checklist_msg_id: Option<i64>,
    pub session_id: Option<String>,
    pub running: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub run_id: Option<String>,
    pub dispatched_at: Option<DateTime<Utc>>,
    /// This task's own Claude model and effort (null: its agent's).
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Whether the owner approved the plan of the task's current planning round: false from a
    /// run started in plan mode until the approval, so a run lost meanwhile is resumed in plan
    /// mode again (see `tasks::plan_first`).
    pub plan_approved: bool,
    /// The fleet's own short handle for this task, as a number (see [`task_key`]). The fleet's
    /// standing sessions have none: they are not work.
    pub key_no: Option<i64>,
    /// When the nightly archive put this done task out of the board's way (see `archive`). The
    /// task stays `done`; working on it again clears this.
    pub archived_at: Option<DateTime<Utc>>,
}

impl TaskRow {
    /// `F-41`, or the task's id when it has no key of its own.
    pub fn key(&self) -> String {
        self.key_no.map_or_else(|| self.id.clone(), task_key)
    }
}

/// The fleet's own task keys: `F-41`.
///
/// A task's id is long and usually carries the ticket it is about (`bioins-BIOINS-5068`), which
/// is awkward to say and easy to mistake for the ticket itself. The key is short, fleet-wide and
/// deliberately one letter, so it cannot be read as a Jira project. [`check_key_prefix`] makes
/// that a guarantee rather than a hope.
pub const TASK_KEY_PREFIX: &str = "F";

pub fn task_key(n: i64) -> String {
    format!("{TASK_KEY_PREFIX}-{n}")
}

/// Reads `F-41` (in any case, with surrounding space) and returns 41.
///
/// Anything else is `None`, including a Jira key that happens to end in a number: the prefix must
/// match exactly, so `DAP-356` and `BIOINS-5068` are never mistaken for the fleet's own.
pub fn parse_task_key(s: &str) -> Option<i64> {
    let (prefix, n) = s.trim().split_once('-')?;
    if !prefix.eq_ignore_ascii_case(TASK_KEY_PREFIX) {
        return None;
    }
    n.parse().ok().filter(|n| *n > 0)
}

/// Refuses a fleet key prefix that a project's Jira key would answer to as well.
///
/// Checked at startup against every project, because the whole point of the key is that the
/// owner can say one and mean exactly one thing. A fleet that silently had both would be worse
/// than one with no keys at all.
pub fn check_key_prefix(jira_keys: &[String]) -> anyhow::Result<()> {
    if let Some(clash) = jira_keys
        .iter()
        .find(|k| k.eq_ignore_ascii_case(TASK_KEY_PREFIX))
    {
        anyhow::bail!(
            "project Jira key {clash:?} is the same as the fleet's own task-key prefix \
             {TASK_KEY_PREFIX:?}: a task key and a ticket would be indistinguishable. \
             Change the project's Jira key, or TASK_KEY_PREFIX in model.rs."
        );
    }
    Ok(())
}

/// The effort levels Claude Code accepts (`claude --help`, 2.1.268).
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// A model name as it may be passed to Claude, everywhere one is accepted (tasks, agents, a
/// node's `--model`, the allow-list, `FLEET_MODEL`): a plain token, never an option. Aliases,
/// full ids (`claude-sonnet-4-5[1m]`), Vertex (`claude-opus-4@20250514`) and Bedrock ids and
/// ARNs (`us.anthropic.…:0`, `arn:aws:bedrock:…/…`) fit; whitespace, `=` and a leading `-` do
/// not.
pub fn check_model_value(what: &str, v: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=200).contains(&v.len())
            && !v.starts_with('-')
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:[]-@/".contains(c)),
        "{what} {v:?} is not a model name: 1-200 letters, digits and . _ : [ ] - @ /, not starting with -"
    );
    Ok(())
}
pub fn check_effort(v: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        EFFORTS.contains(&v),
        "effort {v:?} is not one of {}",
        EFFORTS.join(", ")
    );
    Ok(())
}

/// The model and effort a run of `task` uses: the task's own, else its agent's, else (for the
/// model) the fleet's default. No effort means Claude's own default.
pub fn resolve_model_effort(
    task: &TaskRow,
    agent: &AgentRow,
    fleet_model: &str,
) -> (String, Option<String>) {
    let model = task
        .model
        .clone()
        .or_else(|| agent.model.clone())
        .unwrap_or_else(|| fleet_model.to_owned());
    let effort = task.effort.clone().or_else(|| agent.effort.clone());
    (model, effort)
}

#[derive(Debug, Clone, FromRow)]
pub struct HumanItemRow {
    pub id: i64,
    pub project: Option<String>,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
    pub kind: String,
    pub blocking: bool,
    pub payload: sqlx::types::Json<serde_json::Value>,
    pub state: String,
    pub answer: Option<sqlx::types::Json<serde_json::Value>>,
    pub tg_chat: Option<i64>,
    pub tg_msg: Option<i64>,
    pub node_id: Option<String>,
    pub call_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Further messages showing the item besides `tg_msg` (its pointer in the task topic).
    pub tg_msgs: Vec<i64>,
}

/// Task states. Kept as text in the database so they read well in SQL.
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const PLANNING: &str = "planning";
    pub const RUNNING: &str = "running";
    pub const WAITING: &str = "waiting";
    pub const DONE: &str = "done";
    pub const STOPPED: &str = "stopped";
    pub const FAILED: &str = "failed";
}

/// The brain's pseudo task and agent ids.
pub const BRAIN_TASK: &str = "brain";
pub const BRAIN_AGENT: &str = "orchestrator";
/// The secretary's (see `secretary`).
pub const SECRETARY_TASK: &str = "secretary";
pub const SECRETARY_AGENT: &str = "secretary";
/// The daily retrospective's (see `retro`).
pub const RETRO_TASK: &str = "retro";
pub const RETRO_AGENT: &str = "retro";

/// The fleet's own long-lived sessions (the brain, the secretary, the retrospective): never
/// planned, never "finished", not listed or administered as ordinary tasks and agents.
pub fn is_builtin_task(id: &str) -> bool {
    BUILTIN_TASKS.contains(&id)
}
pub fn is_builtin_agent(id: &str) -> bool {
    BUILTIN_AGENTS.contains(&id)
}

/// The same ids as slices, for the queries that have to exclude them in SQL.
pub const BUILTIN_AGENTS: &[&str] = &[BRAIN_AGENT, SECRETARY_AGENT, RETRO_AGENT];
pub const BUILTIN_TASKS: &[&str] = &[BRAIN_TASK, SECRETARY_TASK, RETRO_TASK];

#[derive(Debug, Clone, FromRow)]
pub struct TodoRow {
    pub id: i64,
    pub title: String,
    pub detail: String,
    pub refs: Vec<String>,
    pub status: String,
    pub created_by: String,
    /// Who last changed its status: `secretary` or `owner`.
    pub updated_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub due: Option<DateTime<Utc>>,
    pub tg_msg: Option<i64>,
    /// A workflow (task id) this todo is about: "Run workflow" nudges it instead of the brain
    /// creating a new task.
    pub workflow_task: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct RuleRow {
    pub id: i64,
    pub text: String,
    pub source: String,
    pub cited: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub tg_msg: Option<i64>,
}

/// What an agent asked to be woken for (see `wakeups.rs`), so it needs no session while it
/// waits.
#[derive(Debug, Clone, FromRow)]
pub struct WakeupRow {
    pub id: i64,
    pub task_id: String,
    /// The agent's own words, handed back when it wakes.
    pub note: String,
    /// `timer` or `script`.
    pub kind: String,
    pub fires_at: Option<DateTime<Utc>>,
    /// `None`: one-shot. Otherwise the interval to re-arm with (`30m`).
    pub every: Option<String>,
    /// The plain-text condition, and the Rhai the brain wrote for it.
    pub wish: Option<String>,
    pub script: Option<String>,
    pub state: String,
    pub fires: i32,
    pub errors: i32,
    /// The script's own scratch between evaluations.
    pub memo: sqlx::types::Json<serde_json::Value>,
    pub created_by: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub last_at: Option<DateTime<Utc>>,
}

/// A task that was turned into a workflow (see `workflows.rs`): its topic stays open, and this
/// row says whether and when it triggers again on its own.
#[derive(Debug, Clone, FromRow)]
pub struct WorkflowRow {
    pub task_id: String,
    /// `None`: on-demand only. Otherwise `every:<N>m`/`every:<N>h` or `daily:HH:MM`.
    pub schedule: Option<String>,
    pub enabled: bool,
    pub tg_msg: Option<i64>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Who approves the plans of its runs: `auto` (the default) or `owner` (`plan_gate`).
    pub plan_gate: String,
}

/// Something the owner kept doing that the fleet now does for them (see `habits.rs`). Created
/// only by an approved `admin_change`, so a habit always has one explicit yes behind it.
#[derive(Debug, Clone, FromRow)]
pub struct HabitRow {
    pub id: i64,
    /// `decide` (answer an item of a known shape) or `do` (a standing request on a schedule).
    pub kind: String,
    pub project: Option<String>,
    pub item_kind: Option<String>,
    /// A `like` pattern over the task title's shape, for a `decide` habit.
    pub title_like: Option<String>,
    pub decision: Option<String>,
    /// `every:<N>m`/`every:<N>h`/`daily:HH:MM`/`weekly:<dow>:HH:MM`, for a `do` habit.
    pub schedule: Option<String>,
    pub brief: Option<String>,
    pub why: String,
    pub active: bool,
    pub paused_why: Option<String>,
    pub cap: i32,
    pub created_at: DateTime<Utc>,
    /// The schedule's anchor: moves when a `do` habit is due, before anything is delivered.
    pub last_used_at: Option<DateTime<Utc>>,
    pub tg_msg: Option<i64>,
    /// When the orchestrator last evaluated it (what it decided is in `habit_log`).
    pub last_checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAgent {
    pub name: String,
    pub role: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(model: Option<&str>, effort: Option<&str>) -> TaskRow {
        TaskRow {
            id: "p-1".into(),
            project: "p".into(),
            title: "t".into(),
            brief: String::new(),
            agent_id: Some("a".into()),
            node_id: None,
            status: "queued".into(),
            topic_id: None,
            checklist_msg_id: None,
            session_id: None,
            running: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            run_id: None,
            dispatched_at: None,
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            plan_approved: true,
            key_no: Some(1),
            archived_at: None,
        }
    }

    fn agent(model: Option<&str>, effort: Option<&str>) -> AgentRow {
        AgentRow {
            id: "a".into(),
            name: "A".into(),
            role: "implementor".into(),
            project: Some("p".into()),
            bot_id: None,
            preferred_node: None,
            model: model.map(Into::into),
            effort: effort.map(Into::into),
        }
    }

    #[test]
    fn task_then_agent_then_fleet() {
        let r = |t, a| resolve_model_effort(&t, &a, "opus");
        assert_eq!(
            r(
                task(Some("haiku"), Some("low")),
                agent(Some("sonnet"), Some("max"))
            ),
            ("haiku".into(), Some("low".into()))
        );
        assert_eq!(
            r(task(None, None), agent(Some("sonnet"), Some("max"))),
            ("sonnet".into(), Some("max".into()))
        );
        // Each falls back on its own.
        assert_eq!(
            r(task(Some("haiku"), None), agent(None, Some("high"))),
            ("haiku".into(), Some("high".into()))
        );
        assert_eq!(
            r(task(None, None), agent(None, None)),
            ("opus".into(), None)
        );
    }

    #[test]
    fn values_are_plain_tokens() {
        assert!(check_model_value("model", "claude-sonnet-4-5[1m]").is_ok());
        assert!(check_model_value("model", "us.anthropic.claude:1").is_ok());
        assert!(check_model_value("model", "--dangerously-skip-permissions").is_err());
        assert!(check_model_value("model", "opus; rm -rf /").is_err());
        assert!(check_model_value("model", "").is_err());
        assert!(check_model_value("model", "claude-opus-4@20250514").is_ok());
        assert!(
            check_model_value("model", "arn:aws:bedrock:us-east-1:1:inference-profile/x").is_ok()
        );
        assert!(check_model_value("model", "opus=1").is_err());
        assert!(check_model_value("model", &"x".repeat(201)).is_err());
        for e in EFFORTS {
            assert!(check_effort(e).is_ok());
        }
        assert!(check_effort("turbo").is_err());
    }
}

#[cfg(test)]
mod key_tests {
    use super::{check_key_prefix, parse_task_key, task_key};

    #[test]
    fn a_fleet_key_is_never_read_out_of_a_jira_ticket() {
        assert_eq!(task_key(41), "F-41");
        assert_eq!(parse_task_key("F-41"), Some(41));
        // Said in passing, in any case, is still the same key.
        assert_eq!(parse_task_key(" f-41 "), Some(41));
        // The whole point: a real ticket is not one of ours, however it is shaped.
        for ticket in [
            "DAP-356",
            "BIOINS-5068",
            "FL-41",
            "FF-41",
            "F41",
            "F-",
            "F-0",
            "F-x",
            "-41",
            "41",
            "",
        ] {
            assert_eq!(
                parse_task_key(ticket),
                None,
                "{ticket:?} is not a fleet key"
            );
        }
    }

    #[test]
    fn a_project_keyed_like_the_fleet_stops_the_orchestrator() {
        assert!(check_key_prefix(&["DAP".into(), "BIOINS".into()]).is_ok());
        // Including the case where only the case differs: Jira keys are matched without it.
        for clash in ["F", "f"] {
            let e = check_key_prefix(&["DAP".into(), clash.into()]).unwrap_err();
            assert!(
                e.to_string().contains("indistinguishable"),
                "a clash must say why: {e}"
            );
        }
    }
}
