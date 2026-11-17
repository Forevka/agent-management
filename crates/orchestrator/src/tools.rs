//! Implementations of the fleet MCP tools (the node forwards agents' calls here).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use fleet_net::EndpointId;
use fleet_proto::{ToolCall, ToolResult};
use serde_json::{Value, json};

use crate::{
    chat::{From, clip, esc},
    core::Orchestrator,
    human::{self, NewItem},
    model::{AgentRow, BRAIN_AGENT, NewAgent, TaskRow},
    tasks,
};

/// Agent-to-agent messages allowed per sender and recipient in ten minutes. Two agents stuck
/// politely answering each other would otherwise burn tokens forever.
const MAX_AGENT_MESSAGES: i64 = 12;

/// Returns `Ok(None)` when the answer comes later (a permission prompt waiting for the human).
pub async fn handle(
    o: &Arc<Orchestrator>,
    node: EndpointId,
    call: ToolCall,
) -> Result<Option<ToolResult>> {
    let args: Value = serde_json::from_str(&call.args).unwrap_or(json!({}));
    let task = o
        .task(&call.task_id)
        .await?
        .with_context(|| format!("unknown task {}", call.task_id))?;
    if task.agent_id.as_deref() != Some(call.agent_id.as_str()) {
        bail!(
            "agent {} is not assigned to task {}",
            call.agent_id,
            task.id
        );
    }
    // Only the node running the task may act for it.
    if task.node_id.as_deref() != Some(node.to_string().as_str()) {
        bail!("task {} does not run on this node", task.id);
    }
    let agent: AgentRow = sqlx::query_as("select * from agents where id = $1")
        .bind(&call.agent_id)
        .fetch_one(&o.db)
        .await?;
    let is_brain = agent.id == BRAIN_AGENT;
    let ok = |text: String| {
        Ok(Some(ToolResult {
            call_id: call.call_id.clone(),
            ok: true,
            result: text,
        }))
    };

    match call.tool.as_str() {
        "permission" => Ok(permission(o, node, &call, &task, &agent, &args).await?),
        "milestones_set" => {
            let items: Vec<String> = serde_json::from_value(args["items"].clone())
                .context("items must be a list of strings")?;
            sqlx::query("delete from milestones where task_id = $1")
                .bind(&task.id)
                .execute(&o.db)
                .await?;
            for (i, title) in items.iter().enumerate() {
                sqlx::query("insert into milestones (task_id, idx, title) values ($1, $2, $3)")
                    .bind(&task.id)
                    .bind(i as i32)
                    .bind(title)
                    .execute(&o.db)
                    .await?;
            }
            render_checklist(o, &task, true).await?;
            ok(format!("{} milestones published", items.len()))
        }
        "milestone_done" => {
            let idx = args["index"].as_i64().context("index required")?;
            let n =
                sqlx::query("update milestones set done = true where task_id = $1 and idx = $2")
                    .bind(&task.id)
                    .bind(idx as i32)
                    .execute(&o.db)
                    .await?
                    .rows_affected();
            if n == 0 {
                bail!("no milestone {idx}; call milestones_set first");
            }
            render_checklist(o, &task, false).await?;
            ok(format!("milestone {idx} done"))
        }
        "ask_human" => {
            let question = args["question"].as_str().context("question required")?;
            let blocking = args["blocking"].as_bool().unwrap_or(true);
            let row = human::create(
                o,
                NewItem {
                    task: Some(&task),
                    project: None,
                    agent_id: Some(agent.id.clone()),
                    kind: "ask",
                    blocking,
                    payload: json!({"question": question, "options": args["options"]}),
                    node_id: None,
                    call_id: None,
                },
            )
            .await?;
            ok(format!(
                "Queued as Q-{}. The answer will arrive as a message starting with [Answer to Q-{}].",
                row.id, row.id
            ))
        }
        "ask_agent" => {
            let target_name = args["agent"].as_str().context("agent required")?;
            let text = args["message"].as_str().context("message required")?;
            let target = tasks::find_agent(o, target_name)
                .await?
                .with_context(|| format!("no agent named {target_name}"))?;
            if target.id == agent.id {
                bail!("you cannot message yourself");
            }
            // Agents talk within their project; the orchestrator is reached by the owner only.
            if target.id == BRAIN_AGENT || (!is_brain && target.project != agent.project) {
                bail!("{} is not an agent of your project", target.name);
            }
            let recent: i64 = sqlx::query_scalar(
                "select count(*) from messages where from_agent = $1 and to_agent = $2 and created_at > now() - interval '10 minutes'",
            )
            .bind(&agent.id)
            .bind(&target.id)
            .fetch_one(&o.db)
            .await?;
            if recent >= MAX_AGENT_MESSAGES {
                bail!(
                    "too many messages to {} in the last 10 minutes; ask the human instead",
                    target.name
                );
            }
            let target_task: TaskRow = sqlx::query_as(
                "select * from tasks where agent_id = $1 order by (status in ('running','planning','waiting')) desc, updated_at desc limit 1",
            )
            .bind(&target.id)
            .fetch_optional(&o.db)
            .await?
            .with_context(|| format!("{} has no task to receive messages in", target.name))?;
            sqlx::query("insert into messages (task_id, from_agent, to_agent, text) values ($1, $2, $3, $4)")
                .bind(&task.id)
                .bind(&agent.id)
                .bind(&target.id)
                .bind(text)
                .execute(&o.db)
                .await?;
            let framed = format!(
                "[Message from {} ({}, task {})] {text}\nReply with ask_agent to {}.",
                agent.name, agent.role, task.id, agent.name
            );
            tasks::deliver(o, &target_task, framed).await?;
            o.post_task(
                &task,
                &format!("→ <b>{}</b>: {}", esc(&target.name), esc(&clip(text, 300))),
            )
            .await;
            ok(format!("Delivered to {}.", target.name))
        }
        "post" => {
            let text = args["text"].as_str().context("text required")?;
            o.post_task(&task, &esc(&clip(text, 1500))).await;
            ok("posted".into())
        }
        "complete" => {
            let summary = args["summary"].as_str().context("summary required")?;
            sqlx::query("update tasks set status = 'done', updated_at = now() where id = $1")
                .bind(&task.id)
                .execute(&o.db)
                .await?;
            o.post_task(
                &task,
                &format!("✅ <b>Done.</b>\n{}", esc(&clip(summary, 3000))),
            )
            .await;
            if let Some(topic) = task.topic_id {
                let _ = o.chat.close_topic(topic).await;
            }
            // The session ends after this turn, freeing the node; follow-ups resume it.
            tasks::stop(o, &task, true).await?;
            ok("Task closed. The session stays resumable for follow-ups.".into())
        }
        "memory_retain" | "memory_recall" | "memory_reflect" | "memory_banks" => {
            ok(memory(o, &agent, &task.project, &call.tool, &args).await?)
        }
        "feed_read" => ok(crate::feeds::read(o, &task.project, is_brain, &args).await?),
        "feed_search" => ok(crate::feeds::search(o, &task.project, is_brain, &args).await?),
        // Orchestration tools: the brain only.
        _ if !is_brain => bail!("unknown tool {}", call.tool),
        "list_projects" => ok(list_projects(o).await?),
        "list_agents" => {
            let rows: Vec<(String, String, String, Option<String>, Option<String>, i64)> = sqlx::query_as(
                "select a.id, a.name, a.role, a.project, b.username,
                        (select count(*) from tasks t where t.agent_id = a.id and t.running) as running
                 from agents a left join bots b on b.id = a.bot_id
                 where a.id <> 'orchestrator' and ($1::text is null or a.project = $1) order by a.project, a.name",
            )
            .bind(args["project"].as_str())
            .fetch_all(&o.db)
            .await?;
            let mut out = Vec::new();
            for (id, name, role, project, bot, running) in rows {
                let recent: Vec<String> = sqlx::query_scalar("select id || ': ' || title || ' [' || status || ']' from tasks where agent_id = $1 order by updated_at desc limit 5")
                    .bind(&id)
                    .fetch_all(&o.db)
                    .await?;
                out.push(json!({"id": id, "name": name, "role": role, "project": project, "bot": bot, "running": running > 0, "recent_tasks": recent}));
            }
            ok(serde_json::to_string_pretty(&out)?)
        }
        "list_tasks" => {
            let rows: Vec<TaskRow> = sqlx::query_as(
                "select * from tasks where id <> 'brain' and ($1::text is null or project = $1) and ($2::text is null or status = $2)
                 order by updated_at desc limit $3",
            )
            .bind(args["project"].as_str())
            .bind(args["status"].as_str())
            .bind(args["limit"].as_i64().unwrap_or(20).clamp(1, 100))
            .fetch_all(&o.db)
            .await?;
            let out: Vec<Value> = rows
                .iter()
                .map(|t| json!({"id": t.id, "title": t.title, "status": t.status, "agent": t.agent_id, "running": t.running, "updated": t.updated_at.to_rfc3339()}))
                .collect();
            ok(serde_json::to_string_pretty(&out)?)
        }
        "create_task" => {
            let new_agent: Option<NewAgent> = args
                .get("new_agent")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?;
            let t = tasks::create_task(
                o,
                tasks::CreateTask {
                    project: args["project"].as_str().context("project required")?.into(),
                    title: args["title"].as_str().context("title required")?.into(),
                    brief: args["brief"].as_str().context("brief required")?.into(),
                    ticket: args["ticket"].as_str().map(Into::into),
                    agent: args["agent"].as_str().map(Into::into),
                    new_agent,
                },
            )
            .await?;
            ok(format!("Created {} (status {}).", t.id, t.status))
        }
        "send_to_task" => {
            let id = args["task_id"].as_str().context("task_id required")?;
            let text = args["text"].as_str().context("text required")?;
            let t = o.task(id).await?.with_context(|| format!("no task {id}"))?;
            tasks::deliver(o, &t, format!("[Follow-up from the orchestrator] {text}")).await?;
            ok(format!("Sent to {id}."))
        }
        "stop_task" => {
            let id = args["task_id"].as_str().context("task_id required")?;
            let t = o.task(id).await?.with_context(|| format!("no task {id}"))?;
            tasks::stop(o, &t, false).await?;
            ok(format!("Stopping {id}."))
        }
        "propose_task" | "propose_slack_reply" => {
            let kind = if call.tool == "propose_task" {
                "task_proposal"
            } else {
                "slack_reply"
            };
            let project = match kind {
                "task_proposal" => args["project"].as_str().map(Into::into),
                _ => {
                    crate::feeds::channel_project(
                        o,
                        args["channel_id"].as_str().unwrap_or_default(),
                    )
                    .await?
                }
            };
            let row = human::create(
                o,
                NewItem {
                    task: None,
                    project: project.or(Some(o.cfg.brain_project.clone())),
                    agent_id: None,
                    kind,
                    blocking: true,
                    payload: args.clone(),
                    node_id: None,
                    call_id: None,
                },
            )
            .await?;
            ok(format!("Proposed as Q-{}; the human decides.", row.id))
        }
        "reply_owner" => {
            let text = args["text"].as_str().context("text required")?;
            o.chat
                .send(
                    From::Orchestrator,
                    None,
                    &esc(&clip(text, 3500)),
                    vec![],
                    None,
                )
                .await?;
            ok("sent".into())
        }
        other => bail!("unknown tool {other}"),
    }
}

