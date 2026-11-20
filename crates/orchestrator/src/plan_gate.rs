//! The plan gate: who approves an agent's plan — the owner, or the orchestrator on their behalf.
//!
//! Every new run of a task plans first (`tasks::plan_first`), and so does every run of a task
//! that is `done`. A workflow is `done` from the moment it is set up, so each of its ticks, and
//! each message the owner writes into its topic, used to end in a plan card waiting for a tap —
//! for a job the owner approved once already and wants off their hands. The gate makes that a
//! property of the workflow (or of a project) rather than of a prompt, which a new session or a
//! lost follow-up would forget.
//!
//! - `owner` (the default for projects and for new workflows): the plan is a card the owner
//!   approves, as always.
//! - `auto` (one tap on a workflow's card; the workflows that existed before the gate): the agent
//!   still plans, and the plan is still recorded, but the orchestrator approves it through the
//!   very path the owner's tap takes and posts it silently in the task's topic, with the way back.
//!   A new workflow is not `auto` by default because the agent sets it up itself
//!   (`workflow_finish`), unasked as far as the orchestrator can tell.
//!
//! **Who sets it.** Only the owner: the workflow card's button, the project page in the web UI.
//! Nothing the brain or an agent can call changes it — no flag on `create_task`, `send_to_task`
//! or `workflow_finish`, and no `admin_change` either — because the brain reads tickets and Slack,
//! and a gate it could open is a gate anything it reads could open (see `docs/plan.md`).
//!
//! **What it never does.** It only answers `ExitPlanMode`: questions and tool permissions wait
//! for the owner whatever the gate says. And a plan that says it needs them (`habits::stopper`),
//! or a task that already has something else waiting on them, is left to them as before.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::json;

use crate::{
    chat::{Button, Notify, clip, esc},
    core::Orchestrator,
    human::{self, Answer},
    model::{HumanItemRow, TaskRow},
    tasks,
};

/// Who approves a task's plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Owner,
    Auto,
}

impl Gate {
    pub fn parse(s: &str) -> Result<Gate> {
        Ok(match s {
            "owner" => Gate::Owner,
            "auto" => Gate::Auto,
            other => bail!("{other:?} is not a plan gate; use \"owner\" or \"auto\""),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Gate::Owner => "owner",
            Gate::Auto => "auto",
        }
    }
}

/// Where a task's gate comes from: its workflow's setting, or its project's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Workflow,
    Project,
}

/// A task's gate and where it was set. A workflow's own setting wins over its project's, so a
/// workflow the owner keeps a hand on stays theirs in an `auto` project, and the other way round.
/// The fleet's own sessions never plan, and nothing of theirs is ever approved on anyone's behalf.
pub async fn gate(o: &Orchestrator, task: &TaskRow) -> Result<(Gate, Source)> {
    if crate::model::is_builtin_task(&task.id) {
        return Ok((Gate::Owner, Source::Project));
    }
    let workflow: Option<String> =
        sqlx::query_scalar("select plan_gate from workflows where task_id = $1")
            .bind(&task.id)
            .fetch_optional(&o.db)
            .await?;
    if let Some(g) = workflow {
        return Ok((Gate::parse(&g)?, Source::Workflow));
    }
    let project: Option<String> =
        sqlx::query_scalar("select plan_gate from projects where slug = $1")
            .bind(&task.project)
            .fetch_optional(&o.db)
            .await?;
    Ok((
        project
            .as_deref()
            .map(Gate::parse)
            .transpose()?
            .unwrap_or(Gate::Owner),
        Source::Project,
    ))
}

/// Whether this plan of `task` is the orchestrator's to approve, and on whose setting. `plan` is
/// the owner's (redacted) copy of the plan text. `None` means the owner approves it, as always —
/// which is also the answer whenever anything here cannot be checked.
pub async fn auto_approves(o: &Orchestrator, task: &TaskRow, plan: &str) -> Option<Source> {
    let (gate, source) = match gate(o, task).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("reading the plan gate of {} failed: {e:#}", task.id);
            return None;
        }
    };
    if gate != Gate::Auto {
        return None;
    }
    if let Some(marker) = crate::habits::stopper(plan) {
        tracing::info!(
            "the plan gate of {} leaves its plan to the owner: it says {marker:?}",
            task.id
        );
        return None;
    }
    // The agent is already waiting on the owner for something of its own (a question it asked):
    // this is not a plan to approve over their head.
    let waiting = sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from human_items where task_id = $1 and state = 'open' and blocking)",
    )
    .bind(&task.id)
    .fetch_one(&o.db)
    .await;
    match waiting {
        Ok(false) => Some(source),
        Ok(true) => None,
        Err(e) => {
            tracing::warn!("checking what {} waits on failed: {e:#}", task.id);
            None
        }
    }
}

