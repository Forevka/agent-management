//! Messages that start, steer and report on Claude Code agent sessions.

use serde::{Deserialize, Serialize};

/// Starts (or resumes) one Claude Code session on a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnAgent {
    pub agent_id: String,
    pub task_id: String,
    /// Project slug; the node resolves it to its local directory.
    pub project: String,
    /// Agent role (`implementor`, `reviewer`, `tester`, or `orchestrator` for the brain);
    /// decides which fleet tools the agent's MCP shim offers.
    pub role: String,
    /// First user message of the session.
    pub prompt: String,
    /// Role, brief and fleet instructions, passed as `--append-system-prompt`.
    pub append_system_prompt: Option<String>,
    /// Resume an earlier session (`--resume`) instead of starting a new one.
    pub resume_session_id: Option<String>,
    /// `--permission-mode` value; new tasks start in `plan`.
    pub permission_mode: String,
    /// Mode the node switches the session to once the human approves the plan
    /// (the `ExitPlanMode` permission request is allowed).
    pub after_plan_mode: Option<String>,
    pub model: Option<String>,
    /// Extra CLI arguments chosen by the orchestrator for this run.
    pub extra_args: Vec<String>,
    /// JSON object of additional MCP servers (e.g. Hindsight), merged next to the fleet's own.
    pub extra_mcp_servers: Option<String>,
    /// Secrets for this task, written as `.env` files outside the project and deleted when
    /// the agent exits.
    pub secret_files: Vec<SecretFile>,
    /// Identifies this particular process run. Every event of the run carries it, so the
    /// orchestrator can ignore late events of a run it has already replaced.
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretFile {
    /// File stem; the node writes `<name>.env`. Restricted to `[A-Za-z0-9_-]`.
    pub name: String,
    /// `KEY=value` lines.
    pub content: String,
}

/// An agent's call to a fleet MCP tool, forwarded by its node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique per node; the result echoes it.
    pub call_id: String,
    pub agent_id: String,
    pub task_id: String,
    pub tool: String,
    /// JSON arguments as the agent sent them.
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    /// `false` makes the tool call fail with `result` as the error text.
    pub ok: bool,
    /// Text returned to the agent. For the permission tool this is the JSON decision.
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopAgent {
    pub agent_id: String,
    pub task_id: String,
    /// Close the session's input and let Claude finish its turn, instead of killing it.
    pub graceful: bool,
    /// Appended in protocol version 2: the run to stop. Empty means the task's current run;
    /// otherwise the node leaves a different (newer) run of the task alone.
    pub run_id: String,
}

/// The content block a watched run is writing, so far (see [`crate::Body::RunPartial`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunPartial {
    pub task_id: String,
    pub run_id: String,
    /// `text`, `thinking`, or `tool` (a tool call being written: `text` is the tool's name).
    pub kind: String,
    /// The block so far, secrets redacted. The whole block every time rather than the newest
    /// piece: a secret split across two pieces would get past redaction, and a tail as long as
    /// the longest secret is held back for the same reason, so no secret's beginning shows.
    pub text: String,
}

/// Something that happened in an agent session on a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEvent {
    pub agent_id: String,
    pub task_id: String,
    pub kind: AgentEventKind,
    /// The run (see [`SpawnAgent::run_id`]) this event belongs to; empty when none applies.
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEventKind {
    /// The Claude process started; `pid` is informational.
    Started { pid: u32 },
    /// Claude reported its session id (from the stream-json `system/init` event).
    Session { session_id: String },
    /// One raw stream-json line, forwarded verbatim for the transcript.
    /// Kept as text so the node never has to understand every event type Claude emits.
    Output { json: String },
    /// One line of Claude's stderr.
    Stderr { line: String },
    /// The process ended. `code` is `None` when it was killed by a signal.
    Exited { code: Option<i32> },
    /// The node could not start the agent at all (unknown project, bad command...).
    SpawnFailed { reason: String },
    /// A message for the agent arrived while it was not running on this node (for example after
    /// the node restarted). The text comes back so the orchestrator can resume the session with it.
    Undelivered { text: String },
}
