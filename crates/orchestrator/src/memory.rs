//! Long-term memory: one Hindsight instance for the whole fleet, reached only by the
//! orchestrator.
//!
//! Agents never talk to Hindsight directly. They call `fleet agent memory ...`, and the
//! orchestrator maps the request onto a bank and forwards it. That keeps Hindsight and its key
//! inside the VPS, and lets the orchestrator enforce who may use which bank:
//! - every agent owns the banks under its prefix `agent-<agent-id>` (its main bank is the
//!   prefix itself, others are `agent-<agent-id>--<name>`);
//! - the agents of a project share `project-<slug>`;
//! - the brain may use any bank;
//! - the secretary only its own: what it remembers comes from third parties' messages, so it
//!   never writes where other agents read.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::model::{AgentRow, BRAIN_AGENT, SECRETARY_AGENT};

#[derive(Clone)]
pub struct Hindsight {
    base: String,
    key: Option<String>,
    http: reqwest::Client,
}

impl Hindsight {
    pub fn new(base: &str, key: Option<String>) -> Result<Self> {
        // reqwest is built without a bundled crypto provider; use ring like the rest of the
        // process. Fails harmlessly if a provider is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            key,
            http,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.context("Hindsight is unreachable")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "Hindsight answered {status}: {}",
                crate::chat::clip(&text, 300)
            );
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// Stores a memory. Processing (fact extraction) runs in the background on Hindsight.
    pub async fn retain(&self, bank: &str, content: &str, context: Option<&str>) -> Result<String> {
        let mut item = json!({ "content": content });
        if let Some(c) = context {
            item["context"] = json!(c);
        }
        self.call(
            reqwest::Method::POST,
            &format!("/v1/default/banks/{bank}/memories"),
            Some(json!({"items": [item], "async": true})),
        )
        .await?;
        Ok(format!("Stored in bank {bank}."))
    }

    pub async fn recall(&self, bank: &str, query: &str, limit: usize) -> Result<String> {
        let v = match self
            .call(
                reqwest::Method::POST,
                &format!("/v1/default/banks/{bank}/memories/recall"),
                Some(json!({"query": query})),
            )
            .await
        {
            Ok(v) => v,
            // A bank nobody has written to yet is simply empty.
            Err(e) if e.to_string().contains("404") => {
                return Ok(format!("Bank {bank} has no memories yet."));
            }
            Err(e) => return Err(e),
        };
        let results = v["results"].as_array().cloned().unwrap_or_default();
        if results.is_empty() {
            return Ok(format!("No memories in {bank} match."));
        }
        let lines: Vec<String> = results
            .iter()
            .take(limit)
            .map(|r| {
                let ctx = r["context"]
                    .as_str()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                format!("- {}{ctx}", r["text"].as_str().unwrap_or_default())
            })
            .collect();
        Ok(format!("Memories from {bank}:\n{}", lines.join("\n")))
    }

    pub async fn reflect(&self, bank: &str, query: &str) -> Result<String> {
        let v = self
            .call(
                reqwest::Method::POST,
                &format!("/v1/default/banks/{bank}/reflect"),
                Some(json!({"query": query})),
            )
            .await?;
        Ok(v["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string()))
    }

    pub async fn banks(&self) -> Result<Vec<String>> {
        let v = self
            .call(reqwest::Method::GET, "/v1/default/banks", None)
            .await?;
        let list = v["banks"]
            .as_array()
            .or_else(|| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|b| {
                b["bank_id"]
                    .as_str()
                    .or(b["id"].as_str())
                    .or(b.as_str())
                    .map(str::to_owned)
            })
            .collect())
    }
}

pub fn agent_prefix(agent: &AgentRow) -> String {
    format!("agent-{}", agent.id)
}

pub fn project_bank(project: &str) -> String {
    format!("project-{project}")
}

/// The bank a request refers to, after checking the caller may use it.
pub fn resolve_bank(
    agent: &AgentRow,
    project: &str,
    bank: Option<&str>,
    shared: bool,
) -> Result<String> {
    if agent.id == SECRETARY_AGENT
        && (shared || bank.is_some_and(|b| b.trim() == project_bank(project)))
    {
        bail!(
            "the secretary keeps its memory in its own banks (agent-secretary, agent-secretary--<name>)"
        );
    }
    if shared {
        return Ok(project_bank(project));
    }
    let name = match bank.map(str::trim).filter(|b| !b.is_empty()) {
        None if agent.id == BRAIN_AGENT => "fleet".to_owned(),
        None => agent_prefix(agent),
        Some(b) if agent.id == BRAIN_AGENT => b.to_owned(),
        Some(b)
            if b == agent_prefix(agent)
                || b.starts_with(&format!("{}--", agent_prefix(agent)))
                || b == project_bank(project) =>
        {
            b.to_owned()
        }
        Some(b) => format!("{}--{b}", agent_prefix(agent)),
    };
    if name.len() > 120
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("bank names may only contain letters, digits, '-' and '_'");
    }
    Ok(name)
}

