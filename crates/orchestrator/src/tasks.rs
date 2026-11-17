//! Tasks and agents: creation, placement on nodes, and delivering messages to sessions.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use fleet_proto::{Body, SecretFile, SpawnAgent, StopAgent};
use serde_json::json;

use crate::{
    chat::{From, clip, esc},
    core::Orchestrator,
    model::{AgentRow, BRAIN_AGENT, BRAIN_TASK, NewAgent, NodeRow, ProjectRow, TaskRow, status},
    prompts,
};

pub struct CreateTask {
    pub project: String,
    pub title: String,
    pub brief: String,
    pub ticket: Option<String>,
    /// Existing agent name or id.
    pub agent: Option<String>,
    pub new_agent: Option<NewAgent>,
}

pub async fn project(o: &Orchestrator, slug: &str) -> Result<ProjectRow> {
    sqlx::query_as("select * from projects where slug = $1")
        .bind(slug)
        .fetch_optional(&o.db)
        .await?
        .with_context(|| format!("unknown project {slug:?}; create it in the web UI first"))
}

/// The project's "needs you" (blocking) or "fyi" topic, created on first use.
pub async fn human_topic(o: &Orchestrator, slug: &str, blocking: bool) -> Result<i64> {
    let p = project(o, slug).await?;
    let (existing, col, label) = if blocking {
        (p.needs_you_topic, "needs_you_topic", "needs you")
    } else {
        (p.fyi_topic, "fyi_topic", "fyi")
    };
    if let Some(t) = existing {
        return Ok(t);
    }
    let topic = o
        .chat
        .create_topic(&format!("{} · {label}", p.slug))
        .await?;
    sqlx::query(&format!("update projects set {col} = $2 where slug = $1"))
        .bind(slug)
        .bind(topic)
        .execute(&o.db)
        .await?;
    Ok(topic)
}

pub async fn find_agent(o: &Orchestrator, name_or_id: &str) -> Result<Option<AgentRow>> {
    Ok(
        sqlx::query_as("select * from agents where id = $1 or lower(name) = lower($1)")
            .bind(name_or_id.trim_start_matches('@'))
            .fetch_optional(&o.db)
            .await?,
    )
}

/// Creates an agent identity with a free bot from the pool.
pub async fn create_agent(o: &Orchestrator, project: &str, new: &NewAgent) -> Result<AgentRow> {
    let name = new.name.trim();
    if name.is_empty() || name.len() > 32 {
        bail!("agent name must be 1-32 characters");
    }
    if !matches!(new.role.as_str(), "implementor" | "reviewer" | "tester") {
        bail!("role must be implementor, reviewer or tester");
    }
    if find_agent(o, name).await?.is_some() {
        bail!("an agent named {name} already exists; reuse it or pick another name");
    }
    let mut tx = o.db.begin().await?;
    let bot: Option<(i64, String)> =
        sqlx::query_as("select id, username from bots where agent_id is null order by id limit 1 for update skip locked")
            .fetch_optional(&mut *tx)
            .await?;
    let Some((bot_id, username)) = bot else {
        bail!("no free agent bots in the pool; add a bot token in the web UI (Agents page)");
    };
    let id = format!(
        "{}-{}",
        slugify(name),
        crate::web::random_token()[..6].to_lowercase()
    );
    sqlx::query("insert into agents (id, name, role, project, bot_id) values ($1, $2, $3, $4, $5)")
        .bind(&id)
        .bind(name)
        .bind(&new.role)
        .bind(project)
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("update bots set agent_id = $2 where id = $1")
        .bind(bot_id)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let display = format!("{name} · {}", new.role);
    let description = format!("Fleet agent {name}: {} for project {project}.", new.role);
    if let Err(e) = o.chat.set_bot_profile(bot_id, &display, &description).await {
        tracing::warn!("renaming bot @{username} failed: {e:#}");
    }
    o.audit(
        "orchestrator",
        "agent.create",
        json!({"agent": id, "bot": username, "project": project}),
    )
    .await;
    Ok(sqlx::query_as("select * from agents where id = $1")
        .bind(&id)
        .fetch_one(&o.db)
        .await?)
}

