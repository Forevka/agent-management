//! The web UI: plan review, project setup, secrets, nodes and the bot pool.
//!
//! There are no passwords. The owner logs in through single-use magic links that only the
//! orchestrator bot hands out (in Telegram, which already authenticates them); a link sets a
//! `SameSite=Strict` session cookie, which also keeps other sites from posting our forms.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Path, RawForm, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde_json::{Value, json};

use crate::{
    admin::{self, Actor, AdminAction, Ref},
    chat::{clip, esc},
    core::Orchestrator,
    human::{self, Answer},
    model::{HumanItemRow, NodeRow, RuleRow, TaskRow, TodoRow},
    secretary,
};

type O = Arc<Orchestrator>;
const COOKIE: &str = "fleet_session";

pub fn router(o: O) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/login/{token}", get(login))
        .route("/", get(dashboard))
        .route("/items/{id}", get(item_page).post(item_post))
        .route("/tasks/{id}", get(task_page))
        .route("/projects", get(projects_page).post(projects_post))
        .route("/projects/{slug}", get(project_page).post(project_post))
        .route("/secrets", get(secrets_page).post(secrets_post))
        .route("/secrets/{id}", get(secret_page).post(secret_post))
        .route("/nodes", get(nodes_page).post(nodes_post))
        .route("/nodes/{id}", get(node_page).post(node_post))
        .route("/agents", get(agents_page).post(agents_post))
        .route("/inbox", get(inbox_page).post(inbox_post))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(o)
}

// ---------------------------------------------------------------- auth

pub fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::rng().fill_bytes(&mut b);
    data_encoding::BASE64URL_NOPAD.encode(&b)
}

/// A single-use login link that lands on `target`.
pub async fn magic_link(o: &Orchestrator, target: &str) -> Result<String> {
    let token = random_token();
    sqlx::query("insert into magic_links (token, target, expires_at) values ($1, $2, now() + make_interval(mins => $3::int))")
        .bind(&token)
        .bind(target)
        .bind(o.cfg.magic_link_minutes as i32)
        .execute(&o.db)
        .await?;
    Ok(format!("{}/login/{token}", o.cfg.public_url))
}

pub async fn new_invite(o: &Orchestrator) -> Result<String> {
    let addr = o
        .invite_addr
        .read()
        .unwrap()
        .clone()
        .context("the orchestrator endpoint is not up yet")?;
    let ticket = fleet_net::InviteTicket::new(addr);
    sqlx::query("insert into invites (secret) values ($1)")
        .bind(&ticket.secret)
        .execute(&o.db)
        .await?;
    o.audit("owner", "invite.create", json!({})).await;
    Ok(ticket.to_string())
}

async fn login(State(o): State<O>, Path(token): Path<String>) -> Response {
    let target: Option<String> =
        sqlx::query_scalar("update magic_links set used = true where token = $1 and not used and expires_at > now() returning target")
            .bind(&token)
            .fetch_optional(&o.db)
            .await
            .ok()
            .flatten();
    let Some(target) = target else {
        return page_status(
            StatusCode::FORBIDDEN,
            "Link expired",
            "<p>This link was already used or has expired. Send <code>/web</code> to the orchestrator bot for a new one.</p>",
        );
    };
    let session = random_token();
    if sqlx::query(
        "insert into web_sessions (token, expires_at) values ($1, now() + interval '30 days')",
    )
    .bind(&session)
    .execute(&o.db)
    .await
    .is_err()
    {
        return page_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error",
            "<p>Could not start a session.</p>",
        );
    }
    let target = if target.starts_with('/') {
        target
    } else {
        "/".into()
    };
    let cookie =
        format!("{COOKIE}={session}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=2592000");
    ([(header::SET_COOKIE, cookie)], Redirect::to(&target)).into_response()
}

async fn authed(o: &Orchestrator, headers: &HeaderMap) -> bool {
    let Some(token) = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            kv.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_owned)
        })
    else {
        return false;
    };
    sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from web_sessions where token = $1 and expires_at > now())",
    )
    .bind(token)
    .fetch_one(&o.db)
    .await
    .unwrap_or(false)
}

macro_rules! require {
    ($o:expr, $h:expr) => {
        if !authed(&$o, &$h).await {
            return page_status(
                StatusCode::UNAUTHORIZED,
                "Sign in",
                "<p>Send <code>/web</code> to the orchestrator bot in Telegram to get a login link.</p>",
            );
        }
    };
}

// ---------------------------------------------------------------- layout

fn page(title: &str, body: &str) -> Response {
    page_status(StatusCode::OK, title, body)
}

