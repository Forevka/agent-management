//! Tasks and agents: creation, placement on nodes, and delivering messages to sessions.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use fleet_proto::{Body, SecretFile, SpawnAgent, StopAgent};
use serde_json::{Value, json};

use crate::{
    chat::{From, Notify, clip, esc},
    core::Orchestrator,
    model::{
        AgentRow, BRAIN_AGENT, BRAIN_TASK, NewAgent, NodeRow, ProjectRow, RETRO_AGENT,
        SECRETARY_AGENT, TaskRow, status,
    },
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
    /// The task's own Claude model and effort (None: the agent's). Checked to be valid here;
    /// whether the brain may choose them is checked where it asks (`tools`).
    pub model: Option<String>,
    pub effort: Option<String>,
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
    // An agent with no bot still works: it speaks as the orchestrator (`task_sender`), and its
    // buttons are the orchestrator's, so they are even answered more reliably. A missing bot is
    // therefore a cosmetic shortfall, not a reason to refuse the agent the fleet needs — so the
    // agent is created either way, and the owner is asked for one more bot (at most daily, so
    // the one send here costs a growing project a moment at most, and only once a day).
    let Some((bot_id, username)) = bot else {
        tx.rollback().await?;
        let agent = create_agent_without_bot(o, project, name, &new.role).await?;
        ask_for_bot(o, &agent.id, name).await;
        return Ok(agent);
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

/// The agent that should run `task` now: the one it is assigned to, a free sibling, or a new
/// one, growing the project up to `FLEET_MAX_AGENTS_PER_PROJECT`.
///
/// `None` means every agent of the project is busy and it may not grow, so the task waits.
///
/// Two rules shape this:
/// - **A session belongs to the agent that opened it.** Resuming someone else's session would
///   hand an agent a transcript addressed to another name, so a task that has already run stays
///   with its agent however long that takes.
/// - **An agent is only worth adding if something can run it.** Growing the pool while no node
///   has a free slot for the project would leave a permanent agent behind for nothing.
async fn agent_for(
    o: &Orchestrator,
    task: &TaskRow,
    resume: Option<&str>,
    grow: bool,
) -> Result<Option<AgentRow>> {
    let assigned: AgentRow = match &task.agent_id {
        Some(a) => {
            sqlx::query_as("select * from agents where id = $1")
                .bind(a)
                .fetch_one(&o.db)
                .await?
        }
        None => bail!("task {} has no agent", task.id),
    };
    if is_free(o, &assigned.id, &task.id).await? {
        return Ok(Some(assigned));
    }
    // The fleet's own standing sessions are one of a kind, and a task that has already run is
    // bound to the agent that opened its session: in both cases the only answer is to wait.
    if resume.is_some() || crate::model::is_builtin_task(&task.id) {
        return Ok(None);
    }
    let siblings: Vec<AgentRow> = sqlx::query_as(
        "select * from agents where project = $1 and role = $2 and id <> $3 order by created_at",
    )
    .bind(&task.project)
    .bind(&assigned.role)
    .bind(&assigned.id)
    .fetch_all(&o.db)
    .await?;
    for a in &siblings {
        if is_free(o, &a.id, &task.id).await? {
            return Ok(Some(a.clone()));
        }
    }
    if !grow {
        return Ok(None);
    }
    // One grower per project at a time, or two tasks dispatched together both read "one below
    // the cap" and the project ends up one agent over it. Counted again inside the lock, since
    // the count above was taken outside it. (Lock order: task, project, agent.)
    let project_lock = o.project_lock(&task.project);
    let _project_held = project_lock.lock().await;
    let have: i64 =
        sqlx::query_scalar("select count(*) from agents where project = $1 and role = $2")
            .bind(&task.project)
            .bind(&assigned.role)
            .fetch_one(&o.db)
            .await?;
    if have >= o.cfg.max_agents_per_project {
        return Ok(None);
    }
    if !project_has_room(o, &task.project).await? {
        return Ok(None);
    }
    // Named after the project's first agent, so a reader can see they are the same role on the
    // same code: `bioins-dev`, then `bioins-dev 2`. The number is the first one free rather than
    // the count, because an agent removed earlier would otherwise collide on the unique name.
    let Some(name) = free_agent_name(o, &assigned.name).await? else {
        return Ok(None);
    };
    let new = crate::model::NewAgent {
        name,
        role: assigned.role.clone(),
    };
    match create_agent(o, &task.project, &new).await {
        Ok(a) => {
            // A grown agent is a second copy of the one it was cloned from, so it must run the
            // same way. Without this it would have no model or effort of its own and fall back
            // to the fleet default — a task would quietly change model (and cost) just because
            // its own specialist happened to be busy. The node preference comes along too: the
            // project is checked out there, and it stays a preference, not a pin.
            let a: AgentRow = sqlx::query_as(
                "update agents set model = $2, effort = $3, preferred_node = $4
                 where id = $1 returning *",
            )
            .bind(&a.id)
            .bind(&assigned.model)
            .bind(&assigned.effort)
            .bind(&assigned.preferred_node)
            .fetch_one(&o.db)
            .await?;
            tracing::info!(
                "project {} grew to {} agents so {} need not wait",
                task.project,
                have + 1,
                task.id
            );
            Ok(Some(a))
        }
        // Growing is an optimisation; if it fails the task simply waits for its own agent.
        Err(e) => {
            tracing::warn!("growing project {} failed: {e:#}", task.project);
            Ok(None)
        }
    }
}

/// The name a project's agents are numbered from: `bioins-dev 2` grows into `bioins-dev 3`,
/// not `bioins-dev 2 2`. A name that merely ends in a word is left alone.
fn agent_name_base(like: &str) -> &str {
    like.rsplit_once(' ')
        .filter(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map_or(like, |(base, _)| base)
        .trim()
}

/// `"<base> 2"`, or the next number up if that name is taken. `None` when even a generous range
/// is used up, which means something is wrong rather than that the fleet should keep counting.
async fn free_agent_name(o: &Orchestrator, like: &str) -> Result<Option<String>> {
    let base = agent_name_base(like);
    for n in 2..=20 {
        let name = format!("{base} {n}");
        if check_agent_name(&name).is_err() {
            return Ok(None);
        }
        if find_agent(o, &name).await?.is_none() {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Whether `agent` has no live run other than `except_task`'s.
async fn is_free(o: &Orchestrator, agent: &str, except_task: &str) -> Result<bool> {
    Ok(!sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from tasks where agent_id = $1 and running and id <> $2)",
    )
    .bind(agent)
    .bind(except_task)
    .fetch_one(&o.db)
    .await?)
}

/// Whether this task's project may still be given another agent, so that a dispatch attempt is
/// worth making even though every agent it has is busy.
async fn can_grow(o: &Orchestrator, task: &TaskRow) -> bool {
    if task.session_id.is_some() || crate::model::is_builtin_task(&task.id) {
        return false;
    }
    // Counted per role, exactly as `agent_for` does it: the cap is on how many of a kind work
    // side by side, so a project may still have an implementor, a reviewer and a tester.
    let have: i64 = sqlx::query_scalar(
        "select count(*) from agents
         where project = $1 and role = (select role from agents where id = $2)",
    )
    .bind(&task.project)
    .bind(task.agent_id.as_deref().unwrap_or_default())
    .fetch_one(&o.db)
    .await
    .unwrap_or(i64::MAX);
    have < o.cfg.max_agents_per_project && project_has_room(o, &task.project).await.unwrap_or(false)
}

/// Whether some online node can run this project (mapped, or assigned to clone), has a free
/// agent slot, and disk space for the run. Only asked for the owner's tasks (the fleet's own
/// sessions never grow), so a node held for its disk counts as full.
async fn project_has_room(o: &Orchestrator, project: &str) -> Result<bool> {
    Ok(node_load(o).await?.iter().any(|(n, load)| {
        n.online
            && n.can_run(project)
            && *load < i64::from(n.max_agents.max(1))
            && !crate::disk::held(n, project)
    }))
}

/// Creates an agent with no Telegram identity of its own; it speaks as the orchestrator.
async fn create_agent_without_bot(
    o: &Orchestrator,
    project: &str,
    name: &str,
    role: &str,
) -> Result<AgentRow> {
    let id = format!(
        "{}-{}",
        slugify(name),
        crate::web::random_token()[..6].to_lowercase()
    );
    sqlx::query("insert into agents (id, name, role, project) values ($1, $2, $3, $4)")
        .bind(&id)
        .bind(name)
        .bind(role)
        .bind(project)
        .execute(&o.db)
        .await?;
    o.audit(
        "orchestrator",
        "agent.create",
        json!({"agent": id, "bot": Value::Null, "project": project}),
    )
    .await;
    Ok(sqlx::query_as("select * from agents where id = $1")
        .bind(&id)
        .fetch_one(&o.db)
        .await?)
}

/// Asks the owner for one more agent bot, at most once a day.
///
/// The ask is a Telegram button that creates the bot and hands it to the orchestrator to manage
/// (see [`Chat::request_bot`]); the owner never sees a token. It is rate-limited because the
/// fleet may grow several botless agents in a row, and because the owner may simply have hit
/// Telegram's cap on how many bots one account can own.
async fn ask_for_bot(o: &Orchestrator, agent_id: &str, for_agent: &str) {
    const KEY: &str = "bot_pool_asked_on";
    // Claiming the day and reading it back is one statement, so two agents created at once ask
    // once between them. The day is claimed even if the send then fails, because the usual
    // reason it fails (no private chat with the bot, or Telegram's cap on owned bots) will not
    // have changed by the next task.
    let claimed: Option<String> = sqlx::query_scalar(
        "insert into kv (key, value)
         values ($1, to_jsonb(to_char(now() at time zone $2, 'YYYY-MM-DD')))
         on conflict (key) do update set value = excluded.value
           where kv.value <> excluded.value
         returning value->>0",
    )
    .bind(KEY)
    .bind(&o.cfg.secretary.timezone)
    .fetch_optional(&o.db)
    .await
    .unwrap_or(None);
    if claimed.is_none() {
        return;
    }
    let username = suggested_bot_username(for_agent, &crate::web::random_token()[..4]);
    // Remembered so the bot that arrives goes to the agent the owner was told about, rather
    // than to whichever botless agent happens to be oldest (see `admin::adopt_managed_bot`).
    let _ = sqlx::query(
        "insert into kv (key, value) values ($1, $2)
         on conflict (key) do update set value = $2",
    )
    .bind(ASKED_FOR)
    .bind(json!(agent_id))
    .execute(&o.db)
    .await;
    // Not fatal: the agent works without a bot, so a failed ask only costs it its own face.
    if let Err(e) = o.chat.request_bot(for_agent, &username).await {
        tracing::info!("asking the owner for an agent bot failed: {e:#}");
    }
}

/// A username Telegram will accept for a new bot: lowercase, ending in `bot`, and unlikely to be
/// taken. `salt` keeps a second attempt for the same agent from colliding with the first.
///
/// **At most 32 characters**, which Telegram enforces. An agent name may be 40 on its own
/// (`check_agent_name`), so the name is the part that gets truncated — the `fleet_` prefix, the
/// salt and the `bot` suffix all have to survive or the name is unusable.
fn suggested_bot_username(for_agent: &str, salt: &str) -> String {
    const MAX: usize = 32;
    let salt = salt.to_lowercase();
    let fixed = "fleet_".len() + 1 + salt.len() + "bot".len();
    let mut name = slugify(for_agent).replace('-', "_");
    name.truncate(MAX.saturating_sub(fixed));
    // A trailing `_` would leave `fleet_alice__ab12bot`; harmless, but ugly in a message the
    // owner reads.
    let name = name.trim_end_matches('_');
    format!("fleet_{name}_{salt}bot")
}

/// `kv` key holding the id of the agent the owner's outstanding bot ask was made for.
pub const ASKED_FOR: &str = "bot_pool_asked_for";

/// Gives back today's bot ask, so the fleet may offer again before tomorrow.
///
/// Used when the owner did tap the button and a bot really was created, but the fleet could not
/// take it in: holding the day then would leave them with a bot nothing uses and no second
/// chance until the next one.
pub async fn release_bot_ask(o: &Orchestrator) {
    let _ = sqlx::query("delete from kv where key = 'bot_pool_asked_on'")
        .execute(&o.db)
        .await;
}

/// The Telegram name and description of an agent's bot.
pub fn bot_profile(name: &str, role: &str, project: &str) -> (String, String) {
    (
        format!("{name} · {role}"),
        format!("Fleet agent {name}: {role} for project {project}."),
    )
}

pub async fn create_task(o: &Arc<Orchestrator>, req: CreateTask) -> Result<TaskRow> {
    if let Some(m) = &req.model {
        crate::model::check_model_value("model", m)?;
    }
    if let Some(e) = &req.effort {
        crate::model::check_effort(e)?;
    }
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
    let topic = o.chat.create_topic(&topic_name(&id, &req.title)).await?;
    // `key_no` is not named here on purpose: the column defaults to the fleet-wide sequence, so
    // every task gets the handle the owner names it by (`F-41`) whichever path creates it.
    sqlx::query("insert into tasks (id, project, title, brief, agent_id, status, topic_id, model, effort) values ($1, $2, $3, $4, $5, 'queued', $6, $7, $8)")
        .bind(&id)
        .bind(&proj.slug)
        .bind(&req.title)
        .bind(&req.brief)
        .bind(&agent.id)
        .bind(topic)
        .bind(&req.model)
        .bind(&req.effort)
        .execute(&o.db)
        .await?;
    let task = o.task(&id).await?.context("task vanished")?;
    let intro = format!(
        // The key leads, because it is what the owner will say back to name this task.
        "🆕 <b>{}</b> · {}\n<code>{}</code>\nAgent: <b>{}</b> ({}) · {}\n<blockquote expandable>{}</blockquote>",
        esc(&task.key()),
        esc(&task.title),
        esc(&task.id),
        esc(&agent.name),
        esc(&agent.role),
        esc(&model_effort_label(&task, &agent, &o.cfg.model, None)),
        esc(&clip(&task.brief, 3000))
    );
    let _ = o
        .chat
        // The brief is for reading later; the owner just asked for the work.
        .send(
            From::Orchestrator,
            Some(topic),
            &intro,
            vec![],
            None,
            Notify::Silent,
        )
        .await;
    o.audit(
        "orchestrator",
        "task.create",
        json!({"task": id, "agent": agent.id}),
    )
    .await;
    // A task made from a ticket is about that ticket: record it as a link so the board can say
    // so, rather than leaving it buried in the id. A ticket the fleet cannot classify (a bare
    // word someone typed) is simply not linked; it is still the task's id either way.
    if let Some(t) = req.ticket.as_deref().filter(|t| !t.trim().is_empty())
        && let Err(e) = crate::links::add(o, &task.id, t, "orchestrator").await
    {
        tracing::debug!("not linking {t:?} to {}: {e:#}", task.id);
    }
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
    dispatch_locked(o, &task, prompt, resume, &Origin::Other).await
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
    dispatch_locked(o, &task, None, resume, &Origin::Other).await
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
    origin: &Origin,
) -> Result<Dispatched> {
    // One agent runs one session at a time (the node would otherwise kill the running session),
    // so a task whose agent is busy is given a free sibling, or a new one if the project may
    // still grow. Only when none of that is possible does it wait.
    let Some(agent) = agent_for(o, task, resume.as_deref(), true).await? else {
        let assigned = task.agent_id.clone().unwrap_or_default();
        let name: String = sqlx::query_scalar("select name from agents where id = $1")
            .bind(&assigned)
            .fetch_optional(&o.db)
            .await?
            .unwrap_or_else(|| assigned.clone());
        let busy: Option<String> =
            sqlx::query_scalar("select id from tasks where agent_id = $1 and running limit 1")
                .bind(&assigned)
                .fetch_optional(&o.db)
                .await?;
        let note = match busy {
            Some(other) => format!(
                "⏳ Queued: {} is busy with <b>{}</b>.",
                esc(&name),
                esc(&other)
            ),
            None => format!("⏳ Queued: {} cannot start it yet.", esc(&name)),
        };
        return queue(o, task, &assigned, prompt.as_deref(), origin, Some(note)).await;
    };
    // `agent` is only a candidate until the claim below rewrites `tasks.agent_id`, so every
    // bail-out from here on must queue the owner's follow-up against the agent the task
    // actually has. Addressing it to a candidate the task never gets would leave the message in
    // the queue for an agent that is not on it: `release_run_messages` only clears `run_id`, and
    // the claim below takes messages by `to_agent`, so nothing would ever deliver it.
    let holder = task.agent_id.clone().unwrap_or_else(|| agent.id.clone());
    // Held from here to the claim: two tasks given this agent at once must not both find it
    // free. (Lock order: task, then agent.) `agent_for` read the same flag a moment ago, so a
    // race only costs this task a turn in the queue, never a killed session.
    let agent_lock = o.agent_lock(&agent.id);
    let _agent_held = agent_lock.lock().await;
    if !is_free(o, &agent.id, &task.id).await? {
        return queue(
            o,
            task,
            &holder,
            prompt.as_deref(),
            origin,
            Some(format!(
                "⏳ Queued: {} just took other work.",
                esc(&agent.name)
            )),
        )
        .await;
    }
    let nodes = node_load(o).await?;
    // Nodes with the project mapped, and nodes that would clone it first.
    let candidates: Vec<&(NodeRow, i64)> = nodes
        .iter()
        .filter(|(n, _)| n.can_run(&task.project))
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
        // For the fleet's own sessions the owner's own message is always told why it waits
        // (they are retried by the fleet's texts; the owner should not wonder).
        let tell_owner =
            crate::model::is_builtin_task(&task.id) && matches!(origin, Origin::Owner(_));
        let note = if told && !tell_owner {
            None
        } else {
            if !told {
                sqlx::query(
                    "insert into task_events (task_id, kind, data) values ($1, 'waiting', $2)",
                )
                .bind(&task.id)
                .bind(format!("no node has project {}", task.project))
                .execute(&o.db)
                .await?;
            }
            // A clone that failed holds the project off that node until the owner retries: say
            // so, rather than send them to assign what is assigned.
            let failed = crate::node_projects::failed_on(o, &task.project).await?;
            Some(if failed.is_empty() {
                format!(
                    "⏳ Waiting for a node with project <b>{}</b>: assign it to a node on the node's page \
                     in the web UI (with the project's repository URL set, the node clones it), or run \
                     <code>fleet project map {} &lt;dir&gt;</code> on a host.",
                    esc(&task.project),
                    esc(&task.project)
                )
            } else {
                let names = failed
                    .iter()
                    .map(|(n, _)| esc(n))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "⏳ Waiting for a node with project <b>{}</b>: cloning it failed on <b>{names}</b> \
                     (the reason is on the node's page in the web UI). Fix that and press Retry there, \
                     or run <code>fleet project map {} &lt;dir&gt;</code> on a host.",
                    esc(&task.project),
                    esc(&task.project)
                )
            })
        };
        return queue(o, task, &holder, prompt.as_deref(), origin, note).await;
    }
    // A resumed session only exists on the node that ran it. Once that node is gone (revoked),
    // the session is gone with it: the task starts afresh elsewhere.
    let resume = resume.filter(|_| {
        task.node_id
            .as_deref()
            .is_none_or(|id| nodes.iter().any(|(n, _)| n.id == id))
    });
    let pinned = resume.as_ref().and(task.node_id.as_deref());
    let has_slot = |(n, load): &&(NodeRow, i64)| n.online && *load < i64::from(n.max_agents.max(1));
    // A node about to run out of disk starts no new task run: it would fail there and cut its
    // session's transcript short. The fleet's own sessions are exempt, so the owner can still
    // reach the fleet while they free space.
    let disk_held = |n: &NodeRow| {
        !crate::model::is_builtin_task(&task.id) && crate::disk::held(n, &task.project)
    };
    let free = |c: &&(NodeRow, i64)| has_slot(c) && !disk_held(&c.0);
    let chosen = match pinned {
        Some(node_id) => candidates
            .iter()
            .find(|(n, _)| n.id == node_id)
            .copied()
            .filter(free),
        None => {
            // A node that already has the project beats one that would have to clone it (minutes,
            // and a second copy of the code), then the agent's own node, then the least busy.
            let preferred = agent.preferred_node.as_deref();
            let mut ok: Vec<&(NodeRow, i64)> = candidates.iter().copied().filter(free).collect();
            ok.sort_by_key(|(n, load)| {
                (
                    !n.has_project(&task.project),
                    Some(n.id.as_str()) != preferred,
                    *load,
                )
            });
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
        // Say so when it is the disk that holds it: the owner can do something about that, and
        // "at capacity" would send them looking at the wrong thing.
        let held_for_disk: Vec<&NodeRow> = candidates
            .iter()
            .filter(|(n, _)| pinned.is_none_or(|id| n.id == id))
            .map(|(n, _)| n)
            .filter(|n| n.online && disk_held(n))
            .collect();
        let note = if held_for_disk.is_empty() {
            format!(
                "⏳ Queued: waiting for {} (offline or at capacity).",
                esc(&waiting_for)
            )
        } else {
            let names = held_for_disk
                .iter()
                .map(|n| match crate::disk::least_free(n) {
                    Some(free) => format!("{} ({} free)", n.name, crate::disk::bytes(free)),
                    None => n.name.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "⏳ Queued: {} almost out of disk space. It starts once space is freed.",
                esc(&names)
            )
        };
        return queue(o, task, &holder, prompt.as_deref(), origin, Some(note)).await;
    };

    let run_id = crate::web::random_token()[..12].to_owned();
    let planning = plan_first(task, resume.as_deref());
    let new_status = if planning {
        status::PLANNING
    } else {
        status::RUNNING
    };
    // Claim the run before building it (which takes the queued follow-ups) and sending it: from
    // here on the task counts as running (capacity, busy agent, message delivery), and only
    // events of this run may change it. Only one claim can win while the task is not running,
    // and none once the agent or the node is gone: the share locks wait for an agent removal or
    // a node revoke in progress, which end or refuse running tasks under their own locks.
    // A run in plan mode opens a planning round, which only the owner's approval closes.
    //
    // `agent_id` is set here too, not earlier: a task handed a free sibling or a newly grown
    // agent only truly changes hands if this claim wins, and from then on its topic, brief and
    // session all belong to that agent.
    let claimed = sqlx::query(
        "update tasks set node_id = $2, run_id = $3, running = true, dispatched_at = now(), status = $4, agent_id = $5, updated_at = now(),
                          plan_approved = plan_approved and not $6, archived_at = null
         where id = $1 and not running
           and exists (select 1 from agents where id = $5 for share)
           and exists (select 1 from nodes where id = $2 for share)",
    )
    .bind(&task.id)
    .bind(&node.id)
    .bind(&run_id)
    .bind(new_status)
    .bind(&agent.id)
    .bind(planning)
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
            &holder,
            prompt.as_deref(),
            origin,
            Some("⏳ Queued: the chosen node just left the fleet.".into()),
        )
        .await;
    }
    // The run is now this task's; remember it as history. A run row outlives the task's
    // `run_id`, which only ever holds the current one, so the owner can look back at the
    // sessions of a task or a project. This is deliberately after the claim: a lost claim is
    // not a session that happened.
    sqlx::query(
        "insert into runs (run_id, task_id, project, agent_id, node_id) values ($1, $2, $3, $4, $5)
         on conflict (run_id) do nothing",
    )
    .bind(&run_id)
    .bind(&task.id)
    .bind(&task.project)
    .bind(&agent.id)
    .bind(&node.id)
    .execute(&o.db)
    .await?;
    // A task given a free sibling or a newly grown agent has changed hands. Two things have to
    // follow the task, or the handover silently loses work:
    //  - the messages waiting for it, which `queue` addressed to the agent it had then. They are
    //    claimed by `to_agent` just below, so without this the owner's follow-up would sit in
    //    the queue for an agent that is no longer on the task.
    //  - the owner's view of it: the topic's intro named the old agent, and nothing else would
    //    ever say otherwise.
    if task.agent_id.as_deref() != Some(agent.id.as_str()) {
        sqlx::query(
            "update messages set to_agent = $2
             where task_id = $1 and from_agent is null and run_id is null",
        )
        .bind(&task.id)
        .bind(&agent.id)
        .execute(&o.db)
        .await?;
        let was: Option<String> = sqlx::query_scalar("select name from agents where id = $1")
            .bind(task.agent_id.as_deref().unwrap_or_default())
            .fetch_optional(&o.db)
            .await?;
        let note = match was {
            Some(was) => format!(
                "🔀 <b>{}</b> is taking this: {} is busy with other work.",
                esc(&agent.name),
                esc(&was)
            ),
            None => format!("🔀 <b>{}</b> is taking this.", esc(&agent.name)),
        };
        let _ = o
            .chat
            .send(
                From::Orchestrator,
                task.topic_id,
                &note,
                vec![],
                None,
                Notify::Silent,
            )
            .await;
    }
    // The run takes the task's waiting messages and the new one. They stay in the queue, tagged
    // with the run, until the node reports it started (then they go); a run that never starts
    // gives them back (`release_run_messages`), so the next one gets them.
    let waited = sqlx::query(
        "update messages set run_id = $3 where task_id = $1 and to_agent = $2 and from_agent is null and run_id is null",
    )
    .bind(&task.id)
    .bind(&agent.id)
    .bind(&run_id)
    .execute(&o.db)
    .await?
    .rows_affected();
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
    let spawn = match build_spawn(o, task, &agent, node, resume, run_id.clone()).await {
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
            let reason = format!("{e:#}");
            notice(
                o,
                task,
                origin,
                &format!("⚠️ Could not prepare the agent's run: {}", esc(&reason)),
                Notify::Sound,
                &format!("prepare: {}", stable_reason(&reason)),
            )
            .await;
            return Ok(Dispatched::Waiting);
        }
    };
    o.server
        .send(node.id.parse()?, Body::SpawnAgent(Box::new(spawn)));
    // A new run takes its first message as one turn (see `working`): the new text alone, or,
    // with texts that waited before it, several at once.
    o.working.message_sent(&task.id, &run_id);
    let first = if prompt.is_some() && waited == 0 {
        origin.clone()
    } else {
        Origin::Other
    };
    crate::secretary::run_started(o, &task.id, &run_id, first);
    if crate::model::is_builtin_task(&task.id) {
        // Who must hear if this run fails on its node: the owner, when their own text is in it,
        // even with other texts that waited before it (the secretary's turn bookkeeping above
        // treats such a turn as nobody's; telling the owner is a different question).
        let tell = if prompt.is_some() {
            origin.clone()
        } else {
            Origin::Other
        };
        tracing::debug!(task = %task.id, run = %run_id, origin = ?tell, "builtin run dispatched");
        o.builtin_runs.lock().unwrap().insert(run_id.clone(), tell);
        // Any start, by a retry or by a new text, counts for the minimum gap between retries.
        if let Some(r) = o.builtin_retry.lock().unwrap().get_mut(&task.id) {
            r.last = Some(std::time::Instant::now());
        }
    }
    Ok(Dispatched::Started)
}

/// New work starts in plan mode so the human approves the plan; follow-ups to a running or
/// approved task continue in auto mode. A task whose plan was never approved resumes in plan
/// mode too: a run lost to a node restart (or an idle close) while it planned must not come
/// back in auto and skip the approval. The fleet's own sessions never plan.
fn plan_first(task: &TaskRow, resume: Option<&str>) -> bool {
    !crate::model::is_builtin_task(&task.id)
        && (resume.is_none() || task.status == status::DONE || !task.plan_approved)
}

/// Leaves a task queued (keeping a follow-up message for when it starts) and says why.
async fn queue(
    o: &Orchestrator,
    task: &TaskRow,
    agent_id: &str,
    prompt: Option<&str>,
    origin: &Origin,
    note: Option<String>,
) -> Result<Dispatched> {
    // Nobody works on a queued task, whichever path queued it (a new message, a redelivery).
    o.stop_working(&task.id);
    // Waiting is being worked on again, so an archived task leaves the archive here too. A
    // finished task that has to wait keeps what `plan_first` would have decided for it had it
    // started at once: once `queued`, its status no longer says it was done, so the approval of
    // its last plan is taken back now (the right-hand side reads the status before this update).
    sqlx::query(
        "update tasks set status = 'queued', archived_at = null,
                          plan_approved = plan_approved and status <> 'done', updated_at = now()
         where id = $1",
    )
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
    // Queue notes are progress, not a call for the owner.
    if let Some(note) = note {
        notice(o, task, origin, &note, Notify::Silent, &note).await;
    }
    Ok(Dispatched::Waiting)
}

/// Tells the owner why a task cannot run now. For the fleet's own sessions, which are retried by
/// every text they get (batches, reports) and by `retry_stuck_builtins`, each reason (`key`: the
/// reason without what changes from one attempt to the next) is told once per episode, since
/// the session last produced output; the owner's own message is always told the current reason.
/// Best effort, like every post: a failure here is logged and never fails the delivery whose
/// text is already kept.
pub(crate) async fn notice(
    o: &Orchestrator,
    task: &TaskRow,
    origin: &Origin,
    html: &str,
    notify: Notify,
    key: &str,
) {
    if let Err(e) = try_notice(o, task, origin, html, notify, key).await {
        tracing::warn!("telling the owner about {} failed: {e:#}", task.id);
    }
}

async fn try_notice(
    o: &Orchestrator,
    task: &TaskRow,
    origin: &Origin,
    html: &str,
    notify: Notify,
    key: &str,
) -> Result<()> {
    let builtin = crate::model::is_builtin_task(&task.id);
    if builtin && !matches!(origin, Origin::Owner(_)) {
        let told: bool = sqlx::query_scalar(
            "select exists(select 1 from task_events where task_id = $1 and kind = 'notice' and data = $2
               and id > coalesce((select max(id) from task_events where task_id = $1 and kind in ('out', 'settings')), 0))",
        )
        .bind(&task.id)
        .bind(key)
        .fetch_one(&o.db)
        .await?;
        if told {
            tracing::debug!(task = %task.id, key, "notice already told this episode");
            return Ok(());
        }
    }
    // Recorded as told only once it was: a post that failed is tried again next time.
    if o.post_task_ok(task, html, notify).await && builtin {
        sqlx::query("insert into task_events (task_id, kind, data) values ($1, 'notice', $2)")
            .bind(&task.id)
            .bind(key)
            .execute(&o.db)
            .await?;
    }
    Ok(())
}