pub async fn create_task(o: &Arc<Orchestrator>, req: CreateTask) -> Result<TaskRow> {
    let proj = project(o, &req.project).await?;
    let agent = match (&req.agent, &req.new_agent) {
        (Some(a), _) => {
            let agent = find_agent(o, a)
                .await?
                .with_context(|| format!("no agent named {a}"))?;
            if agent.project.as_deref() != Some(proj.slug.as_str()) {
                bail!(
                    "agent {} belongs to project {:?}, not {}",
                    agent.name,
                    agent.project,
                    proj.slug
                );
            }
            agent
        }
        (None, Some(new)) => create_agent(o, &proj.slug, new).await?,
        (None, None) => bail!("give either agent (an existing agent) or new_agent"),
    };

    let id = match &req.ticket {
        Some(t) if !t.trim().is_empty() => format!("{}-{}", proj.slug, slugify_ticket(t)),
        _ => {
            let n: i32 = sqlx::query_scalar("update projects set next_ticket = next_ticket + 1 where slug = $1 returning next_ticket - 1")
                .bind(&proj.slug)
                .fetch_one(&o.db)
                .await?;
            format!("{}-{n}", proj.slug)
        }
    };
    if o.task(&id).await?.is_some() {
        bail!("task {id} already exists; use send_to_task to continue it");
    }
    let topic = o
        .chat
        .create_topic(&clip(&format!("{id} · {}", req.title), 120))
        .await?;
    sqlx::query("insert into tasks (id, project, title, brief, agent_id, status, topic_id) values ($1, $2, $3, $4, $5, 'queued', $6)")
        .bind(&id)
        .bind(&proj.slug)
        .bind(&req.title)
        .bind(&req.brief)
        .bind(&agent.id)
        .bind(topic)
        .execute(&o.db)
        .await?;
    let task = o.task(&id).await?.context("task vanished")?;
    let intro = format!(
        "🆕 <b>{}</b> · {}\nAgent: <b>{}</b> ({})\n<blockquote expandable>{}</blockquote>",
        esc(&task.id),
        esc(&task.title),
        esc(&agent.name),
        esc(&agent.role),
        esc(&clip(&task.brief, 3000))
    );
    let _ = o
        .chat
        .send(From::Orchestrator, Some(topic), &intro, vec![], None)
        .await;
    o.audit(
        "orchestrator",
        "task.create",
        json!({"task": id, "agent": agent.id}),
    )
    .await;
    dispatch(o, &task, None, None).await?;
    Ok(o.task(&id).await?.unwrap_or(task))
}

/// Starts a task's agent on a suitable node, or leaves it queued with a note.
///
/// `prompt` overrides the first message (follow-ups); `resume` resumes the stored session.
pub async fn dispatch(
    o: &Arc<Orchestrator>,
    task: &TaskRow,
    prompt: Option<String>,
    resume: Option<String>,
) -> Result<bool> {
    let agent = match &task.agent_id {
        Some(a) => {
            sqlx::query_as::<_, AgentRow>("select * from agents where id = $1")
                .bind(a)
                .fetch_one(&o.db)
                .await?
        }
        None => bail!("task {} has no agent", task.id),
    };
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes")
        .fetch_all(&o.db)
        .await?;
    let candidates: Vec<&NodeRow> = nodes
        .iter()
        .filter(|n| n.has_project(&task.project))
        .collect();
    if candidates.is_empty() {
        let note = format!(
            "⚠️ No node has project <b>{}</b> mapped. Run <code>fleet project map {} &lt;dir&gt;</code> on a host.",
            esc(&task.project),
            esc(&task.project)
        );
        o.post_task(task, &note).await;
        return Ok(false);
    }
    // A resumed session only exists on the node that ran it.
    let pinned = resume.as_ref().and(task.node_id.as_deref());
    let pick = |n: &&NodeRow| n.online && (n.running.0.len() as i32) < n.max_agents.max(1);
    let chosen = match pinned {
        Some(node_id) => candidates
            .iter()
            .find(|n| n.id == node_id)
            .copied()
            .filter(pick),
        None => {
            let preferred = agent.preferred_node.as_deref();
            let mut free: Vec<&NodeRow> = candidates.iter().copied().filter(pick).collect();
            free.sort_by_key(|n| (Some(n.id.as_str()) != preferred, n.running.0.len()));
            free.first().copied()
        }
    };
    let Some(node) = chosen else {
        let waiting_for = pinned
            .and_then(|id| nodes.iter().find(|n| n.id == id))
            .map(|n| n.name.clone())
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .map(|n| n.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            });
        sqlx::query("update tasks set status = 'queued', updated_at = now() where id = $1")
            .bind(&task.id)
            .execute(&o.db)
            .await?;
        if let Some(p) = &prompt {
            // Keep the follow-up so it is not lost while the task waits.
            sqlx::query("insert into messages (task_id, to_agent, text) values ($1, $2, $3)")
                .bind(&task.id)
                .bind(&agent.id)
                .bind(p)
                .execute(&o.db)
                .await?;
        }
        o.post_task(
            task,
            &format!(
                "⏳ Queued: waiting for {} (offline or at capacity).",
                esc(&waiting_for)
            ),
        )
        .await;
        return Ok(false);
    };

    let spawn = build_spawn(o, task, &agent, prompt, resume).await?;
    sqlx::query("update tasks set node_id = $2, updated_at = now() where id = $1")
        .bind(&task.id)
        .bind(&node.id)
        .execute(&o.db)
        .await?;
    o.server.send(node.id.parse()?, Body::SpawnAgent(spawn));
    Ok(true)
}

