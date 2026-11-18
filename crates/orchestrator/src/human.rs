//! The human queue: everything waiting on the owner.
//!
//! Item kinds:
//! - `ask`: an agent's `ask_human` question. Answered by button or reply; the answer is
//!   delivered to the agent as a new message.
//! - `plan`, `question`, `permission`: Claude permission prompts (ExitPlanMode,
//!   AskUserQuestion, other tools). The agent's tool call is blocked until answered; the
//!   answer is sent back to the node as the tool result.
//! - `task_proposal`, `slack_reply`: suggestions from the brain, applied only on approval.
//! - `admin_change`: a change to the fleet's configuration proposed by the brain (see
//!   `admin.rs`). It runs only when the owner approves, and the outcome goes back to the brain.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use fleet_proto::{Body, ToolResult};
use serde_json::{Value, json};

use crate::{
    chat::{Button, From, Keyboard, Notify, clip, esc},
    core::Orchestrator,
    model::{HumanItemRow, TaskRow, status},
    tasks,
};

pub struct NewItem<'a> {
    pub task: Option<&'a TaskRow>,
    pub project: Option<String>,
    pub agent_id: Option<String>,
    pub kind: &'static str,
    pub blocking: bool,
    pub payload: Value,
    pub node_id: Option<String>,
    pub call_id: Option<String>,
}

/// What a permission item shows people: its redacted copy of the call (`display`), or the call
/// itself for items made before there was one. Never what is sent back to Claude.
pub(crate) fn shown(p: &Value) -> &Value {
    if p["display"].is_null() {
        &p["input"]
    } else {
        &p["display"]
    }
}

pub async fn create(o: &Orchestrator, item: NewItem<'_>) -> Result<HumanItemRow> {
    let project = item
        .project
        .clone()
        .or_else(|| item.task.map(|t| t.project.clone()));
    let row: HumanItemRow = sqlx::query_as(
        "insert into human_items (project, task_id, agent_id, kind, blocking, payload, node_id, call_id)
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning *",
    )
    .bind(&project)
    .bind(item.task.map(|t| t.id.clone()))
    .bind(&item.agent_id)
    .bind(item.kind)
    .bind(item.blocking)
    .bind(&item.payload)
    .bind(&item.node_id)
    .bind(&item.call_id)
    .fetch_one(&o.db)
    .await?;
    // The agent now waits on the owner: "typing…" would say the opposite.
    if item.blocking
        && let Some(task) = item.task
    {
        o.stop_working(&task.id);
    }
    if let Err(e) = announce(o, &row, item.task).await {
        tracing::warn!("announcing Q-{} failed: {e:#}", row.id);
    }
    if item.blocking
        && let Some(task) = item.task
        && matches!(item.kind, "ask" | "question" | "permission")
    {
        sqlx::query("update tasks set status = 'waiting', updated_at = now() where id = $1 and status = 'running'")
            .bind(&task.id)
            .execute(&o.db)
            .await?;
    }
    Ok(row)
}