/// Approves `item` (a plan created without a card, see `human::create_with`) on the gate's
/// behalf, and says so in the task's topic. If the approval fails, the item is announced after
/// all, so it waits for the owner like any other instead of for nobody.
pub async fn approve(o: &Arc<Orchestrator>, item: &HumanItemRow, task: &TaskRow, source: Source) {
    // The same call the owner's tap makes, so this is an approval in every respect: the node's
    // blocked tool call is answered, the session switches out of plan mode, the task runs.
    if let Err(e) = human::answer(o, item.id, Answer::Approve).await {
        tracing::warn!(
            "the plan gate could not approve Q-{} of {}: {e:#}",
            item.id,
            task.id
        );
        // Now the agent does wait on the owner, so "typing…" would say the opposite (what
        // `human::create_with` does for a plan announced at once).
        o.stop_working(&task.id);
        if let Err(e) = human::announce(o, item, Some(task)).await {
            tracing::error!(
                "Q-{} of {} is open and could not be announced: {e:#}",
                item.id,
                task.id
            );
        }
        return;
    }
    // `answer` also succeeds when there was nothing left to approve (the run ended and its plan
    // was cancelled in between). Only a plan really approved here is recorded as the gate's, and
    // it is marked so: the habits' reflection counts the owner's own decisions, not these.
    let ours = sqlx::query(
        "update human_items set answer = answer || '{\"by\": \"plan_gate\"}'::jsonb
          where id = $1 and coalesce((answer->>'approved')::bool, false)",
    )
    .bind(item.id)
    .execute(&o.db)
    .await;
    match ours {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return,
        Err(e) => tracing::warn!("marking Q-{} as the gate's failed: {e:#}", item.id),
    }
    // Under its own actor, beside the "owner" row `human::answer` writes: an approval nobody
    // tapped must be findable afterwards, with the setting that made it.
    o.audit(
        "owner (via plan gate)",
        "plan_gate.apply",
        json!({"item": item.id, "task": task.id, "gate": match source {
            Source::Workflow => "workflow",
            Source::Project => "project",
        }}),
    )
    .await;
    let plan = human::owner_payload(item)["plan"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if let Err(e) = post(o, item, task, source, &plan).await {
        tracing::warn!(
            "posting the auto-approved plan of {} failed: {e:#}",
            task.id
        );
    }
}

/// The record of an approved plan in the task's topic: silent (it needs nothing from the owner),
/// with the plan itself and the one tap that hands plans back to them.
async fn post(
    o: &Arc<Orchestrator>,
    item: &HumanItemRow,
    task: &TaskRow,
    source: Source,
    plan: &str,
) -> Result<()> {
    let (why, keyboard) = match source {
        // Telegram refuses callback data over 64 bytes, and a refused send would lose the record
        // itself: a very long task id gets a link to the task's page instead of the button.
        Source::Workflow => {
            let data = format!("wf|{}|gate_owner", task.id);
            let button = if data.len() <= 64 {
                Button::callback("🔒 Ask me for plans", data)
            } else {
                Button::url(
                    "Open the task",
                    crate::web::magic_link(o, &format!("/tasks/{}", task.id)).await?,
                )
            };
            (
                "This workflow's plans don't wait for you. Tap 🔒 on its settings card to \
                 approve them yourself again."
                    .to_owned(),
                vec![vec![button]],
            )
        }
        Source::Project => (
            format!(
                "Plans of {} don't wait for you; change that on its project page.",
                task.project
            ),
            vec![vec![Button::url(
                "Project settings",
                crate::web::magic_link(o, &format!("/projects/{}", task.project)).await?,
            )]],
        ),
    };
    let html = format!(
        "📋 <b>Plan approved automatically</b> · <b>Q-{}</b>\n<blockquote expandable>{}</blockquote>\n<i>{}</i>",
        item.id,
        esc(&clip(plan, 2500)),
        esc(&why)
    );
    let topic = match task.topic_id {
        Some(t) => t,
        None => tasks::human_topic(o, &task.project, false).await?,
    };
    o.chat
        .send(
            o.task_sender(task).await,
            Some(topic),
            &html,
            keyboard,
            None,
            Notify::Silent,
        )
        .await?;
    Ok(())
}

/// Sets a project's gate. The owner's alone: called from the web UI's project page, and
/// deliberately not an `AdminAction`, so the brain cannot even propose it.
pub async fn set_project(o: &Orchestrator, slug: &str, gate: Gate) -> Result<String> {
    let n = sqlx::query("update projects set plan_gate = $2 where slug = $1")
        .bind(slug)
        .bind(gate.as_str())
        .execute(&o.db)
        .await?
        .rows_affected();
    anyhow::ensure!(n == 1, "unknown project {slug:?}");
    o.audit(
        "owner",
        "plan_gate.project",
        json!({"project": slug, "gate": gate.as_str()}),
    )
    .await;
    Ok(match gate {
        Gate::Owner => format!("Plans of {slug} wait for your approval."),
        Gate::Auto => {
            format!("Plans of {slug} are approved automatically and posted in their topic.")
        }
    })
}

/// How a gate reads on a workflow's card.
pub fn describe(gate: Gate) -> &'static str {
    match gate {
        Gate::Owner => "Plans: wait for your approval.",
        Gate::Auto => "Plans: approved automatically and posted here.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_and_refuses_the_rest() {
        for g in [Gate::Owner, Gate::Auto] {
            assert_eq!(Gate::parse(g.as_str()).unwrap(), g);
        }
        assert!(Gate::parse("yes").is_err());
        assert!(Gate::parse("").is_err());
    }
}
