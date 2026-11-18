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
}

impl NodeRow {
    pub fn has_project(&self, slug: &str) -> bool {
        self.projects.0.iter().any(|p| p.project == slug)
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

/// The fleet's own long-lived sessions (the brain, the secretary): never planned, never
/// "finished", not listed or administered as ordinary tasks and agents.
pub fn is_builtin_task(id: &str) -> bool {
    id == BRAIN_TASK || id == SECRETARY_TASK
}
pub fn is_builtin_agent(id: &str) -> bool {
    id == BRAIN_AGENT || id == SECRETARY_AGENT
}

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