fn page_status(code: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t} · Fleet</title><style>
:root{{--bg:#f7f7f5;--fg:#1d1d1b;--muted:#6b6b66;--card:#fff;--line:#e3e3de;--accent:#3355cc;--ok:#1d7a3a;--bad:#b3261e}}
@media (prefers-color-scheme:dark){{:root{{--bg:#151514;--fg:#ececea;--muted:#9a9a94;--card:#1f1f1d;--line:#33332f;--accent:#8fa8ff;--ok:#5cc27e;--bad:#ff8a80}}}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,Segoe UI,sans-serif}}
header{{display:flex;gap:16px;align-items:center;padding:12px 16px;border-bottom:1px solid var(--line);flex-wrap:wrap}}
header b{{margin-right:8px}}header a{{color:var(--muted);text-decoration:none}}header a:hover{{color:var(--fg)}}
main{{max-width:960px;margin:0 auto;padding:16px}}h1{{font-size:22px;margin:8px 0 16px}}h2{{font-size:17px;margin:24px 0 8px}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px 16px;margin:10px 0;overflow-wrap:anywhere}}
table{{width:100%;border-collapse:collapse}}td,th{{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line);vertical-align:top}}
.muted{{color:var(--muted)}}a{{color:var(--accent)}}code,pre{{font-family:ui-monospace,Consolas,monospace;font-size:13px}}
pre{{white-space:pre-wrap;background:var(--bg);padding:10px;border-radius:6px;overflow:auto}}
input[type=text],input[type=password],input[type=number],textarea,select{{width:100%;padding:8px;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg);font:inherit}}
textarea{{min-height:90px}}label{{display:block;margin:8px 0 4px}}.row{{display:flex;gap:8px;flex-wrap:wrap;align-items:center}}
button{{padding:8px 14px;border-radius:6px;border:1px solid var(--line);background:var(--card);color:var(--fg);font:inherit;cursor:pointer}}
button.primary{{background:var(--accent);border-color:var(--accent);color:#fff}}button.danger{{color:var(--bad)}}
.pill{{display:inline-block;padding:1px 8px;border-radius:99px;border:1px solid var(--line);font-size:12px}}
.on{{color:var(--ok)}}.off{{color:var(--muted)}}.plan h1,.plan h2,.plan h3{{font-size:16px}}
</style></head><body><header><b>Fleet</b><a href="/">Dashboard</a><a href="/projects">Projects</a><a href="/secrets">Secrets</a><a href="/nodes">Nodes</a><a href="/agents">Agents</a><a href="/inbox">Inbox</a></header>
<main><h1>{t}</h1>{body}</main></body></html>"#,
        t = esc(title)
    );
    (code, Html(html)).into_response()
}

fn err_page(e: anyhow::Error) -> Response {
    page_status(
        StatusCode::BAD_REQUEST,
        "Error",
        &format!(
            "<div class=card><pre>{}</pre></div><p><a href=\"javascript:history.back()\">Back</a></p>",
            esc(&format!("{e:#}"))
        ),
    )
}

/// Renders Markdown from an agent. Raw HTML in it is shown as text, never interpreted.
fn markdown(md: &str) -> String {
    let parser = Parser::new_ext(
        md,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    )
    .map(|ev| match ev {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        // Links keep only harmless schemes (no `javascript:` or `data:`). Images become plain
        // links, so a plan cannot make the browser fetch anything by being viewed.
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
        other => other,
    });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

fn safe_url(url: pulldown_cmark::CowStr<'_>) -> pulldown_cmark::CowStr<'_> {
    let lower = url.trim().to_ascii_lowercase();
    let ok = ["http://", "https://", "mailto:"]
        .iter()
        .any(|p| lower.starts_with(p))
        || lower.starts_with('#')
        || lower.starts_with('/');
    if ok { url } else { "#".into() }
}

/// No scripts at all, nothing loaded from elsewhere, forms only back to us, no framing.
async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    resp
}

fn form(body: &[u8]) -> Vec<(String, String)> {
    form_urlencoded::parse(body).into_owned().collect()
}

fn field<'a>(f: &'a [(String, String)], name: &str) -> Option<&'a str> {
    f.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn fields<'a>(f: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    f.iter()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .collect()
}

// ---------------------------------------------------------------- dashboard

async fn dashboard(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    let r: Result<String> = async {
        let items = human::open_items(&o).await?;
        let tasks: Vec<TaskRow> =
            sqlx::query_as("select * from tasks where id not in ('brain', 'secretary') and status <> 'done' order by updated_at desc limit 50").fetch_all(&o.db).await?;
        let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name").fetch_all(&o.db).await?;
        let mut s = String::from("<h2>Waiting on you</h2>");
        if items.is_empty() {
            s.push_str("<p class=muted>Nothing.</p>");
        }
        for i in &items {
            s.push_str(&format!(
                "<div class=card><a href=\"/items/{id}\">Q-{id}</a> <span class=pill>{kind}</span> {blk} <span class=muted>{ctx}</span><br>{summary}</div>",
                id = i.id,
                kind = esc(&i.kind),
                blk = if i.blocking { "🔴" } else { "🟡" },
                ctx = esc(i.task_id.as_deref().or(i.project.as_deref()).unwrap_or("")),
                summary = esc(&clip(&item_summary(i), 200)),
            ));
        }
        s.push_str("<h2>Open tasks</h2><table><tr><th>Task</th><th>Status</th><th>Agent</th></tr>");
        for t in &tasks {
            s.push_str(&format!(
                "<tr><td><a href=\"/tasks/{id}\">{id}</a> {title}</td><td>{st}{run}</td><td>{ag}</td></tr>",
                id = esc(&t.id),
                title = esc(&t.title),
                st = esc(&t.status),
                run = if t.running { " ▶" } else { "" },
                ag = esc(t.agent_id.as_deref().unwrap_or(""))
            ));
        }
        s.push_str("</table><h2>Nodes</h2><table>");
        for n in &nodes {
            s.push_str(&format!(
                "<tr><td><a href=\"/nodes/{id}\">{name}</a></td><td class={c}>{st}</td><td>{r}/{m} agents</td></tr>",
                id = esc(&n.id),
                name = esc(&n.name),
                c = if n.online { "on" } else { "off" },
                st = if n.online { "online" } else { "offline" },
                r = n.running.0.len(),
                m = n.max_agents
            ));
        }
        s.push_str("</table>");
        Ok(s)
    }
    .await;
    match r {
        Ok(body) => page("Dashboard", &body),
        Err(e) => err_page(e),
    }
}

pub(crate) fn item_summary(i: &HumanItemRow) -> String {
    let p = &i.payload.0;
    p["question"]
        .as_str()
        .or(p["title"].as_str())
        .or(p["summary"].as_str())
        .or(p["text"].as_str())
        .or(p["tool_name"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            human::shown(p)["questions"][0]["question"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- items (plans, questions)

async fn item_page(State(o): State<O>, h: HeaderMap, Path(id): Path<i64>) -> Response {
    require!(o, h);
    match render_item(&o, id).await {
        Ok((title, body)) => page(&title, &body),
        Err(e) => err_page(e),
    }
}

async fn render_item(o: &Orchestrator, id: i64) -> Result<(String, String)> {
    let item = human::item(o, id).await?;
    // Plans and their questions are reviewed together on one page per task.
    let related: Vec<HumanItemRow> = match &item.task_id {
        Some(t) if matches!(item.kind.as_str(), "plan" | "question") => {
            sqlx::query_as("select * from human_items where task_id = $1 and state = 'open' and kind in ('plan','question') order by id")
                .bind(t)
                .fetch_all(&o.db)
                .await?
        }
        _ => vec![item.clone()],
    };
    let task = match &item.task_id {
        Some(t) => o.task(t).await?,
        None => None,
    };
    let mut s = String::new();
    if let Some(t) = &task {
        s.push_str(&format!(
            "<p class=muted>Task <a href=\"/tasks/{id}\">{id}</a>: {title}</p>",
            id = esc(&t.id),
            title = esc(&t.title)
        ));
    }
    if item.state != "open" {
        s.push_str(&format!(
            "<div class=card>Q-{} is <b>{}</b>.</div>",
            item.id,
            esc(&item.state)
        ));
        if let Some(a) = &item.answer {
            s.push_str(&format!(
                "<pre>{}</pre>",
                esc(&serde_json::to_string_pretty(&a.0)?)
            ));
        }
        return Ok((format!("Q-{}", item.id), s));
    }
    s.push_str(&format!("<form method=post action=\"/items/{}\">", item.id));
    let mut has_plan = false;
    for it in &related {
        let p = &it.payload.0;
        match it.kind.as_str() {
            "plan" => {
                has_plan = true;
                s.push_str(&format!(
                    "<h2>Plan · Q-{}</h2><div class=\"card plan\">{}</div>",
                    it.id,
                    markdown(
                        p["plan"]
                            .as_str()
                            .or(human::shown(p)["plan"].as_str())
                            .unwrap_or("(empty plan)")
                    )
                ));
            }
            "question" => {
                s.push_str(&format!("<h2>Questions · Q-{}</h2>", it.id));
                // The shown (redacted) wording; answers go by option index, which the server
                // maps back to the real labels (question_answers).
                for (qi, q) in human::shown(p)["questions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    let name = format!("i{}_q{qi}", it.id);
                    let multi = q["multiSelect"].as_bool().unwrap_or(false);
                    s.push_str(&format!(
                        "<div class=card><b>{}</b>",
                        esc(q["question"].as_str().unwrap_or_default())
                    ));
                    for (oi, opt) in q["options"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .enumerate()
                    {
                        let label = opt["label"].as_str().unwrap_or_default();
                        s.push_str(&format!(
                            "<label><input type={ty} name=\"{name}\" value={oi}> {v}<br><span class=muted>{d}</span></label>",
                            ty = if multi { "checkbox" } else { "radio" },
                            v = esc(label),
                            d = esc(opt["description"].as_str().unwrap_or_default())
                        ));
                    }
                    s.push_str(&format!(
                        "<label>Other / notes<input type=text name=\"{name}_other\"></label></div>"
                    ));
                }
            }
            "ask" => {
                s.push_str(&format!(
                    "<div class=card><b>{}</b>",
                    esc(p["question"].as_str().unwrap_or_default())
                ));
                for (i, opt) in p["options"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    s.push_str(&format!(
                        "<label><input type=radio name=choice value={i}> {}</label>",
                        esc(opt.as_str().unwrap_or_default())
                    ));
                }
                s.push_str("<label>Or answer in your own words<textarea name=text></textarea></label></div><button class=primary name=action value=answer>Send answer</button>");
            }
            "permission" => {
                s.push_str(&format!(
                    "<div class=card>The agent wants to use <code>{}</code>:<pre>{}</pre></div><div class=row><button class=primary name=action value=allow>Allow</button><button class=danger name=action value=deny>Deny</button></div>",
                    esc(p["tool_name"].as_str().unwrap_or_default()),
                    esc(&serde_json::to_string_pretty(human::shown(p))?)
                ));
            }
            "task_proposal" => {
                s.push_str(&format!(
                    "<div class=card><b>{}</b> in {}<div class=plan>{}</div></div><div class=row><button class=primary name=action value=accept>Create task</button><button name=action value=dismiss>Dismiss</button></div>",
                    esc(p["title"].as_str().unwrap_or_default()),
                    esc(p["project"].as_str().unwrap_or_default()),
                    markdown(p["brief"].as_str().unwrap_or_default())
                ));
            }
            "slack_reply" => {
                s.push_str(&format!(
                    "<div class=card>Reply in <code>{}</code>, posted <b>as you</b>:<pre>{}</pre><span class=muted>{}</span></div><div class=row><button class=primary name=action value=accept>Send</button><button name=action value=dismiss>Dismiss</button></div>",
                    esc(p["channel_id"].as_str().unwrap_or_default()),
                    esc(p["text"].as_str().unwrap_or_default()),
                    esc(p["context"].as_str().unwrap_or_default())
                ));
            }
            "admin_change" => {
                s.push_str(&format!(
                    "<div class=card>The orchestrator asks to change the fleet:<p><b>{}</b></p>{}<details><summary class=muted>Exact change</summary><pre>{}</pre></details></div><div class=row><button class=primary name=action value=accept>Approve</button><button name=action value=dismiss>Reject</button></div>",
                    esc(p["summary"].as_str().unwrap_or_default()),
                    p["note"]
                        .as_str()
                        .map(|n| format!("<p class=muted>Orchestrator's note (its own words): {}</p>", esc(n)))
                        .unwrap_or_default(),
                    esc(&serde_json::to_string_pretty(&p["action"])?)
                ));
            }
            _ => {}
        }
    }
    if related.iter().any(|i| i.kind == "question") && !has_plan {
        s.push_str("<button class=primary name=action value=questions>Submit answers</button>");
    }
    if has_plan {
        s.push_str(
            "<h2>Decision</h2><label>Feedback (required to request changes)<textarea name=feedback></textarea></label>\
             <div class=row><button class=primary name=action value=approve>Approve plan</button><button name=action value=revise>Request changes</button></div>",
        );
    }
    s.push_str("</form>");
    let title = if has_plan {
        "Plan review".to_owned()
    } else {
        format!("Q-{}", item.id)
    };
    Ok((title, s))
}

async fn item_post(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<i64>,
    RawForm(body): RawForm,
) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<Vec<String>> = async {
        let item = human::item(&o, id).await?;
        let action = field(&f, "action").unwrap_or_default();
        let mut done = Vec::new();
        // Question answers on a plan page are submitted with whichever button was pressed.
        if let Some(task) = &item.task_id {
            let questions: Vec<HumanItemRow> =
                sqlx::query_as("select * from human_items where task_id = $1 and state = 'open' and kind = 'question' order by id").bind(task).fetch_all(&o.db).await?;
            for q in questions {
                if let Some(map) = question_answers(&q, &f) {
                    done.push(human::answer(&o, q.id, Answer::Questions(map)).await?);
                }
            }
        }
        let main: Option<(i64, Answer)> = match (item.kind.as_str(), action) {
            (_, "approve" | "revise") => {
                // The plan may be another item than the one in the URL (a question's page).
                let plan_id = if item.kind == "plan" {
                    Some(item.id)
                } else {
                    sqlx::query_scalar("select id from human_items where task_id = $1 and state = 'open' and kind = 'plan'")
                        .bind(&item.task_id)
                        .fetch_optional(&o.db)
                        .await?
                };
                let answer = if action == "approve" {
                    Answer::Approve
                } else {
                    let fb = field(&f, "feedback")
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .context("write feedback to request changes")?;
                    Answer::Revise(fb.to_owned())
                };
                plan_id.map(|id| (id, answer))
            }
            ("ask", _) => {
                let text = field(&f, "text").map(str::trim).filter(|t| !t.is_empty());
                let answer = match (text, field(&f, "choice")) {
                    (Some(t), _) => Answer::Text(t.to_owned()),
                    (None, Some(c)) => Answer::Choice(c.parse()?),
                    _ => anyhow::bail!("pick an option or write an answer"),
                };
                Some((item.id, answer))
            }
            ("permission", "allow") => Some((item.id, Answer::Allow)),
            ("permission", _) => Some((item.id, Answer::Deny)),
            ("task_proposal" | "slack_reply" | "admin_change", "accept") => {
                Some((item.id, Answer::Accept))
            }
            ("task_proposal" | "slack_reply" | "admin_change", "dismiss") => {
                Some((item.id, Answer::Dismiss))
            }
            // A decision is only ever taken on the button that says it.
            ("task_proposal" | "slack_reply" | "admin_change", other) => {
                anyhow::bail!("unknown action {other:?}")
            }
            // Questions were handled above.
            _ => None,
        };
        if let Some((id, ans)) = main {
            done.push(human::answer(&o, id, ans).await?);
        }
        Ok(done)
    }
    .await;
    match r {
        Ok(done) if done.is_empty() => page(
            "Nothing submitted",
            "<p>Answer at least one question, or approve the plan.</p>",
        ),
        Ok(done) => page(
            "Thanks",
            &format!(
                "<div class=card>{}</div><p>You can close this page.</p>",
                done.iter().map(|d| esc(d)).collect::<Vec<_>>().join("<br>")
            ),
        ),
        Err(e) => err_page(e),
    }
}

fn question_answers(
    q: &HumanItemRow,
    f: &[(String, String)],
) -> Option<serde_json::Map<String, Value>> {
    let mut map = serde_json::Map::new();
    for (qi, question) in q.payload.0["input"]["questions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let name = format!("i{}_q{qi}", q.id);
        // Options come as indices (the page may show redacted labels); the answer is the real
        // label, from the question Claude asked.
        let mut parts: Vec<String> = fields(f, &name)
            .into_iter()
            .filter_map(|i| i.parse::<usize>().ok())
            .filter_map(|i| question["options"][i]["label"].as_str().map(str::to_owned))
            .collect();
        if let Some(other) = field(f, &format!("{name}_other"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            parts.push(other.to_owned());
        }
        if parts.is_empty() {
            // Every question must be answered before the agent continues.
            return None;
        }
        map.insert(
            question["question"].as_str().unwrap_or_default().to_owned(),
            Value::String(parts.join(", ")),
        );
    }
    (!map.is_empty()).then_some(map)
}

// ---------------------------------------------------------------- tasks

async fn task_page(State(o): State<O>, h: HeaderMap, Path(id): Path<String>) -> Response {
    require!(o, h);
    let r: Result<String> = async {
        let t = o.task(&id).await?.context("no such task")?;
        // What it runs with: its own model and effort, else its agent's, else the fleet's.
        let runs_with = match &t.agent_id {
            Some(a) => match sqlx::query_as::<_, crate::model::AgentRow>("select * from agents where id = $1")
                .bind(a)
                .fetch_optional(&o.db)
                .await?
            {
                Some(agent) => crate::tasks::model_effort_label(&t, &agent, &o.cfg.model),
                None => String::new(),
            },
            None => String::new(),
        };
        let events: Vec<(String, String)> =
            sqlx::query_as("select kind, data from task_events where task_id = $1 order by id desc limit 400").bind(&id).fetch_all(&o.db).await?;
        let mut s = format!(
            "<div class=card><b>{}</b> · {} · status <span class=pill>{}</span>{}<br><span class=muted>agent {} · {} · node {} · session {}</span><div class=plan>{}</div></div><h2>Transcript (latest first)</h2>",
            esc(&t.id),
            esc(&t.title),
            esc(&t.status),
            if t.running { " ▶ running" } else { "" },
            esc(t.agent_id.as_deref().unwrap_or("-")),
            esc(&runs_with),
            esc(t.node_id.as_deref().map(|n| &n[..n.len().min(10)]).unwrap_or("-")),
            esc(t.session_id.as_deref().unwrap_or("-")),
            markdown(&t.brief)
        );
        for (kind, data) in events {
            let text = transcript_line(&kind, &data);
            if !text.is_empty() {
                s.push_str(&format!("<div class=card><span class=pill>{}</span> {}</div>", esc(&kind), text));
            }
        }
        Ok(s)
    }
    .await;
    match r {
        Ok(b) => page(&format!("Task {id}"), &b),
        Err(e) => err_page(e),
    }
}

/// Shows assistant text and tool use from stream-json events; everything else stays terse.
fn transcript_line(kind: &str, data: &str) -> String {
    // What the owner was told about the task (stored as the HTML or reason that was used).
    if kind == "notice" {
        return format!(
            "<span class=muted>📣 told the owner: {}</span>",
            esc(&clip(&plain_text(data), 500))
        );
    }
    if kind != "out" {
        return format!("<code>{}</code>", esc(&clip(data, 500)));
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return format!("<code>{}</code>", esc(&clip(data, 300)));
    };
    match v["type"].as_str() {
        Some("assistant") => v["message"]["content"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .map(|p| match p["type"].as_str() {
                        Some("text") => markdown(p["text"].as_str().unwrap_or_default()),
                        Some("tool_use") => format!(
                            "🔧 <code>{}</code> <code class=muted>{}</code>",
                            esc(p["name"].as_str().unwrap_or("?")),
                            esc(&clip(&p["input"].to_string(), 300))
                        ),
                        _ => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        Some("result") => format!(
            "turn finished · {} · cost ${:.4}",
            esc(v["subtype"].as_str().unwrap_or("")),
            v["total_cost_usd"].as_f64().unwrap_or(0.0)
        ),
        Some("system") if v["subtype"] == "init" => format!(
            "session started · model {}",
            esc(v["model"].as_str().unwrap_or("?"))
        ),
        _ => String::new(),
    }
}

/// Telegram HTML as plain text: tags dropped, the few entities `chat::esc` writes turned back.
fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------- projects

async fn projects_page(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("select slug, name, description from projects order by slug")
            .fetch_all(&o.db)
            .await
            .unwrap_or_default();
    let mut s = String::from("<table><tr><th>Project</th><th>Description</th></tr>");
    for (slug, name, d) in rows {
        s.push_str(&format!(
            "<tr><td><a href=\"/projects/{s}\">{s}</a> {n}</td><td class=muted>{d}</td></tr>",
            s = esc(&slug),
            n = esc(&name),
            d = esc(&clip(&d, 120))
        ));
    }
    s.push_str(
        "</table><h2>New project</h2><form method=post class=card>\
         <label>Slug (used in task ids and on nodes: a-z, 0-9, -)<input type=text name=slug required pattern=\"[a-z0-9][a-z0-9-]{0,39}\"></label>\
         <label>Name<input type=text name=name required></label><label>Description (helps the orchestrator route requests)<textarea name=description></textarea></label>\
         <button class=primary>Create</button></form>",
    );
    page("Projects", &s)
}

async fn projects_post(State(o): State<O>, h: HeaderMap, RawForm(body): RawForm) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<String> = async {
        let slug = field(&f, "slug").unwrap_or_default().trim().to_owned();
        let action = AdminAction::ProjectCreate {
            slug: slug.clone(),
            name: field(&f, "name").map(str::to_owned),
            description: field(&f, "description").unwrap_or_default().to_owned(),
        };
        admin::execute(&o, Actor::Owner, &action).await?;
        Ok(slug)
    }
    .await;
    match r {
        Ok(slug) => Redirect::to(&format!("/projects/{slug}")).into_response(),
        Err(e) => err_page(e),
    }
}

async fn project_page(State(o): State<O>, h: HeaderMap, Path(slug): Path<String>) -> Response {
    require!(o, h);
    let r: Result<String> = async {
        let p = crate::tasks::project(&o, &slug).await?;
        let folders: Vec<(i64, String, bool)> = sqlx::query_as(
            "select f.id, f.name, exists(select 1 from project_secrets ps where ps.project = $1 and ps.folder_id = f.id) from secret_folders f order by f.name",
        )
        .bind(&slug)
        .fetch_all(&o.db)
        .await?;
        let channels: Vec<(String, String, String, bool)> =
            sqlx::query_as("select source, channel_id, name, triage from feed_channels where project = $1 order by name").bind(&slug).fetch_all(&o.db).await?;
        let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name").fetch_all(&o.db).await?;
        let mut s = format!(
            "<form method=post class=card><input type=hidden name=op value=edit><label>Name<input type=text name=name value=\"{n}\"></label>\
             <label>Description<textarea name=description>{d}</textarea></label><h2>Secret folders for agents</h2>",
            n = esc(&p.name),
            d = esc(&p.description)
        );
        if folders.is_empty() {
            s.push_str("<p class=muted>No folders yet: create them on the <a href=\"/secrets\">Secrets</a> page.</p>");
        }
        for (id, name, on) in folders {
            s.push_str(&format!("<label><input type=checkbox name=folder value={id}{}> {}</label>", if on { " checked" } else { "" }, esc(&name)));
        }
        s.push_str("<p><button class=primary>Save</button></p></form><h2>Nodes with this project</h2><ul>");
        for n in nodes.iter().filter(|n| n.has_project(&slug)) {
            let path = n.projects.0.iter().find(|m| m.project == slug).map(|m| m.path.clone()).unwrap_or_default();
            s.push_str(&format!("<li>{} <code>{}</code> <span class={}>●</span></li>", esc(&n.name), esc(&path), if n.online { "on" } else { "off" }));
        }
        s.push_str(&format!(
            "</ul><p class=muted>Map it on a host with <code>fleet project map {} &lt;directory&gt;</code>.</p><h2>Slack channels</h2><table>",
            esc(&slug)
        ));
        for (source, id, name, triage) in channels {
            s.push_str(&format!(
                "<tr><td>{source} <code>{id}</code> {name}</td><td>{t}</td><td><form method=post><input type=hidden name=op value=unchannel><input type=hidden name=channel value=\"{id}\"><button class=danger>Remove</button></form></td></tr>",
                source = esc(&source),
                id = esc(&id),
                name = esc(&name),
                t = if triage { "triage" } else { "mirror" }
            ));
        }
        s.push_str(
            "</table><form method=post class=card><input type=hidden name=op value=channel>\
             <label>Slack channel id (e.g. C0123ABCD; you must be a member)<input type=text name=channel required></label>\
             <label><input type=checkbox name=triage value=1> Triage: show new messages to the orchestrator, which may propose tasks or replies</label>\
             <button>Add channel</button></form>",
        );
        Ok(s)
    }
    .await;
    match r {
        Ok(b) => page(&format!("Project {slug}"), &b),
        Err(e) => err_page(e),
    }
}

async fn project_post(
    State(o): State<O>,
    h: HeaderMap,
    Path(slug): Path<String>,
    RawForm(body): RawForm,
) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<()> = async {
        let action = match field(&f, "op").unwrap_or_default() {
            "edit" => AdminAction::ProjectEdit {
                slug: slug.clone(),
                name: Some(field(&f, "name").unwrap_or(&slug).to_owned()),
                description: Some(field(&f, "description").unwrap_or_default().to_owned()),
                folders: Some(
                    fields(&f, "folder")
                        .into_iter()
                        .map(str::parse)
                        .collect::<Result<_, _>>()?,
                ),
            },
            "channel" => AdminAction::ChannelAdd {
                project: slug.clone(),
                channel: field(&f, "channel").unwrap_or_default().to_owned(),
                triage: field(&f, "triage").is_some(),
            },
            "unchannel" => AdminAction::ChannelRemove {
                project: slug.clone(),
                channel: field(&f, "channel").unwrap_or_default().to_owned(),
            },
            _ => anyhow::bail!("unknown operation"),
        };
        admin::execute(&o, Actor::Owner, &action).await?;
        Ok(())
    }
    .await;
    match r {
        Ok(()) => Redirect::to(&format!("/projects/{slug}")).into_response(),
        Err(e) => err_page(e),
    }
}

// ---------------------------------------------------------------- secrets

async fn secrets_page(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    let rows: Vec<(i64, String, String, i64)> =
        sqlx::query_as("select f.id, f.name, f.preset, (select count(*) from secret_items i where i.folder_id = f.id) from secret_folders f order by f.name")
            .fetch_all(&o.db)
            .await
            .unwrap_or_default();
    let mut s = String::from(
        "<p class=muted>Folders of key/value secrets, encrypted at rest. Bind folders to projects; agents get them as .env files outside the repository. The folder named <code>slack</code> with key <code>SLACK_USER_TOKEN</code> powers the Slack feed.</p><table>",
    );
    for (id, name, preset, n) in rows {
        s.push_str(&format!(
            "<tr><td><a href=\"/secrets/{id}\">{}</a></td><td class=muted>{} · {n} keys</td></tr>",
            esc(&name),
            esc(&preset)
        ));
    }
    let opts: String = admin::PRESETS
        .iter()
        .map(|(p, _)| format!("<option>{p}</option>"))
        .collect();
    s.push_str(&format!(
        "</table><h2>New folder</h2><form method=post class=card><label>Name (e.g. \"Jira\", \"SSH access server X\")<input type=text name=name required></label><label>Preset<select name=preset>{opts}</select></label><button class=primary>Create</button></form>"
    ));
    page("Secrets", &s)
}

async fn secrets_post(State(o): State<O>, h: HeaderMap, RawForm(body): RawForm) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<i64> = admin::create_folder(
        &o,
        Actor::Owner,
        field(&f, "name").unwrap_or_default(),
        field(&f, "preset").unwrap_or("custom"),
    )
    .await;
    match r {
        Ok(id) => Redirect::to(&format!("/secrets/{id}")).into_response(),
        Err(e) => err_page(e),
    }
}

async fn secret_page(State(o): State<O>, h: HeaderMap, Path(id): Path<i64>) -> Response {
    require!(o, h);
    let r: Result<(String, String)> = async {
        let (name, preset): (String, String) = sqlx::query_as("select name, preset from secret_folders where id = $1").bind(id).fetch_one(&o.db).await?;
        let keys: Vec<String> = sqlx::query_scalar("select key from secret_items where folder_id = $1 order by key").bind(id).fetch_all(&o.db).await?;
        let mut s = String::from("<table>");
        for k in &keys {
            s.push_str(&format!(
                "<tr><td><code>{k}</code></td><td class=muted>••••••</td><td><form method=post><input type=hidden name=op value=unset><input type=hidden name=key value=\"{k}\"><button class=danger>Delete</button></form></td></tr>",
                k = esc(k)
            ));
        }
        s.push_str("</table><h2>Set a value</h2>");
        let suggested: Vec<&str> = admin::PRESETS.iter().find(|(p, _)| *p == preset).map(|(_, k)| k.to_vec()).unwrap_or_default();
        let missing: Vec<&&str> = suggested.iter().filter(|k| !keys.iter().any(|x| x == **k)).collect();
        if !missing.is_empty() {
            s.push_str(&format!("<p class=muted>Preset keys not set yet: {}</p>", missing.iter().map(|k| format!("<code>{k}</code>")).collect::<Vec<_>>().join(", ")));
        }
        s.push_str(
            "<form method=post class=card><input type=hidden name=op value=set><label>Key<input type=text name=key required pattern=\"[A-Za-z_][A-Za-z0-9_]*\"></label>\
             <label>Value (stored encrypted; never shown again)<textarea name=value required></textarea></label><button class=primary>Save</button></form>\
             <form method=post><input type=hidden name=op value=delete><button class=danger>Delete folder</button></form>",
        );
        Ok((name, s))
    }
    .await;
    match r {
        Ok((name, b)) => page(&format!("Secrets: {name}"), &b),
        Err(e) => err_page(e),
    }
}

async fn secret_post(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<i64>,
    RawForm(body): RawForm,
) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<bool> = async {
        let key = field(&f, "key").unwrap_or_default().trim().to_owned();
        match field(&f, "op").unwrap_or_default() {
            "set" => {
                admin::set_secret(&o, id, &key, field(&f, "value").unwrap_or_default()).await?;
                Ok(false)
            }
            "unset" => {
                admin::execute(
                    &o,
                    Actor::Owner,
                    &AdminAction::SecretUnset {
                        folder: Ref::Id(id),
                        key,
                    },
                )
                .await?;
                Ok(false)
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
                Ok(true)
            }
            _ => anyhow::bail!("unknown operation"),
        }
    }
    .await;
    match r {
        Ok(true) => Redirect::to("/secrets").into_response(),
        Ok(false) => Redirect::to(&format!("/secrets/{id}")).into_response(),
        Err(e) => err_page(e),
    }
}

// ---------------------------------------------------------------- nodes

async fn nodes_page(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await
        .unwrap_or_default();
    let mut s = String::from(
        "<table><tr><th>Node</th><th>Status</th><th>Projects</th><th>Version</th></tr>",
    );
    for n in &nodes {
        s.push_str(&format!(
            "<tr><td><a href=\"/nodes/{id}\">{name}</a> <span class=muted>{os}</span></td><td class={c}>{st} · {r}/{m}</td><td>{p}</td><td>{v}</td></tr>",
            id = esc(&n.id),
            name = esc(&n.name),
            os = esc(&n.os),
            c = if n.online { "on" } else { "off" },
            st = if n.online { "online" } else { "offline" },
            r = n.running.0.len(),
            m = n.max_agents,
            p = esc(&n.projects.0.iter().map(|p| p.project.as_str()).collect::<Vec<_>>().join(", ")),
            v = esc(&n.app_version)
        ));
    }
    s.push_str("</table><form method=post><button class=primary>Create invite</button></form>");
    page("Nodes", &s)
}

async fn nodes_post(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    match new_invite(&o).await {
        Ok(t) => page(
            "Invite",
            &format!(
                "<p>Run this on the new host (single use):</p><pre>fleet join {}</pre>",
                esc(&t)
            ),
        ),
        Err(e) => err_page(e),
    }
}

async fn node_page(State(o): State<O>, h: HeaderMap, Path(id): Path<String>) -> Response {
    require!(o, h);
    let r: Result<(String, String)> = async {
        let n = o.node(&id).await?.context("no such node")?;
        let sel = |m: &str| if n.auth_mode == m { " selected" } else { "" };
        let s = format!(
            "<div class=card><span class={c}>{st}</span> · {os} · version {v}<br><span class=muted>id {id}</span><br>Projects: {p}</div>\
             <form method=post class=card><input type=hidden name=op value=save>\
             <label>Claude command: program and leading arguments, one per line (e.g. <code>claude</code>, or a full path to claude.exe)<textarea name=command>{cmd}</textarea></label>\
             <label>Authentication<select name=auth_mode><option value=default{s1}>Claude's own login on this host (subscription)</option><option value=oauth_token{s2}>Subscription token (claude setup-token)</option><option value=api_key{s3}>Anthropic API key</option></select></label>\
             <label>Token or key (leave empty to keep the stored one)<input type=password name=auth_secret autocomplete=off></label>\
             <label>Max agents at once<input type=number name=max_agents min=1 max=64 value={max}></label><button class=primary>Save and push to node</button></form>\
             <form method=post><input type=hidden name=op value=revoke><button class=danger>Revoke node</button></form>",
            c = if n.online { "on" } else { "off" },
            st = if n.online { "online" } else { "offline" },
            os = esc(&n.os),
            v = esc(&n.app_version),
            id = esc(&n.id),
            p = esc(&n.projects.0.iter().map(|p| format!("{} → {}", p.project, p.path)).collect::<Vec<_>>().join("; ")),
            cmd = esc(&n.claude_command.0.join("\n")),
            s1 = sel("default"),
            s2 = sel("oauth_token"),
            s3 = sel("api_key"),
            max = n.max_agents
        );
        Ok((n.name, s))
    }
    .await;
    match r {
        Ok((name, b)) => page(&format!("Node {name}"), &b),
        Err(e) => err_page(e),
    }
}

async fn node_post(
    State(o): State<O>,
    h: HeaderMap,
    Path(id): Path<String>,
    RawForm(body): RawForm,
) -> Response {
    require!(o, h);
    let f = form(&body);
    // (revoked, push error)
    let r: Result<(bool, Option<String>)> = async {
        match field(&f, "op").unwrap_or_default() {
            "save" => {
                let max: i32 = field(&f, "max_agents")
                    .unwrap_or("4")
                    .parse()
                    .context("max agents must be a number")?;
                let command: Vec<String> = field(&f, "command")
                    .unwrap_or("claude")
                    .lines()
                    .map(str::to_owned)
                    .collect();
                // The token or key is web-only, so it is not part of the shared action; it is
                // saved together with the settings, which are validated first.
                let saved = admin::save_node_settings(
                    &o,
                    Actor::Owner,
                    &id,
                    Some(&command),
                    Some(field(&f, "auth_mode").unwrap_or("default")),
                    Some(max),
                    field(&f, "auth_secret")
                        .map(str::trim)
                        .filter(|s| !s.is_empty()),
                )
                .await?;
                Ok((false, saved.push_error))
            }
            "revoke" => {
                admin::execute(
                    &o,
                    Actor::Owner,
                    &AdminAction::NodeRevoke { node: id.clone() },
                )
                .await?;
                Ok((true, None))
            }
            _ => anyhow::bail!("unknown operation"),
        }
    }
    .await;
    match r {
        Ok((true, _)) => Redirect::to("/nodes").into_response(),
        Ok((false, None)) => Redirect::to(&format!("/nodes/{id}")).into_response(),
        Ok((false, Some(e))) => page(
            "Saved, not pushed",
            &format!(
                "<div class=card>The settings are saved, but sending them to the node failed:<pre>{}</pre>The node gets them when it next connects.</div><p><a href=\"/nodes/{}\">Back to the node</a></p>",
                esc(&e),
                esc(&id)
            ),
        ),
        Err(e) => err_page(e),
    }
}

// ---------------------------------------------------------------- agents and bots

async fn agents_page(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    let agents: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "select a.id, a.name, a.role, a.project, b.username, a.model, a.effort from agents a left join bots b on b.id = a.bot_id where a.id not in ('orchestrator', 'secretary') order by a.project, a.name",
    )
    .fetch_all(&o.db)
    .await
    .unwrap_or_default();
    let bots: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("select id, username, agent_id from bots order by id")
            .fetch_all(&o.db)
            .await
            .unwrap_or_default();
    let mut s = String::from(
        "<h2>Agents</h2><p class=muted>Model and effort are each agent's defaults (empty: the fleet's model and Claude's default effort); a task may set its own. A node whose Claude command sets <code>--model</code> or <code>--effort</code> keeps its own.</p>\
         <table><tr><th>Agent</th><th>Role</th><th>Project</th><th>Bot</th><th>Model · effort</th></tr>",
    );
    for (id, name, role, project, bot, model, effort) in agents {
        let options: String = std::iter::once("")
            .chain(crate::model::EFFORTS)
            .map(|e| {
                let sel = if effort.as_deref().unwrap_or("") == e {
                    " selected"
                } else {
                    ""
                };
                let label = if e.is_empty() { "(default)" } else { e };
                format!("<option value=\"{e}\"{sel}>{label}</option>")
            })
            .collect();
        s.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>@{}</td><td><form method=post class=row>\
             <input type=hidden name=op value=settings><input type=hidden name=agent value=\"{}\">\
             <input type=text name=model value=\"{}\" placeholder=\"{}\" maxlength=64 style=\"width:10em\">\
             <select name=effort style=\"width:8em\">{options}</select><button>Save</button></form></td></tr>",
            esc(&name),
            esc(&role),
            esc(project.as_deref().unwrap_or("")),
            esc(bot.as_deref().unwrap_or("-")),
            esc(&id),
            esc(model.as_deref().unwrap_or("")),
            esc(&o.cfg.model)
        ));
    }
    s.push_str("</table><h2>Bot pool</h2><table>");
    let free = bots.iter().filter(|b| b.2.is_none()).count();
    for (id, username, agent) in &bots {
        let action = if agent.is_none() {
            format!(
                "<form method=post><input type=hidden name=op value=delete><input type=hidden name=id value={id}><button class=danger>Remove</button></form>"
            )
        } else {
            String::new()
        };
        s.push_str(&format!(
            "<tr><td>@{}</td><td>{}</td><td>{action}</td></tr>",
            esc(username),
            esc(agent.as_deref().unwrap_or("free"))
        ));
    }
    s.push_str(&format!(
        "</table><p class=muted>{free} free. Create bots with @BotFather, add each one to the fleet group, then register its token here.</p>\
         <form method=post class=card><input type=hidden name=op value=add><label>Bot token<input type=password name=token required autocomplete=off></label><button class=primary>Add to pool</button></form>"
    ));
    page("Agents", &s)
}

async fn agents_post(State(o): State<O>, h: HeaderMap, RawForm(body): RawForm) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<()> = async {
        match field(&f, "op").unwrap_or_default() {
            "add" => admin::add_bot(&o, field(&f, "token").unwrap_or_default()).await?,
            "delete" => {
                let id: i64 = field(&f, "id").unwrap_or_default().parse()?;
                admin::execute(
                    &o,
                    Actor::Owner,
                    &AdminAction::BotRemove { bot: Ref::Id(id) },
                )
                .await?;
            }
            // The owner may use any valid model and effort (the allow-list is for the brain).
            "settings" => {
                admin::execute(
                    &o,
                    Actor::Owner,
                    &AdminAction::AgentSettings {
                        agent: field(&f, "agent").unwrap_or_default().to_owned(),
                        model: Some(field(&f, "model").unwrap_or_default().to_owned()),
                        effort: Some(field(&f, "effort").unwrap_or_default().to_owned()),
                    },
                )
                .await?;
            }
            _ => anyhow::bail!("unknown operation"),
        }
        Ok(())
    }
    .await;
    match r {
        Ok(()) => Redirect::to("/agents").into_response(),
        Err(e) => err_page(e),
    }
}

// ---------------------------------------------------------------- inbox (the secretary)

async fn inbox_page(State(o): State<O>, h: HeaderMap) -> Response {
    require!(o, h);
    match render_inbox(&o).await {
        Ok(body) => page("Inbox", &body),
        Err(e) => err_page(e),
    }
}

/// The secretary's work: open todos (with the same actions as in Telegram), the rules, and
/// what it decided about recent messages. Everything from messages is escaped.
pub(crate) async fn render_inbox(o: &Orchestrator) -> Result<String> {
    let todos: Vec<TodoRow> = sqlx::query_as(
        "(select * from todos where status = 'open' order by id desc limit 200)
         union all (select * from todos where status <> 'open' order by updated_at desc limit 20)",
    )
    .fetch_all(&o.db)
    .await?;
    let rules: Vec<RuleRow> = sqlx::query_as(
        "select * from secretary_rules where active or created_at > now() - interval '30 days' order by active desc, id desc limit 100",
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
    let mut refs: Vec<String> = todos.iter().flat_map(|t| t.refs.clone()).collect();
    refs.extend(triage.iter().flat_map(|t| t.1.clone()));
    let msgs = secretary::messages_by_ref(o, &refs).await?;
    let source = |r: &str| match msgs.get(r) {
        Some((conv, author, text, link, attention)) => {
            let who = format!("{} · {}", esc(conv), esc(author.as_deref().unwrap_or("?")));
            let who = match link {
                Some(l) if l.starts_with("https://") => {
                    format!("<a href=\"{}\" rel=noopener>{who}</a>", esc(l))
                }
                _ => who,
            };
            // A conversation the owner turned down in its source, as the secretary sees it.
            let badge = match attention.as_deref() {
                Some("muted") => " <span class=pill>muted</span>",
                Some("mentions_only") => " <span class=pill>mentions only</span>",
                _ => "",
            };
            format!("{who}{badge}: {}", esc(&clip(text, 200)))
        }
        None => format!("<span class=muted>{} (no longer kept)</span>", esc(r)),
    };

    let mut s = format!(
        "<p class=muted>{messages_today} messages in {batches_today} batches in the last day · {pending} waiting for the next batch.</p><h2>Todos</h2>"
    );
    let open: Vec<&TodoRow> = todos.iter().filter(|t| t.status == "open").collect();
    if open.is_empty() {
        s.push_str("<p class=muted>Nothing open.</p>");
    }
    for t in &open {
        let due = t
            .due
            .map(|d| format!(" <span class=pill>due {}</span>", d.format("%Y-%m-%d")))
            .unwrap_or_default();
        let sources: String = t
            .refs
            .iter()
            .map(|r| format!("<li>{}</li>", source(r)))
            .collect();
        s.push_str(&format!(
            "<div class=card><b>T-{id}</b> {title}{due}<br>{detail}<ul class=muted>{sources}</ul>\
             <form method=post class=row><input type=hidden name=op value=todo><input type=hidden name=id value={id}>\
             <button name=action value=d>✅ Done</button><button name=action value=x>🗑 Dismiss</button><button name=action value=m>🛠 Make task</button></form></div>",
            id = t.id,
            title = esc(&t.title),
            detail = esc(&t.detail),
        ));
    }
    let settled: Vec<&TodoRow> = todos.iter().filter(|t| t.status != "open").collect();
    if !settled.is_empty() {
        s.push_str("<h2>Recently settled</h2><table>");
        for t in settled {
            // What the secretary settled on its own can be reopened, as in Telegram; a todo
            // sent for a task stays as it is.
            let by = t.updated_by.as_deref().unwrap_or("owner");
            let reopen = if t.status != "tasked" {
                format!(
                    "<form method=post><input type=hidden name=op value=todo><input type=hidden name=id value={}><button name=action value=o>↩️ Reopen</button></form>",
                    t.id
                )
            } else {
                String::new()
            };
            s.push_str(&format!(
                "<tr><td>T-{}</td><td>{}</td><td class=muted>{} by the {}</td><td>{reopen}</td></tr>",
                t.id,
                esc(&t.title),
                esc(if t.status == "tasked" { "task requested" } else { &t.status }),
                esc(if by == "secretary" { "secretary" } else { "owner" })
            ));
        }
        s.push_str("</table>");
    }

    s.push_str("<h2>Rules</h2><p class=muted>How the secretary triages, as you taught it. Every change is also posted to the Inbox topic.</p><table>");
    for r in &rules {
        let action = if r.active {
            format!(
                "<form method=post><input type=hidden name=op value=rule_remove><input type=hidden name=id value={}><button class=danger>Remove</button></form>",
                r.id
            )
        } else {
            "<span class=muted>removed</span>".into()
        };
        let why = r
            .cited
            .as_deref()
            .map(|c| format!("<br><span class=muted>asked: “{}”</span>", esc(c)))
            .unwrap_or_default();
        s.push_str(&format!(
            "<tr><td>R-{}</td><td>{}{why}</td><td class=muted>{}</td><td>{action}</td></tr>",
            r.id,
            esc(&r.text),
            esc(&r.source)
        ));
    }
    s.push_str(
        "</table><form method=post class=card><input type=hidden name=op value=rule_add>\
         <label>New rule<input type=text name=text required maxlength=500 placeholder=\"Messages in #random are noise\"></label><button class=primary>Add rule</button></form>",
    );

    s.push_str(
        "<h2>Recent triage</h2><table><tr><th>When</th><th>Decision</th><th>Messages</th></tr>",
    );
    for (at, refs, decision, note) in &triage {
        let items: String = refs
            .iter()
            .take(10)
            .map(|r| format!("<li>{}</li>", source(r)))
            .collect();
        let more = if refs.len() > 10 {
            format!("<li class=muted>and {} more</li>", refs.len() - 10)
        } else {
            String::new()
        };
        s.push_str(&format!(
            "<tr><td class=muted>{}</td><td><span class=pill>{}</span><br>{}</td><td><ul>{items}{more}</ul></td></tr>",
            at.format("%m-%d %H:%M"),
            esc(decision),
            esc(note)
        ));
    }
    s.push_str("</table>");
    Ok(s)
}

async fn inbox_post(State(o): State<O>, h: HeaderMap, RawForm(body): RawForm) -> Response {
    require!(o, h);
    let f = form(&body);
    let r: Result<()> = async {
        match field(&f, "op").unwrap_or_default() {
            // The same actions as the Inbox buttons in Telegram.
            "todo" => {
                let id: i64 = field(&f, "id").unwrap_or_default().parse()?;
                let action = field(&f, "action").unwrap_or_default();
                anyhow::ensure!(matches!(action, "d" | "x" | "m" | "o"), "unknown action");
                secretary::button(&o, &format!("t|{id}|{action}")).await?;
            }
            "rule_remove" => {
                let id: i64 = field(&f, "id").unwrap_or_default().parse()?;
                secretary::button(&o, &format!("r|{id}|u")).await?;
            }
            "rule_add" => {
                secretary::owner_rule(&o, field(&f, "text").unwrap_or_default()).await?;
            }
            other => anyhow::bail!("unknown action {other:?}"),
        }
        Ok(())
    }
    .await;
    match r {
        Ok(()) => Redirect::to("/inbox").into_response(),
        Err(e) => err_page(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_does_not_pass_html_through() {
        let out = markdown("# Plan\n<script>alert(1)</script>\n- a <b>x</b>");
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(out.contains("<h1>"));
        let links = markdown(
            "[a](javascript:alert(1)) [b](https://ok.example) ![c](http://img.example/x.png) [d](JaVaScRiPt:x)",
        );
        assert!(!links.to_lowercase().contains("javascript"), "{links}");
        assert!(
            links.contains("https://ok.example") && !links.contains("<img"),
            "{links}"
        );
    }

    #[test]
    fn transcript_lines() {
        let l = transcript_line(
            "out",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
        );
        assert!(l.contains("hi") && l.contains("Bash"));
        assert!(transcript_line("out", r#"{"type":"user"}"#).is_empty());
        let n = transcript_line(
            "notice",
            "⏳ Waiting for <b>a&amp;b</b>: run <code>map &lt;dir&gt;</code>",
        );
        assert!(
            n.contains("told the owner: ⏳ Waiting for a&amp;b: run map &lt;dir&gt;"),
            "{n}"
        );
    }
}
