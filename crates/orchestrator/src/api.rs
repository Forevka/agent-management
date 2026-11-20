//! The JSON API behind the web UI.
//!
//! The UI is a single-page app, so this is the whole of what it can see and do. Two rules hold
//! everywhere in here:
//!
//! - **Explicit shapes.** Nothing serialises a database row straight to the wire. Rows carry
//!   things the browser must never see (`nodes.auth_secret` is the sharp one), so every
//!   response is built field by field. A new column is then invisible until someone adds it
//!   here on purpose.
//! - **No new authority.** Every mutation goes through the same `admin`, `human` and
//!   `secretary` entry points the Telegram side uses, with `Actor::Owner`. This module parses
//!   and validates; it never decides.
//!
//! Authentication is the session cookie [`super::web`] sets from a magic link. It is
//! `SameSite=Strict`, which is also what keeps another site from driving this API in the
//! owner's browser; mutations additionally require a JSON content type, which a cross-site
//! form post cannot send.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    admin::{self, Actor, AdminAction, Ref},
    core::Orchestrator,
    human::{self, Answer},
    model::{AgentRow, HumanItemRow, NodeRow, RuleRow, TaskRow, TodoRow},
    secretary,
};

type O = Arc<Orchestrator>;

pub fn router() -> Router<O> {
    Router::new()
        .route("/api/overview", get(overview))
        .route("/api/board", get(board))
        .route("/api/items/{id}", get(item).post(item_answer))
        .route("/api/tasks", get(tasks))
        .route("/api/tasks/{id}", get(task))
        .route("/api/tasks/{id}/events", get(task_events))
        .route("/api/tasks/{id}/links", post(task_link))
        .route("/api/tasks/{id}/attachments", post(task_attach))
        .route("/api/attachments/{id}/file", get(attachment_file))
        .route("/api/runs", get(runs))
        .route("/api/projects", get(projects).post(projects_act))
        .route("/api/projects/{slug}", get(project).post(project_act))
        .route("/api/secrets", get(secrets).post(secrets_act))
        .route("/api/secrets/{id}", get(secret).post(secret_act))
        .route("/api/nodes", get(nodes).post(nodes_act))
        .route("/api/nodes/{id}", get(node).post(node_act))
        .route("/api/agents", get(agents).post(agents_act))
        .route("/api/inbox", get(inbox).post(inbox_act))
        .route("/api/habits", get(habits).post(habits_act))
        .route("/api/retro", get(retro).post(retro_act))
        .route("/api/people", get(people).post(people_act))
        .route("/api/people/{id}", get(person).post(person_act))
        .route("/api/kit", get(kit).post(kit_act))
        .route("/api/kit/{id}", get(kit_item))
        .route("/api/invite", post(invite))
}

// ---------------------------------------------------------------- plumbing

/// An API error the browser can render: a status and a message, as JSON.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    /// Anything a handler returns with `?`. The owner is the only reader, and the messages are
    /// the same ones Telegram shows them, so they are passed through rather than swallowed.
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

impl From<sqlx::Error> for ApiError {
    /// A query that failed is the server's fault, not the request's, so it reads as a 500.
    fn from(e: sqlx::Error) -> Self {
        tracing::warn!("api query failed: {e:#}");
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the database query failed".into(),
        )
    }
}

type Api = std::result::Result<Json<Value>, ApiError>;

/// Every handler starts here. Unauthenticated reads and writes both get a 401 with a hint the
/// UI turns into its sign-in screen.
async fn auth(o: &Orchestrator, h: &HeaderMap) -> std::result::Result<(), ApiError> {
    if super::web::authed(o, h).await {
        return Ok(());
    }
    Err(ApiError(
        StatusCode::UNAUTHORIZED,
        "Send /web to the orchestrator bot in Telegram for a login link.".into(),
    ))
}

/// A mutation additionally insists on a JSON body. The session cookie is `SameSite=Strict`, so
/// a cross-site request does not carry it at all; this is the second lock, because a form post
/// (`application/x-www-form-urlencoded`) is the one cross-site shape that needs no preflight.
fn json_only(h: &HeaderMap) -> std::result::Result<(), ApiError> {
    let ok = h
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if ok {
        return Ok(());
    }
    Err(ApiError(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "expected a JSON body".into(),
    ))
}

macro_rules! guard {
    ($o:expr, $h:expr) => {
        auth(&$o, &$h).await?
    };
    ($o:expr, $h:expr, write) => {{
        auth(&$o, &$h).await?;
        json_only(&$h)?;
    }};
}

/// What a POST body says to do. The `op` names match what the old forms sent, so the actions
/// are the ones already covered by tests and audited by name.
#[derive(Deserialize, Default)]
struct Act {
    #[serde(default)]
    op: String,
    #[serde(default)]
    body: Value,
}

/// A string field of an action body, trimmed; absent and blank are the same thing.
fn s(v: &Value, key: &str) -> Option<String> {
    v[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn need(v: &Value, key: &str) -> Result<String> {
    s(v, key).with_context(|| format!("{key} is required"))
}

// ---------------------------------------------------------------- shapes

fn task_json(t: &TaskRow) -> Value {
    json!({
        "id": t.id,
        // The short handle the owner names this task by, everywhere a task is shown.
        "key": t.key(),
        "project": t.project,
        "title": t.title,
        "status": t.status,
        "running": t.running,
        "agent": t.agent_id,
        "node": t.node_id,
        "session": t.session_id,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
    })
}

fn node_json(n: &NodeRow) -> Value {
    // Deliberately not `auth_secret`: it never leaves the vault side of the process. The UI
    // only needs to know whether one is set.
    json!({
        "id": n.id,
        "name": n.name,
        "os": n.os,
        "version": n.app_version,
        "online": n.online,
        "last_seen": n.last_seen,
        "command": n.claude_command.0,
        "auth_mode": n.auth_mode,
        "has_auth_secret": n.auth_secret.is_some(),
        "max_agents": n.max_agents,
        "running": n.running.0,
        "projects": n.projects.0.iter().map(|p| json!({"project": p.project, "path": p.path})).collect::<Vec<_>>(),
        "disk": crate::disk::json(n),
        // Where it clones assigned projects (null: its default, ~/fleet-projects).
        "projects_root": n.projects_root,
    })
}

fn item_json(i: &HumanItemRow) -> Value {
    json!({
        "id": i.id,
        "kind": i.kind,
        // A task's page lists every item it ever had, so each one has to say whether it is still
        // waiting: without this an answered question read as open (`state` undefined), and the
        // page told the owner they were being waited on after they had answered.
        "state": i.state,
        "blocking": i.blocking,
        "project": i.project,
        "task": i.task_id,
        "agent": i.agent_id,
        "created_at": i.created_at,
        "summary": crate::web::item_summary(i),
    })
}

fn agent_json(a: &AgentRow) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "role": a.role,
        "project": a.project,
        "bot": a.bot_id,
        "preferred_node": a.preferred_node,
        "model": a.model,
        "effort": a.effort,
    })
}