async fn build_spawn(
    o: &Orchestrator,
    task: &TaskRow,
    agent: &AgentRow,
    prompt: Option<String>,
    resume: Option<String>,
) -> Result<SpawnAgent> {
    let secret_files = secret_files(o, &task.project).await?;
    let names: Vec<String> = secret_files
        .iter()
        .map(|f| format!("{}.env", f.name))
        .collect();
    let is_brain = task.id == BRAIN_TASK;
    let system = if is_brain {
        prompts::brain_system_prompt(o.cfg.hindsight_mcp_url.is_some())
    } else {
        prompts::agent_system_prompt(agent, task, &names, o.cfg.hindsight_mcp_url.is_some())
    };
    // New work starts in plan mode so the human approves the plan; follow-ups to a running or
    // approved task continue in auto mode. The brain never plans.
    let plan_first = !is_brain && (resume.is_none() || task.status == status::DONE);
    let extra_mcp = o.cfg.hindsight_mcp_url.as_ref().map(|base| {
        let mut server = json!({"type": "http", "url": format!("{base}/mcp/{}/", task.project)});
        if let Some(key) = &o.cfg.hindsight_api_key {
            server["headers"] = json!({"Authorization": format!("Bearer {key}")});
        }
        json!({ "hindsight": server }).to_string()
    });
    // Follow-ups queued while the task waited for a node go first.
    let queued: Vec<String> = sqlx::query_scalar(
        "delete from messages where task_id = $1 and to_agent = $2 returning text",
    )
    .bind(&task.id)
    .bind(&agent.id)
    .fetch_all(&o.db)
    .await?;
    let mut first = prompt.unwrap_or_else(|| prompts::task_prompt(task));
    if !queued.is_empty() {
        first = format!("{}\n\n{first}", queued.join("\n\n"));
    }
    Ok(SpawnAgent {
        agent_id: agent.id.clone(),
        task_id: task.id.clone(),
        project: task.project.clone(),
        role: if is_brain {
            "orchestrator".into()
        } else {
            agent.role.clone()
        },
        prompt: first,
        append_system_prompt: Some(system),
        resume_session_id: resume,
        permission_mode: if plan_first {
            "plan".into()
        } else {
            "auto".into()
        },
        after_plan_mode: Some("auto".into()),
        model: Some(o.cfg.model.clone()),
        extra_args: vec![],
        extra_mcp_servers: extra_mcp,
        secret_files,
    })
}

/// The project's bound secret folders as `.env` files.
pub async fn secret_files(o: &Orchestrator, project: &str) -> Result<Vec<SecretFile>> {
    let rows: Vec<(String, String, Vec<u8>)> = sqlx::query_as(
        "select f.name, i.key, i.value from project_secrets ps
         join secret_folders f on f.id = ps.folder_id
         join secret_items i on i.folder_id = f.id
         where ps.project = $1 order by f.name, i.key",
    )
    .bind(project)
    .fetch_all(&o.db)
    .await?;
    let mut files: Vec<SecretFile> = Vec::new();
    for (folder, key, value) in rows {
        let value = o.vault.decrypt(&value)?;
        let name = slugify(&folder).replace('-', "_");
        let line = format!("{key}={}\n", env_quote(&value));
        match files.iter_mut().find(|f| f.name == name) {
            Some(f) => f.content.push_str(&line),
            None => files.push(SecretFile {
                name,
                content: line,
            }),
        }
    }
    Ok(files)
}

