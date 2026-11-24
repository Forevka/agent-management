//! Every third-party access the fleet has, in one place: Slack, Google Chat, each Jira account,
//! Telegram, Hindsight, the projects' repositories and the kit's MCP servers — what each is
//! connected as, whether it works right now, and the credential it uses, which the owner sets
//! here without ever seeing it again.
//!
//! **Live status** is kept in memory by whatever talks to the service (the feeds as they connect
//! and drop, the Telegram poll, each Hindsight call), so it says how things stand since this
//! orchestrator started; what lasts (a feed's last message, a Jira account's last read) comes
//! from the database. **Credentials** are the secret folders the integrations read by name
//! (`slack`, `gchat`, each Jira folder): only the keys an integration uses can be set from here,
//! values are write-only (never returned, audited without the value), and Telegram and
//! Hindsight, configured in `deploy/.env`, are only shown. A Test makes one small call with the
//! stored credential and says what the service answered, never the credential.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::{admin, chat::clip, core::Orchestrator, feeds, jira};

/// How a service stands, as the last thing that talked to it found it.
#[derive(Debug, Clone)]
pub struct Status {
    /// connected, running, disconnected, stopped, ok, error
    pub state: &'static str,
    pub detail: Option<String>,
    pub since: DateTime<Utc>,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<DateTime<Utc>>,
}

static REGISTRY: LazyLock<RwLock<HashMap<String, Status>>> = LazyLock::new(Default::default);

/// Records how a service stands now. A state that does not change keeps its `since`. Never held
/// across an await: it is a plain lock around a map.
pub fn report(key: &str, state: &'static str, detail: Option<String>) {
    let now = Utc::now();
    let bad = matches!(state, "disconnected" | "stopped" | "error");
    let Ok(mut map) = REGISTRY.write() else {
        return;
    };
    let prev = map.get(key).cloned();
    let since = match &prev {
        Some(p) if p.state == state => p.since,
        _ => now,
    };
    map.insert(
        key.to_owned(),
        Status {
            state,
            detail: detail.map(|d| clip(&d, 300)),
            since,
            last_ok: if bad {
                prev.as_ref().and_then(|p| p.last_ok)
            } else {
                Some(now)
            },
            last_error: if bad {
                Some(now)
            } else {
                prev.as_ref().and_then(|p| p.last_error)
            },
        },
    );
}

pub fn status(key: &str) -> Option<Status> {
    REGISTRY.read().ok()?.get(key).cloned()
}

fn status_json(s: Option<Status>, fallback: &str) -> Value {
    match s {
        Some(s) => json!({"state": s.state, "detail": s.detail, "since": s.since,
                          "last_ok": s.last_ok, "last_error": s.last_error}),
        None => json!({"state": fallback}),
    }
}

/// The keys an integration's folder holds, and which it needs.
fn preset_keys(preset: &str) -> &'static [&'static str] {
    admin::PRESETS
        .iter()
        .find(|(p, _)| *p == preset)
        .map(|(_, k)| *k)
        .unwrap_or(&[])
}

/// Which of `keys` the folder named `folder` has a value for (matched by name like
/// `Orchestrator::secret`, case aside), and the folder's id when it exists.
async fn keys_set(o: &Orchestrator, folder: &str) -> Result<(Option<i64>, Vec<String>)> {
    let id: Option<i64> = sqlx::query_scalar(
        "select id from secret_folders where lower(name) = lower($1)
         order by (name = $1) desc, id limit 1",
    )
    .bind(folder)
    .fetch_optional(&o.db)
    .await?;
    let Some(id) = id else {
        return Ok((None, vec![]));
    };
    let keys: Vec<String> =
        sqlx::query_scalar("select key from secret_items where folder_id = $1 order by key")
            .bind(id)
            .fetch_all(&o.db)
            .await?;
    Ok((Some(id), keys))
}

fn keys_json(all: &[&str], required: &[&str], set: &[String]) -> Value {
    Value::Array(
        all.iter()
            .map(|k| {
                json!({"key": k, "set": set.iter().any(|s| s == k), "required": required.contains(k)})
            })
            .collect(),
    )
}

/// One credential-backed integration: its folder, its preset, and every key it reads.
struct Editable {
    folder: String,
    preset: &'static str,
    keys: &'static [&'static str],
}

/// What `id` may have set on it from this page, if anything.
fn editable(id: &str) -> Option<Editable> {
    match id {
        "slack" => Some(Editable {
            folder: feeds::SLACK_FOLDER.into(),
            preset: "slack",
            keys: preset_keys("slack"),
        }),
        "gchat" => Some(Editable {
            folder: feeds::GCHAT_FOLDER.into(),
            preset: "gchat",
            keys: preset_keys("gchat"),
        }),
        _ => id.strip_prefix("jira:").map(|folder| Editable {
            folder: folder.to_owned(),
            preset: "jira",
            keys: preset_keys("jira"),
        }),
    }
}