/// Posts the item in the project's human topic (and a pointer in the task topic).
async fn announce(o: &Orchestrator, row: &HumanItemRow, task: Option<&TaskRow>) -> Result<()> {
    let from = match task {
        Some(t) => o.task_sender(t).await,
        None => From::Orchestrator,
    };
    let task_ref = task
        .map(|t| format!(" · <b>{}</b>", esc(&t.id)))
        .unwrap_or_default();
    let p = &row.payload.0;
    let (html, keyboard): (String, Keyboard) = match row.kind.as_str() {
        "ask" => {
            let q = p["question"].as_str().unwrap_or_default();
            let options: Vec<&str> = p["options"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let kb = options
                .iter()
                .enumerate()
                .map(|(i, opt)| vec![Button::callback(clip(opt, 60), format!("h|{}|{i}", row.id))])
                .collect();
            (
                format!(
                    "❓ <b>Q-{}</b>{task_ref}\n{}\n<i>Reply to this message to answer in your own words.</i>",
                    row.id,
                    esc(q)
                ),
                kb,
            )
        }
        "plan" => (
            format!("📋 <b>Q-{}</b>{task_ref}: plan ready for review.", row.id),
            vec![
                vec![Button::url("Open plan", link_placeholder(row.id))],
                vec![Button::callback(
                    "Approve as is",
                    format!("p|{}|ok", row.id),
                )],
            ],
        ),
        "question" => {
            // Buttons answer by option index, so the shown labels may be the redacted ones.
            let qs = shown(p)["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut kb: Keyboard = vec![vec![Button::url(
                "Answer on the web",
                link_placeholder(row.id),
            )]];
            // A single single-choice question can be answered right in Telegram.
            if qs.len() == 1 && !qs[0]["multiSelect"].as_bool().unwrap_or(false) {
                for (i, opt) in qs[0]["options"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                    .take(6)
                {
                    kb.push(vec![Button::callback(
                        clip(opt["label"].as_str().unwrap_or("?"), 60),
                        format!("q|{}|{i}", row.id),
                    )]);
                }
            }
            let text: Vec<String> = qs
                .iter()
                .map(|q| format!("• {}", esc(q["question"].as_str().unwrap_or_default())))
                .collect();
            (
                format!("❓ <b>Q-{}</b>{task_ref}\n{}", row.id, text.join("\n")),
                kb,
            )
        }
        "permission" => {
            let tool = p["tool_name"].as_str().unwrap_or("?");
            let input = clip(&shown(p).to_string(), 600);
            (
                format!(
                    "🔐 <b>Q-{}</b>{task_ref} wants to use <code>{}</code>\n<pre>{}</pre>",
                    row.id,
                    esc(tool),
                    esc(&input)
                ),
                vec![vec![
                    Button::callback("Allow", format!("a|{}|y", row.id)),
                    Button::callback("Deny", format!("a|{}|n", row.id)),
                ]],
            )
        }
        "task_proposal" => (
            format!(
                "💡 <b>Q-{}</b> Proposed task for <b>{}</b>: {}\n<blockquote expandable>{}</blockquote>{}",
                row.id,
                esc(p["project"].as_str().unwrap_or_default()),
                esc(p["title"].as_str().unwrap_or_default()),
                esc(&clip(p["brief"].as_str().unwrap_or_default(), 2500)),
                p["source"]
                    .as_str()
                    .map(|s| format!("\nSource: {}", esc(s)))
                    .unwrap_or_default()
            ),
            vec![vec![
                Button::callback("Create task", format!("x|{}|y", row.id)),
                Button::callback("Dismiss", format!("x|{}|n", row.id)),
            ]],
        ),
        "slack_reply" => (
            format!(
                "✉️ <b>Q-{}</b> Proposed Slack reply (sent <b>as you</b>) in <code>{}</code>:\n<blockquote>{}</blockquote>{}",
                row.id,
                esc(p["channel_id"].as_str().unwrap_or_default()),
                esc(p["text"].as_str().unwrap_or_default()),
                p["context"]
                    .as_str()
                    .map(|c| format!("\n<i>{}</i>", esc(&clip(c, 500))))
                    .unwrap_or_default()
            ),
            vec![vec![
                Button::callback("Send", format!("x|{}|y", row.id)),
                Button::callback("Dismiss", format!("x|{}|n", row.id)),
            ]],
        ),
        "admin_change" => (
            format!(
                "🛠 <b>Q-{}</b> The orchestrator asks to change the fleet:\n<b>{}</b>{}\n<i>Tap a button, or reply yes / no. Any other reply rejects it, with your words as the reason.</i>",
                row.id,
                esc(p["summary"].as_str().unwrap_or_default()),
                // The summary is ours; the note is the brain's own words, so it is labelled.
                p["note"]
                    .as_str()
                    .map(|n| format!(
                        "\n<i>Orchestrator's note (its own words):</i> {}",
                        esc(&clip(n, 500))
                    ))
                    .unwrap_or_default()
            ),
            vec![vec![
                Button::callback("Approve", format!("x|{}|y", row.id)),
                Button::callback("Reject", format!("x|{}|n", row.id)),
            ]],
        ),
        other => bail!("unknown item kind {other}"),
    };
    // Plan and question pages need a magic link; replace the placeholder now that we have the id.
    let keyboard = with_links(o, row.id, keyboard).await?;
    let topic = match &row.project {
        Some(p) => Some(tasks::human_topic(o, p, row.blocking).await?),
        None => None,
    };
    // What needs the owner sounds ("needs you"); an FYI item does not.
    let notify = if row.blocking {
        Notify::Sound
    } else {
        Notify::Silent
    };
    let msg = o
        .chat
        .send(from, topic, &html, keyboard.clone(), None, notify)
        .await?;
    let state: String =
        sqlx::query_scalar("update human_items set tg_msg = $2 where id = $1 returning state")
            .bind(row.id)
            .bind(msg)
            .fetch_one(&o.db)
            .await?;
    settle_late_copy(o, from, row.id, msg, &state).await;
    if let Some(t) = task {
        let pointer = match row.kind.as_str() {
            "plan" => format!("📋 Plan ready: see <b>Q-{}</b>.", row.id),
            _ => format!("⏸ Waiting on <b>Q-{}</b>.", row.id),
        };
        // Plans and questions can be answered from the task topic too, so the pointer carries
        // the same buttons. Like every copy, it is recorded and updated when the item is
        // answered (see `answer`). It is silent: the card in "needs you" already sounded.
        let kb = if matches!(row.kind.as_str(), "plan" | "question") {
            keyboard
        } else {
            vec![]
        };
        let sender = o.task_sender(t).await;
        if let Ok(copy) = o
            .chat
            .send(sender, t.topic_id, &pointer, kb, None, Notify::Silent)
            .await
        {
            let state: String = sqlx::query_scalar(
                "update human_items set tg_msgs = array_append(tg_msgs, $2) where id = $1 returning state",
            )
            .bind(row.id)
            .bind(copy)
            .fetch_one(&o.db)
            .await?;
            settle_late_copy(o, sender, row.id, copy, &state).await;
        }
    }
    Ok(())
}