// ---------------------------------------------------------------- overview

async fn overview(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    let items = human::open_items(&o).await?;
    let tasks: Vec<TaskRow> = sqlx::query_as(
        "select * from tasks where id <> all($1) and status not in ('done', 'archived') order by updated_at desc limit 50",
    )
    .bind(crate::model::BUILTIN_TASKS)
    .fetch_all(&o.db)
    .await?;
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await?;
    // One row per project for the landing page, so the owner sees where the work is without
    // opening anything.
    let per_project: Vec<(String, String, i64, i64, i64)> = sqlx::query_as(
        "select p.slug, p.name,
                count(t.id) filter (where t.status not in ('done','archived','stopped','failed')) as open,
                count(t.id) filter (where t.running) as running,
                count(t.id) as total
         from projects p left join tasks t on t.project = p.slug and t.id not in ('brain','secretary','retro')
         group by p.slug, p.name order by p.slug",
    )
    .fetch_all(&o.db)
    .await?;
    let (todos, sessions_today): (i64, i64) = sqlx::query_as(
        "select (select count(*) from todos where status = 'open'),
                (select count(*) from runs where started_at > now() - interval '24 hours')",
    )
    .fetch_one(&o.db)
    .await?;
    Ok(Json(json!({
        "items": items.iter().map(item_json).collect::<Vec<_>>(),
        "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
        "nodes": nodes.iter().map(node_json).collect::<Vec<_>>(),
        "projects": per_project.iter().map(|(slug, name, open, running, total)| json!({
            "slug": slug, "name": name, "open": open, "running": running, "total": total,
        })).collect::<Vec<_>>(),
        "todos_open": todos,
        "sessions_today": sessions_today,
    })))
}

/// Everything the board needs, in one call: one card per task, already carrying what the card
/// shows, so the page runs no query of its own and the columns are a pure grouping.
///
/// Unlike the overview this includes work that has just finished — a board with no "done" column
/// would hide the thing the owner most wants to see at a glance, that the fleet got somewhere.
async fn board(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    // Open work always, finished work only while it is still news, and never what the nightly
    // archive has put away: that is the point of archiving. (The archive leaves `updated_at`
    // alone, so without the explicit test a task done at 20:59 would stay on the board for a day
    // after it was archived.) `limit` is a backstop: a fleet that somehow has hundreds of open
    // tasks should still render.
    let rows: Vec<TaskRow> = sqlx::query_as(
        "select * from tasks
         where id <> all($1)
           and status <> 'archived'
           and (status not in ('done', 'stopped', 'failed')
                or updated_at > now() - interval '24 hours')
         order by updated_at desc limit 120",
    )
    .bind(crate::model::BUILTIN_TASKS)
    .fetch_all(&o.db)
    .await?;
    let ids: Vec<String> = rows.iter().map(|t| t.id.clone()).collect();
    // Two counts per card, both in one query each rather than one per row.
    let sessions: Vec<(String, i64)> = sqlx::query_as(
        "select task_id, count(*) from runs where task_id = any($1) group by task_id",
    )
    .bind(&ids)
    .fetch_all(&o.db)
    .await?;
    let milestones: Vec<(String, i64, i64)> = sqlx::query_as(
        "select task_id, count(*) filter (where done), count(*)
         from milestones where task_id = any($1) group by task_id",
    )
    .bind(&ids)
    .fetch_all(&o.db)
    .await?;
    // What each task is waiting on, if anything. The blocking one wins: it is the one that has
    // actually stopped an agent, and a card has room to name one thing.
    let open = human::open_items(&o).await?;
    // What each task is about outside the fleet, for the whole board in one query.
    let links = crate::links::of_tasks(&o, &ids).await?;
    // And what each has produced, so a card can say there is something to open.
    let attachments = crate::attachments::of_tasks(&o, &ids).await?;
    let cards: Vec<Value> = rows
        .iter()
        .map(|t| {
            let mut v = task_json(t);
            v["links"] = json!(
                links
                    .iter()
                    .filter(|l| l.task_id == t.id)
                    .map(crate::links::json)
                    .collect::<Vec<_>>()
            );
            v["attachments"] = json!(
                attachments
                    .iter()
                    .filter(|a| a.task_id == t.id)
                    .map(crate::attachments::json)
                    .collect::<Vec<_>>()
            );
            v["sessions"] = json!(
                sessions
                    .iter()
                    .find(|(id, _)| *id == t.id)
                    .map_or(0, |(_, n)| *n)
            );
            let (done, total) = milestones
                .iter()
                .find(|(id, _, _)| *id == t.id)
                .map_or((0, 0), |(_, d, n)| (*d, *n));
            v["milestones_done"] = json!(done);
            v["milestones"] = json!(total);
            v["brief"] = json!(crate::chat::clip(&t.brief, 240));
            // When the state on the card began, so it can count up: a run's own start while it
            // runs, and otherwise the last thing that happened to it.
            v["since"] = json!(if t.running {
                t.dispatched_at.unwrap_or(t.updated_at)
            } else {
                t.updated_at
            });
            v["needs_you"] = match open
                .iter()
                .find(|i| i.task_id.as_deref() == Some(t.id.as_str()))
            {
                Some(i) => json!({
                    "id": i.id, "kind": i.kind, "blocking": i.blocking, "since": i.created_at,
                }),
                None => Value::Null,
            };
            v
        })
        .collect();
    Ok(Json(json!({ "tasks": cards })))
}

// ---------------------------------------------------------------- tasks and sessions