/// The last message a feed delivered, which outlives a restart.
async fn last_message(o: &Orchestrator, source: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(
        sqlx::query_scalar("select max(created_at) from feed_messages where source = $1")
            .bind(source)
            .fetch_one(&o.db)
            .await?,
    )
}

/// The Integrations page: every integration with its category, its keys (never their values),
/// how it stands and what it is connected as.
pub async fn json(o: &Orchestrator) -> Result<Value> {
    let mut out = Vec::new();

    // Slack: Socket Mode with an app token, polling the mapped channels without one.
    let (_, slack_keys) = keys_set(o, feeds::SLACK_FOLDER).await?;
    let has = |keys: &[String], k: &str| keys.iter().any(|s| s == k);
    let channels: i64 =
        sqlx::query_scalar("select count(*) from feed_channels where source = 'slack'")
            .fetch_one(&o.db)
            .await?;
    let slack_configured = has(&slack_keys, feeds::SLACK_TOKEN_KEY);
    out.push(json!({
        "id": "slack", "kind": "slack", "category": "messaging", "name": "Slack",
        "description": "Every conversation you are in reaches the secretary; it drafts replies you approve.",
        "folder": feeds::SLACK_FOLDER, "editable": true,
        "keys": keys_json(preset_keys("slack"), &[feeds::SLACK_TOKEN_KEY], &slack_keys),
        "configured": slack_configured,
        "status": status_json(status("slack"), if slack_configured { "starting" } else { "not_configured" }),
        "meta": {
            "mode": if !slack_configured { None }
                    else if has(&slack_keys, feeds::SLACK_APP_TOKEN_KEY) { Some("Socket Mode: every conversation") }
                    else { Some("polling the mapped channels") },
            "channels": channels,
            "last_message": last_message(o, "slack").await?,
        },
    }));

    let (_, gchat_keys) = keys_set(o, feeds::GCHAT_FOLDER).await?;
    let gchat_configured = preset_keys("gchat").iter().all(|k| has(&gchat_keys, k));
    out.push(json!({
        "id": "gchat", "kind": "gchat", "category": "messaging", "name": "Google Chat",
        "description": "Your Google Chat spaces and direct messages reach the secretary, like Slack.",
        "folder": feeds::GCHAT_FOLDER, "editable": true,
        "keys": keys_json(preset_keys("gchat"), preset_keys("gchat"), &gchat_keys),
        "configured": gchat_configured,
        "status": status_json(status("gchat"), if gchat_configured { "starting" } else { "not_configured" }),
        "meta": {"last_message": last_message(o, "gchat").await?},
    }));

    // Each Jira account: every folder a project reads, the shared one, and any folder made
    // with the Jira preset.
    let folders: Vec<String> = sqlx::query_scalar(
        "select distinct f from (
            select coalesce(nullif(jira_folder, ''), $1) as f from projects where jira_key is not null
            union select name from secret_folders where preset = 'jira') x order by f",
    )
    .bind(jira::DEFAULT_FOLDER)
    .fetch_all(&o.db)
    .await?;
    let accounts = crate::jira_sync::accounts_json(o).await?;
    for folder in folders {
        let (_, keys) = keys_set(o, &folder).await?;
        let projects: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "select slug, name, jira_key, color from projects
             where jira_key is not null and lower(coalesce(nullif(jira_folder, ''), $2)) = lower($1)
             order by slug",
        )
        .bind(&folder)
        .bind(jira::DEFAULT_FOLDER)
        .fetch_all(&o.db)
        .await?;
        let acc = &accounts[&folder];
        let configured = preset_keys("jira").iter().all(|k| has(&keys, k));
        let state = if !configured {
            "not_configured"
        } else if acc.is_null() {
            "unused"
        } else if acc["ok"].as_bool() == Some(true) {
            "ok"
        } else {
            "error"
        };
        out.push(json!({
            "id": format!("jira:{folder}"), "kind": "jira", "category": "trackers",
            "name": acc["site"].as_str().map(|s| format!("Jira · {s}")).unwrap_or_else(|| format!("Jira · {folder}")),
            "description": "Tickets of the projects below, read into the fleet's copy for the Jira page and the roadmap.",
            "folder": folder, "editable": true,
            "keys": keys_json(preset_keys("jira"), preset_keys("jira"), &keys),
            "configured": configured,
            "status": {"state": state, "detail": acc["error"], "last_ok": acc["ok_at"], "since": acc["at"]},
            "meta": {
                "site": acc["site"], "account": acc["account"], "last_read": acc["at"],
                "projects": projects.iter().map(|p| json!({
                    "slug": p.0, "name": p.1, "key": p.2,
                    "color": admin::project_color(&p.0, p.3.as_deref()),
                })).collect::<Vec<_>>(),
            },
        }));
    }

    // Configured in deploy/.env: shown, never edited here.
    out.push(json!({
        "id": "telegram", "kind": "telegram", "category": "messaging", "name": "Telegram",
        "description": "How you command the fleet: the forum group, its topics and the agents' bots. Set in deploy/.env.",
        "editable": false, "configured": true, "keys": [],
        "status": status_json(status("telegram"), "starting"),
        "meta": {"bots": sqlx::query_scalar::<_, i64>("select count(*) from bots").fetch_one(&o.db).await?},
    }));
    out.push(json!({
        "id": "hindsight", "kind": "hindsight", "category": "memory", "name": "Hindsight",
        "description": "The fleet's long-term memory: what agents retain and recall. Set in deploy/.env.",
        "editable": false, "configured": o.memory.is_some(), "keys": [],
        "status": status_json(status("hindsight"), if o.memory.is_some() { "idle" } else { "not_configured" }),
        "meta": {},
    }));

    // Where each project's code lives, and how its clones stand on the nodes.
    let repos: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "select slug, name, color, repo_url from projects where repo_url is not null order by slug",
    )
    .fetch_all(&o.db)
    .await?;
    for (slug, name, color, url) in repos {
        let nodes = crate::node_projects::project_json(o, &slug).await?;
        let failed = nodes.iter().any(|n| n["status"] == "failed");
        out.push(json!({
            "id": format!("repo:{slug}"), "kind": "repo", "category": "code",
            "name": format!("{name} repository"),
            "description": url,
            "editable": false, "configured": true, "keys": [],
            "status": {"state": if failed { "error" } else { "ok" }},
            "meta": {"project": slug, "color": admin::project_color(&slug, color.as_deref()),
                     "nodes": nodes, "link": format!("/projects/{slug}/settings")},
        }));
    }

    // The kit's MCP servers: their own page holds their config.
    let mcps: Vec<(i64, String, bool)> =
        sqlx::query_as("select id, name, enabled from kit_items where kind = 'mcp' order by name")
            .fetch_all(&o.db)
            .await?;
    for (id, name, enabled) in mcps {
        let states: Vec<String> =
            sqlx::query_scalar("select state from node_kit where item_id = $1")
                .bind(id)
                .fetch_all(&o.db)
                .await?;
        let bad = states.iter().any(|s| s == "failed" || s == "conflict");
        out.push(json!({
            "id": format!("mcp:{id}"), "kind": "mcp", "category": "mcp", "name": name,
            "description": "An MCP server from your kit, given to every worker session.",
            "editable": false, "configured": true, "keys": [],
            "status": {"state": if !enabled { "off" } else if bad { "error" } else { "ok" }},
            "meta": {"link": format!("/kit/{id}"), "nodes": states.len(),
                     "installed": states.iter().filter(|s| *s == "installed").count()},
        }));
    }
    Ok(json!({"integrations": out}))
}