/// A Claude permission prompt: plan approval, AskUserQuestion, or a tool needing approval.
async fn permission(
    o: &Arc<Orchestrator>,
    node: EndpointId,
    call: &ToolCall,
    task: &TaskRow,
    agent: &AgentRow,
    args: &Value,
) -> Result<Option<ToolResult>> {
    // Nodes send pending permission prompts again after every reconnect (the orchestrator may
    // have restarted): an open one is already waiting, an answered one gets its answer again.
    let existing: Option<(String, Option<sqlx::types::Json<Value>>)> =
        sqlx::query_as("select state, answer from human_items where node_id = $1 and call_id = $2")
            .bind(node.to_string())
            .bind(&call.call_id)
            .fetch_optional(&o.db)
            .await?;
    match existing {
        Some((state, _)) if state == "open" => return Ok(None),
        Some((_, Some(answer))) if answer.0.get("decision").is_some() => {
            return Ok(Some(ToolResult {
                call_id: call.call_id.clone(),
                ok: true,
                result: answer.0["decision"].to_string(),
            }));
        }
        Some(_) => {
            return Ok(Some(ToolResult {
                call_id: call.call_id.clone(),
                ok: true,
                result: json!({"behavior": "deny", "message": "This request was cancelled; ask again if it is still needed."}).to_string(),
            }));
        }
        None => {}
    }
    let tool = args["tool_name"].as_str().unwrap_or_default();
    let kind = match tool {
        "ExitPlanMode" => "plan",
        "AskUserQuestion" => "question",
        _ => "permission",
    };
    let payload = json!({"tool_name": tool, "input": args["input"], "plan": args["input"]["plan"]});
    human::create(
        o,
        NewItem {
            task: Some(task),
            project: None,
            agent_id: Some(agent.id.clone()),
            kind,
            blocking: true,
            payload,
            node_id: Some(node.to_string()),
            call_id: Some(call.call_id.clone()),
        },
    )
    .await?;
    Ok(None)
}

