//! Serving the web UI: authentication, the embedded single-page app, and nothing else.
//!
//! There are no passwords. The owner logs in through single-use magic links that only the
//! orchestrator bot hands out (in Telegram, which already authenticates them); a link sets a
//! `SameSite=Strict` session cookie, which also keeps another site from driving the API in the
//! owner's browser.
//!
//! A link can point at **any page**, which is the point: the orchestrator can send the owner a
//! link straight to the task, the session list or the approval it is talking about. The target
//! is checked by [`safe_target`] first, because a redirect target that came from an agent is
//! attacker-adjacent input.
//!
//! The pages themselves live in `apps/web` and are compiled into this binary; everything they
//! read or change goes through [`crate::api`].

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde_json::json;

use crate::{core::Orchestrator, human, model::HumanItemRow};

type O = Arc<Orchestrator>;
const COOKIE: &str = "fleet_session";

/// The built UI. `apps/web/dist` is produced by `npm run build` there; `build.rs` leaves a
/// placeholder page if it is missing, so a Rust-only build needs no node.
#[derive(rust_embed::Embed)]
// Relative to this crate's root, which is where rust-embed resolves it from.
#[folder = "../../apps/web/dist"]
struct Assets;

pub fn router(o: O) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/login/{token}", get(login))
        .merge(crate::api::router())
        // Everything else is the app: its own files by name, and index.html for any route it
        // owns, so a deep link the orchestrator sent lands on the right page after a reload.
        .fallback(get(spa))
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

/// Is this a path on our own site, and only a path?
///
/// `starts_with('/')` is not enough: `//evil.example` is a protocol-relative URL that a browser
/// follows off-site, and a `\` is treated as `/` by some browsers. Since an agent may now choose
/// where a link points, this is checked rather than trusted.
pub(crate) fn safe_target(target: &str) -> Option<String> {
    let t = target.trim();
    let ok = t.starts_with('/')
        && !t.starts_with("//")
        && !t.starts_with("/\\")
        && !t.contains('\\')
        && !t.contains(['\n', '\r', '\t'])
        // A path, a query and a fragment are all the app needs; anything else is not a page.
        && !t.contains("://");
    ok.then(|| t.to_owned())
}

/// A single-use login link that lands on `target`, which may be any page of the UI
/// (`/tasks/bioins-7`, `/sessions?project=dap`, `/items/12`).
pub async fn magic_link(o: &Orchestrator, target: &str) -> Result<String> {
    let target = safe_target(target)
        .with_context(|| format!("{target:?} is not a page of the fleet's own UI"))?;
    let token = random_token();
    sqlx::query("insert into magic_links (token, target, expires_at) values ($1, $2, now() + make_interval(mins => $3::int))")
        .bind(&token)
        .bind(&target)
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
        return expired();
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
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not start a session",
        )
            .into_response();
    }
    // Checked again on the way out: the row was written before `safe_target` existed, or by a
    // version of this code that allowed more.
    let target = safe_target(&target).unwrap_or_else(|| "/".into());
    let cookie =
        format!("{COOKIE}={session}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=2592000");
    ([(header::SET_COOKIE, cookie)], Redirect::to(&target)).into_response()
}

fn expired() -> Response {
    (
        StatusCode::FORBIDDEN,
        Html(
            r#"<!doctype html><meta charset="utf-8"><title>Link expired · Fleet</title>
<body style="font:15px/1.6 system-ui,sans-serif;margin:3rem auto;max-width:30rem;padding:0 1rem">
<h1>Link expired</h1><p>This link was already used or has expired. Send <code>/web</code> to the
orchestrator bot for a new one.</p>"#,
        ),
    )
        .into_response()
}

pub(crate) async fn authed(o: &Orchestrator, headers: &HeaderMap) -> bool {
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

// ---------------------------------------------------------------- the app

/// One of the app's files, or the app itself.
///
/// Unknown paths get `index.html` with a 200, which is what a single-page app needs: the router
/// in the browser decides what `/tasks/bioins-7` means. No authentication here — the app is the
/// same for everyone and holds nothing; every byte of data behind it needs the cookie.
async fn spa(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // `rust-embed` resolves names relative to the folder, and refuses to escape it, so a
    // traversal attempt simply does not match a file.
    if let Some(file) = Assets::get(path).filter(|_| !path.is_empty()) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                // Vite fingerprints asset names, so they can be cached hard; index.html below
                // must not be, or a deploy would not be picked up.
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            file.data,
        )
            .into_response();
    }
    match Assets::get("index.html") {
        Some(index) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            index.data,
        )
            .into_response(),
        // Only if the binary was built without the UI *and* without the placeholder.
        None => (
            StatusCode::NOT_FOUND,
            "the UI is not built into this binary",
        )
            .into_response(),
    }
}

async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            // Everything is served from here, so every source is 'self' and nothing may be
            // fetched from elsewhere. `style-src` needs 'unsafe-inline' because the UI's
            // components set style attributes; scripts do not, so they stay strict.
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
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

// ---------------------------------------------------------------- shared with the API

/// One line describing what an item is about, for a list or a Telegram card.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_target_is_a_page_of_this_site_and_nothing_else() {
        // The pages the orchestrator will want to send the owner to.
        for ok in [
            "/",
            "/tasks/bioins-7",
            "/sessions?project=dap",
            "/items/12",
            "/projects/bioins",
            "/tasks?status=failed&q=a%20b",
        ] {
            assert_eq!(safe_target(ok).as_deref(), Some(ok), "{ok}");
        }
        // Anything that could leave the site. `//host` is the one that reads like a path and is
        // not: browsers follow it off-site.
        for bad in [
            "//evil.example",
            "https://evil.example",
            "http://evil.example/x",
            "/\\evil.example",
            "/tasks\\..\\x",
            "javascript:alert(1)",
            "tasks/7",
            "",
            " ",
            "/x\nLocation: https://evil.example",
        ] {
            assert_eq!(safe_target(bad), None, "{bad:?} must be refused");
        }
        // Surrounding whitespace is trimmed, not a reason to refuse.
        assert_eq!(safe_target("  /inbox  ").as_deref(), Some("/inbox"));
    }

    /// What replaced HTML escaping when the server stopped rendering pages.
    ///
    /// Everything the UI shows — a Slack message, a Jira description, an agent's own output —
    /// is written by someone who is not the owner. React puts strings in text nodes, so they
    /// are characters and not markup, *unless* someone reaches for `dangerouslySetInnerHTML`.
    /// Nothing in the UI may, and this is the check that keeps it that way.
    #[test]
    fn the_ui_never_sets_html_directly() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/src");
        let Ok(entries) = std::fs::read_dir(&src) else {
            // A published build without the UI sources: nothing to check.
            return;
        };
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut dirs: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        while let Some(p) = dirs.pop() {
            if p.is_dir() {
                dirs.extend(
                    std::fs::read_dir(&p)
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                        .map(|e| e.path()),
                );
            } else {
                files.push(p);
            }
        }
        assert!(!files.is_empty(), "the UI sources were found but are empty");
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            assert!(
                !text.contains("dangerouslySetInnerHTML"),
                "{} sets HTML directly; untrusted text must stay text",
                f.display()
            );
        }
    }

    #[test]
    fn the_app_is_served_for_its_own_routes() {
        // A deep link must come back as the app, not a 404, or a link to a task would break on
        // reload. The placeholder counts: what matters is that index.html answers.
        assert!(
            Assets::get("index.html").is_some(),
            "index.html is embedded"
        );
    }
}
