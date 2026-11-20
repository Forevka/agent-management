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
    /// How many agents **of one role** a project may have, so its tasks run side by side
    /// instead of queueing behind a single specialist. The orchestrator adds one when a task
    /// would otherwise wait and a node has room; `1` keeps a project's work strictly serial.
    ///
    /// Per role, not per project, because the roles do different jobs: a project is still free
    /// to have an implementor, a reviewer and a tester without any of them crowding out another.
    pub max_agents_per_project: i64,
    /// Delete a finished task's topic once it has been quiet this long, so the forum does not
    /// fill up with work that is over. `0` keeps every topic for ever.
    pub delete_topics_after_minutes: i64,
    /// The secretary's settings (see `secretary`).
    pub secretary: SecretaryConfig,
    /// The daily retrospective's settings (see `retro`).
    pub retro: RetroConfig,
    /// Models and effort levels the brain may choose for a task (the owner's own settings on
    /// the web may use any valid value).
    pub allowed_models: Vec<String>,
    pub allowed_efforts: Vec<String>,
    /// The nightly archive of finished tasks (see `archive`).
    pub archive: ArchiveConfig,
    /// The largest file a task may carry as an attachment (see `attachments`).
    pub attachment_max_bytes: i64,
}

/// When finished tasks are moved out of sight (see `archive`).
#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    /// The local time of day, as minutes after midnight in `timezone`. `None` turns the archive
    /// off.
    pub at_minute: Option<u32>,
    /// An IANA zone Postgres knows. Its own setting rather than `FLEET_TIMEZONE`, because the
    /// owner asked for 21:00 Kyiv time specifically and that must not move when the digest's
    /// zone does.
    pub timezone: String,
}

impl ArchiveConfig {
    fn from_env() -> Result<Self> {
        let at = std::env::var("FLEET_ARCHIVE_AT").unwrap_or_else(|_| "21:00".into());
        let timezone = std::env::var("FLEET_ARCHIVE_TIMEZONE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Europe/Kyiv".into());
        Ok(Self {
            at_minute: parse_time_of_day(&at).context("FLEET_ARCHIVE_AT")?,
            timezone: timezone.trim().to_owned(),
        })
    }

    pub fn test() -> Self {
        // Off: the e2e drives the archive with its own clock, so the wall clock of whoever runs
        // the tests can never archive something a later step expects to find done.
        Self {
            at_minute: None,
            timezone: "Europe/Kyiv".into(),
        }
    }
}

/// `HH:MM` as minutes after midnight; `off` (or empty) is `None`.
pub fn parse_time_of_day(s: &str) -> Result<Option<u32>> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let (h, m) = s
        .split_once(':')
        .with_context(|| format!("{s:?} is not a time of day: HH:MM, or off"))?;
    let (h, m): (u32, u32) = (
        h.parse().with_context(|| format!("{s:?}: bad hour"))?,
        m.parse().with_context(|| format!("{s:?}: bad minute"))?,
    );
    anyhow::ensure!(
        h < 24 && m < 60,
        "{s:?} is not a time of day: HH:MM, or off"
    );
    Ok(Some(h * 60 + m))
}

/// 50 MiB unless `FLEET_ATTACHMENT_MAX_MB` says otherwise.
const DEFAULT_ATTACHMENT_MAX_MB: i64 = 50;

impl Config {
    /// Whether the brain may give a task this model and effort.
    pub fn check_allowed(&self, model: Option<&str>, effort: Option<&str>) -> Result<()> {
        if let Some(m) = model {
            anyhow::ensure!(
                self.allowed_models.iter().any(|a| a == m),
                "model {m:?} is not allowed; allowed: {} (FLEET_ALLOWED_MODELS)",
                self.allowed_models.join(", ")
            );
        }
        if let Some(e) = effort {
            anyhow::ensure!(
                self.allowed_efforts.iter().any(|a| a == e),
                "effort {e:?} is not allowed; allowed: {} (FLEET_ALLOWED_EFFORTS)",
                self.allowed_efforts.join(", ")
            );
        }
        Ok(())
    }
}

/// The models the brain may choose when the owner set none: the fleet's own and the cheaper
/// aliases (a pricier model per task is the owner's choice: they opt in).
fn default_allowed_models(fleet_model: &str) -> Vec<String> {
    let mut v = vec![fleet_model.to_owned()];
    for m in ["sonnet", "haiku"] {
        if !v.iter().any(|x| x == m) {
            v.push(m.to_owned());
        }
    }
    v
}