/// Posts (first time) or edits the task's milestone checklist.
async fn render_checklist(o: &Orchestrator, task: &TaskRow, replace: bool) -> Result<()> {
    let items: Vec<(String, bool)> =
        sqlx::query_as("select title, done from milestones where task_id = $1 order by idx")
            .bind(&task.id)
            .fetch_all(&o.db)
            .await?;
    let done = items.iter().filter(|(_, d)| *d).count();
    let mut html = format!("<b>Milestones</b> · {done}/{}\n", items.len());
    for (title, d) in &items {
        html.push_str(&format!("{} {}\n", if *d { "☑" } else { "☐" }, esc(title)));
    }
    let from = o.task_sender(task).await;
    let current: Option<i64> =
        sqlx::query_scalar("select checklist_msg_id from tasks where id = $1")
            .bind(&task.id)
            .fetch_one(&o.db)
            .await?;
    match current.filter(|_| !replace) {
        Some(msg) => o.chat.edit(from, msg, &html, vec![]).await?,
        None => {
            let msg = o
                .chat
                .send(from, task.topic_id, &html, vec![], None)
                .await?;
            sqlx::query("update tasks set checklist_msg_id = $2 where id = $1")
                .bind(&task.id)
                .bind(msg)
                .execute(&o.db)
                .await?;
        }
    }
    Ok(())
}

