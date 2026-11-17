//! Orchestrator settings, from the environment (see `deploy/.env.example`).

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// `https://fleet.example.com`, used for magic links. No trailing slash.
    pub public_url: String,
    /// Telegram user id of the only person allowed to command the fleet.
    pub owner_id: i64,
    /// Default model for agents.
    pub model: String,
    /// Base URL of Hindsight as agents' hosts reach it (`http://127.0.0.1:8888` on the VPS,
    /// or `https://memory.example.com` when Windows nodes need it too). Each agent gets the
    /// `hindsight` MCP server at `<base>/mcp/<project>/`, one memory bank per project.
    pub hindsight_mcp_url: Option<String>,
    /// Hindsight's API key, sent as a bearer token.
    pub hindsight_api_key: Option<String>,
    /// The project the brain session runs in (mapped on the VPS node, e.g. `fleet`).
    pub brain_project: String,
    /// How long magic links stay valid.
    pub magic_link_minutes: i64,
    /// Retention for transcripts, feed history and the audit log.
    pub retention_days: i64,
    /// Remind about unanswered blocking items after this many minutes.
    pub remind_after_minutes: i64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            public_url: std::env::var("FLEET_PUBLIC_URL")
                .context("FLEET_PUBLIC_URL is required")?
                .trim_end_matches('/')
                .to_owned(),
            owner_id: std::env::var("TELEGRAM_OWNER_ID")
                .context("TELEGRAM_OWNER_ID is required")?
                .parse()
                .context("TELEGRAM_OWNER_ID must be a number")?,
            model: std::env::var("FLEET_MODEL").unwrap_or_else(|_| "opus".into()),
            hindsight_mcp_url: std::env::var("FLEET_HINDSIGHT_MCP_BASE")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|s| s.trim_end_matches('/').to_owned()),
            hindsight_api_key: std::env::var("HINDSIGHT_API_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            brain_project: std::env::var("FLEET_BRAIN_PROJECT").unwrap_or_else(|_| "fleet".into()),
            magic_link_minutes: env_num("FLEET_MAGIC_LINK_MINUTES", 60 * 24),
            retention_days: env_num("FLEET_RETENTION_DAYS", 90),
            remind_after_minutes: env_num("FLEET_REMIND_AFTER_MINUTES", 120),
        })
    }

    /// Settings for tests.
    pub fn test() -> Self {
        Self {
            public_url: "https://fleet.test".into(),
            owner_id: 42,
            model: "opus".into(),
            hindsight_mcp_url: Some("http://hindsight:8888".into()),
            hindsight_api_key: Some("hk".into()),
            brain_project: "fleet".into(),
            magic_link_minutes: 60,
            retention_days: 90,
            remind_after_minutes: 120,
        }
    }
}

fn env_num(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