#[derive(Deserialize)]
struct TaskQuery {
    project: Option<String>,
    status: Option<String>,
    agent: Option<String>,
    /// Words to look for, each anywhere in the task: its id or F-key, title, brief, summary,
    /// status, agent, links and attachments (see `search`).
    q: Option<String>,
    /// Include the fleet's own standing sessions (the brain and the secretary), which are
    /// hidden by default because they are never "work".
    #[serde(default)]
    builtin: bool,
    /// Include archived tasks without searching. A query, or `status=archived`, includes them
    /// anyway: someone looking for a task should find it however old it is.
    #[serde(default)]
    archived: bool,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Every task, filtered — the question the old dashboard could not answer, which only listed
/// what was unfinished and never per project.
async fn tasks(State(o): State<O>, h: HeaderMap, Query(q): Query<TaskQuery>) -> Api {
    guard!(o, h);
    let blank = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let (rows, total) = crate::search::tasks(
        &o,
        &crate::search::Filter {
            project: blank(q.project),
            status: blank(q.status),
            agent: blank(q.agent),
            q: blank(q.q),
            builtin: q.builtin,
            archived: q.archived,
            limit: q.limit.unwrap_or(100).clamp(1, 500),
            offset: q.offset.unwrap_or(0).max(0),
        },
    )
    .await?;
    // How many sessions each listed task has had, so the list can say so without a query per
    // row.
    let ids: Vec<String> = rows.iter().map(|t| t.id.clone()).collect();
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "select task_id, count(*) from runs where task_id = any($1) group by task_id",
    )
    .bind(&ids)
    .fetch_all(&o.db)
    .await?;
    let tasks: Vec<Value> = rows
        .iter()
        .map(|t| {
            let mut v = task_json(t);
            let n = counts
                .iter()
                .find(|(id, _)| *id == t.id)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            v["sessions"] = json!(n);
            v
        })
        .collect();
    Ok(Json(json!({"tasks": tasks, "total": total})))
}

#[derive(Deserialize)]
struct RunQuery {
    project: Option<String>,
    task: Option<String>,
    agent: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// The fleet's sessions, newest first: every agent process the fleet has started, filterable by
/// project, task or agent.
async fn runs(State(o): State<O>, h: HeaderMap, Query(q): Query<RunQuery>) -> Api {
    guard!(o, h);
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    const WHERE: &str = "where ($1::text is null or r.project = $1)
           and ($2::text is null or r.task_id = $2)
           and ($3::text is null or r.agent_id = $3)";
    let rows: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
    )> = sqlx::query_as(&format!(
        "select r.run_id, r.task_id, r.project, r.agent_id, r.node_id, r.session_id, r.status,
                r.outcome, r.started_at, r.ended_at, t.title
         from runs r left join tasks t on t.id = r.task_id
         {WHERE} order by r.started_at desc limit $4 offset $5"
    ))
    .bind(&q.project)
    .bind(&q.task)
    .bind(&q.agent)
    .bind(limit)
    .bind(offset)
    .fetch_all(&o.db)
    .await?;
    let total: i64 = sqlx::query_scalar(&format!("select count(*) from runs r {WHERE}"))
        .bind(&q.project)
        .bind(&q.task)
        .bind(&q.agent)
        .fetch_one(&o.db)
        .await?;
    Ok(Json(json!({
        "runs": rows.iter().map(|r| json!({
            "run_id": r.0, "task": r.1, "project": r.2, "agent": r.3, "node": r.4,
            "session": r.5, "status": r.6, "outcome": r.7,
            "started_at": r.8, "ended_at": r.9, "title": r.10,
        })).collect::<Vec<_>>(),
        "total": total,
    })))
}

async fn task(State(o): State<O>, h: HeaderMap, Path(id): Path<String>) -> Api {
    guard!(o, h);
    let t = o.task(&id).await?.context("no such task")?;
    let milestones: Vec<(i32, String, bool)> =
        sqlx::query_as("select idx, title, done from milestones where task_id = $1 order by idx")
            .bind(&id)
            .fetch_all(&o.db)
            .await?;
    let sessions: Vec<(
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "select run_id, node_id, session_id, status, outcome, started_at, ended_at
         from runs where task_id = $1 order by started_at desc",
    )
    .bind(&id)
    .fetch_all(&o.db)
    .await?;
    let items: Vec<HumanItemRow> =
        sqlx::query_as("select * from human_items where task_id = $1 order by id desc limit 50")
            .bind(&id)
            .fetch_all(&o.db)
            .await?;
    let mut v = task_json(&t);
    v["brief"] = json!(t.brief);
    v["model"] = json!(t.model);
    v["effort"] = json!(t.effort);
    v["run_id"] = json!(t.run_id);
    v["topic_id"] = json!(t.topic_id);
    v["links"] = json!(
        crate::links::of_task(&o, &t.id)
            .await?
            .iter()
            .map(crate::links::json)
            .collect::<Vec<_>>()
    );
    v["attachments"] = json!(
        crate::attachments::of_task(&o, &t.id)
            .await?
            .iter()
            .map(crate::attachments::json)
            .collect::<Vec<_>>()
    );
    // The agent's closing words, kept since the archive made "what did that task end up as" a
    // question asked weeks later.
    v["summary"] = json!(
        sqlx::query_scalar::<_, Option<String>>("select summary from tasks where id = $1")
            .bind(&t.id)
            .fetch_one(&o.db)
            .await?
    );
    Ok(Json(json!({
        "task": v,
        "milestones": milestones.iter().map(|(i, title, done)| json!({"idx": i, "title": title, "done": done})).collect::<Vec<_>>(),
        "sessions": sessions.iter().map(|r| json!({
            "run_id": r.0, "node": r.1, "session": r.2, "status": r.3,
            "outcome": r.4, "started_at": r.5, "ended_at": r.6,
        })).collect::<Vec<_>>(),
        "items": items.iter().map(item_json).collect::<Vec<_>>(),
    })))
}

/// Attaches or removes one of a task's outside references. The owner's own hand: an agent does
/// the same thing through `link_add` (see `tools`), and the brain through the same tool.
async fn task_link(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    let reference = need(&a.body, "ref")?;
    if a.op == "remove" {
        let gone = crate::links::remove(&o, &id, &reference, "owner").await?;
        return Ok(Json(json!({"removed": gone})));
    }
    let l = crate::links::add(&o, &id, &reference, "owner").await?;
    Ok(Json(json!({"link": crate::links::json(&l)})))
}