/// Whether the caller may see a bank in `memory banks`.
pub fn visible(agent: &AgentRow, project: &str, bank: &str) -> bool {
    agent.id == BRAIN_AGENT
        || (bank == project_bank(project) && agent.id != SECRETARY_AGENT)
        || bank == agent_prefix(agent)
        || bank.starts_with(&format!("{}--", agent_prefix(agent)))
}

/// How a task ended, for the memory saved about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed,
}

/// Most of a summary (or failure reason) saved with a task's outcome.
const OUTCOME_TEXT_MAX: usize = 4000;

/// The memory saved when a task ends: which task, by whom, and what it says it did (or what went
/// wrong), clipped.
pub fn outcome_memory(
    task_id: &str,
    title: &str,
    agent: &str,
    outcome: Outcome,
    text: &str,
) -> String {
    let text = crate::chat::clip(text.trim(), OUTCOME_TEXT_MAX);
    match outcome {
        Outcome::Done => {
            format!("Task {task_id} \"{title}\" (agent {agent}) was completed.\nSummary: {text}")
        }
        Outcome::Failed => {
            format!("Task {task_id} \"{title}\" (agent {agent}) failed.\nWhat happened: {text}")
        }
    }
}

/// Saves how a task ended to its project's shared bank (the one `memory --shared` uses), so the
/// project's memory builds up even when no agent thought to retain anything. Not for the
/// fleet's own sessions, and only with memory configured. In the background and best effort:
/// the caller never waits on Hindsight or fails because of it. Everything is passed through the
/// project's secret redaction first (the title and names too, not only the summary).
pub fn remember_outcome(
    o: &std::sync::Arc<crate::core::Orchestrator>,
    task: &crate::model::TaskRow,
    agent: &str,
    outcome: Outcome,
    text: &str,
) {
    if crate::model::is_builtin_task(&task.id) {
        return;
    }
    let Some(mem) = o.memory.clone() else {
        return;
    };
    let content = outcome_memory(&task.id, &task.title, agent, outcome, text);
    let (o, project, task_id) = (o.clone(), task.project.clone(), task.id.clone());
    tokio::spawn(async move {
        let content = match crate::tasks::redact_project_secrets(&o, &project, &content).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("not saving how {task_id} ended: redacting it failed: {e:#}");
                return;
            }
        };
        let context = match outcome {
            Outcome::Done => "task completion",
            Outcome::Failed => "task failure",
        };
        if let Err(e) = mem
            .retain(&project_bank(&project), &content, Some(context))
            .await
        {
            tracing::warn!("saving how {task_id} ended to memory failed: {e:#}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_memories_say_which_task_and_stay_compact() {
        let m = outcome_memory(
            "demo-1",
            "Paint it",
            "Alice",
            Outcome::Done,
            "Blue, two coats.",
        );
        assert_eq!(
            m,
            "Task demo-1 \"Paint it\" (agent Alice) was completed.\nSummary: Blue, two coats."
        );
        let long = "x".repeat(10_000);
        let m = outcome_memory("demo-1", "Paint it", "Alice", Outcome::Failed, &long);
        assert!(m.contains("failed.\nWhat happened: ") && m.chars().count() < 4200);
    }

    fn agent(id: &str) -> AgentRow {
        AgentRow {
            id: id.into(),
            name: id.into(),
            role: "implementor".into(),
            project: Some("p".into()),
            bot_id: None,
            preferred_node: None,
            model: None,
            effort: None,
        }
    }

    #[test]
    fn banks_stay_under_the_agent_prefix() {
        let a = agent("alice-x1");
        assert_eq!(
            resolve_bank(&a, "p", None, false).unwrap(),
            "agent-alice-x1"
        );
        assert_eq!(
            resolve_bank(&a, "p", Some("notes"), false).unwrap(),
            "agent-alice-x1--notes"
        );
        assert_eq!(
            resolve_bank(&a, "p", Some("agent-alice-x1--notes"), false).unwrap(),
            "agent-alice-x1--notes"
        );
        assert_eq!(resolve_bank(&a, "p", None, true).unwrap(), "project-p");
        // Naming another agent's bank lands under our own prefix instead.
        assert_eq!(
            resolve_bank(&a, "p", Some("agent-bob-y2"), false).unwrap(),
            "agent-alice-x1--agent-bob-y2"
        );
        assert!(resolve_bank(&a, "p", Some("../x"), false).is_err());
        let brain = agent(BRAIN_AGENT);
        assert_eq!(resolve_bank(&brain, "fleet", None, false).unwrap(), "fleet");
        assert_eq!(
            resolve_bank(&brain, "fleet", Some("project-p"), false).unwrap(),
            "project-p"
        );
        assert!(visible(&a, "p", "agent-alice-x1--notes") && visible(&a, "p", "project-p"));
        assert!(!visible(&a, "p", "agent-bob-y2") && !visible(&a, "p", "project-q"));
    }
}
