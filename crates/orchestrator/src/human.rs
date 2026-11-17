//! The human queue: everything waiting on the owner.
//!
//! Item kinds:
//! - `ask`: an agent's `ask_human` question. Answered by button or reply; the answer is
//!   delivered to the agent as a new message.
//! - `plan`, `question`, `permission`: Claude permission prompts (ExitPlanMode,
//!   AskUserQuestion, other tools). The agent's tool call is blocked until answered; the
//!   answer is sent back to the node as the tool result.
//! - `task_proposal`, `slack_reply`: suggestions from the brain, applied only on approval.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use fleet_proto::{Body, ToolResult};
use serde_json::{Value, json};

use crate::{
    chat::{Button, From, Keyboard, clip, esc},
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
            let qs = p["input"]["questions"]
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
            let input = clip(&p["input"].to_string(), 600);
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
        other => bail!("unknown item kind {other}"),
    };
    // Plan and question pages need a magic link; replace the placeholder now that we have the id.
    let keyboard = with_links(o, row.id, keyboard).await?;
    let topic = match &row.project {
        Some(p) => Some(tasks::human_topic(o, p, row.blocking).await?),
        None => None,
    };
    let msg = o
        .chat
        .send(from, topic, &html, keyboard.clone(), None)
        .await?;
    sqlx::query("update human_items set tg_msg = $2 where id = $1")
        .bind(row.id)
        .bind(msg)
        .execute(&o.db)
        .await?;
    if let Some(t) = task {
        let pointer = match row.kind.as_str() {
            "plan" => format!("📋 Plan ready: see <b>Q-{}</b>.", row.id),
            _ => format!("⏸ Waiting on <b>Q-{}</b>.", row.id),
        };
        let kb = if matches!(row.kind.as_str(), "plan" | "question") {
            keyboard
        } else {
            vec![]
        };
        let _ = o
            .chat
            .send(o.task_sender(t).await, t.topic_id, &pointer, kb, None)
            .await;
    }
    Ok(())
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
            if let Some(msg) = row.tg_msg {
                let from = match &row.task_id {
                    Some(t) => match o.task(t).await? {
                        Some(task) => o.task_sender(&task).await,
                        None => From::Orchestrator,
                    },
                    None => From::Orchestrator,
                };
                let _ = o
                    .chat
                    .edit(
                        from,
                        msg,
                        &format!("✅ <b>Q-{id}</b> answered: {}", esc(&clip(summary, 500))),
                        vec![],
                    )
                    .await;
            }
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
            let summary = answers
                .iter()
                .map(|(k, v)| format!("{}: {}", clip(k, 60), v.as_str().unwrap_or_default()))
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
        (kind, _) => bail!("that answer does not fit a {kind} item"),
    }
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
         and coalesce(reminded_at, created_at) < now() - make_interval(mins => $1::int)",
    )
    .bind(o.cfg.remind_after_minutes as i32)
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
            .send(From::Orchestrator, Some(topic), &text, vec![], row.tg_msg)
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
        sqlx::query_as("select * from human_items where tg_msg = $1 and state = 'open'")
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