/// The owner attaching to a task by hand: a link (`url`), a file in pieces (`begin`, one `chunk`
/// per piece of at most 1 MiB as base64, `finish`), or taking one away (`remove`). The same
/// checks as an agent's upload, in `attachments`.
async fn task_attach(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    let b = &a.body;
    match a.op.as_str() {
        "url" => {
            let row = crate::attachments::add_url(
                &o,
                &id,
                &need(b, "url")?,
                &need(b, "title")?,
                "owner",
                None,
            )
            .await?;
            Ok(Json(json!({"attachment": crate::attachments::json(&row)})))
        }
        "begin" => {
            let upload = crate::attachments::begin(
                &o,
                &id,
                &need(b, "title")?,
                &need(b, "file_name")?,
                b["size"].as_i64().context("size is required")?,
                &need(b, "sha256")?,
                "owner",
                None,
            )
            .await?;
            Ok(Json(
                json!({"upload": upload, "chunk_size": crate::attachments::CHUNK}),
            ))
        }
        "chunk" => {
            let data = data_encoding::BASE64
                .decode(need(b, "data")?.as_bytes())
                .context("data must be base64")?;
            crate::attachments::chunk(
                &o,
                b["upload"].as_i64().context("upload is required")?,
                &id,
                "owner",
                b["index"].as_i64().context("index is required")?,
                &data,
            )
            .await?;
            Ok(Json(json!({"ok": true})))
        }
        "finish" => {
            let row = crate::attachments::finish(
                &o,
                b["upload"].as_i64().context("upload is required")?,
                &id,
                "owner",
                None,
            )
            .await?;
            Ok(Json(json!({"attachment": crate::attachments::json(&row)})))
        }
        "remove" => {
            let n = b["id"]
                .as_i64()
                .map(Ok)
                .unwrap_or_else(|| crate::attachments::parse_id(&need(b, "id")?))?;
            let gone = crate::attachments::remove(&o, &id, n, "owner", None).await?;
            Ok(Json(json!({"removed": gone})))
        }
        other => Err(anyhow::anyhow!("unknown op {other:?}").into()),
    }
}

/// A file attachment, streamed out of its stored pieces.
///
/// Always a download, except for the raster images a page may preview: whatever name or type
/// an agent gave a file, the owner's browser never renders it as a page (`nosniff`, and a
/// sandboxing CSP in case anything tries).
async fn attachment_file(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> std::result::Result<axum::response::Response, ApiError> {
    guard!(o, h);
    let n = crate::attachments::parse_id(&id)?;
    let a = crate::attachments::get(&o, n)
        .await?
        .filter(|a| a.kind == "file")
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no file A-{n}")))?;
    let name = a.file_name.clone().unwrap_or_else(|| format!("A-{n}"));
    let inline = a.inline();
    let mime = if inline {
        a.mime.clone().unwrap_or_default()
    } else {
        "application/octet-stream".into()
    };
    let count = crate::attachments::chunk_count(&a);
    let db = o.clone();
    let stream = futures_util::stream::unfold(0i64, move |idx| {
        let o = db.clone();
        async move {
            if idx >= count {
                return None;
            }
            let piece = crate::attachments::read_chunk(&o, n, idx)
                .await
                .map(axum::body::Bytes::from)
                .map_err(std::io::Error::other);
            Some((piece, idx + 1))
        }
    });
    let disposition = format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        name.replace(|c: char| !c.is_ascii() || c == '"' || c == '\\', "_"),
        utf8_percent(&name)
    );
    let mut r = axum::response::Response::new(axum::body::Body::from_stream(stream));
    let hs = r.headers_mut();
    let hv = |s: &str| {
        header::HeaderValue::from_str(s)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    };
    hs.insert(header::CONTENT_TYPE, hv(&mime)?);
    hs.insert(header::CONTENT_DISPOSITION, hv(&disposition)?);
    hs.insert(
        header::CONTENT_LENGTH,
        hv(&a.size.unwrap_or(0).to_string())?,
    );
    hs.insert(header::X_CONTENT_TYPE_OPTIONS, hv("nosniff")?);
    hs.insert(
        header::CONTENT_SECURITY_POLICY,
        hv("sandbox; default-src 'none'")?,
    );
    hs.insert(header::CACHE_CONTROL, hv("private, no-cache")?);
    if let Some(sha) = &a.sha256 {
        hs.insert(header::ETAG, hv(&format!("\"{sha}\""))?);
    }
    Ok(r)
}

/// RFC 5987 encoding of a file name, for `filename*`.
fn utf8_percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Deserialize)]
struct EventQuery {
    /// Page backwards through the transcript: the oldest id already seen.
    before: Option<i64>,
    limit: Option<i64>,
}

/// A task's transcript. The rows are raw agent output — a tool result can hold anything a
/// repository or a Slack message does — so they are sent as data and the UI renders them as
/// text, never as markup.
async fn task_events(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<EventQuery>,
) -> Api {
    guard!(o, h);
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>, String, String)> = sqlx::query_as(
        "select id, at, kind, data from task_events
         where task_id = $1 and ($2::bigint is null or id < $2)
         order by id desc limit $3",
    )
    .bind(&id)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&o.db)
    .await?;
    Ok(Json(json!({
        "events": rows.iter().rev().map(|(id, at, kind, data)| json!({
            "id": id, "at": at, "kind": kind,
            // Long lines are clipped: a transcript page must not become a megabyte because one
            // tool read a file.
            "data": crate::chat::clip(data, 20_000),
        })).collect::<Vec<_>>(),
        "more": rows.len() as i64 == limit,
    })))
}

// ---------------------------------------------------------------- the human queue

async fn item(State(o): State<O>, h: HeaderMap, Path(id): Path<i64>) -> Api {
    guard!(o, h);
    let it = human::item(&o, id).await?;
    // A plan page gathers the task's other open items, the way the Telegram card does: the
    // questions belonging to a plan are answered together with it.
    let related: Vec<HumanItemRow> = match &it.task_id {
        Some(task) => {
            sqlx::query_as(
                "select * from human_items where task_id = $1 and state = 'open' order by id",
            )
            .bind(task)
            .fetch_all(&o.db)
            .await?
        }
        None => vec![],
    };
    let shape = |i: &HumanItemRow| {
        let mut v = item_json(i);
        // `human::owner_payload` is what the owner may see, with secrets redacted and the
        // question, plan or summary where the page looks for it.
        v["payload"] = human::owner_payload(i);
        v["state"] = json!(i.state);
        v
    };
    let mut v = shape(&it);
    // The page answers a question on its own, so it says which task is waiting, by name.
    if let Some(id) = &it.task_id
        && let Some(task) = o.task(id).await?
    {
        v["task_title"] = json!(task.title);
    }
    v["related"] = json!(
        related
            .iter()
            .filter(|r| r.id != it.id)
            .map(shape)
            .collect::<Vec<_>>()
    );
    Ok(Json(v))
}

