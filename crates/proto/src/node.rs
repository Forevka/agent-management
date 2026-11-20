//! Handshake and node state messages.

use serde::{Deserialize, Serialize};

/// First frame a node sends on a new connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    /// Human-readable name chosen at join time (e.g. "home-pc"); the identity is the iroh key.
    pub node_name: String,
    pub app_version: String,
    pub os: String,
    /// One-time secret from an invite ticket. Only present on the first connection of a node
    /// whose key the orchestrator does not know yet.
    pub join_secret: Option<String>,
    /// Highest orchestrator sequence number this node has processed, so the orchestrator
    /// can replay anything newer.
    pub last_received_seq: u64,
    /// Random id of this node process. Sequence numbers only mean something within one pair
    /// of incarnations: when either side restarts, both streams start over.
    pub incarnation: u64,
    /// The orchestrator incarnation that `last_received_seq` counts (0 if none yet).
    pub last_received_from: u64,
}

/// The orchestrator's answer to an accepted [`Hello`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    /// Highest node sequence number the orchestrator has processed, counted in the node's
    /// current incarnation.
    pub last_received_seq: u64,
    /// Random id of the orchestrator process.
    pub incarnation: u64,
}

/// Sent instead of [`Welcome`] before the orchestrator closes the connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub reason: String,
}

/// A node's current state, sent after every (re)connect and whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub projects: Vec<ProjectMapping>,
    pub max_agents: u32,
    pub running_agents: Vec<String>,
    /// Run ids of the agent processes alive on this node.
    pub running_runs: Vec<String>,
}

/// Free space on the disks a node's sessions write to. Claude Code keeps every session, subagent
/// and transcript under its config dir (`~/.claude`) and fails mid-run when that disk is full,
/// so the orchestrator needs to see it coming rather than learn it from failed tasks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskReport {
    /// Where Claude Code keeps its sessions on this node, for the owner to read.
    pub claude_dir: String,
    /// One entry per volume, so a disk holding `~/.claude` and three repositories shows once.
    pub volumes: Vec<DiskVolume>,
    /// The largest folders under `<claude_dir>/projects` (one per project directory Claude Code
    /// has worked in), largest first: where to look when space runs out.
    pub biggest: Vec<FolderSize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskVolume {
    /// The volume's root as the node's OS names it (`C:\`, or a device id on unix).
    pub volume: String,
    /// Bytes this user may still write.
    pub free: u64,
    pub total: u64,
    /// Whether the Claude Code config dir is on this volume.
    pub claude_dir: bool,
    /// Slugs of the mapped projects whose directories are on this volume.
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderSize {
    pub name: String,
    pub bytes: u64,
}

/// A server-side project (by slug) mapped to a local directory on the node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectMapping {
    pub project: String,
    pub path: String,
}

/// A project the orchestrator knows, as listed to nodes for their local mapping UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub slug: String,
    pub name: String,
}

/// How a node runs Claude Code, configured per node in the orchestrator's web UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSettings {
    /// Program and leading arguments, e.g. `["claude"]` or `["C:/tools/claude.cmd"]`.
    pub claude_command: Vec<String>,
    pub auth: ClaudeAuth,
    /// Agents this node runs at once; more are queued by the orchestrator.
    pub max_agents: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClaudeAuth {
    /// Whatever login Claude Code already has on this host (a subscription via `claude login`).
    Default,
    /// A long-lived subscription token from `claude setup-token` (`CLAUDE_CODE_OAUTH_TOKEN`).
    OAuthToken(String),
    /// An Anthropic API key (`ANTHROPIC_API_KEY`).
    ApiKey(String),
}