/// Delivers text to a task's agent: into the live session, or by resuming it.
pub async fn deliver(o: &Arc<Orchestrator>, task: &TaskRow, text: String) -> Result<()> {
    let agent_id = task.agent_id.clone().context("task has no agent")?;
    let live = task.running
        && match &task.node_id {
            Some(n) => o.server.is_connected(n.parse()?),
            None => false,
        };
    if live {
        let node: iroh::EndpointId = task.node_id.as_deref().unwrap_or_default().parse()?;
        o.server.send(
            node,
            Body::SendToAgent {
                agent_id,
                task_id: task.id.clone(),
                text,
            },
        );
        return Ok(());
    }
    if task.status == status::DONE
        || task.status == status::STOPPED
        || task.status == status::FAILED
    {
        if let Some(topic) = task.topic_id {
            let _ = o.chat.reopen_topic(topic).await;
        }
        sqlx::query("update tasks set status = 'running', updated_at = now() where id = $1")
            .bind(&task.id)
            .execute(&o.db)
            .await?;
    }
    dispatch(o, task, Some(text), task.session_id.clone()).await?;
    Ok(())
}

pub async fn stop(o: &Orchestrator, task: &TaskRow) -> Result<()> {
    if let (Some(node), Some(agent)) = (&task.node_id, &task.agent_id) {
        o.server.send(
            node.parse()?,
            Body::StopAgent(StopAgent {
                agent_id: agent.clone(),
                task_id: task.id.clone(),
            }),
        );
    }
    sqlx::query("update tasks set status = 'stopped', updated_at = now() where id = $1")
        .bind(&task.id)
        .execute(&o.db)
        .await?;
    Ok(())
}

/// Tries every queued task (after a node connects or frees capacity).
pub async fn dispatch_queued(o: &Arc<Orchestrator>) {
    let queued: Vec<TaskRow> = match sqlx::query_as(
        "select * from tasks where status = 'queued' and not running order by created_at",
    )
    .fetch_all(&o.db)
    .await
    {
        Ok(q) => q,
        Err(e) => return tracing::warn!("listing queued tasks failed: {e:#}"),
    };
    for task in queued {
        // Only retry when a node can take it now, so the topic is not spammed with queue notes.
        let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes where online")
            .fetch_all(&o.db)
            .await
            .unwrap_or_default();
        let can_run = nodes.iter().any(|n| {
            n.has_project(&task.project)
                && (n.running.0.len() as i32) < n.max_agents.max(1)
                && (task.session_id.is_none() || task.node_id.as_deref() == Some(n.id.as_str()))
        });
        if !can_run {
            continue;
        }
        let resume = task.session_id.clone();
        if let Err(e) = dispatch(o, &task, None, resume).await {
            tracing::warn!("dispatching {} failed: {e:#}", task.id);
        }
    }
}

/// Sends the owner's message to the brain, starting or resuming its session as needed.
pub async fn to_brain(o: &Arc<Orchestrator>, text: String) -> Result<()> {
    ensure_brain(o).await?;
    let task = o.task(BRAIN_TASK).await?.context("brain task missing")?;
    deliver(o, &task, text).await
}

async fn ensure_brain(o: &Orchestrator) -> Result<()> {
    let slug = &o.cfg.brain_project;
    sqlx::query("insert into projects (slug, name, description) values ($1, 'Fleet', 'The orchestrator brain''s workspace') on conflict do nothing")
        .bind(slug)
        .execute(&o.db)
        .await?;
    sqlx::query("insert into agents (id, name, role, project) values ($1, 'Orchestrator', 'orchestrator', $2) on conflict do nothing")
        .bind(BRAIN_AGENT)
        .bind(slug)
        .execute(&o.db)
        .await?;
    sqlx::query(
        "insert into tasks (id, project, title, brief, agent_id, status) values ($1, $2, 'Orchestrator brain', '', $3, 'running')
         on conflict do nothing",
    )
    .bind(BRAIN_TASK)
    .bind(slug)
    .bind(BRAIN_AGENT)
    .execute(&o.db)
    .await?;
    Ok(())
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(40).collect()
}

/// Tracker keys keep their case (`ABC-123`) but lose anything unsafe for ids and topic names.
fn slugify_ticket(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect()
}

fn env_quote(v: &str) -> String {
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@+=,".contains(c))
    {
        v.to_owned()
    } else {
        format!(
            "\"{}\"",
            v.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Alice the Builder!"), "alice-the-builder");
        assert_eq!(slugify_ticket(" ABC-123 "), "ABC-123");
        assert_eq!(env_quote("abc"), "abc");
        assert_eq!(env_quote("a b\"c"), "\"a b\\\"c\"");
    }
}