/// Sets one key of an integration's credential, making its folder (with the right preset) when
/// there is none. Only the keys that integration reads.
pub async fn set_key(o: &Orchestrator, id: &str, key: &str, value: &str) -> Result<String> {
    let e = editable(id).with_context(|| format!("{id} is not set from this page"))?;
    ensure!(
        e.keys.contains(&key),
        "{id} reads {}, not {key}",
        e.keys.join(", ")
    );
    ensure!(!value.trim().is_empty(), "the value is empty");
    let folder = match keys_set(o, &e.folder).await?.0 {
        Some(id) => id,
        None => admin::create_folder(o, admin::Actor::Owner, &e.folder, e.preset).await?,
    };
    admin::set_secret(o, folder, key, value).await?;
    Ok(format!("{key} is set"))
}

pub async fn unset_key(o: &Arc<Orchestrator>, id: &str, key: &str) -> Result<String> {
    let e = editable(id).with_context(|| format!("{id} is not set from this page"))?;
    ensure!(e.keys.contains(&key), "{id} does not read {key}");
    let Some(folder) = keys_set(o, &e.folder).await?.0 else {
        return Ok(format!("{key} was not set"));
    };
    admin::execute(
        o,
        admin::Actor::Owner,
        &admin::AdminAction::SecretUnset {
            folder: admin::Ref::Id(folder),
            key: key.to_owned(),
        },
    )
    .await?;
    Ok(format!("{key} is removed"))
}

static LAST_TEST: LazyLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
    LazyLock::new(Default::default);

