//! Tasks and agents: creation, placement on nodes, and delivering messages to sessions.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use fleet_proto::{Body, SecretFile, SpawnAgent, StopAgent};
use serde_json::{Value, json};

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

/// Agent names show up in Telegram, bot profiles, summaries the owner approves and messages to
/// other agents, so they are kept plain: 1-40 letters, digits, spaces and `_.-`.
pub fn check_agent_name(name: &str) -> Result<()> {
    let n = name.chars().count();
    ensure!(
        (1..=40).contains(&n)
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || " _.-".contains(c)),
        "agent name must be 1-40 characters of letters, digits, spaces, _ . and -"
    );
    Ok(())
}

/// Creates an agent identity with a free bot from the pool.
pub async fn create_agent(o: &Orchestrator, project: &str, new: &NewAgent) -> Result<AgentRow> {
    let name = new.name.trim();
    check_agent_name(name)?;
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
    let (display, description) = bot_profile(name, &new.role, project);
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

/// The Telegram name and description of an agent's bot.
pub fn bot_profile(name: &str, role: &str, project: &str) -> (String, String) {
    (
        format!("{name} · {role}"),
        format!("Fleet agent {name}: {role} for project {project}."),
    )
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
) -> Result<Dispatched> {
    let lock = o.task_lock(&task.id);
    let _held = lock.lock().await;
    // The caller's copy may predate a run another caller started while we waited.
    let task = o.task(&task.id).await?.context("task vanished")?;
    if task.running {
        return Ok(Dispatched::Lost);
    }
    dispatch_locked(o, &task, prompt, resume).await
}

/// Starts a queued task (resuming its session), unless it stopped being queued in the meantime:
/// the caller's list of queued tasks may be stale by the time each one's turn comes.
async fn dispatch_if_queued(o: &Arc<Orchestrator>, task: &TaskRow) -> Result<Dispatched> {
    let lock = o.task_lock(&task.id);
    let _held = lock.lock().await;
    let task = o.task(&task.id).await?.context("task vanished")?;
    if task.running || task.status != status::QUEUED {
        return Ok(Dispatched::Lost);
    }
    let resume = task.session_id.clone();
    dispatch_locked(o, &task, None, resume).await
}

/// What [`dispatch`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatched {
    /// A run was claimed and sent to a node.
    Started,
    /// No node can take it now: the task waits (with any follow-up kept for later).
    Waiting,
    /// Another dispatch claimed the task first; nothing was sent.
    Lost,
}

