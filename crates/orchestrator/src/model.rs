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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAgent {
    pub name: String,
    pub role: String,
}