/// Answering an item. Every branch hands the decision to `human::answer`, which is what
/// Telegram's buttons call, so an approval means exactly the same thing from either side.
async fn item_answer(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<i64>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    let it = human::item(&o, id).await?;
    let mut done: Vec<String> = Vec::new();
    // Questions submitted alongside a plan decision, keyed by the question item's id.
    if let Some(map) = a.body["questions"].as_object() {
        for (qid, answers) in map {
            let qid: i64 = qid.parse().context("question id")?;
            // Keyed by the question's own text, as `Answer::Questions` expects, so the server
            // maps the owner's picks back to the real option labels.
            let picked = answers.as_object().cloned().unwrap_or_default();
            done.push(human::answer(&o, qid, Answer::Questions(picked)).await?);
        }
    }
    let answer = match (it.kind.as_str(), a.op.as_str()) {
        (_, "approve") => Some(Answer::Approve),
        (_, "revise") => Some(Answer::Revise(
            s(&a.body, "feedback").context("write feedback to request changes")?,
        )),
        ("ask", "answer") => Some(match (s(&a.body, "text"), a.body["choice"].as_u64()) {
            (Some(t), _) => Answer::Text(t),
            (None, Some(c)) => Answer::Choice(c as usize),
            _ => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "pick an option or write an answer".into(),
                ));
            }
        }),
        ("permission", "allow") => Some(Answer::Allow),
        ("permission", "deny") => Some(Answer::Deny),
        ("task_proposal" | "slack_reply" | "reply" | "admin_change", "accept") => {
            Some(Answer::Accept)
        }
        ("task_proposal" | "slack_reply" | "reply" | "admin_change", "dismiss") => {
            Some(Answer::Dismiss)
        }
        // Only the questions above were asked for.
        (_, "questions") => None,
        (kind, op) => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("{op:?} is not something a {kind} can answer"),
            ));
        }
    };
    if let Some(answer) = answer {
        // A plan decision may arrive on a question's page; the plan is the item that takes it.
        let target = if matches!(answer, Answer::Approve | Answer::Revise(_)) && it.kind != "plan" {
            sqlx::query_scalar(
                "select id from human_items where task_id = $1 and state = 'open' and kind = 'plan'",
            )
            .bind(&it.task_id)
            .fetch_optional(&o.db)
            .await?
            .context("there is no open plan to decide")?
        } else {
            it.id
        };
        done.push(human::answer(&o, target, answer).await?);
    }
    if done.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "nothing to answer".into(),
        ));
    }
    Ok(Json(json!({"done": done})))
}

// ---------------------------------------------------------------- projects

async fn projects(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    // `projects_json` answers with a bare array, because that is what the brain's `admin_read`
    // wants. Every response here is an object instead, so a field can be added later without
    // changing the shape the UI already parses.
    Ok(Json(json!({"projects": admin::projects_json(&o).await?})))
}

async fn projects_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        "create" => {
            let action = AdminAction::ProjectCreate {
                slug: need(&a.body, "slug")?,
                name: s(&a.body, "name"),
                description: s(&a.body, "description").unwrap_or_default(),
            };
            let (action, summary) = admin::prepare(&o, action).await?;
            admin::execute(&o, Actor::Owner, &action).await?;
            Ok(Json(json!({"done": summary})))
        }
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

async fn project(State(o): State<O>, h: HeaderMap, Path(slug): Path<String>) -> Api {
    guard!(o, h);
    // Resolves the slug (and errors if there is no such project) before the rest is read.
    crate::tasks::project(&o, &slug).await?;
    let folders: Vec<(i64, String, bool)> = sqlx::query_as(
        "select f.id, f.name, exists(select 1 from project_secrets ps where ps.project = $1 and ps.folder_id = f.id)
         from secret_folders f order by f.name",
    )
    .bind(&slug)
    .fetch_all(&o.db)
    .await?;
    let channels: Vec<(String, String, String, bool)> = sqlx::query_as(
        "select source, channel_id, name, triage from feed_channels where project = $1 order by name",
    )
    .bind(&slug)
    .fetch_all(&o.db)
    .await?;
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await?;
    let tasks: Vec<TaskRow> = sqlx::query_as(
        "select * from tasks where project = $1 and id not in ('brain','secretary','retro') order by updated_at desc limit 100",
    )
    .bind(&slug)
    .fetch_all(&o.db)
    .await?;
    let (sessions, open): (i64, i64) = sqlx::query_as(
        "select (select count(*) from runs where project = $1),
                (select count(*) from tasks where project = $1 and status not in ('done','archived','stopped','failed') and id not in ('brain','secretary','retro'))",
    )
    .bind(&slug)
    .fetch_one(&o.db)
    .await?;
    Ok(Json(json!({
        "project": admin::project_json(&o, &slug).await?,
        "folders": folders.iter().map(|(id, name, on)| json!({"id": id, "name": name, "bound": on})).collect::<Vec<_>>(),
        "channels": channels.iter().map(|(source, id, name, triage)| json!({
            "source": source, "channel_id": id, "name": name, "triage": triage,
        })).collect::<Vec<_>>(),
        "nodes": nodes.iter().filter(|n| n.has_project(&slug)).map(|n| json!({
            "id": n.id, "name": n.name, "online": n.online,
            "path": n.projects.0.iter().find(|m| m.project == slug).map(|m| m.path.clone()),
        })).collect::<Vec<_>>(),
        // The nodes it is assigned to (cloned there on demand), and how it stands on each.
        "assigned": crate::node_projects::project_json(&o, &slug).await?,
        "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
        "sessions": sessions,
        "open_tasks": open,
    })))
}

async fn project_act(
    State(o): State<O>,
    h: HeaderMap,
    Path(slug): Path<String>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    // Who approves the project's plans. Not an `AdminAction`, on purpose: those can be proposed
    // by the brain, and this one is the owner's alone (see `plan_gate`).
    if a.op == "plan_gate" {
        let gate = crate::plan_gate::Gate::parse(&need(&a.body, "gate")?)?;
        let done = crate::plan_gate::set_project(&o, &slug, gate).await?;
        return Ok(Json(json!({"done": done})));
    }
    // Where its code lives, for the nodes that clone it. The owner's alone, for the same reason.
    if a.op == "repo_url" {
        let done =
            admin::set_repo_url(&o, Actor::Owner, &slug, s(&a.body, "url").as_deref()).await?;
        return Ok(Json(json!({"done": done})));
    }
    let action = match a.op.as_str() {
        "edit" => AdminAction::ProjectEdit {
            slug,
            name: s(&a.body, "name"),
            description: s(&a.body, "description"),
            // Absent leaves the bindings alone; a list (even an empty one) replaces them.
            folders: a.body["folders"]
                .as_array()
                .map(|f| f.iter().filter_map(|v| v.as_i64()).collect::<Vec<_>>()),
        },
        // A channel may be named rather than given by id: `prepare` resolves a Slack name
        // against the conversations the owner is in.
        "channel" => AdminAction::ChannelAdd {
            project: slug,
            channel: need(&a.body, "channel")?,
            triage: a.body["triage"].as_bool().unwrap_or(false),
        },
        "unchannel" => AdminAction::ChannelRemove {
            project: slug,
            channel: need(&a.body, "channel")?,
        },
        "jira" => AdminAction::ProjectJira {
            project: slug,
            key: s(&a.body, "key").or(Some(String::new())),
            board: s(&a.body, "board").or(Some(String::new())),
            folder: s(&a.body, "folder").or(Some(String::new())),
        },
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown operation {other:?}"),
            ));
        }
    };
    let (action, summary) = admin::prepare(&o, action).await?;
    admin::execute(&o, Actor::Owner, &action).await?;
    Ok(Json(json!({"done": summary})))
}