/// [`dispatch`] with the task's lock already held.
async fn dispatch_locked(
    o: &Arc<Orchestrator>,
    task: &TaskRow,
    prompt: Option<String>,
    resume: Option<String>,
) -> Result<Dispatched> {
    let agent = match &task.agent_id {
        Some(a) => {
            sqlx::query_as::<_, AgentRow>("select * from agents where id = $1")
                .bind(a)
                .fetch_one(&o.db)
                .await?
        }
        None => bail!("task {} has no agent", task.id),
    };
    // Held from the busy check to the claim: two tasks of the agent dispatched at once must not
    // both find it free. (Lock order: task, then agent.)
    let agent_lock = o.agent_lock(&agent.id);
    let _agent_held = agent_lock.lock().await;
    // One agent runs one session at a time: a specialist busy with another task takes this one
    // when it is done (the node would otherwise kill the running session).
    let busy: Option<String> = sqlx::query_scalar(
        "select id from tasks where agent_id = $1 and running and id <> $2 limit 1",
    )
    .bind(&agent.id)
    .bind(&task.id)
    .fetch_optional(&o.db)
    .await?;
    if let Some(other) = busy {
        return queue(
            o,
            task,
            &agent.id,
            prompt.as_deref(),
            Some(format!(
                "⏳ Queued: {} is busy with <b>{}</b>.",
                esc(&agent.name),
                esc(&other)
            )),
        )
        .await;
    }
    let nodes = node_load(o).await?;
    let candidates: Vec<&(NodeRow, i64)> = nodes
        .iter()
        .filter(|(n, _)| n.has_project(&task.project))
        .collect();
    if candidates.is_empty() {
        // The task waits (with the message kept) until a node with the project connects; the
        // owner is told once per wait, not once per message. A wait starts at the last dispatch
        // (or the task's creation), and the note is marked in the transcript.
        let told: bool = sqlx::query_scalar(
            "select exists(select 1 from task_events where task_id = $1 and kind = 'waiting' and at >= $2)",
        )
        .bind(&task.id)
        .bind(task.dispatched_at.unwrap_or(task.created_at))
        .fetch_one(&o.db)
        .await?;
        let note = if told {
            None
        } else {
            sqlx::query("insert into task_events (task_id, kind, data) values ($1, 'waiting', $2)")
                .bind(&task.id)
                .bind(format!("no node has project {}", task.project))
                .execute(&o.db)
                .await?;
            Some(format!(
                "⏳ Waiting for a node with project <b>{}</b>: run <code>fleet project map {} &lt;dir&gt;</code> on a host.",
                esc(&task.project),
                esc(&task.project)
            ))
        };
        return queue(o, task, &agent.id, prompt.as_deref(), note).await;
    }
    // A resumed session only exists on the node that ran it. Once that node is gone (revoked),
    // the session is gone with it: the task starts afresh elsewhere.
    let resume = resume.filter(|_| {
        task.node_id
            .as_deref()
            .is_none_or(|id| nodes.iter().any(|(n, _)| n.id == id))
    });
    let pinned = resume.as_ref().and(task.node_id.as_deref());
    let free = |(n, load): &&(NodeRow, i64)| n.online && *load < i64::from(n.max_agents.max(1));
    let chosen = match pinned {
        Some(node_id) => candidates
            .iter()
            .find(|(n, _)| n.id == node_id)
            .copied()
            .filter(free),
        None => {
            let preferred = agent.preferred_node.as_deref();
            let mut ok: Vec<&(NodeRow, i64)> = candidates.iter().copied().filter(free).collect();
            ok.sort_by_key(|(n, load)| (Some(n.id.as_str()) != preferred, *load));
            ok.first().copied()
        }
    };
    let Some((node, _)) = chosen else {
        let waiting_for = pinned
            .and_then(|id| nodes.iter().find(|(n, _)| n.id == id))
            .map(|(n, _)| n.name.clone())
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .map(|(n, _)| n.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            });
        return queue(
            o,
            task,
            &agent.id,
            prompt.as_deref(),
            Some(format!(
                "⏳ Queued: waiting for {} (offline or at capacity).",
                esc(&waiting_for)
            )),
        )
        .await;
    };

    let run_id = crate::web::random_token()[..12].to_owned();
    let new_status = if plan_first(task, resume.as_deref()) {
        status::PLANNING
    } else {
        status::RUNNING
    };
    // Claim the run before building it (which takes the queued follow-ups) and sending it: from
    // here on the task counts as running (capacity, busy agent, message delivery), and only
    // events of this run may change it. Only one claim can win while the task is not running,
    // and none once the agent or the node is gone: the share locks wait for an agent removal or
    // a node revoke in progress, which end or refuse running tasks under their own locks.
    let claimed = sqlx::query(
        "update tasks set node_id = $2, run_id = $3, running = true, dispatched_at = now(), status = $4, updated_at = now()
         where id = $1 and not running
           and exists (select 1 from agents where id = $5 for share)
           and exists (select 1 from nodes where id = $2 for share)",
    )
    .bind(&task.id)
    .bind(&node.id)
    .bind(&run_id)
    .bind(new_status)
    .bind(&agent.id)
    .execute(&o.db)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        let (running, agent_exists): (bool, bool) = sqlx::query_as(
            "select running, exists(select 1 from agents where id = $2) from tasks where id = $1",
        )
        .bind(&task.id)
        .bind(&agent.id)
        .fetch_one(&o.db)
        .await?;
        if running || !agent_exists {
            return Ok(Dispatched::Lost);
        }
        // The chosen node was revoked a moment ago: wait (keeping the message) for the next
        // chance, which the revoke itself triggers.
        return queue(
            o,
            task,
            &agent.id,
            prompt.as_deref(),
            Some("⏳ Queued: the chosen node just left the fleet.".into()),
        )
        .await;
    }
    // The run takes the task's waiting messages and the new one. They stay in the queue, tagged
    // with the run, until the node reports it started (then they go); a run that never starts
    // gives them back (`release_run_messages`), so the next one gets them.
    sqlx::query(
        "update messages set run_id = $3 where task_id = $1 and to_agent = $2 and from_agent is null and run_id is null",
    )
    .bind(&task.id)
    .bind(&agent.id)
    .bind(&run_id)
    .execute(&o.db)
    .await?;
    if let Some(p) = &prompt {
        sqlx::query(
            "insert into messages (task_id, to_agent, text, run_id) values ($1, $2, $3, $4)",
        )
        .bind(&task.id)
        .bind(&agent.id)
        .bind(p)
        .bind(&run_id)
        .execute(&o.db)
        .await?;
    }
    let spawn = match build_spawn(o, task, &agent, resume, run_id.clone()).await {
        Ok(s) => s,
        Err(e) => {
            // Release the claim (back to the node it had), give the messages back for the next
            // try, and mark the task failed: retrying on every node event would repeat the same
            // failure. A new message to the task tries again.
            sqlx::query(
                "update tasks set running = false, node_id = $3, status = 'failed', updated_at = now() where id = $1 and run_id = $2",
            )
            .bind(&task.id)
            .bind(&run_id)
            .bind(&task.node_id)
            .execute(&o.db)
            .await?;
            release_run_messages(o, &run_id).await?;
            tracing::warn!("preparing a run of {} failed: {e:#}", task.id);
            o.post_task(
                task,
                &format!(
                    "⚠️ Could not prepare the agent's run: {}",
                    esc(&format!("{e:#}"))
                ),
            )
            .await;
            return Ok(Dispatched::Waiting);
        }
    };
    o.server
        .send(node.id.parse()?, Body::SpawnAgent(Box::new(spawn)));
    Ok(Dispatched::Started)
}