/// The effort levels the brain may choose when the owner set none: not the expensive ones.
const DEFAULT_ALLOWED_EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// A comma-separated list from the environment; `default` only when the variable is unset (set
/// but empty is an empty list: nothing allowed).
fn env_list(name: &str, default: Vec<String>) -> Vec<String> {
    match std::env::var(name) {
        Ok(v) => v
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => default,
    }
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

/// The daily retrospective (see `retro`): one careful run a day, so this is where a strong model
/// and a high effort are worth their price.
#[derive(Debug, Clone)]
pub struct RetroConfig {
    /// The hour (0-23) in `secretary.timezone` at which the day is reviewed. A negative hour
    /// turns the retrospective off, which is the default: it does nothing until the owner asks
    /// for it and has somewhere for its tasks to run.
    pub hour: i64,
    /// Its Claude model and effort.
    pub model: String,
    pub effort: String,
    /// At most this many proposals per run: a day's improvements the owner will actually read.
    pub max_proposals: i64,
    /// The project a proposal becomes a task in — the fleet's own source, never a product
    /// project. "Make task" refuses when it does not exist.
    pub project: String,
}

impl RetroConfig {
    fn from_env(fleet_model: &str) -> Self {
        let text = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| default.into())
        };
        Self {
            hour: env_num("FLEET_RETRO_HOUR", -1),
            model: text("FLEET_RETRO_MODEL", fleet_model),
            effort: text("FLEET_RETRO_EFFORT", "high"),
            max_proposals: env_num("FLEET_RETRO_MAX_PROPOSALS", 3),
            project: text("FLEET_RETRO_PROJECT", "fleet-src"),
        }
    }

    pub fn test() -> Self {
        Self {
            // Off: the e2e forces a run itself, so no clock makes the test flaky.
            hour: -1,
            model: "opus".into(),
            effort: "high".into(),
            max_proposals: 2,
            project: "fleet-src".into(),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // A full model id, not the `opus` alias: an alias is resolved by whichever Claude CLI a
        // node happens to have, so the same task could run on different models on two machines,
        // and the fleet would have no way to say which. Pinned here, overridable per agent and
        // per task as before.
        let model = std::env::var("FLEET_MODEL").unwrap_or_else(|_| "claude-opus-5-5".into());
        crate::model::check_model_value("FLEET_MODEL", model.trim()).context(
            "FLEET_MODEL must be a Claude model name (an alias, an id, a Vertex or Bedrock id)",
        )?;
        let model = model.trim().to_owned();
        let allowed_models = env_list("FLEET_ALLOWED_MODELS", default_allowed_models(&model));
        let allowed_efforts = env_list(
            "FLEET_ALLOWED_EFFORTS",
            DEFAULT_ALLOWED_EFFORTS.map(str::to_owned).to_vec(),
        );
        for m in &allowed_models {
            crate::model::check_model_value("FLEET_ALLOWED_MODELS: model", m)?;
        }
        for e in &allowed_efforts {
            crate::model::check_effort(e).context("FLEET_ALLOWED_EFFORTS")?;
        }
        // The retrospective's default model is the fleet's own.
        let model_for_retro = model.clone();
        Ok(Self {
            public_url: std::env::var("FLEET_PUBLIC_URL")
                .context("FLEET_PUBLIC_URL is required")?
                .trim_end_matches('/')
                .to_owned(),
            owner_id: std::env::var("TELEGRAM_OWNER_ID")
                .context("TELEGRAM_OWNER_ID is required")?
                .parse()
                .context("TELEGRAM_OWNER_ID must be a number")?,
            model,
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
            max_agents_per_project: env_num("FLEET_MAX_AGENTS_PER_PROJECT", 3).max(1),
            delete_topics_after_minutes: env_num("FLEET_TOPIC_DELETE_AFTER_MINUTES", 60),
            secretary: SecretaryConfig::from_env(),
            retro: RetroConfig::from_env(&model_for_retro),
            allowed_models,
            allowed_efforts,
            archive: ArchiveConfig::from_env()?,
            // 1 MiB to 4 GiB: past that a file is not something to keep in the database.
            attachment_max_bytes: env_num("FLEET_ATTACHMENT_MAX_MB", DEFAULT_ATTACHMENT_MAX_MB)
                .clamp(1, 4096)
                * 1024
                * 1024,
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
            max_agents_per_project: 3,
            // Off by default in tests: the one test that wants it sets its own value.
            delete_topics_after_minutes: 0,
            secretary: SecretaryConfig::test(),
            retro: RetroConfig::test(),
            allowed_models: default_allowed_models("opus"),
            allowed_efforts: DEFAULT_ALLOWED_EFFORTS.map(str::to_owned).to_vec(),
            archive: ArchiveConfig::test(),
            // Small, so the e2e can show a file over the limit being refused without making one
            // of 50 MiB.
            attachment_max_bytes: 4 * 1024 * 1024,
        }
    }
}

fn env_num(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brain_chooses_within_the_allow_list() {
        let cfg = Config::test();
        assert!(cfg.check_allowed(None, None).is_ok());
        assert!(cfg.check_allowed(Some("opus"), Some("high")).is_ok());
        // The expensive efforts are the owner's to opt into.
        assert!(cfg.check_allowed(None, Some("max")).is_err());
        assert!(cfg.check_allowed(None, Some("xhigh")).is_err());
        assert!(cfg.check_allowed(Some("haiku"), Some("low")).is_ok());
        let e = cfg.check_allowed(Some("gpt-9"), None).unwrap_err();
        assert!(e.to_string().contains("not allowed") && e.to_string().contains("sonnet"));
        let narrow = Config {
            allowed_models: vec!["haiku".into()],
            allowed_efforts: vec!["low".into(), "medium".into()],
            ..Config::test()
        };
        assert!(narrow.check_allowed(Some("opus"), None).is_err());
        assert!(narrow.check_allowed(None, Some("high")).is_err());
        assert!(narrow.check_allowed(Some("haiku"), Some("medium")).is_ok());
        assert_eq!(
            default_allowed_models("claude-opus-4-1"),
            ["claude-opus-4-1", "sonnet", "haiku"]
        );
        assert_eq!(default_allowed_models("sonnet"), ["sonnet", "haiku"]);
    }

    #[test]
    fn a_time_of_day_is_hh_mm_or_off() {
        assert_eq!(parse_time_of_day("21:00").unwrap(), Some(21 * 60));
        assert_eq!(parse_time_of_day(" 07:05 ").unwrap(), Some(7 * 60 + 5));
        assert_eq!(parse_time_of_day("off").unwrap(), None);
        assert_eq!(parse_time_of_day("").unwrap(), None);
        for bad in ["24:00", "21:60", "21", "nine", "-1:00"] {
            assert!(parse_time_of_day(bad).is_err(), "{bad:?}");
        }
    }
}