// ---------------------------------------------------------------- secrets

async fn secrets(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    let folders: Vec<(i64, String, Option<String>, i64, Vec<String>)> = sqlx::query_as(
        "select f.id, f.name, f.preset, count(i.key), coalesce(array_agg(p.project) filter (where p.project is not null), '{}')
         from secret_folders f
         left join secret_items i on i.folder_id = f.id
         left join project_secrets p on p.folder_id = f.id
         group by f.id, f.name, f.preset order by f.name",
    )
    .fetch_all(&o.db)
    .await?;
    Ok(Json(json!({
        "folders": folders.iter().map(|(id, name, preset, keys, projects)| json!({
            "id": id, "name": name, "preset": preset, "keys": keys, "projects": projects,
        })).collect::<Vec<_>>(),
        "presets": admin::PRESETS.iter().map(|(name, keys)| json!({"name": name, "keys": keys})).collect::<Vec<_>>(),
    })))
}

async fn secrets_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        "create" => {
            let (action, summary) = admin::prepare(
                &o,
                AdminAction::FolderCreate {
                    name: need(&a.body, "name")?,
                    preset: s(&a.body, "preset"),
                },
            )
            .await?;
            admin::execute(&o, Actor::Owner, &action).await?;
            Ok(Json(json!({"done": summary})))
        }
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

/// A folder's keys. Values are never read back — not here, not anywhere: the vault is
/// write-only from the UI's side, and a key's presence is all the owner needs to see.
async fn secret(State(o): State<O>, h: HeaderMap, Path(id): Path<i64>) -> Api {
    guard!(o, h);
    let (name, preset): (String, Option<String>) =
        sqlx::query_as("select name, preset from secret_folders where id = $1")
            .bind(id)
            .fetch_optional(&o.db)
            .await?
            .context("no such folder")?;
    // Only the key names. A value is write-only from here, and the table keeps no timestamp.
    let keys: Vec<String> =
        sqlx::query_scalar("select key from secret_items where folder_id = $1 order by key")
            .bind(id)
            .fetch_all(&o.db)
            .await?;
    let projects: Vec<String> = sqlx::query_scalar(
        "select project from project_secrets where folder_id = $1 order by project",
    )
    .bind(id)
    .fetch_all(&o.db)
    .await?;
    Ok(Json(json!({
        "id": id,
        "name": name,
        "preset": preset,
        // The preset's suggested keys, so the UI can show which are still missing. A folder may
        // hold any keys regardless.
        "expected": admin::PRESETS.iter()
            .find(|(p, _)| Some(*p) == preset.as_deref())
            .map(|(_, keys)| *keys)
            .unwrap_or(&[]),
        "keys": keys.iter().map(|k| json!({"key": k})).collect::<Vec<_>>(),
        "projects": projects,
    })))
}

async fn secret_act(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<i64>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        "set" => {
            let key = need(&a.body, "key")?;
            let value = a.body["value"].as_str().context("value is required")?;
            admin::set_secret(&o, id, &key, value).await?;
            Ok(Json(json!({"done": format!("{key} is set")})))
        }
        "unset" => {
            let action = AdminAction::SecretUnset {
                folder: Ref::Id(id),
                key: need(&a.body, "key")?,
            };
            admin::execute(&o, Actor::Owner, &action).await?;
            Ok(Json(json!({"done": "removed"})))
        }
        "delete" => {
            admin::execute(
                &o,
                Actor::Owner,
                &AdminAction::FolderDelete {
                    folder: Ref::Id(id),
                },
            )
            .await?;
            Ok(Json(json!({"done": "the folder is gone"})))
        }
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

// ---------------------------------------------------------------- nodes and agents

async fn nodes(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await?;
    Ok(Json(
        json!({"nodes": nodes.iter().map(node_json).collect::<Vec<_>>()}),
    ))
}

async fn nodes_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        "invite" => Ok(Json(json!({"ticket": crate::web::new_invite(&o).await?}))),
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

/// A fresh invite ticket, as its own route so the UI's "add a node" button is one call.
async fn invite(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h, write);
    Ok(Json(json!({"ticket": crate::web::new_invite(&o).await?})))
}

async fn node(State(o): State<O>, h: HeaderMap, Path(id): Path<String>) -> Api {
    guard!(o, h);
    let n = o.node(&id).await?.context("no such node")?;
    let projects: Vec<(String, String, Option<String>)> =
        sqlx::query_as("select slug, name, repo_url from projects order by slug")
            .fetch_all(&o.db)
            .await?;
    let sessions: Vec<(String, String, chrono::DateTime<chrono::Utc>, String)> = sqlx::query_as(
        "select run_id, task_id, started_at, status from runs where node_id = $1 order by started_at desc limit 50",
    )
    .bind(&id)
    .fetch_all(&o.db)
    .await?;
    let mut v = node_json(&n);
    v["known_projects"] = json!(
        projects
            .iter()
            .map(|(slug, name, repo_url)| json!({"slug": slug, "name": name, "repo_url": repo_url}))
            .collect::<Vec<_>>()
    );
    // The projects the owner assigned to it, and how each stands there.
    v["assignments"] = json!(crate::node_projects::node_json(&o, &n).await?);
    // Whether it reads assignments at all: an older node has to update before it clones.
    v["reads_assignments"] = json!(fleet_proto::node_is_current(&n.app_version));
    v["sessions"] = json!(
        sessions
            .iter()
            .map(|(run, task, at, st)| json!({
                "run_id": run, "task": task, "started_at": at, "status": st,
            }))
            .collect::<Vec<_>>()
    );
    Ok(Json(v))
}