/// New work starts in plan mode so the human approves the plan; follow-ups to a running or
/// approved task continue in auto mode. The brain never plans.
fn plan_first(task: &TaskRow, resume: Option<&str>) -> bool {
    task.id != BRAIN_TASK && (resume.is_none() || task.status == status::DONE)
}

/// Leaves a task queued (keeping a follow-up message for when it starts) and says why.
async fn queue(
    o: &Orchestrator,
    task: &TaskRow,
    agent_id: &str,
    prompt: Option<&str>,
    note: Option<String>,
) -> Result<Dispatched> {
    sqlx::query("update tasks set status = 'queued', updated_at = now() where id = $1")
        .bind(&task.id)
        .execute(&o.db)
        .await?;
    if let Some(p) = prompt {
        // Keep the follow-up so it is not lost while the task waits.
        sqlx::query("insert into messages (task_id, to_agent, text) values ($1, $2, $3)")
            .bind(&task.id)
            .bind(agent_id)
            .bind(p)
            .execute(&o.db)
            .await?;
    }
    if let Some(note) = note {
        o.post_task(task, &note).await;
    }
    Ok(Dispatched::Waiting)
}

/// Every node with the number of agent runs it is assigned (from our own bookkeeping, which is
/// current even before the node reports back).
async fn node_load(o: &Orchestrator) -> Result<Vec<(NodeRow, i64)>> {
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes")
        .fetch_all(&o.db)
        .await?;
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        let load: i64 =
            sqlx::query_scalar("select count(*) from tasks where node_id = $1 and running")
                .bind(&n.id)
                .fetch_one(&o.db)
                .await?;
        out.push((n, load));
    }
    Ok(out)
}

/// Gives the messages a run took back to its task's queue: for a run that ended without ever
/// starting (once it starts they are gone already, so this is harmless for any ended run).
///
/// Returns how many were given back.
pub(crate) async fn release_run_messages(o: &Orchestrator, run_id: &str) -> Result<u64> {
    Ok(
        sqlx::query("update messages set run_id = null where run_id = $1")
            .bind(run_id)
            .execute(&o.db)
            .await?
            .rows_affected(),
    )
}

/// The run started: the messages it took are delivered.
pub(crate) async fn run_started(o: &Orchestrator, run_id: &str) -> Result<()> {
    sqlx::query("delete from messages where run_id = $1")
        .bind(run_id)
        .execute(&o.db)
        .await?;
    Ok(())
}