async fn list_projects(o: &Orchestrator) -> Result<String> {
    let projects: Vec<(String, String, String)> =
        sqlx::query_as("select slug, name, description from projects order by slug")
            .fetch_all(&o.db)
            .await?;
    let nodes: Vec<crate::model::NodeRow> = sqlx::query_as("select * from nodes")
        .fetch_all(&o.db)
        .await?;
    let mut out = Vec::new();
    for (slug, name, description) in projects {
        let on: Vec<Value> = nodes
            .iter()
            .filter(|n| n.has_project(&slug))
            .map(|n| json!({"node": n.name, "online": n.online}))
            .collect();
        let open: i64 = sqlx::query_scalar(
            "select count(*) from tasks where project = $1 and status not in ('done')",
        )
        .bind(&slug)
        .fetch_one(&o.db)
        .await?;
        out.push(json!({"slug": slug, "name": name, "description": description, "nodes": on, "open_tasks": open}));
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

/// `fleet agent memory ...`: forwards to Hindsight under the caller's bank rules.
async fn memory(
    o: &Orchestrator,
    agent: &AgentRow,
    project: &str,
    tool: &str,
    args: &Value,
) -> Result<String> {
    use crate::memory::{resolve_bank, visible};
    let mem = o
        .memory
        .as_ref()
        .context("long-term memory is not configured on this fleet")?;
    // The brain works in its own pseudo-project; its shared bank is the fleet's.
    let bank = || {
        resolve_bank(
            agent,
            project,
            args["bank"].as_str(),
            args["shared"].as_bool().unwrap_or(false),
        )
    };
    match tool {
        "memory_retain" => {
            let content = args["content"].as_str().context("text required")?;
            mem.retain(&bank()?, content, args["context"].as_str())
                .await
        }
        "memory_recall" => {
            let query = args["query"].as_str().context("query required")?;
            mem.recall(
                &bank()?,
                query,
                args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize,
            )
            .await
        }
        "memory_reflect" => {
            let query = args["query"].as_str().context("query required")?;
            mem.reflect(&bank()?, query).await
        }
        _ => {
            let mut banks: Vec<String> = mem
                .banks()
                .await?
                .into_iter()
                .filter(|b| visible(agent, project, b))
                .collect();
            for default in [
                crate::memory::agent_prefix(agent),
                crate::memory::project_bank(project),
            ] {
                if agent.id != BRAIN_AGENT && !banks.contains(&default) {
                    banks.push(default);
                }
            }
            banks.sort();
            Ok(format!(
                "Banks you can use (your prefix: {}):\n{}",
                crate::memory::agent_prefix(agent),
                banks
                    .iter()
                    .map(|b| format!("- {b}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        }
    }
}