async fn node_act(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        "save" => {
            let command: Option<Vec<String>> = a.body["command"]
                .as_str()
                .map(|c| c.lines().map(str::to_owned).collect());
            let max = a.body["max_agents"].as_i64().map(|m| m as i32);
            let saved = admin::save_node_settings(
                &o,
                Actor::Owner,
                &id,
                command.as_deref(),
                a.body["auth_mode"].as_str(),
                max,
                s(&a.body, "auth_secret").as_deref(),
            )
            .await?;
            // Saved either way; whether the node has it yet is a separate fact the UI shows.
            Ok(Json(json!({
                "done": "settings saved",
                "push_error": saved.push_error,
            })))
        }
        "revoke" => {
            admin::execute(&o, Actor::Owner, &AdminAction::NodeRevoke { node: id }).await?;
            Ok(Json(json!({"done": "the node is revoked"})))
        }
        // Which projects this node clones, and where: the owner's alone (see `node_projects`).
        "projects_root" => {
            let done =
                admin::set_projects_root(&o, Actor::Owner, &id, s(&a.body, "root").as_deref())
                    .await?;
            Ok(Json(json!({"done": done})))
        }
        "retry" => {
            let done =
                admin::retry_assignment(&o, Actor::Owner, &id, &need(&a.body, "project")?).await?;
            Ok(Json(json!({"done": done})))
        }
        "assign" | "unassign" => {
            let done = admin::assign_project(
                &o,
                Actor::Owner,
                &id,
                &need(&a.body, "project")?,
                a.op == "assign",
            )
            .await?;
            Ok(Json(json!({"done": done})))
        }
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

async fn agents(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    let agents: Vec<AgentRow> = sqlx::query_as("select * from agents order by id")
        .fetch_all(&o.db)
        .await?;
    // Bots: the pool of Telegram identities agents speak as. Tokens stay in the vault.
    let bots: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("select b.id, b.username, a.id from bots b left join agents a on a.bot_id = b.id order by b.id")
            .fetch_all(&o.db)
            .await?;
    Ok(Json(json!({
        "agents": agents.iter().map(agent_json).collect::<Vec<_>>(),
        "bots": bots.iter().map(|(id, username, agent)| json!({
            "id": id, "username": username, "agent": agent,
        })).collect::<Vec<_>>(),
        "efforts": crate::model::EFFORTS,
        // The model is free text (the owner is not held to the brain's allow-list); these are
        // what the UI offers as the default and as suggestions.
        "default_model": o.cfg.model,
        "suggested_models": o.cfg.allowed_models,
    })))
}

async fn agents_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        "add" => {
            admin::add_bot(&o, a.body["token"].as_str().unwrap_or_default()).await?;
            Ok(Json(json!({"done": "the bot joined the pool"})))
        }
        "delete" => {
            let id = a.body["id"].as_i64().context("id is required")?;
            admin::execute(
                &o,
                Actor::Owner,
                &AdminAction::BotRemove { bot: Ref::Id(id) },
            )
            .await?;
            Ok(Json(json!({"done": "the bot is out of the pool"})))
        }
        // The owner may pick any valid model and effort; the allow-list is the brain's.
        "settings" => {
            admin::execute(
                &o,
                Actor::Owner,
                &AdminAction::AgentSettings {
                    agent: need(&a.body, "agent")?,
                    model: Some(s(&a.body, "model").unwrap_or_default()),
                    effort: Some(s(&a.body, "effort").unwrap_or_default()),
                },
            )
            .await?;
            Ok(Json(json!({"done": "saved"})))
        }
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

// ---------------------------------------------------------------- the secretary's inbox

async fn inbox(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    let todos: Vec<TodoRow> = sqlx::query_as(
        "(select * from todos where status = 'open' order by id desc limit 200)
         union all (select * from todos where status <> 'open' order by updated_at desc limit 20)",
    )
    .fetch_all(&o.db)
    .await?;
    let rules: Vec<RuleRow> = sqlx::query_as(
        "select * from secretary_rules where active or created_at > now() - interval '30 days'
         order by active desc, id desc limit 100",
    )
    .fetch_all(&o.db)
    .await?;
    let triage: Vec<(chrono::DateTime<chrono::Utc>, Vec<String>, String, String)> = sqlx::query_as(
        "select at, refs, decision, note from secretary_triage order by id desc limit 50",
    )
    .fetch_all(&o.db)
    .await?;
    let (pending, batches_today, messages_today): (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from feed_messages where secretary = 'pending'),
                (select count(*) from secretary_batches where at > now() - interval '1 day'),
                (select coalesce(sum(messages), 0) from secretary_batches where at > now() - interval '1 day')",
    )
    .fetch_one(&o.db)
    .await?;
    // Every todo and triage row cites the messages behind it. Resolved here, in one lookup, so
    // the UI can show who wrote what instead of an opaque reference — including whether the
    // owner has that conversation muted, which is why the secretary left it alone.
    let mut refs: Vec<String> = todos.iter().flat_map(|t| t.refs.clone()).collect();
    refs.extend(triage.iter().flat_map(|t| t.1.clone()));
    let sources = secretary::messages_by_ref(&o, &refs).await?;
    Ok(Json(json!({
        "sources": sources.iter().map(|(r, m)| (r.clone(), json!({
            "label": m.label(),
            "link": m.web_link(),
            "attention": m.attention,
            // Clipped: a source line is a hint, not the message.
            "text": crate::chat::clip(&m.text, 400),
        }))).collect::<serde_json::Map<_, _>>(),
        "todos": todos.iter().map(|t| json!({
            "id": t.id, "title": t.title, "detail": t.detail, "refs": t.refs,
            "status": t.status, "created_by": t.created_by, "created_at": t.created_at,
            "updated_at": t.updated_at, "due": t.due, "workflow": t.workflow_task,
        })).collect::<Vec<_>>(),
        "rules": rules.iter().map(|r| json!({
            "id": r.id, "text": r.text, "active": r.active,
            "source": r.source, "cited": r.cited, "created_at": r.created_at,
        })).collect::<Vec<_>>(),
        "triage": triage.iter().map(|(at, refs, decision, note)| json!({
            "at": at, "refs": refs, "decision": decision, "note": note,
        })).collect::<Vec<_>>(),
        "stats": {"pending": pending, "batches_today": batches_today, "messages_today": messages_today},
    })))
}