/// A copy recorded after its item was already answered or cancelled (the owner was quick, or
/// the run ended meanwhile) missed the update of all copies: it gets the outcome here.
async fn settle_late_copy(o: &Orchestrator, from: From, id: i64, msg: i64, state: &str) {
    if state == "open" {
        return;
    }
    let html = format!("<b>Q-{id}</b> is {}.", esc(state));
    if let Err(e) = o.chat.edit(from, msg, &html, vec![]).await {
        tracing::debug!("updating a late copy of Q-{id} failed: {e:#}");
    }
}

const LINK_PLACEHOLDER: &str = "about:fleet-item-";

fn link_placeholder(id: i64) -> String {
    format!("{LINK_PLACEHOLDER}{id}")
}

async fn with_links(o: &Orchestrator, id: i64, kb: Keyboard) -> Result<Keyboard> {
    let mut out = kb;
    for row in &mut out {
        for b in row.iter_mut() {
            if b.url
                .as_deref()
                .is_some_and(|u| u.starts_with(LINK_PLACEHOLDER))
            {
                b.url = Some(crate::web::magic_link(o, &format!("/items/{id}")).await?);
            }
        }
    }
    Ok(out)
}

pub async fn item(o: &Orchestrator, id: i64) -> Result<HumanItemRow> {
    sqlx::query_as("select * from human_items where id = $1")
        .bind(id)
        .fetch_optional(&o.db)
        .await?
        .with_context(|| format!("no item Q-{id}"))
}

