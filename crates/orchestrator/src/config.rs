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
    /// Hindsight's API as the orchestrator reaches it (`http://hindsight:8888` in Compose).
    /// Agents use memory through the orchestrator, so only the orchestrator needs this.
    pub hindsight_url: Option<String>,
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
    /// The secretary's settings (see `secretary`).
    pub secretary: SecretaryConfig,
}

#[derive(Debug, Clone)]
pub struct SecretaryConfig {
    /// Claude model of the secretary session: a cheap one, as it reads every message.
    pub model: String,
    /// Messages to the owner personally (direct, group, a mention) wait this long for more to
    /// arrive before the secretary sees them.
    pub debounce_secs: i64,
    /// Other traffic waits at most this long...
    pub batch_secs: i64,
    /// ...or until this many messages are waiting.
    pub batch_size: i64,
    /// At most this many messages are shown in one batch; the rest are counted per conversation.
    pub max_batch: i64,
    /// Batches per hour at most; more messages wait for the next hour.
    pub turns_per_hour: i64,
    /// Escalations to the brain per hour at most; more are refused (the secretary adds a todo).
    pub escalations_per_hour: i64,
    /// The daily digest's hour (0-23) in `timezone`; a negative one turns the digest off.
    pub digest_hour: i64,
    /// The owner's IANA time zone (`Europe/Kyiv`), for the digest and the times shown.
    pub timezone: String,
    /// How long messages of conversations not mapped to a project are kept.
    pub feed_retention_days: i64,
}

impl SecretaryConfig {
    fn from_env() -> Self {
        let text = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| default.into())
        };
        Self {
            model: text("FLEET_SECRETARY_MODEL", "haiku"),
            debounce_secs: env_num("FLEET_SECRETARY_DEBOUNCE_SECS", 30),
            batch_secs: env_num("FLEET_SECRETARY_BATCH_SECS", 600),
            batch_size: env_num("FLEET_SECRETARY_BATCH_SIZE", 50),
            max_batch: env_num("FLEET_SECRETARY_MAX_BATCH", 80),
            turns_per_hour: env_num("FLEET_SECRETARY_TURNS_PER_HOUR", 30),
            escalations_per_hour: env_num("FLEET_SECRETARY_ESCALATIONS_PER_HOUR", 10),
            digest_hour: env_num("FLEET_SECRETARY_DIGEST_HOUR", 9),
            timezone: text("FLEET_TIMEZONE", "UTC"),
            feed_retention_days: env_num("FLEET_FEED_RETENTION_DAYS", 90),
        }
    }

    pub fn test() -> Self {
        Self {
            model: "haiku".into(),
            debounce_secs: 30,
            batch_secs: 600,
            batch_size: 50,
            max_batch: 80,
            turns_per_hour: 30,
            escalations_per_hour: 10,
            digest_hour: 9,
            timezone: "UTC".into(),
            feed_retention_days: 90,
        }
    }
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
            hindsight_url: std::env::var("HINDSIGHT_URL")
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
            secretary: SecretaryConfig::from_env(),
        })
    }

    /// Settings for tests.
    pub fn test() -> Self {
        Self {
            public_url: "https://fleet.test".into(),
            owner_id: 42,
            model: "opus".into(),
            hindsight_url: None,
            hindsight_api_key: Some("hk".into()),
            brain_project: "fleet".into(),
            magic_link_minutes: 60,
            retention_days: 90,
            remind_after_minutes: 120,
            secretary: SecretaryConfig::test(),
        }
    }
}

fn env_num(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