/// One small call with the stored credential: who the service says we are. At most one per
/// integration every few seconds.
pub async fn test(o: &Orchestrator, id: &str) -> Result<Value> {
    {
        let mut last = LAST_TEST.lock().map_err(|_| anyhow::anyhow!("busy"))?;
        if last
            .get(id)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            bail!("tested a moment ago; try again in a few seconds");
        }
        last.insert(id.to_owned(), std::time::Instant::now());
    }
    let http = {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?
    };
    let secret = |folder: &'static str, key: &'static str| async move {
        o.secret(folder, key)
            .await?
            .with_context(|| format!("{key} is not set"))
    };
    match id {
        "slack" => {
            let user = secret(feeds::SLACK_FOLDER, feeds::SLACK_TOKEN_KEY).await?;
            let me: Value = http
                .post("https://slack.com/api/auth.test")
                .bearer_auth(&user)
                .send()
                .await
                .context("Slack could not be reached")?
                .json()
                .await?;
            ensure!(
                me["ok"].as_bool() == Some(true),
                "Slack refused the user token: {}",
                me["error"].as_str().unwrap_or("no reason given")
            );
            let mut said = format!(
                "Signed in as {} in {}",
                me["user"].as_str().unwrap_or("?"),
                me["team"].as_str().unwrap_or("?")
            );
            if let Some(app) = o
                .secret(feeds::SLACK_FOLDER, feeds::SLACK_APP_TOKEN_KEY)
                .await?
            {
                let open: Value = http
                    .post("https://slack.com/api/apps.connections.open")
                    .bearer_auth(&app)
                    .send()
                    .await
                    .context("Slack could not be reached")?
                    .json()
                    .await?;
                ensure!(
                    open["ok"].as_bool() == Some(true),
                    "Slack refused the app token: {}",
                    open["error"].as_str().unwrap_or("no reason given")
                );
                said.push_str("; the app token opens Socket Mode");
            }
            Ok(json!({"ok": true, "detail": said}))
        }
        "gchat" => {
            let token: Value = http
                .post("https://oauth2.googleapis.com/token")
                .form(&[
                    (
                        "client_id",
                        secret(feeds::GCHAT_FOLDER, feeds::GCHAT_CLIENT_ID_KEY).await?,
                    ),
                    (
                        "client_secret",
                        secret(feeds::GCHAT_FOLDER, feeds::GCHAT_CLIENT_SECRET_KEY).await?,
                    ),
                    (
                        "refresh_token",
                        secret(feeds::GCHAT_FOLDER, feeds::GCHAT_REFRESH_TOKEN_KEY).await?,
                    ),
                    ("grant_type", "refresh_token".to_owned()),
                ])
                .send()
                .await
                .context("Google could not be reached")?
                .json()
                .await?;
            let access = token["access_token"].as_str().with_context(|| {
                format!(
                    "Google refused the credential: {}",
                    token["error_description"]
                        .as_str()
                        .or(token["error"].as_str())
                        .unwrap_or("no reason given")
                )
            })?;
            let resp = http
                .get("https://chat.googleapis.com/v1/spaces?pageSize=1")
                .bearer_auth(access)
                .send()
                .await
                .context("Google Chat could not be reached")?;
            ensure!(
                resp.status().is_success(),
                "Google Chat answered {}",
                resp.status().as_u16()
            );
            Ok(json!({"ok": true, "detail": "The credential works and Google Chat answers"}))
        }
        _ => {
            let folder = id
                .strip_prefix("jira:")
                .with_context(|| format!("{id} has no test"))?;
            let j = jira::for_folder(o, folder).await?;
            let me = j.myself().await?;
            Ok(
                json!({"ok": true, "detail": format!("Signed in to {} as {}", j.site(), me.display_name)}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_integrations_own_keys_are_set_here() {
        let slack = editable("slack").unwrap();
        assert!(slack.keys.contains(&"SLACK_APP_TOKEN"));
        assert!(!slack.keys.contains(&"JIRA_API_TOKEN"));
        assert_eq!(editable("jira:Jira-bioins").unwrap().folder, "Jira-bioins");
        assert!(editable("telegram").is_none(), "set in deploy/.env");
        assert!(editable("hindsight").is_none());
    }

    #[test]
    fn a_status_keeps_when_it_began_and_when_it_last_worked() {
        report("test-svc", "connected", None);
        let first = status("test-svc").unwrap();
        report("test-svc", "connected", None);
        let second = status("test-svc").unwrap();
        assert_eq!(
            second.since, first.since,
            "still connected since the first report"
        );
        report("test-svc", "disconnected", Some("x".repeat(1000)));
        let s = status("test-svc").unwrap();
        assert_eq!(s.state, "disconnected");
        assert_eq!(s.last_ok, second.last_ok, "the last time it worked is kept");
        assert!(s.last_error.is_some());
        assert!(s.detail.unwrap().chars().count() <= 301, "clipped");
    }
}