/// How the owner answered.
pub enum Answer {
    /// Picked option `n` (ask / single question) or typed text.
    Choice(usize),
    Text(String),
    /// Plan approved, optionally with answers to pending questions.
    Approve,
    /// Plan rejected with feedback for the agent.
    Revise(String),
    /// Answers to AskUserQuestion: question text -> answer.
    Questions(serde_json::Map<String, Value>),
    Allow,
    Deny,
    Accept,
    Dismiss,
}

/// Applies an answer. Returns a short confirmation for the UI.
pub async fn answer(o: &Arc<Orchestrator>, id: i64, ans: Answer) -> Result<String> {
    // Claim the item atomically so a double click or a web+Telegram race answers once.
    let claimed: Option<HumanItemRow> =
        sqlx::query_as("update human_items set state = 'answered', answered_at = now() where id = $1 and state = 'open' returning *")
            .bind(id)
            .fetch_optional(&o.db)
            .await?;
    let Some(row) = claimed else {
        let row = item(o, id).await?;
        return Ok(format!("Q-{id} is already {}", row.state));
    };
    let result = apply(o, &row, ans).await;
    match &result {
        Ok((summary, stored)) => {
            sqlx::query(
                "update human_items set answer = coalesce(answer, '{}'::jsonb) || $2 where id = $1",
            )
            .bind(id)
            .bind(stored)
            .execute(&o.db)
            .await?;
            // Every copy now shows the outcome, wherever it was answered.
            retire_copies(o, vec![row.clone()], &format!("answered: {summary}"));
            if let Some(task_id) = &row.task_id {
                // Still waiting if another blocking question of the task is open.
                sqlx::query(
                    "update tasks set status = 'running', updated_at = now() where id = $1 and status = 'waiting'
                     and not exists (select 1 from human_items where task_id = $1 and state = 'open' and blocking)",
                )
                .bind(task_id)
                    .execute(&o.db)
                    .await?;
            }
            o.audit(
                "owner",
                "item.answer",
                json!({"item": id, "kind": row.kind, "answer": summary}),
            )
            .await;
        }
        Err(_) => {
            // Re-open so the owner can try again.
            sqlx::query("update human_items set state = 'open', answered_at = null where id = $1")
                .bind(id)
                .execute(&o.db)
                .await?;
        }
    }
    result.map(|(s, _)| format!("Q-{id}: {s}"))
}