async fn build_spawn(
    o: &Orchestrator,
    task: &TaskRow,
    agent: &AgentRow,
    resume: Option<String>,
    run_id: String,
) -> Result<SpawnAgent> {
    let secret_files = secret_files(o, &task.project).await?;
    let names: Vec<String> = secret_files
        .iter()
        .map(|f| format!("{}.env", f.name))
        .collect();
    let is_brain = task.id == BRAIN_TASK;
    let system = if is_brain {
        prompts::brain_system_prompt(o.memory.is_some())
    } else {
        prompts::agent_system_prompt(agent, task, &names, o.memory.is_some())
    };
    let plan_first = plan_first(task, resume.as_deref());
    // What this run took: follow-ups that waited for it, then the new message, in order.
    let texts: Vec<String> =
        sqlx::query_scalar("select text from messages where run_id = $1 order by id")
            .bind(&run_id)
            .fetch_all(&o.db)
            .await?;
    // A fresh session (nothing to resume: a new task, or its old session's node is gone) knows
    // nothing yet, so it starts with the task's brief, then the messages. The brain has no brief
    // beyond its system prompt. A resumed session already has it; it only gets the brief when
    // there is nothing else to say (a queued task starting).
    let fresh = resume.is_none() && !is_brain;
    let mut parts = Vec::new();
    if fresh || texts.is_empty() {
        parts.push(prompts::task_prompt(task));
    }
    parts.extend(texts);
    let first = parts.join("\n\n");
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
        extra_mcp_servers: None,
        secret_files,
        run_id,
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

/// Removes the values of the project's secret folders from agent-written text before it goes
/// anywhere else (Telegram, the brain), with the same rules the node applies to transcripts.
pub async fn redact_project_secrets(o: &Orchestrator, project: &str, text: &str) -> Result<String> {
    let values = project_secret_values(o, project).await?;
    let mut out = text.to_owned();
    fleet_proto::redact::redact_secrets(&mut out, &values);
    Ok(out)
}

/// The tool argument fields that carry an agent's free text, which is passed on to people and
/// other agents. Everything else (ids, slugs, names, pages, channels, banks) is left exactly as
/// given: a secret that happens to equal a project slug must not break the call.
const FREE_TEXT_FIELDS: &[&str] = &[
    "text",
    "summary",
    "question",
    "options",
    "message",
    "brief",
    "title",
    "items",
    "content",
    "context",
    "note",
    "description",
    "source",
    "query",
    "ticket",
];

/// A copy of `v` to show people, with the project's secret values taken out of every string.
/// Only for display: the original is what the agent gets back.
pub async fn redact_display(o: &Orchestrator, project: &str, v: &Value) -> Result<Value> {
    fn all(v: &mut Value, values: &[String]) {
        match v {
            Value::String(s) => fleet_proto::redact::redact_secrets(s, values),
            Value::Array(a) => a.iter_mut().for_each(|x| all(x, values)),
            Value::Object(m) => m.values_mut().for_each(|x| all(x, values)),
            _ => {}
        }
    }
    let mut copy = v.clone();
    all(&mut copy, &project_secret_values(o, project).await?);
    Ok(copy)
}

/// [`redact_project_secrets`] over the free-text fields of an agent's tool arguments.
pub async fn redact_project_json(o: &Orchestrator, project: &str, v: &mut Value) -> Result<()> {
    let values = project_secret_values(o, project).await?;
    if !values.is_empty() {
        redact_free_text(v, &values);
    }
    Ok(())
}

/// Redacts every string under a [`FREE_TEXT_FIELDS`] key, at any depth (a proposal's `params`
/// included); strings one by one, so the JSON stays intact whatever the values are.
fn redact_free_text(v: &mut Value, values: &[String]) {
    fn all(v: &mut Value, values: &[String]) {
        match v {
            Value::String(s) => fleet_proto::redact::redact_secrets(s, values),
            Value::Array(a) => a.iter_mut().for_each(|x| all(x, values)),
            Value::Object(m) => m.values_mut().for_each(|x| all(x, values)),
            _ => {}
        }
    }
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if FREE_TEXT_FIELDS.contains(&k.as_str()) {
                    all(x, values);
                } else {
                    redact_free_text(x, values);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact_free_text(x, values)),
        _ => {}
    }
}

/// The decrypted values of the project's bound secret folders.
async fn project_secret_values(o: &Orchestrator, project: &str) -> Result<Vec<String>> {
    let encrypted: Vec<Vec<u8>> = sqlx::query_scalar(
        "select i.value from project_secrets ps join secret_items i on i.folder_id = ps.folder_id where ps.project = $1",
    )
    .bind(project)
    .fetch_all(&o.db)
    .await?;
    encrypted.iter().map(|e| o.vault.decrypt(e)).collect()
}

