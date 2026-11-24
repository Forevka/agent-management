//! The fleet's standing Telegram topics: created on first use and remembered in `kv`, so a
//! fleet that never uses one never has it.
//!
//! What goes where is a product decision (see `docs/plan.md`): the **Inbox** is only for someone
//! reaching the owner on a third-party platform (todos, digests and answers about those
//! messages, reply drafts, and what was passed to the orchestrator about them);
//! **agent-communication** is the fleet talking to itself (the brain's notes to the secretary,
//! agents' messages to each other), which the owner can read but never has to;
//! **Rules** is the log of the secretary's triage rules and the owner's habits, so everything
//! standing is searchable in one place; **Retro** is the daily retrospective's: what the fleet
//! concluded about its own day and what it proposes to do better; **Roadmap** is the sprint
//! plan's: the cards that approve tickets and a line for each ticket the fleet starts.
//! A task's own topic stays that agent's work, and General stays what needs the owner.

use anyhow::Result;
use serde_json::json;

use crate::core::Orchestrator;

pub const INBOX: &str = "📋 Inbox";
pub const AGENTS: &str = "🤖 agent-communication";
pub const RULES: &str = "📜 Rules";
pub const RETRO: &str = "📈 Retro";
pub const ROADMAP: &str = "🗺 Roadmap";

const INBOX_KEY: &str = "inbox_topic";
const AGENTS_KEY: &str = "agents_topic";
const RULES_KEY: &str = "rules_topic";
const RETRO_KEY: &str = "retro_topic";
const ROADMAP_KEY: &str = "roadmap_topic";

/// The topic id remembered under `key`, if it was created already.
async fn topic_if_any(o: &Orchestrator, key: &str) -> Result<Option<i64>> {
    Ok(o.kv_get(key).await?.and_then(|v| v.as_i64()))
}

/// The topic remembered under `key`, created on first use.
async fn topic(o: &Orchestrator, key: &str, name: &str) -> Result<i64> {
    if let Some(t) = topic_if_any(o, key).await? {
        return Ok(t);
    }
    // One creator: two first uses at once must not make two topics.
    let lock = o.task_lock(&format!("topic:{key}"));
    let _held = lock.lock().await;
    if let Some(t) = topic_if_any(o, key).await? {
        return Ok(t);
    }
    let t = o.chat.create_topic(name).await?;
    o.kv_set(key, json!(t)).await?;
    Ok(t)
}

/// The Inbox topic, if it was created already.
pub async fn inbox_if_any(o: &Orchestrator) -> Result<Option<i64>> {
    topic_if_any(o, INBOX_KEY).await
}

pub async fn inbox(o: &Orchestrator) -> Result<i64> {
    topic(o, INBOX_KEY, INBOX).await
}

pub async fn agents(o: &Orchestrator) -> Result<i64> {
    topic(o, AGENTS_KEY, AGENTS).await
}

pub async fn rules(o: &Orchestrator) -> Result<i64> {
    topic(o, RULES_KEY, RULES).await
}

/// Where the daily retrospective puts its verdict and its proposals.
pub async fn retro(o: &Orchestrator) -> Result<i64> {
    topic(o, RETRO_KEY, RETRO).await
}

/// Where the roadmap asks the owner to approve tickets and says what it started.
pub async fn roadmap(o: &Orchestrator) -> Result<i64> {
    topic(o, ROADMAP_KEY, ROADMAP).await
}