async fn apply(o: &Arc<Orchestrator>, row: &HumanItemRow, ans: Answer) -> Result<(String, Value)> {
    let p = &row.payload.0;
    match (row.kind.as_str(), ans) {
        ("ask", ans) => {
            let text = match ans {
                Answer::Choice(i) => p["options"][i]
                    .as_str()
                    .context("no such option")?
                    .to_owned(),
                Answer::Text(t) => t,
                _ => bail!("answer Q-{} with an option or text", row.id),
            };
            let task = o
                .task(row.task_id.as_deref().context("question has no task")?)
                .await?
                .context("task gone")?;
            tasks::deliver(
                o,
                &task,
                format!("[Answer to Q-{} from the human] {text}", row.id),
            )
            .await?;
            Ok((text.clone(), json!({"text": text})))
        }
        ("plan", Answer::Approve) => {
            reply_permission(
                o,
                row,
                json!({"behavior": "allow", "updatedInput": p["input"]}),
            )?;
            if let Some(t) = &row.task_id {
                sqlx::query(
                    "update tasks set status = 'running', updated_at = now() where id = $1",
                )
                .bind(t)
                .execute(&o.db)
                .await?;
            }
            Ok(("plan approved".into(), json!({"approved": true})))
        }
        ("plan", Answer::Revise(feedback)) | ("plan", Answer::Text(feedback)) => {
            let message = format!(
                "The human asked for changes to the plan. Revise it and present it again with ExitPlanMode.\n\nFeedback:\n{feedback}"
            );
            reply_permission(o, row, json!({"behavior": "deny", "message": message}))?;
            Ok((
                format!("changes requested: {feedback}"),
                json!({"approved": false, "feedback": feedback}),
            ))
        }
        ("question", ans) => {
            let qs = p["input"]["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let answers = match ans {
                Answer::Questions(m) => m,
                Answer::Choice(i) if qs.len() == 1 => {
                    let label = qs[0]["options"][i]["label"]
                        .as_str()
                        .context("no such option")?
                        .to_owned();
                    let mut m = serde_json::Map::new();
                    m.insert(
                        qs[0]["question"].as_str().unwrap_or_default().to_owned(),
                        Value::String(label),
                    );
                    m
                }
                Answer::Text(t) if qs.len() == 1 => {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        qs[0]["question"].as_str().unwrap_or_default().to_owned(),
                        Value::String(t),
                    );
                    m
                }
                _ => bail!("answer the questions on the web page"),
            };
            let mut updated = p["input"].clone();
            updated["answers"] = Value::Object(answers.clone());
            reply_permission(
                o,
                row,
                json!({"behavior": "allow", "updatedInput": updated}),
            )?;
            // Claude gets the answers keyed by its own words; people see the redacted copy: each
            // question, and each chosen option, as `shown` has it (the owner's own text as is).
            let shown_qs = &shown(p)["questions"];
            let summary = answers
                .iter()
                .map(|(k, v)| {
                    let v = v.as_str().unwrap_or_default();
                    let i = qs.iter().position(|q| q["question"] == k.as_str());
                    let question = i
                        .and_then(|i| shown_qs[i]["question"].as_str())
                        .unwrap_or("?");
                    let answer = i
                        .and_then(|i| {
                            let options = qs[i]["options"].as_array()?;
                            let j = options.iter().position(|o| o["label"] == v)?;
                            shown_qs[i]["options"][j]["label"].as_str()
                        })
                        .unwrap_or(v);
                    format!("{}: {}", clip(question, 60), answer)
                })
                .collect::<Vec<_>>()
                .join("; ");
            Ok((summary, Value::Object(answers)))
        }
        ("permission", Answer::Allow) => {
            reply_permission(
                o,
                row,
                json!({"behavior": "allow", "updatedInput": p["input"]}),
            )?;
            Ok(("allowed".into(), json!({"allow": true})))
        }
        ("permission", Answer::Deny) => {
            reply_permission(
                o,
                row,
                json!({"behavior": "deny", "message": "The human denied this action. Find another way or ask with ask_human."}),
            )?;
            Ok(("denied".into(), json!({"allow": false})))
        }
        // A typed reply to a permission prompt (its card or its pointer) is a "no, and here is
        // why": the agent gets the owner's words instead of a bare denial.
        ("permission", Answer::Text(t)) => {
            reply_permission(
                o,
                row,
                json!({"behavior": "deny", "message": format!("The human denied this action and wrote: {t}")}),
            )?;
            Ok((format!("denied: {t}"), json!({"allow": false, "note": t})))
        }
        ("task_proposal", Answer::Accept) => {
            let project = p["project"].as_str().unwrap_or_default().to_owned();
            let agent = pick_implementor(o, &project).await?;
            let task = tasks::create_task(
                o,
                tasks::CreateTask {
                    project: project.clone(),
                    title: p["title"].as_str().unwrap_or("Task").into(),
                    brief: p["brief"].as_str().unwrap_or_default().into(),
                    ticket: None,
                    agent: agent.clone(),
                    new_agent: agent.is_none().then(|| crate::model::NewAgent {
                        name: format!("{}-dev", tasks::slugify(&project)),
                        role: "implementor".into(),
                    }),
                    // The agent's defaults: a proposal does not choose them.
                    model: None,
                    effort: None,
                },
            )
            .await?;
            Ok((format!("created {}", task.id), json!({"task": task.id})))
        }
        ("slack_reply", Answer::Accept) => {
            let poster = o
                .feed_poster
                .read()
                .await
                .clone()
                .context("no Slack source is configured")?;
            let ts = poster
                .post_reply(
                    p["channel_id"].as_str().unwrap_or_default(),
                    p["thread_id"].as_str(),
                    p["text"].as_str().unwrap_or_default(),
                )
                .await?;
            Ok(("sent to Slack".into(), json!({"ts": ts})))
        }
        ("task_proposal" | "slack_reply", Answer::Dismiss) => {
            Ok(("dismissed".into(), json!({"dismissed": true})))
        }
        // The item was claimed before this runs, so an approved change runs at most once. From
        // here on nothing may fail: an error would re-open the item and allow a second run.
        ("admin_change", ans) => {
            // Too old to run whatever the answer: the fleet has likely moved on since.
            if Utc::now() - row.created_at
                > chrono::Duration::hours(crate::admin::PROPOSAL_TTL_HOURS)
            {
                let summary = format!(
                    "expired: proposals are only good for {}h; ask again if it is still needed",
                    crate::admin::PROPOSAL_TTL_HOURS
                );
                tell_brain(o, row.id, &summary);
                return Ok((summary, json!({"expired": true})));
            }
            // A typed reply: a plain yes or no decides, anything else rejects with the owner's
            // words as the reason (the announcement says so).
            let (approve, reason) = match ans {
                Answer::Accept | Answer::Approve => (true, None),
                Answer::Dismiss | Answer::Deny => (false, None),
                Answer::Text(t) | Answer::Revise(t) => {
                    match t
                        .trim()
                        .trim_end_matches(['.', '!'])
                        .to_lowercase()
                        .as_str()
                    {
                        "yes" | "y" | "ok" | "okay" | "approve" | "approved" => (true, None),
                        "no" | "n" | "reject" | "rejected" => (false, None),
                        _ => (false, Some(t)),
                    }
                }
                _ => bail!("approve or reject Q-{}", row.id),
            };
            if approve {
                let (summary, stored) = crate::admin::run_approved(o, row).await?;
                tell_brain(o, row.id, &summary);
                return Ok((summary, stored));
            }
            let summary = match &reason {
                Some(r) => format!("rejected: {r}"),
                None => "rejected by the owner".into(),
            };
            tell_brain(o, row.id, &summary);
            Ok((summary, json!({"approved": false, "note": reason})))
        }
        (kind, _) => bail!("that answer does not fit a {kind} item"),
    }
}