/// Delivers text to a task's agent: into its session if a run is assigned (queued in the link
/// while the node is offline), or by resuming the session.
///
/// Runs under the task's lock and reads the task afresh: two deliveries at once (say, two task
/// reports for an idle brain) must start one run, not two, and the second message must go to
/// the run the first one started. The lock also keeps that run's spawn ahead of the message on
/// the link, so the node never sees a message for a run it does not have yet.
pub async fn deliver(o: &Arc<Orchestrator>, task: &TaskRow, text: String) -> Result<()> {
    let lock = o.task_lock(&task.id);
    let _held = lock.lock().await;
    let task = o.task(&task.id).await?.context("task vanished")?;
    let agent_id = task.agent_id.clone().context("task has no agent")?;
    let send = |task: &TaskRow, text: String| -> Result<bool> {
        let (true, Some(node), Some(run_id)) = (task.running, &task.node_id, &task.run_id) else {
            return Ok(false);
        };
        // Addressed to this run: if it is gone by the time this arrives, the node hands the
        // text back (`Undelivered` with this run id) and it is redelivered by resuming.
        o.server.send(
            node.parse()?,
            Body::SendToRun {
                agent_id: agent_id.clone(),
                task_id: task.id.clone(),
                run_id: run_id.clone(),
                text,
            },
        );
        Ok(true)
    };
    if send(&task, text.clone())? {
        return Ok(());
    }
    if matches!(
        task.status.as_str(),
        status::DONE | status::STOPPED | status::FAILED
    ) && let Some(topic) = task.topic_id
    {
        let _ = o.chat.reopen_topic(topic).await;
    }
    match dispatch_locked(o, &task, Some(text.clone()), task.session_id.clone()).await? {
        Dispatched::Started | Dispatched::Waiting => Ok(()),
        // Claimed by something that did not take the lock: hand the text to that run.
        Dispatched::Lost => {
            let task = o.task(&task.id).await?.context("task vanished")?;
            if send(&task, text)? {
                return Ok(());
            }
            // No run took it: the claim was refused because the agent is gone.
            let agent_exists: bool =
                sqlx::query_scalar("select exists(select 1 from agents where id = $1)")
                    .bind(&agent_id)
                    .fetch_one(&o.db)
                    .await?;
            ensure!(
                agent_exists,
                "the agent of task {} was removed; the message was not delivered",
                task.id
            );
            bail!("task {} changed while delivering; try again", task.id)
        }
    }
}

/// Stops a task's agent. Graceful lets it finish its current turn; the session stays resumable.
pub async fn stop(o: &Orchestrator, task: &TaskRow, graceful: bool) -> Result<()> {
    // Under the task's lock and on a fresh copy: a run started since the caller read the task
    // is the one to stop, on the node it actually runs on.
    let lock = o.task_lock(&task.id);
    let _held = lock.lock().await;
    let task = o.task(&task.id).await?.context("task vanished")?;
    // Recorded before the node is told: the kill's exit event can arrive at once, and it must
    // find the task stopped, not running (which would read as a failure).
    if !graceful {
        sqlx::query("update tasks set status = 'stopped', updated_at = now() where id = $1 and status <> 'done'").bind(&task.id).execute(&o.db).await?;
    }
    // Only a running task has a process to stop; otherwise the node would stop whatever its
    // agent is running now, possibly another task.
    if let (true, Some(node), Some(agent)) = (task.running, &task.node_id, &task.agent_id) {
        o.server.send(
            node.parse()?,
            Body::StopAgent(StopAgent {
                agent_id: agent.clone(),
                task_id: task.id.clone(),
                graceful,
                run_id: task.run_id.clone().unwrap_or_default(),
            }),
        );
    }
    Ok(())
}

/// Tries every queued task (after a node connects or an agent frees up).
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
        // Only retry when it can start now, so the topic is not spammed with queue notes.
        let Ok(nodes) = node_load(o).await else {
            return;
        };
        let agent_busy: bool = sqlx::query_scalar(
            "select exists(select 1 from tasks where agent_id = $1 and running and id <> $2)",
        )
        .bind(&task.agent_id)
        .bind(&task.id)
        .fetch_one(&o.db)
        .await
        .unwrap_or(true);
        // A session can only resume on its node, unless that node is gone (see `dispatch`).
        let session_node = task
            .session_id
            .as_ref()
            .and(task.node_id.as_deref())
            .filter(|id| nodes.iter().any(|(n, _)| n.id == *id));
        let can_run = !agent_busy
            && nodes.iter().any(|(n, load)| {
                n.online
                    && n.has_project(&task.project)
                    && *load < i64::from(n.max_agents.max(1))
                    && session_node.is_none_or(|id| id == n.id)
            });
        if !can_run {
            continue;
        }
        if let Err(e) = dispatch_if_queued(o, &task).await {
            tracing::warn!("dispatching {} failed: {e:#}", task.id);
        }
    }
}