async fn inbox_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    match a.op.as_str() {
        // The same four actions as the Inbox buttons in Telegram, by the same code path.
        "todo" => {
            let id = a.body["id"].as_i64().context("id is required")?;
            let action = need(&a.body, "action")?;
            if !matches!(action.as_str(), "d" | "x" | "m" | "o") {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("unknown action {action:?}"),
                ));
            }
            let note = secretary::button(&o, &format!("t|{id}|{action}")).await?;
            Ok(Json(json!({"done": note})))
        }
        "rule_remove" => {
            let id = a.body["id"].as_i64().context("id is required")?;
            let note = secretary::button(&o, &format!("r|{id}|u")).await?;
            Ok(Json(json!({"done": note})))
        }
        "rule_add" => {
            secretary::owner_rule(&o, &need(&a.body, "text")?).await?;
            Ok(Json(json!({"done": "the rule is noted"})))
        }
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {other:?}"),
        )),
    }
}

// ---------------------------------------------------------------- habits

async fn habits(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    Ok(Json(crate::habits::json(&o, false).await?))
}

/// The same three things the buttons on a habit's card do, by the same code path, so the web and
/// Telegram cannot disagree about what pausing one means.
async fn habits_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    let id = a.body["id"].as_i64().context("id is required")?;
    let action = match a.op.as_str() {
        "pause" => "p",
        "resume" => "r",
        "delete" => "d",
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown operation {other:?}"),
            ));
        }
    };
    let note = crate::habits::button(&o, &format!("hb|{id}|{action}")).await?;
    Ok(Json(json!({"done": note})))
}

// ---------------------------------------------------------------- retrospective

async fn retro(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    Ok(Json(crate::retro::json(&o).await?))
}

/// The same two decisions the cards offer, by the same code path.
async fn retro_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    // A review on demand, over as many days as asked for; the scheduled one covers the last day.
    if a.op == "run" {
        let days = a.body["days"].as_i64().unwrap_or(1);
        let note = crate::retro::run_now(&o, days).await?;
        return Ok(Json(json!({"done": note})));
    }
    let id = a.body["id"].as_i64().context("id is required")?;
    let action = match a.op.as_str() {
        "make_task" => "t",
        "dismiss" => "x",
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown operation {other:?}"),
            ));
        }
    };
    let note = crate::retro::button(&o, &format!("rt|{id}|{action}")).await?;
    Ok(Json(json!({"done": note})))
}

// ---------------------------------------------------------------- people

#[derive(Deserialize)]
struct PeopleQuery {
    /// A name, handle, tag, project or position, any part of it.
    q: Option<String>,
    tag: Option<String>,
    project: Option<String>,
    language: Option<String>,
    limit: Option<i64>,
}

/// The people records, filtered: what the retrospective has written and the owner reviews.
async fn people(State(o): State<O>, h: HeaderMap, Query(q): Query<PeopleQuery>) -> Api {
    guard!(o, h);
    let filter = crate::people::Filter {
        q: q.q,
        tag: q.tag,
        project: q.project,
        language: q.language,
        limit: q.limit.unwrap_or(200),
    };
    Ok(Json(crate::people::list_json(&o, &filter).await?))
}

/// The owner adds someone the retrospective has not met yet.
async fn people_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    guard!(o, h, write);
    if a.op != "create" {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown operation {:?}", a.op),
        ));
    }
    let id = crate::people::owner_create(&o, &need(&a.body, "name")?).await?;
    Ok(Json(json!({"done": format!("Added #{id}."), "id": id})))
}

async fn person(State(o): State<O>, h: HeaderMap, Path(id): Path<i64>) -> Api {
    guard!(o, h);
    Ok(Json(crate::people::detail_json(&o, id).await?))
}

/// The owner's edit (whatever the form sends replaces what is there) or delete.
async fn person_act(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<i64>,
    Json(a): Json<Act>,
) -> Api {
    guard!(o, h, write);
    let done = match a.op.as_str() {
        "edit" => crate::people::owner_edit(&o, id, &a.body).await?,
        "delete" => crate::people::owner_delete(&o, id).await?,
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown operation {other:?}"),
            ));
        }
    };
    Ok(Json(json!({"done": done})))
}

// ---------------------------------------------------------------- the kit

async fn kit(State(o): State<O>, h: HeaderMap) -> Api {
    guard!(o, h);
    Ok(Json(crate::kit::json(&o).await?))
}

/// One item for its editor: a skill's files in full, an MCP server's config with placeholders.
async fn kit_item(State(o): State<O>, h: HeaderMap, Path(id): Path<i64>) -> Api {
    guard!(o, h);
    Ok(Json(crate::kit::item_json(&o, id).await?))
}

/// Every change to the kit, as the owner (the kit refuses anyone else; see `kit.rs`).
async fn kit_act(State(o): State<O>, h: HeaderMap, Json(a): Json<Act>) -> Api {
    use crate::kit;
    guard!(o, h, write);
    let b = &a.body;
    let id = || b["id"].as_i64().context("id is required");
    let done = match a.op.as_str() {
        // What a node has, to choose from. The answer carries no secret value.
        "capture" => {
            return Ok(Json(
                kit::capture(&o, Actor::Owner, &need(b, "node")?).await?,
            ));
        }
        "import" => {
            let sel: kit::Selection = serde_json::from_value(b.clone())
                .context("skills, plugins and mcp are lists of names")?;
            kit::import(&o, Actor::Owner, &need(b, "capture")?, &sel).await?
        }
        "enable" => kit::set_enabled(&o, Actor::Owner, id()?, true).await?,
        "disable" => kit::set_enabled(&o, Actor::Owner, id()?, false).await?,
        "delete" => kit::delete(&o, Actor::Owner, id()?).await?,
        "skill_save" => {
            let files: std::collections::BTreeMap<String, String> =
                serde_json::from_value(b["files"].clone()).context("files are {path: text}")?;
            kit::save_skill(&o, Actor::Owner, &need(b, "name")?, &files).await?
        }
        "mcp_save" => {
            // The editor sends the config as the text the owner typed.
            let config: Value = match &b["config"] {
                Value::String(text) => {
                    serde_json::from_str(text).context("the config is not valid JSON")?
                }
                other => other.clone(),
            };
            kit::save_mcp(&o, Actor::Owner, &need(b, "name")?, config).await?
        }
        "plugin_add" => {
            let p = fleet_proto::KitPlugin {
                plugin: need(b, "plugin")?,
                marketplace: need(b, "marketplace")?,
                marketplace_source: need(b, "source")?,
            };
            kit::add_plugin(&o, Actor::Owner, &p).await?
        }
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown operation {other:?}"),
            ));
        }
    };
    Ok(Json(json!({"done": done})))
}