/// Shows `outcome` ("answered: …", "cancelled: …") on every message that shows these items,
/// their cards and their pointers in task topics, and removes the buttons, so nothing answered
/// or cancelled still looks open anywhere. In the background, so a button press is answered at
/// once and node events never wait for Telegram; a failed edit is only logged.
pub fn retire_copies(o: &Arc<Orchestrator>, rows: Vec<HumanItemRow>, outcome: &str) {
    if rows.is_empty() {
        return;
    }
    let (o, outcome) = (o.clone(), outcome.to_owned());
    let icon = if outcome.starts_with("answered") {
        "✅"
    } else {
        "⏹"
    };
    tokio::spawn(async move {
        for row in rows {
            // All copies of an item were sent by the same bot: its task's, or the orchestrator.
            let from = match &row.task_id {
                Some(t) => match o.task(t).await {
                    Ok(Some(task)) => o.task_sender(&task).await,
                    _ => From::Orchestrator,
                },
                None => From::Orchestrator,
            };
            let html = format!("{icon} <b>Q-{}</b> {}", row.id, esc(&clip(&outcome, 500)));
            for msg in row.tg_msg.iter().chain(&row.tg_msgs) {
                if let Err(e) = o.chat.edit(from, *msg, &html, vec![]).await {
                    tracing::debug!("updating a copy of Q-{} failed: {e:#}", row.id);
                }
            }
        }
    });
}