pub async fn to_brain(o: &Arc<Orchestrator>, text: String) -> Result<()> {
    ensure_brain(o).await?;
    let task = o.task(BRAIN_TASK).await?.context("brain task missing")?;
    deliver(o, &task, text).await
}

/// Text written by an agent (or reported by a node), framed for the brain: JSON-quoted, so it
/// cannot pass itself off as one of our own `[...]` lines, and clipped.
pub fn untrusted(text: &str, max: usize) -> String {
    serde_json::to_string(&clip(text, max)).unwrap_or_default()
}

/// Tells the brain that something happened to `task` (it ended, could not start). Queued and
/// delivered in the background, in order (see [`Orchestrator::tell_brain`]): the caller's own
/// work is done and must not wait for the brain. Never for the brain's own task, which would
/// make it talk to itself.
pub fn notify_brain(o: &Orchestrator, task: &TaskRow, text: String) {
    if task.id != BRAIN_TASK {
        o.tell_brain(text);
    }
}

async fn ensure_brain(o: &Orchestrator) -> Result<()> {
    let slug = &o.cfg.brain_project;
    let created = sqlx::query("insert into projects (slug, name, description) values ($1, 'Fleet', 'The orchestrator brain''s workspace') on conflict do nothing")
        .bind(slug)
        .execute(&o.db)
        .await?
        .rows_affected()
        == 1;
    if created {
        // The only project not created through `admin`; nodes list it for mapping too. The
        // brain does not depend on it, so a failure is only logged.
        if let Err(e) = o.push_project_list(None).await {
            tracing::warn!("sending the project list to nodes failed: {e:#}");
        }
    }
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
    fn only_free_text_is_redacted() {
        // A secret that equals a project slug (and a bank, a channel...) leaves those alone.
        let values = vec!["bikeshed-prod".to_owned()];
        let mut args = serde_json::json!({
            "project": "bikeshed-prod",
            "bank": "bikeshed-prod",
            "task_id": "bikeshed-prod-1",
            "text": "deployed bikeshed-prod",
            "options": ["use bikeshed-prod", "no"],
            "params": {"slug": "bikeshed-prod", "description": "the bikeshed-prod site"},
        });
        redact_free_text(&mut args, &values);
        assert_eq!(
            args,
            serde_json::json!({
                "project": "bikeshed-prod",
                "bank": "bikeshed-prod",
                "task_id": "bikeshed-prod-1",
                "text": "deployed [secret]",
                "options": ["use [secret]", "no"],
                "params": {"slug": "bikeshed-prod", "description": "the [secret] site"},
            })
        );
    }

    #[test]
    fn agent_names_stay_plain() {
        for ok in ["Alice", "web-dev 2", "a.b_c", &"x".repeat(40)] {
            assert!(check_agent_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            &"x".repeat(41),
            "Al\nice",
            "<b>x</b>",
            "[Admin change Q-1]",
            "é",
        ] {
            assert!(check_agent_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Alice the Builder!"), "alice-the-builder");
        assert_eq!(slugify_ticket(" ABC-123 "), "ABC-123");
        assert_eq!(env_quote("abc"), "abc");
        assert_eq!(env_quote("a b\"c"), "\"a b\\\"c\"");
    }

    #[test]
    fn secret_values_survive_the_trip_to_the_node() {
        for v in [
            "plain",
            "p@ss w\\rd",
            "say \"hi\"",
            "-----BEGIN KEY-----\nabc\n-----END KEY-----",
            "x=y#z 'q'",
            "$HOME `cmd`",
        ] {
            let line = format!("K={}\n", env_quote(v));
            assert_eq!(
                fleet_node::secrets::parse_env(&line)["K"],
                v,
                "value {v:?} via {line:?}"
            );
        }
    }
}
