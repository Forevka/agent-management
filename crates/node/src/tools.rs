//! The one MCP tool agents' sessions load: Claude Code's permission-prompt hook.
//!
//! Everything else agents do with the fleet goes through the `fleet agent` CLI (see
//! [`crate::agent_cli`] and [`crate::skill`]). The permission hook has to be an MCP tool,
//! because that is the only way `--permission-prompt-tool` can route plan approvals,
//! AskUserQuestion and tool permissions to the human.

use serde_json::{Value, json};

/// Passed to Claude as `--permission-prompt-tool mcp__fleet__permission`.
pub const PERMISSION_TOOL: &str = "permission";

/// The role whose sessions also get the orchestration commands.
pub const BRAIN_ROLE: &str = "orchestrator";

pub fn list() -> Vec<Value> {
    vec![json!({
        "name": PERMISSION_TOOL,
        "description": "Internal: Claude Code routes permission prompts (plan approval, questions, risky tools) here. Do not call it yourself.",
        "inputSchema": {"type":"object","properties":{"tool_name":{"type":"string"},"input":{"type":"object"},"tool_use_id":{"type":"string"}},"required":["tool_name","input"]}
    })]
}