/// A reason without what changes from one attempt to the next (paths, ids, numbers), so the
/// same trouble is recognised as the same.
pub(crate) fn stable_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .map(|w| {
            if w.contains('/') || w.contains('\\') {
                "{path}"
            } else if w.chars().filter(char::is_ascii_digit).count() >= 3 {
                "{n}"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
/// Every node with the number of agent runs it is assigned (from our own bookkeeping, which is
/// current even before the node reports back), and the projects it would clone for a task
/// (`NodeRow::clonable`), which is what lets placement ask `NodeRow::can_run`.
async fn node_load(o: &Orchestrator) -> Result<Vec<(NodeRow, i64)>> {
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes")
        .fetch_all(&o.db)
        .await?;
    let mut clonable = crate::node_projects::clonable(o).await?;
    let mut out = Vec::with_capacity(nodes.len());
    for mut n in nodes {
        n.clonable = clonable.remove(&n.id).unwrap_or_default();
        let load: i64 =
            // The fleet's own sessions (brain, secretary, retrospective) mostly wait; they take
            // no slot, or a node that hosts them would have fewer for real work.
            sqlx::query_scalar("select count(*) from tasks where node_id = $1 and running and id <> all($2)")
                .bind(&n.id)
                .bind(crate::model::BUILTIN_TASKS)
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
    node: &NodeRow,
    resume: Option<String>,
    run_id: String,
) -> Result<SpawnAgent> {
    let is_brain = task.id == BRAIN_TASK;
    // By the agent, not the task: whatever task the secretary's agent were ever given, its
    // restrictions come with it. The retrospective is held the same way, for the same reason —
    // it reads a whole day of other people's and other agents' words.
    let is_secretary = agent.id == SECRETARY_AGENT;
    let is_retro = agent.id == RETRO_AGENT;
    let is_reader = is_secretary || is_retro;
    // They read third parties' messages all day: no secrets at all, so a message that talks one
    // of them into something has nothing to leak.
    let secret_files = if is_reader {
        vec![]
    } else {
        secret_files(o, &task.project).await?
    };
    let names: Vec<String> = secret_files
        .iter()
        .map(|f| format!("{}.env", f.name))
        .collect();
    let system = if is_brain {
        prompts::brain_system_prompt(o.memory.is_some())
    } else if is_secretary {
        prompts::secretary_system_prompt(o.memory.is_some())
    } else if is_retro {
        prompts::retro_system_prompt(o.memory.is_some())
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
    // there is nothing else to say (a queued task starting). The secretary has no brief at all.
    let fresh = resume.is_none() && !is_brain;
    let mut parts = Vec::new();
    if (fresh || texts.is_empty()) && !is_reader {
        parts.push(prompts::task_prompt(task));
    }
    // The secretary's queued texts (batches, notes) could pile up while it cannot run: its first
    // turn keeps the newest that fit and says what was left out.
    let texts = if is_secretary {
        newest_within(texts, SECRETARY_FIRST_TURN_BYTES)
    } else {
        texts
    };
    parts.extend(texts);
    let first = parts.join("\n\n");
    // The owner's kit: its enabled MCP servers, and with the node's `--strict-mcp-config` nothing
    // else, are what a worker's session gets. Not the brain: its tools are the fleet's own, and
    // an MCP server that can write (Jira, Slack) would go around the approvals its changes wait
    // for. Not the secretary or the retrospective: they read other people's words all day and
    // stay as narrow as they are. And only to a node level with this build, which writes the
    // config as a per-run secret and removes it when the run ends; an older node would keep it.
    let extra_mcp_servers =
        if is_reader || is_brain || !fleet_proto::node_is_current(&node.app_version) {
            None
        } else {
            crate::kit::agent_servers(o).await
        };
    // The secretary runs on its own cheap model, whatever the node's command sets (the last
    // `--model` wins), with only Bash (and Skill, to load its skill) among Claude's tools, no
    // settings files and no MCP servers but the fleet's, in `dontAsk` mode with only
    // `fleet agent` pre-approved: anything else is refused without asking anyone, so
    // third-party text can never get it to run other commands (checked against the real CLI;
    // see `docs/plan.md`). It runs in an empty directory of its own (the node sees its role).
    let (permission_mode, model, extra_args) = if is_reader {
        refuse_widening(&node.claude_command.0)?;
        let mut args = [
            "--model",
            if is_retro {
                &o.cfg.retro.model
            } else {
                &o.cfg.secretary.model
            },
            "--tools",
            "Bash,Skill",
            "--allowedTools",
            "Bash(fleet agent:*)",
            "--strict-mcp-config",
            "--setting-sources",
            "",
        ]
        .map(str::to_owned)
        .to_vec();
        // The retrospective is one careful run a day, so it is worth the effort the owner set.
        if is_retro {
            args.extend(["--effort".to_owned(), o.cfg.retro.effort.clone()]);
        }
        ("dontAsk".to_owned(), None, args)
    } else {
        // The task's own model and effort, else its agent's, else the fleet's model. The model
        // goes as `model`, which the node leaves out when its own command sets `--model`;
        // effort is left out here when the node's command sets `--effort` (which of two
        // `--effort`s Claude takes cannot be relied on).
        let (model, effort) = crate::model::resolve_model_effort(task, agent, &o.cfg.model);
        let extra = match effort {
            Some(e) if !sets_flag(&node.claude_command.0, "--effort") => {
                vec!["--effort".to_owned(), e]
            }
            _ => vec![],
        };
        let mode = if plan_first { "plan" } else { "auto" };
        (mode.to_owned(), Some(model), extra)
    };
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
        permission_mode,
        after_plan_mode: Some("auto".into()),
        model,
        extra_args,
        extra_mcp_servers,
        secret_files,
        run_id,
    })
}
/// Whether a Claude command (after its program) has `flag`, alone or as `flag=value`.
fn sets_flag(command: &[String], flag: &str) -> bool {
    command
        .iter()
        .skip(1)
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
}

/// "model X, effort Y" as a task runs, for the owner to see. With the node's Claude command
/// (once the task has a node), it says where the node's own `--model`/`--effort` take over.
pub fn model_effort_label(
    task: &TaskRow,
    agent: &AgentRow,
    fleet_model: &str,
    node_command: Option<&[String]>,
) -> String {
    let (model, effort) = crate::model::resolve_model_effort(task, agent, fleet_model);
    let by_node = |flag: &str| node_command.is_some_and(|c| sets_flag(c, flag));
    let model = if by_node("--model") {
        "model set by the node's command".to_owned()
    } else {
        format!("model {model}")
    };
    match (effort, by_node("--effort")) {
        (_, true) => format!("{model}, effort set by the node's command"),
        (Some(e), false) => format!("{model}, effort {e}"),
        (None, false) => model,
    }
}

/// About the most (in bytes) of the secretary's waiting texts its first turn takes.
const SECRETARY_FIRST_TURN_BYTES: usize = 60_000;

/// The secretary's waiting texts, as its first turn takes them: every text from the owner, the
/// brain or the orchestrator, and as many batches, newest first, as fit in `max` bytes with
/// them (the newest batch always, whole: `flush` bounds a batch's size). Older batches that do
/// not fit are left out, with a note saying how many; nothing is cut in the middle.
fn newest_within(texts: Vec<String>, max: usize) -> Vec<String> {
    let batch = |t: &str| t.starts_with(crate::secretary::BATCH_PREFIX);
    let mut used: usize = texts.iter().filter(|t| !batch(t)).map(String::len).sum();
    let mut keep = vec![true; texts.len()];
    let mut newest = true;
    for (i, t) in texts.iter().enumerate().rev().filter(|(_, t)| batch(t)) {
        if newest || used + t.len() <= max {
            used += t.len();
        } else {
            keep[i] = false;
        }
        newest = false;
    }
    let dropped = keep.iter().filter(|k| !**k).count();
    let mut out: Vec<String> = texts
        .into_iter()
        .zip(keep)
        .filter_map(|(t, k)| k.then_some(t))
        .collect();
    if dropped > 0 {
        out.insert(
            0,
            format!(
                "[{dropped} earlier batches were left out (too much at once). Read what you missed with `fleet agent feed read --hours N`.]"
            ),
        );
    }
    out
}
/// Flags in a node's Claude command that would widen what the secretary may do. Its session is
/// refused on such a node rather than run with them (the owner set them for their own agents).
const WIDENING_FLAGS: [&str; 12] = [
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--add-dir",
    "--permission-mode",
    "--allowedTools",
    "--allowed-tools",
    "--settings",
    "--setting-sources",
    "--tools",
    "--mcp-config",
    "--plugin-dir",
    "--agents",
];

fn refuse_widening(command: &[String]) -> Result<()> {
    for arg in command.iter().skip(1) {
        let flag = arg.split('=').next().unwrap_or(arg);
        if WIDENING_FLAGS.contains(&flag) {
            bail!(
                "the node's Claude command has {flag}, which would widen what the secretary may do; run the secretary on a node without it"
            );
        }
    }
    Ok(())
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
pub(crate) async fn project_secret_values(o: &Orchestrator, project: &str) -> Result<Vec<String>> {
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
pub async fn deliver(o: &Arc<Orchestrator>, task: &TaskRow, text: String) -> Result<Delivered> {
    deliver_as(o, task, text, Origin::Other).await
}

/// Where a text handed to a task comes from: the owner's own words are always told why they
/// cannot be delivered now, and the secretary counts the owner's turns (see
/// `secretary::Turns`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The owner wrote it (the text is what they wrote).
    Owner(String),
    /// The brain's note for the secretary.
    Note,
    /// A batch of third parties' messages.
    Batch,
    /// The daily digest request.
    Digest,
    /// Anything else, or several texts at once.
    Other,
}

/// A task topic's name, so a topic made again later looks like the one before it.
fn topic_name(id: &str, title: &str) -> String {
    clip(&format!("{id} · {title}"), 120)
}

/// Gives a task a topic it no longer has, and remembers it. Only for a task whose topic was
/// deleted after it went quiet: a task is created with one.
async fn new_topic(o: &Orchestrator, task: &TaskRow) -> Result<i64> {
    let topic = o
        .chat
        .create_topic(&topic_name(&task.id, &task.title))
        .await?;
    sqlx::query("update tasks set topic_id = $2 where id = $1")
        .bind(&task.id)
        .bind(topic)
        .execute(&o.db)
        .await?;
    Ok(topic)
}

/// Deletes the topic of a task that is over and has been quiet for a while, so the forum does
/// not fill up with finished work. Run from housekeeping.
///
/// Only a task's own topic, ever: the standing topics (Inbox, Rules, agent-communication), a
/// project's "needs you" topic and the feed channels' topics belong to no task, so this query
/// cannot reach them, and the fleet's own sessions are skipped explicitly. A task still waiting
/// on the owner keeps its topic too, and `topic_id` is cleared only when Telegram confirms, so
/// nothing is forgotten that still exists.
pub async fn prune_topics(o: &Orchestrator) -> Result<()> {
    let after = o.cfg.delete_topics_after_minutes;
    if after <= 0 {
        return Ok(());
    }
    if deletes_blocked(o).await {
        return Ok(());
    }
    let stale: Vec<(String, i64)> = sqlx::query_as(
        "select id, topic_id from tasks
         where topic_id is not null and not running
           and status in ('done', 'stopped', 'failed')
           and updated_at < now() - make_interval(mins => $1::int)
           and not exists (select 1 from human_items
                           where task_id = tasks.id and state = 'open' and blocking)
         order by updated_at
         limit 20",
    )
    .bind(after as i32)
    .fetch_all(&o.db)
    .await?;
    for (id, topic) in stale {
        if crate::model::is_builtin_task(&id) {
            continue;
        }
        match o.chat.delete_topic(topic).await {
            Ok(crate::chat::Deleted::Gone) => {
                sqlx::query("update tasks set topic_id = null where id = $1 and topic_id = $2")
                    .bind(&id)
                    .bind(topic)
                    .execute(&o.db)
                    .await?;
                o.audit(
                    "orchestrator",
                    "topic.delete",
                    serde_json::json!({"task": id, "topic": topic}),
                )
                .await;
            }
            // The owner has to grant the right; asking again every five minutes would only
            // fill the log, so this stands down for a day and says so once.
            Ok(crate::chat::Deleted::NotAllowed) => {
                block_deletes(o).await;
                return Ok(());
            }
            Err(e) => tracing::warn!("deleting {id}'s topic failed: {e:#}"),
        }
    }
    Ok(())
}

/// Key under which "the bot may not delete topics" is remembered, with the time it may be worth
/// trying again.
const DELETE_BLOCKED_KEY: &str = "topic_delete_blocked_until";

async fn deletes_blocked(o: &Orchestrator) -> bool {
    let until = o
        .kv_get(DELETE_BLOCKED_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()));
    until.is_some_and(|u| u > Utc::now())
}

/// Stops trying for a day and tells the owner once what to change. Theirs to fix: a bot cannot
/// give itself an admin right.
async fn block_deletes(o: &Orchestrator) {
    let _ = o
        .kv_set(
            DELETE_BLOCKED_KEY,
            serde_json::json!((Utc::now() + chrono::Duration::hours(24)).to_rfc3339()),
        )
        .await;
    tracing::warn!("cannot delete finished tasks' topics: the bot lacks the right");
    o.chat
        .send(
            From::Orchestrator,
            None,
            "🧹 I cannot tidy up finished tasks' topics: I need the <b>Delete messages</b> admin \
             right in this group (alongside Manage topics). Give it to me and I will clear them \
             as they go quiet, or set <code>FLEET_TOPIC_DELETE_AFTER_MINUTES=0</code> to leave \
             every topic alone. I will not ask again today.",
            vec![],
            None,
            Notify::Silent,
        )
        .await
        .ok();
}

/// [`deliver`], saying where the text comes from.
pub async fn deliver_as(
    o: &Arc<Orchestrator>,
    task: &TaskRow,
    text: String,
    origin: Origin,
) -> Result<Delivered> {
    let lock = o.task_lock(&task.id);
    let _held = lock.lock().await;
    let mut task = o.task(&task.id).await?.context("task vanished")?;
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
        // One more message the run will answer with a turn of its own (see working); recorded
        // under the task's lock, so in the order the run receives them.
        o.working.message_sent(&task.id, run_id);
        crate::secretary::turn_sent(o, &task.id, run_id, origin.clone());
        // The owner's text sent into a run of the fleet's own sessions that has not started yet
        // (a retry in flight): if that run fails on its node, the owner must hear why.
        if crate::model::is_builtin_task(&task.id)
            && matches!(origin, Origin::Owner(_))
            && let Some(tell) = o.builtin_runs.lock().unwrap().get_mut(run_id)
        {
            *tell = origin.clone();
        }
        Ok(true)
    };
    if send(&task, text.clone())? {
        return Ok(Delivered::ToRun);
    }
    if matches!(
        task.status.as_str(),
        status::DONE | status::STOPPED | status::FAILED
    ) {
        match task.topic_id {
            Some(topic) => {
                let _ = o.chat.reopen_topic(topic).await;
            }
            // Its topic was deleted once the task went quiet (`prune_topics`), so the follow-up
            // gets a new one rather than landing in General. Not for the fleet's own sessions:
            // the brain has no topic of its own and talks in General on purpose.
            None if !crate::model::is_builtin_task(&task.id) => match new_topic(o, &task).await {
                Ok(topic) => task.topic_id = Some(topic),
                Err(e) => tracing::warn!("a new topic for {} failed: {e:#}", task.id),
            },
            None => {}
        }
    }
    match dispatch_locked(
        o,
        &task,
        Some(text.clone()),
        task.session_id.clone(),
        &origin,
    )
    .await?
    {
        Dispatched::Started => Ok(Delivered::ToRun),
        Dispatched::Waiting => Ok(Delivered::Queued),
        // Claimed by something that did not take the lock: hand the text to that run.
        Dispatched::Lost => {
            let task = o.task(&task.id).await?.context("task vanished")?;
            if send(&task, text)? {
                return Ok(Delivered::ToRun);
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

/// Where [deliver] put the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivered {
    /// To the task's running run, or to the run it just started.
    ToRun,
    /// Kept for later: no node can take the task now (or its run could not be prepared).
    Queued,
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
    stop_run(o, &task, graceful)
}

/// What the owner changes about a task on the web. `None` leaves a field as it is; an empty model
/// or effort goes back to the agent's default, as it does for an agent's own settings.
#[derive(Debug, Default)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub brief: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What [`owner_edit`] did.
pub struct Edited {
    pub task: TaskRow,
    /// The fields that really changed, by name.
    pub changed: Vec<&'static str>,
    /// Where the new brief went, when the agent had to be told (its session already read the
    /// old one); `None` when the next fresh session reads it anyway.
    pub brief_sent: Option<Delivered>,
}

/// The one task the owner names from the web: any of the fleet's work, never one of its own
/// standing sessions (the brain, the secretary, the retrospective), which have their own resets.
async fn owner_task(o: &Orchestrator, id: &str) -> Result<TaskRow> {
    let task = o.task(id).await?.with_context(|| format!("no task {id}"))?;
    ensure!(
        !crate::model::is_builtin_task(&task.id),
        "{} is one of the fleet's own sessions, not a task to change from here",
        task.id
    );
    Ok(task)
}

/// The owner edits a task's title, brief, model or effort.
///
/// The model and effort are read at every dispatch, so they apply from the task's next run; the
/// run going now keeps what it started with. The brief is read only by a fresh session (see
/// `dispatch_locked`): a task whose session already started is told of the new one as the owner's
/// message, and a finished task with a session is refused a new brief, because telling it would
/// resume work the owner ended — a message does that on purpose. The title is the record's, and
/// the topic is renamed to match.
pub async fn owner_edit(o: &Arc<Orchestrator>, id: &str, edit: TaskEdit) -> Result<Edited> {
    let title = edit.title.as_deref().map(str::trim);
    ensure!(title != Some(""), "a task needs a title");
    let brief = edit.brief.as_deref().map(str::trim);
    ensure!(brief != Some(""), "a task needs a brief");
    // Checked exactly as at creation: any valid value, since the brain's allow-list is the
    // brain's and this is the owner's own hand.
    let (model, effort) =
        crate::admin::agent_settings_values(edit.model.as_deref(), edit.effort.as_deref())?;
    let id = owner_task(o, id).await?.id;
    let (before, task) = {
        // Under the task's lock and on a fresh copy, like every other change to a task: a run
        // being claimed meanwhile must see either the old fields or the new, never half.
        let lock = o.task_lock(&id);
        let _held = lock.lock().await;
        let before = o.task(&id).await?.context("task vanished")?;
        let title = title.filter(|t| *t != before.title);
        let brief = brief.filter(|b| *b != before.brief.trim());
        let model = model.filter(|m| *m != before.model);
        let effort = effort.filter(|e| *e != before.effort);
        ensure!(
            title.is_some() || brief.is_some() || model.is_some() || effort.is_some(),
            "nothing changed"
        );
        let finished = matches!(
            before.status.as_str(),
            status::DONE | status::STOPPED | status::FAILED
        );
        ensure!(
            brief.is_none() || !finished || before.session_id.is_none(),
            "{} is {} and its agent has already read its brief; send it a message instead, \
             which resumes it",
            before.key(),
            before.status
        );
        sqlx::query(
            "update tasks set title = coalesce($2, title), brief = coalesce($3, brief),
                    model = case when $4 then $5 else model end,
                    effort = case when $6 then $7 else effort end,
                    updated_at = now()
             where id = $1",
        )
        .bind(&id)
        .bind(title)
        .bind(brief)
        .bind(model.is_some())
        .bind(model.clone().flatten())
        .bind(effort.is_some())
        .bind(effort.clone().flatten())
        .execute(&o.db)
        .await?;
        (before, o.task(&id).await?.context("task vanished")?)
    };
    let mut changed = Vec::new();
    if task.title != before.title {
        changed.push("title");
    }
    if task.brief != before.brief {
        changed.push("brief");
    }
    if task.model != before.model {
        changed.push("model");
    }
    if task.effort != before.effort {
        changed.push("effort");
    }
    // What changed, not the brief's text: it is the owner's prose and may quote anything.
    o.audit(
        crate::admin::Actor::Owner.audit_name(),
        "task.edit",
        json!({
            "task": task.id,
            "changed": changed,
            "title": changed.contains(&"title").then_some(&task.title),
            "model": changed.contains(&"model").then_some(&task.model),
            "effort": changed.contains(&"effort").then_some(&task.effort),
        }),
    )
    .await;
    if let (true, Some(topic)) = (changed.contains(&"title"), task.topic_id)
        && let Err(e) = o
            .chat
            .rename_topic(topic, &topic_name(&task.id, &task.title))
            .await
    {
        tracing::info!("renaming {}'s topic failed: {e:#}", task.id);
    }
    if task.topic_id.is_some() {
        let note = format!(
            "✏️ You changed this task's {} on the web.{}",
            changed.join(", "),
            if changed.contains(&"model") || changed.contains(&"effort") {
                " The model and effort apply from its next run."
            } else {
                ""
            }
        );
        o.chat
            .send(
                From::Orchestrator,
                task.topic_id,
                &esc(&note),
                vec![],
                None,
                Notify::Silent,
            )
            .await
            .ok();
    }
    // A session that already began (or a first run already given the old brief) has to be told;
    // one that has not starts from the new brief by itself.
    let brief_sent = if changed.contains(&"brief") && (task.session_id.is_some() || task.running) {
        let text = format!(
            "The owner updated this task's brief. Work to the new one from here on:\n\n{}",
            task.brief
        );
        Some(
            deliver_as(
                o,
                &task,
                format!("[Message from the human] {text}"),
                Origin::Owner(text),
            )
            .await?,
        )
    } else {
        None
    };
    Ok(Edited {
        task: o.task(&task.id).await?.unwrap_or(task),
        changed,
        brief_sent,
    })
}

/// The owner stops a task from the web, as `/stop` in its topic does.
pub async fn owner_stop(o: &Orchestrator, id: &str) -> Result<TaskRow> {
    let task = owner_task(o, id).await?;
    stop(o, &task, false).await?;
    o.audit(
        crate::admin::Actor::Owner.audit_name(),
        "task.stop",
        json!({"task": task.id}),
    )
    .await;
    Ok(task)
}

/// The owner writes to a task from the web: delivered exactly as their message in its Telegram
/// topic is (see `tg::owner_message`), and shown in that topic, so the conversation there still
/// reads whole when the agent answers it.
pub async fn owner_send(o: &Arc<Orchestrator>, id: &str, text: &str) -> Result<Delivered> {
    let text = text.trim();
    ensure!(!text.is_empty(), "write something to send");
    let task = owner_task(o, id).await?;
    let delivered = deliver_as(
        o,
        &task,
        format!("[Message from the human] {text}"),
        Origin::Owner(text.to_owned()),
    )
    .await?;
    // Read again: a follow-up to a task whose topic was pruned gave it a new one.
    let topic = o.task(&task.id).await?.and_then(|t| t.topic_id);
    if topic.is_some() {
        o.chat
            .send(
                From::Orchestrator,
                topic,
                &format!("💬 <i>You, on the web:</i>\n{}", esc(&clip(text, 3000))),
                vec![],
                None,
                Notify::Silent,
            )
            .await
            .ok();
    }
    Ok(delivered)
}

/// Starts one of the fleet's own sessions (brain, secretary) afresh: forgets its session and
/// stops its run, in one step under the task's lock, so neither a message in between nor the
/// stopped run reporting its session late brings the old session back. Its next text starts a
/// new one (texts already waiting are picked up by `retry_stuck_builtins`).
pub async fn reset_builtin(o: &Orchestrator, id: &str) -> Result<()> {
    ensure!(
        crate::model::is_builtin_task(id),
        "{id} is not one of the fleet's own sessions"
    );
    let lock = o.task_lock(id);
    let _held = lock.lock().await;
    let Some(task) = o.task(id).await? else {
        return Ok(());
    };
    sqlx::query(
        "update tasks set session_id = null, status = 'stopped', updated_at = now() where id = $1",
    )
    .bind(id)
    .execute(&o.db)
    .await?;
    stop_run(o, &task, false)
}

/// Tells the node to stop the task's run, if it has one.
fn stop_run(o: &Orchestrator, task: &TaskRow, graceful: bool) -> Result<()> {
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
        // Asked without `grow`, so this only reports whether an agent is free right now: the
        // dispatch below does the growing. Retrying a task that cannot start would re-post its
        // queue note every round.
        let has_agent = matches!(
            agent_for(o, &task, task.session_id.as_deref(), false).await,
            Ok(Some(_))
        ) || can_grow(o, &task).await;
        // A session can only resume on its node, unless that node is gone (see `dispatch`).
        let session_node = task
            .session_id
            .as_ref()
            .and(task.node_id.as_deref())
            .filter(|id| nodes.iter().any(|(n, _)| n.id == *id));
        let can_run = has_agent
            && nodes.iter().any(|(n, load)| {
                n.online
                    && n.can_run(&task.project)
                    && *load < i64::from(n.max_agents.max(1))
                    && session_node.is_none_or(|id| id == n.id)
                    && (crate::model::is_builtin_task(&task.id)
                        || !crate::disk::held(n, &task.project))
            });
        if !can_run {
            continue;
        }
        if let Err(e) = dispatch_if_queued(o, &task).await {
            tracing::warn!("dispatching {} failed: {e:#}", task.id);
        }
    }
}

/// How long the fleet's own sessions wait before a stuck one is tried again: at first, and at
/// most (the wait doubles in between).
const BUILTIN_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(300);
const BUILTIN_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(1800);
/// A retry asked for at once (a node connected, settings changed) still waits this long after
/// the last attempt.
const BUILTIN_RETRY_MIN_GAP: std::time::Duration = std::time::Duration::from_secs(60);

/// When a stuck session of the fleet is tried next, the wait after that, and when it was last
/// tried. Cleared only when a run actually starts (see `core`), so a run that fails on its node
/// keeps backing off.
#[derive(Debug, Clone, Copy)]
pub struct RetryState {
    pub at: std::time::Instant,
    pub wait: std::time::Duration,
    pub last: Option<std::time::Instant>,
}

/// Starts the fleet's own sessions (brain, secretary) that are stuck: not running, failed to
/// start, queued for a node or stopped (a reset), with texts waiting for them. Nothing else
/// would: ordinary tasks are started by the owner's messages and by `dispatch_queued`, but these
/// get their texts from the fleet (reports, batches the secretary holds back while one waits).
/// Tried after [`BUILTIN_RETRY_FIRST`], backing off to [`BUILTIN_RETRY_MAX`], and sooner after a
/// node connects or settings change ([`retry_builtins_now`]).
pub async fn retry_stuck_builtins(o: &Arc<Orchestrator>) {
    for id in [BRAIN_TASK, crate::model::SECRETARY_TASK] {
        // One session's trouble must not keep the other from being tried.
        if let Err(e) = retry_stuck(o, id).await {
            tracing::warn!("trying {id} again failed: {e:#}");
        }
    }
}

/// Whether a session of the fleet is stuck with texts waiting.
async fn stuck(o: &Orchestrator, task: &TaskRow) -> Result<bool> {
    if task.running
        || !matches!(
            task.status.as_str(),
            status::FAILED | status::QUEUED | status::STOPPED
        )
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "select exists(select 1 from messages where task_id = $1 and to_agent = $2 and from_agent is null and run_id is null)",
    )
    .bind(&task.id)
    .bind(&task.agent_id)
    .fetch_one(&o.db)
    .await?)
}

async fn retry_stuck(o: &Arc<Orchestrator>, id: &str) -> Result<()> {
    let Some(task) = o.task(id).await? else {
        return Ok(());
    };
    let is_stuck = stuck(o, &task).await?;
    let due = {
        let mut retry = o.builtin_retry.lock().unwrap();
        let now = std::time::Instant::now();
        match retry.get_mut(id) {
            // Not stuck, and not starting either: forget it (a starting run keeps its entry
            // until it reports that it started).
            _ if !is_stuck && !task.running => {
                retry.remove(id);
                false
            }
            _ if !is_stuck => false,
            None => {
                retry.insert(
                    id.to_owned(),
                    RetryState {
                        at: now + BUILTIN_RETRY_FIRST,
                        wait: BUILTIN_RETRY_FIRST,
                        last: None,
                    },
                );
                false
            }
            Some(r) if now >= r.at => {
                r.wait = (r.wait * 2).min(BUILTIN_RETRY_MAX);
                r.at = now + r.wait;
                r.last = Some(now);
                true
            }
            Some(_) => false,
        }
    };
    if !due {
        return Ok(());
    }
    // Under the lock, afresh: a message may have started it, or taken what waited, meanwhile.
    let lock = o.task_lock(id);
    let _held = lock.lock().await;
    let task = o.task(id).await?.context("task vanished")?;
    if !stuck(o, &task).await? {
        return Ok(());
    }
    tracing::info!("trying {id} again: it is stuck with texts waiting");
    let resume = task.session_id.clone();
    dispatch_locked(o, &task, None, resume, &Origin::Other).await?;
    Ok(())
}

/// The fleet's own sessions already found stuck are tried at the next round, but not sooner than
/// a minute after their last attempt: a node connected, or settings or a node's projects
/// changed, so what kept them from starting may be fixed. Only sessions with an entry (seen
/// stuck by `retry_stuck`): an entry made here for a healthy session would stay "due" and make
/// its first retry immediate whenever it later got stuck, skipping the first wait.
pub fn retry_builtins_now(o: &Orchestrator) {
    let mut retry = o.builtin_retry.lock().unwrap();
    let now = std::time::Instant::now();
    for r in retry.values_mut() {
        let earliest = r.last.map_or(now, |l| l + BUILTIN_RETRY_MIN_GAP);
        r.at = r.at.min(earliest.max(now));
    }
}
/// A run of one of the fleet's sessions started: it is no longer stuck.
pub fn builtin_started(o: &Orchestrator, task_id: &str) {
    o.builtin_retry.lock().unwrap().remove(task_id);
}

/// Tests: the next round tries `id` (skipping the waits).
#[cfg(test)]
pub(crate) fn retry_due_now(o: &Orchestrator, id: &str) {
    if let Some(r) = o.builtin_retry.lock().unwrap().get_mut(id) {
        r.at = std::time::Instant::now();
    }
}
pub async fn to_brain(o: &Arc<Orchestrator>, text: String) -> Result<Delivered> {
    to_brain_as(o, text, Origin::Other).await
}

/// [`to_brain`], saying where the text comes from (the owner's own messages are always told
/// why the brain cannot take them now).
pub async fn to_brain_as(o: &Arc<Orchestrator>, text: String, origin: Origin) -> Result<Delivered> {
    ensure_brain(o).await?;
    let task = o.task(BRAIN_TASK).await?.context("brain task missing")?;
    deliver_as(o, &task, text, origin).await
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
    if !crate::model::is_builtin_task(&task.id) {
        o.tell_brain(text);
    }
}

pub(crate) async fn ensure_brain(o: &Orchestrator) -> Result<()> {
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
    fn an_unapproved_plan_is_resumed_in_plan_mode() {
        let task = |id: &str, status: &str, plan_approved: bool| TaskRow {
            id: id.into(),
            project: "p".into(),
            title: "t".into(),
            brief: String::new(),
            agent_id: Some("a".into()),
            node_id: None,
            status: status.into(),
            topic_id: None,
            checklist_msg_id: None,
            session_id: Some("s".into()),
            running: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            run_id: None,
            dispatched_at: None,
            model: None,
            effort: None,
            plan_approved,
            key_no: Some(1),
            archived_at: None,
        };
        // New work plans; so does a follow-up to finished work.
        assert!(plan_first(&task("p-1", "queued", true), None));
        assert!(plan_first(&task("p-1", status::DONE, true), Some("s")));
        // An approved task carries on in auto mode when resumed.
        assert!(!plan_first(&task("p-1", status::RUNNING, true), Some("s")));
        assert!(!plan_first(&task("p-1", status::FAILED, true), Some("s")));
        // Its plan never approved, a resumed run (lost to a restart while it planned, then
        // queued, stopped or failed) plans again rather than skipping the approval.
        for status in [
            status::PLANNING,
            status::QUEUED,
            status::STOPPED,
            status::FAILED,
        ] {
            assert!(
                plan_first(&task("p-1", status, false), Some("s")),
                "{status}"
            );
        }
        // The fleet's own sessions never plan.
        assert!(!plan_first(&task(BRAIN_TASK, "queued", false), None));
    }

    #[test]
    fn the_secretarys_first_turn_keeps_the_newest_texts() {
        let b = |n: usize| format!("[Inbox batch: {}", "x".repeat(n));
        let owner = "[Message from the owner in the Inbox] hi".to_owned();
        // Everything fits: all kept.
        assert_eq!(
            newest_within(vec![b(10), owner.clone(), b(20)], 1000),
            vec![b(10), owner.clone(), b(20)]
        );
        // Too much: older batches go, the owner's text and the newest batch stay, whole.
        let out = newest_within(vec![b(500), b(400), owner.clone(), b(300)], 400);
        assert!(
            out[0].contains("2 earlier batches were left out"),
            "{out:?}"
        );
        assert_eq!(&out[1..], &[owner.clone(), b(300)][..]);
        let out = newest_within(vec![b(500), owner.clone(), b(900)], 100);
        assert_eq!(&out[1..], &[owner, b(900)][..], "never clipped");
    }
    #[test]
    fn reasons_are_compared_without_what_changes() {
        assert_eq!(
            stable_reason("spawning /opt/x/claude failed (os error 2) for run 3f9a12b7c0de"),
            stable_reason("spawning /opt/y/claude failed (os error 2) for run 77aa0011bb22")
        );
        assert_ne!(
            stable_reason("auth failed"),
            stable_reason("no such program")
        );
    }

    #[test]
    fn a_flag_counts_alone_or_with_its_value_after_the_program() {
        let cmd = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(sets_flag(&cmd(&["claude", "--effort", "high"]), "--effort"));
        assert!(sets_flag(&cmd(&["claude", "--effort=max"]), "--effort"));
        assert!(!sets_flag(
            &cmd(&["claude", "--effort-foo", "x"]),
            "--effort"
        ));
        assert!(
            !sets_flag(&cmd(&["--effort"]), "--effort"),
            "the program is not a flag"
        );
        assert!(!sets_flag(&cmd(&["claude", "--model", "opus"]), "--effort"));
    }

    #[test]
    fn widening_flags_keep_the_secretary_off_a_node() {
        let cmd = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(refuse_widening(&cmd(&["claude", "--model", "opus"])).is_ok());
        for bad in [
            "--dangerously-skip-permissions",
            "--add-dir=/",
            "--allowed-tools",
            "--settings",
            "--permission-mode=bypassPermissions",
        ] {
            assert!(refuse_widening(&cmd(&["claude", bad])).is_err(), "{bad}");
        }
        // The program itself is not a flag, whatever its name.
        assert!(refuse_widening(&cmd(&["--add-dir"])).is_ok());
    }

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

#[cfg(test)]
mod growth_tests {
    use super::{agent_name_base, suggested_bot_username};

    #[test]
    fn a_suggested_bot_username_is_one_telegram_accepts() {
        // Telegram's rules: at most 32 characters, and it must end in `bot`.
        let ordinary = suggested_bot_username("Alice 2", "AB12");
        assert_eq!(ordinary, "fleet_alice_2_ab12bot");
        // An agent name may be 40 characters on its own, so the name is what gives way. The
        // prefix, the salt and the suffix must all still be there, or the bot cannot be made.
        let long = suggested_bot_username(&"a".repeat(40), "CDEF");
        assert_eq!(long.len(), 32, "{long} must fit Telegram's limit");
        assert!(
            long.starts_with("fleet_") && long.ends_with("cdefbot"),
            "{long}"
        );
        for name in ["Alice 2", &"a".repeat(40), "x", "Bob-the-Builder", "r2d2"] {
            let u = suggested_bot_username(name, "0f9e");
            assert!(u.len() <= 32, "{u} is {} characters", u.len());
            assert!(u.ends_with("bot"), "{u}");
            assert!(
                u.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{u} has a character Telegram will not take"
            );
        }
    }

    #[test]
    fn a_grown_agent_is_numbered_from_the_first_of_its_kind() {
        // The ordinary case, and growing again from an already numbered agent.
        assert_eq!(agent_name_base("bioins-dev"), "bioins-dev");
        assert_eq!(agent_name_base("bioins-dev 2"), "bioins-dev");
        assert_eq!(agent_name_base("bioins-dev 17"), "bioins-dev");
        // A name that ends in a word, or in a number that is part of it, is left alone: the
        // fleet must not read "Mechanic" out of "Mechanic v2" or a digit out of "r2d2".
        assert_eq!(agent_name_base("Mechanic"), "Mechanic");
        assert_eq!(agent_name_base("Mechanic v2"), "Mechanic v2");
        assert_eq!(agent_name_base("r2d2"), "r2d2");
    }
}