/// Reports an admin change's outcome to the brain. Queued, in order with the brain's other
/// reports: the change already happened, and the owner's button press is answered without
/// waiting for the brain's session to start.
fn tell_brain(o: &Orchestrator, id: i64, outcome: &str) {
    o.tell_brain(format!("[Admin change Q-{id}] {outcome}"));
}

async fn pick_implementor(o: &Orchestrator, project: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "select a.id from agents a where a.project = $1 and a.role = 'implementor'
         order by (select count(*) from tasks t where t.agent_id = a.id and t.status in ('running','planning','waiting')), a.last_active desc nulls last limit 1",
    )
    .bind(project)
    .fetch_optional(&o.db)
    .await?)
}

/// Answers a blocked permission tool call on its node.
fn reply_permission(o: &Orchestrator, row: &HumanItemRow, decision: Value) -> Result<()> {
    let node = row.node_id.as_deref().context("item has no node")?;
    let call_id = row.call_id.clone().context("item has no call")?;
    // Kept so a prompt the node sends again (after an orchestrator restart lost the answer in
    // flight) is answered the same way. Merged, so it commutes with the summary written later.
    let (db, id, stored) = (
        o.db.clone(),
        row.id,
        json!({ "decision": decision.clone() }),
    );
    tokio::spawn(async move {
        let _ = sqlx::query(
            "update human_items set answer = coalesce(answer, '{}'::jsonb) || $2 where id = $1",
        )
        .bind(id)
        .bind(stored)
        .execute(&db)
        .await;
    });
    o.server.send(
        node.parse()?,
        Body::ToolResult(ToolResult {
            call_id,
            ok: true,
            result: decision.to_string(),
        }),
    );
    Ok(())
}

/// Re-pings blocking items nobody answered.
pub async fn remind(o: &Orchestrator) -> Result<()> {
    let due: Vec<HumanItemRow> = sqlx::query_as(
        "select * from human_items where state = 'open' and blocking
         and coalesce(reminded_at, created_at) < now() - make_interval(mins => $1::int)
         and not (kind = 'admin_change' and created_at < now() - make_interval(hours => $2::int))",
    )
    .bind(o.cfg.remind_after_minutes as i32)
    // Expired proposals are not worth a reminder; answering one only says it expired.
    .bind(crate::admin::PROPOSAL_TTL_HOURS as i32)
    .fetch_all(&o.db)
    .await?;
    for row in due {
        let Some(project) = &row.project else {
            continue;
        };
        let topic = tasks::human_topic(o, project, true).await?;
        let age = Utc::now() - row.created_at;
        let text = format!(
            "⏰ Still waiting on <b>Q-{}</b> ({}h).",
            row.id,
            age.num_hours()
        );
        let _ = o
            .chat
            // A reminder is a call for the owner.
            .send(
                From::Orchestrator,
                Some(topic),
                &text,
                vec![],
                row.tg_msg,
                Notify::Sound,
            )
            .await;
        sqlx::query("update human_items set reminded_at = now() where id = $1")
            .bind(row.id)
            .execute(&o.db)
            .await?;
    }
    Ok(())
}

/// Finds the open item a Telegram reply refers to.
pub async fn by_message(o: &Orchestrator, msg_id: i64) -> Result<Option<HumanItemRow>> {
    Ok(
        // A reply to any copy of the item (the card or its pointer) answers it.
        sqlx::query_as(
            "select * from human_items where (tg_msg = $1 or $1 = any(tg_msgs)) and state = 'open'",
        )
        .bind(msg_id)
        .fetch_optional(&o.db)
        .await?,
    )
}

pub async fn open_items(o: &Orchestrator) -> Result<Vec<HumanItemRow>> {
    Ok(sqlx::query_as(
        "select * from human_items where state = 'open' order by blocking desc, created_at",
    )
    .fetch_all(&o.db)
    .await?)
}

/// Marks a task waiting/running consistently when its agent asks something non-blocking.
pub fn is_waiting(task: &TaskRow) -> bool {
    task.status == status::WAITING
}
