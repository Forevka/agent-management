//! The kit: the owner's Claude Code skills, plugins and MCP servers, kept here, installed on
//! every node, and the allowlist of MCP servers for agent sessions.
//!
//! - **Owner only.** Every change here takes an [`Actor`] and refuses anything but
//!   [`Actor::Owner`], signed in on the web. None of it is an [`crate::admin::AdminAction`],
//!   which is the surface the brain proposes from: what runs in every agent session on every
//!   machine is not something an agent may suggest, even for the owner's approval.
//! - **An MCP server's config is a secret, whole.** Secrets take too many shapes in a config (an
//!   env value, a header in an argument, a token as a URL's user, a DSN, JSON inside an argument)
//!   for any rule to find them all, so none is attempted for storage: the config is sealed with
//!   the [`crate::vault::Vault`] into `kit_items.config`, and `kit_items.spec` holds only
//!   [`fleet_proto::kit::config_summary`] (its type, its command's name, its endpoint's host). It
//!   is opened only to go to a node over the authenticated link, in the [`Kit`] and in an agent's
//!   spawn, and to be shown to the owner *masked* ([`fleet_proto::kit::mask_config`]). Nothing of
//!   it reaches a log, the audit log or a status.
//! - **The import** asks a node what it has ([`capture`]); the answer, raw configs included,
//!   stays in memory only, under an id, until the owner imports from it or it expires.
//! - **Installing** is the node's (`fleet_node::kit`); this side sends the desired state to
//!   every node that is current ([`fleet_proto::node_is_current`]) on connect and after every
//!   change, and records what each reports in `node_kit`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use fleet_net::EndpointId;
use fleet_proto::{
    Body, Kit, KitCapture, KitCaptureRequest, KitContent, KitFile, KitItemStatus, KitMcp,
    KitPlugin, KitSkill, KitSkipped,
    kit::{
        MASK, MAX_SKILL_BYTES, MAX_SKILL_FILE_BYTES, MAX_SKILL_FILES, PART_BUDGET, Parts,
        clean_source, config_summary, mask_config, safe_skill_path, valid_name,
    },
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{admin::Actor, core::Orchestrator};

/// How long the owner waits for a node to report what it has.
const CAPTURE_WAIT: Duration = Duration::from_secs(60);
/// How long a finished capture (with its raw configs) is kept for the owner to import from.
const CAPTURE_KEEP: Duration = Duration::from_secs(30 * 60);
const KINDS: [&str; 3] = ["skill", "plugin", "mcp"];
/// The longest MCP server config the kit takes: far more than any real one, and small enough
/// that a kit of them always fits its messages.
const MAX_MCP_CONFIG_BYTES: usize = 64 * 1024;

/// Captures asked for and not yet imported. In memory only: they hold raw configs.
#[derive(Default)]
pub struct Captures {
    pending: HashMap<String, Pending>,
    ready: HashMap<String, Ready>,
}

struct Pending {
    node: String,
    parts: Parts,
    skipped: Vec<KitSkipped>,
    done: Option<oneshot::Sender<()>>,
}

struct Ready {
    node: String,
    at: Instant,
    content: KitContent,
    skipped: Vec<KitSkipped>,
}

fn owner_only(actor: Actor) -> Result<()> {
    ensure!(
        actor == Actor::Owner,
        "the kit is changed by the owner on the web only"
    );
    Ok(())
}

// ---------------------------------------------------------------- MCP configs, sealed

fn check_mcp(name: &str, config: &Value) -> Result<()> {
    ensure!(valid_name(name), "{name:?} is not a usable MCP server name");
    ensure!(
        config.is_object(),
        "an MCP server's config is a JSON object"
    );
    ensure!(
        config.to_string().len() <= MAX_MCP_CONFIG_BYTES,
        "an MCP server's config is at most {} KB",
        MAX_MCP_CONFIG_BYTES / 1024
    );
    Ok(())
}

/// Stores an MCP server: the whole config sealed, and only its summary in the clear.
async fn store_mcp(o: &Orchestrator, name: &str, config: &Value) -> Result<()> {
    check_mcp(name, config)?;
    sqlx::query(
        "insert into kit_items (kind, name, spec, config) values ('mcp', $1, $2, $3)
         on conflict (kind, name) do update set spec = $2, config = $3, updated_at = now()",
    )
    .bind(name)
    .bind(sqlx::types::Json(config_summary(config)))
    .bind(o.vault.encrypt(&config.to_string()))
    .execute(&o.db)
    .await?;
    Ok(())
}

/// An MCP item's sealed config, opened.
fn open_config(o: &Orchestrator, sealed: Option<&[u8]>) -> Result<Value> {
    let sealed = sealed.context("the server has no stored config")?;
    let text = o
        .vault
        .decrypt(sealed)
        .context("its config cannot be decrypted (was FLEET_MASTER_KEY changed?)")?;
    Ok(serde_json::from_str(&text)?)
}

/// The config the owner saved in the editor, which shows the current one masked.
///
/// A masked value is kept only when nothing else changed: the saved config is the masked view
/// exactly, so it is the current config. Any change at all, outside the masked values or by
/// typing over one of them, keeps no secret, and every `[masked]` must be typed again (or the
/// whole config pasted): a server whose package, host, URL or any other setting changed may send
/// its secrets somewhere else, and a value typed over its mask may be the host they go to. So no
/// edit can redirect the existing secrets unseen, and a masked view is never saved over one.
pub fn merge_masked(new: &Value, old: &Value) -> Result<Value> {
    fn has_mask(v: &Value) -> bool {
        match v {
            Value::String(s) => s.contains(MASK),
            Value::Array(a) => a.iter().any(has_mask),
            Value::Object(m) => m.values().any(has_mask),
            _ => false,
        }
    }
    if !has_mask(new) {
        return Ok(new.clone());
    }
    ensure!(
        *new == mask_config(old).0,
        "the server changed, so no secret is kept: type every {MASK} value again, or paste the whole config"
    );
    Ok(old.clone())
}

// ---------------------------------------------------------------- skills and plugins

/// Checks a skill's files: safe paths, a `SKILL.md`, text only, and the size caps the nodes
/// apply too.
pub fn check_skill(name: &str, files: &BTreeMap<String, String>) -> Result<()> {
    ensure!(valid_name(name), "{name:?} is not a usable skill name");
    ensure!(files.contains_key("SKILL.md"), "a skill needs a SKILL.md");
    ensure!(
        files.len() <= MAX_SKILL_FILES,
        "a skill has at most {MAX_SKILL_FILES} files"
    );
    let mut total = 0;
    for (path, text) in files {
        ensure!(safe_skill_path(path), "{path:?} is not a safe file path");
        ensure!(
            !fleet_proto::kit::secret_file(path),
            "{path} looks like a secret file; secrets belong in the vault, not in a skill"
        );
        ensure!(!text.contains('\0'), "{path} is not text");
        ensure!(
            text.len() <= MAX_SKILL_FILE_BYTES,
            "{path} is larger than {} KB",
            MAX_SKILL_FILE_BYTES / 1024
        );
        total += text.len();
    }
    ensure!(
        total <= MAX_SKILL_BYTES,
        "the skill is larger than {} MB",
        MAX_SKILL_BYTES / (1024 * 1024)
    );
    Ok(())
}

async fn store_skill(o: &Orchestrator, name: &str, files: &BTreeMap<String, String>) -> Result<()> {
    check_skill(name, files)?;
    sqlx::query(
        "insert into kit_items (kind, name, files) values ('skill', $1, $2)
         on conflict (kind, name) do update set files = $2, updated_at = now()",
    )
    .bind(name)
    .bind(sqlx::types::Json(files))
    .execute(&o.db)
    .await?;
    Ok(())
}

fn check_plugin(p: &KitPlugin) -> Result<()> {
    ensure!(
        valid_name(&p.plugin) && valid_name(&p.marketplace),
        "a plugin and its marketplace are named with letters, digits, '-', '_' and '.'"
    );
    ensure!(
        clean_source(&p.marketplace_source),
        "the marketplace source is `owner/repo`, a URL or a path, with no user, password, token or query in it; give the node access another way (a git credential helper, an SSH key)"
    );
    Ok(())
}

async fn store_plugin(o: &Orchestrator, p: &KitPlugin) -> Result<()> {
    check_plugin(p)?;
    sqlx::query(
        "insert into kit_items (kind, name, spec) values ('plugin', $1, $2)
         on conflict (kind, name) do update set spec = $2, updated_at = now()",
    )
    .bind(p.id())
    .bind(sqlx::types::Json(json!({
        "plugin": p.plugin,
        "marketplace": p.marketplace,
        "marketplace_source": p.marketplace_source.trim(),
    })))
    .execute(&o.db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- changes (owner only)

/// A skill, created or replaced with these files.
pub async fn save_skill(
    o: &Arc<Orchestrator>,
    actor: Actor,
    name: &str,
    files: &BTreeMap<String, String>,
) -> Result<String> {
    owner_only(actor)?;
    store_skill(o, name, files).await?;
    o.audit(
        actor.audit_name(),
        "kit.skill.save",
        json!({"skill": name, "files": files.keys().collect::<Vec<_>>()}),
    )
    .await;
    changed(o).await;
    Ok(format!("skill {name} saved"))
}

/// An MCP server, created or replaced with this config. For a server already in the kit, values
/// left masked keep their current ones ([`merge_masked`]).
pub async fn save_mcp(
    o: &Arc<Orchestrator>,
    actor: Actor,
    name: &str,
    config: Value,
) -> Result<String> {
    owner_only(actor)?;
    let current: Option<Option<Vec<u8>>> =
        sqlx::query_scalar("select config from kit_items where kind = 'mcp' and name = $1")
            .bind(name)
            .fetch_optional(&o.db)
            .await?;
    let config = match current {
        Some(sealed) => merge_masked(&config, &open_config(o, sealed.as_deref())?)?,
        None => {
            ensure!(
                !config.to_string().contains(MASK),
                "the config still reads {MASK}: type the values"
            );
            config
        }
    };
    store_mcp(o, name, &config).await?;
    o.audit(actor.audit_name(), "kit.mcp.save", json!({"mcp": name}))
        .await;
    changed(o).await;
    Ok(format!("MCP server {name} saved"))
}

pub async fn add_plugin(o: &Arc<Orchestrator>, actor: Actor, p: &KitPlugin) -> Result<String> {
    owner_only(actor)?;
    store_plugin(o, p).await?;
    o.audit(
        actor.audit_name(),
        "kit.plugin.save",
        json!({"plugin": p.id()}),
    )
    .await;
    changed(o).await;
    Ok(format!("plugin {} added", p.id()))
}

pub async fn set_enabled(
    o: &Arc<Orchestrator>,
    actor: Actor,
    id: i64,
    enabled: bool,
) -> Result<String> {
    owner_only(actor)?;
    let row: Option<(String, String)> = sqlx::query_as(
        "update kit_items set enabled = $2, updated_at = now() where id = $1 returning kind, name",
    )
    .bind(id)
    .bind(enabled)
    .fetch_optional(&o.db)
    .await?;
    let (kind, name) = row.context("no such kit item")?;
    o.audit(
        actor.audit_name(),
        if enabled { "kit.enable" } else { "kit.disable" },
        json!({"kind": kind, "name": name}),
    )
    .await;
    changed(o).await;
    Ok(format!(
        "{kind} {name} {}",
        if enabled { "enabled" } else { "disabled" }
    ))
}

/// Deletes an item, its sealed config with it. The nodes remove what they installed of it when
/// the new kit reaches them.
pub async fn delete(o: &Arc<Orchestrator>, actor: Actor, id: i64) -> Result<String> {
    owner_only(actor)?;
    let row: Option<(String, String)> =
        sqlx::query_as("delete from kit_items where id = $1 returning kind, name")
            .bind(id)
            .fetch_optional(&o.db)
            .await?;
    let (kind, name) = row.context("no such kit item")?;
    o.audit(
        actor.audit_name(),
        "kit.delete",
        json!({"kind": kind, "name": name}),
    )
    .await;
    changed(o).await;
    Ok(format!("{kind} {name} deleted"))
}

/// After every change: every node that is connected gets the new kit.
async fn changed(o: &Arc<Orchestrator>) {
    if let Err(e) = push_all(o).await {
        tracing::warn!("sending the kit to the nodes failed: {e:#}");
    }
}

// ---------------------------------------------------------------- import

/// Asks a node what it has and waits for the answer. Returns what the owner may choose from;
/// the raw capture stays here under its id (see [`import`]).
pub async fn capture(o: &Arc<Orchestrator>, actor: Actor, node_id: &str) -> Result<Value> {
    owner_only(actor)?;
    let node = o.node(node_id).await?.context("no such node")?;
    let peer: EndpointId = node.id.parse()?;
    ensure!(
        node.online && o.server.is_connected(peer),
        "{} is offline",
        node.name
    );
    ensure!(
        fleet_proto::node_is_current(&node.app_version),
        "{} runs {}; it can report its kit once it runs this release ({})",
        node.name,
        node.app_version,
        fleet_proto::BUILD_VERSION
    );
    let id = crate::web::random_token();
    let (tx, rx) = oneshot::channel();
    o.kit_captures.lock().unwrap().pending.insert(
        id.clone(),
        Pending {
            node: node.id.clone(),
            parts: Parts::default(),
            skipped: Vec::new(),
            done: Some(tx),
        },
    );
    o.server.send(
        peer,
        Body::KitCaptureRequest(KitCaptureRequest { id: id.clone() }),
    );
    let arrived = tokio::time::timeout(CAPTURE_WAIT, rx).await;
    if !matches!(arrived, Ok(Ok(()))) {
        o.kit_captures.lock().unwrap().pending.remove(&id);
        bail!("{} did not report what it has within a minute", node.name);
    }
    o.audit(actor.audit_name(), "kit.capture", json!({"node": node.id}))
        .await;
    candidates(o, &id).await
}

/// A part of a node's answer to [`capture`]. Only parts from the node that was asked count.
pub fn capture_part(o: &Orchestrator, peer: EndpointId, c: KitCapture) {
    let mut caps = o.kit_captures.lock().unwrap();
    let Some(p) = caps.pending.get_mut(&c.id) else {
        return;
    };
    if p.node != peer.to_string() {
        tracing::warn!(node = %peer.fmt_short(), "a kit capture from a node that was not asked");
        return;
    }
    p.skipped.extend(c.skipped);
    let Some(content) = p.parts.add(&c.id, c.part, c.last, c.content) else {
        return;
    };
    let mut p = caps.pending.remove(&c.id).expect("just seen");
    if let Some(done) = p.done.take() {
        let _ = done.send(());
    }
    caps.ready.insert(
        c.id,
        Ready {
            node: p.node,
            at: Instant::now(),
            content,
            skipped: p.skipped,
        },
    );
}

/// Drops captures nobody imported from, with their raw configs. Housekeeping calls it.
pub fn forget_stale(o: &Orchestrator) {
    o.kit_captures
        .lock()
        .unwrap()
        .ready
        .retain(|_, r| r.at.elapsed() < CAPTURE_KEEP);
}

async fn in_kit(o: &Orchestrator) -> Result<BTreeSet<(String, String)>> {
    Ok(
        sqlx::query_as::<_, (String, String)>("select kind, name from kit_items")
            .fetch_all(&o.db)
            .await?
            .into_iter()
            .collect(),
    )
}

/// What a capture offers. An MCP server's config only masked, a plugin with an unusable source
/// not at all (the node leaves those out).
async fn candidates(o: &Orchestrator, id: &str) -> Result<Value> {
    let have = in_kit(o).await?;
    let caps = o.kit_captures.lock().unwrap();
    let r = caps
        .ready
        .get(id)
        .context("that capture has expired; ask the node again")?;
    let has = |kind: &str, name: &str| have.contains(&(kind.to_owned(), name.to_owned()));
    Ok(json!({
        "capture": id,
        "node": r.node,
        "skills": r.content.skills.iter().map(|s| json!({
            "name": s.name,
            "in_kit": has("skill", &s.name),
            "files": s.files.iter().map(|f| json!({"path": f.path, "bytes": f.text.len()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "plugins": r.content.plugins.iter().filter(|p| clean_source(&p.marketplace_source)).map(|p| json!({
            "name": p.id(),
            "in_kit": has("plugin", &p.id()),
            "marketplace_source": p.marketplace_source,
        })).collect::<Vec<_>>(),
        "mcp": r.content.mcp.iter().map(|m| {
            let config: Value = serde_json::from_str(&m.config).unwrap_or(Value::Null);
            let (masked, found) = mask_config(&config);
            json!({
                "name": m.name,
                "in_kit": has("mcp", &m.name),
                "summary": config_summary(&config),
                "config": masked,
                "masked": found.len(),
            })
        }).collect::<Vec<_>>(),
        "skipped": r.skipped.iter().map(|s| json!({"kind": s.kind, "name": s.name, "reason": s.reason})).collect::<Vec<_>>(),
    }))
}

/// The items of a capture the owner chose, by name.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Selection {
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
}

/// Takes the chosen items of a capture into the kit (replacing items of the same name). The
/// capture is gone afterwards, raw configs and all.
pub async fn import(
    o: &Arc<Orchestrator>,
    actor: Actor,
    capture_id: &str,
    sel: &Selection,
) -> Result<String> {
    owner_only(actor)?;
    let r = o
        .kit_captures
        .lock()
        .unwrap()
        .ready
        .remove(capture_id)
        .context("that capture has expired; ask the node again")?;
    let pick = |names: &[String], name: &str| names.iter().any(|n| n == name);
    let skill_files = |s: &KitSkill| -> BTreeMap<String, String> {
        s.files
            .iter()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect()
    };
    // All of it checked before anything is stored, so a bad item leaves nothing half imported,
    // and the capture stays for another try.
    let mut configs = Vec::new();
    let checked = (|| -> Result<()> {
        for s in r
            .content
            .skills
            .iter()
            .filter(|s| pick(&sel.skills, &s.name))
        {
            check_skill(&s.name, &skill_files(s)).with_context(|| format!("skill {}", s.name))?;
        }
        for p in r
            .content
            .plugins
            .iter()
            .filter(|p| pick(&sel.plugins, &p.id()))
        {
            check_plugin(p).with_context(|| format!("plugin {}", p.id()))?;
        }
        for m in r.content.mcp.iter().filter(|m| pick(&sel.mcp, &m.name)) {
            let v: Value = serde_json::from_str(&m.config)
                .with_context(|| format!("the config of {} is not JSON", m.name))?;
            check_mcp(&m.name, &v).with_context(|| format!("MCP server {}", m.name))?;
            configs.push((m.name.clone(), v));
        }
        Ok(())
    })();
    if let Err(e) = checked {
        o.kit_captures
            .lock()
            .unwrap()
            .ready
            .insert(capture_id.to_owned(), r);
        return Err(e);
    }
    let mut done = Vec::new();
    for s in r
        .content
        .skills
        .iter()
        .filter(|s| pick(&sel.skills, &s.name))
    {
        store_skill(o, &s.name, &skill_files(s)).await?;
        done.push(format!("skill {}", s.name));
    }
    for p in r
        .content
        .plugins
        .iter()
        .filter(|p| pick(&sel.plugins, &p.id()))
    {
        store_plugin(o, p).await?;
        done.push(format!("plugin {}", p.id()));
    }
    for (name, config) in &configs {
        store_mcp(o, name, config).await?;
        done.push(format!("MCP server {name}"));
    }
    o.audit(
        actor.audit_name(),
        "kit.import",
        json!({"node": r.node, "items": done}),
    )
    .await;
    changed(o).await;
    Ok(if done.is_empty() {
        "nothing imported".into()
    } else {
        format!("imported {}", done.join(", "))
    })
}

// ---------------------------------------------------------------- to the nodes

/// An item as stored.
#[derive(Debug, sqlx::FromRow)]
struct ItemRow {
    id: i64,
    kind: String,
    name: String,
    enabled: bool,
    spec: sqlx::types::Json<Value>,
    files: Option<sqlx::types::Json<BTreeMap<String, String>>>,
    config: Option<Vec<u8>>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

const ITEM_COLUMNS: &str = "id, kind, name, enabled, spec, files, config, updated_at";

async fn items(o: &Orchestrator) -> Result<Vec<ItemRow>> {
    Ok(sqlx::query_as(&format!(
        "select {ITEM_COLUMNS} from kit_items order by kind, name"
    ))
    .fetch_all(&o.db)
    .await?)
}

/// The enabled kit with its configs opened, and the MCP items that could not be opened (with
/// why), which are left out.
async fn desired(o: &Orchestrator) -> Result<(KitContent, Vec<(i64, String)>)> {
    let mut kit = KitContent::default();
    let mut unopened = Vec::new();
    for it in items(o).await?.into_iter().filter(|i| i.enabled) {
        match it.kind.as_str() {
            "skill" => kit.skills.push(KitSkill {
                name: it.name,
                files: it
                    .files
                    .map(|f| f.0)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(path, text)| KitFile { path, text })
                    .collect(),
            }),
            "plugin" => {
                let s = &it.spec.0;
                let field = |k: &str| s[k].as_str().unwrap_or_default().to_owned();
                kit.plugins.push(KitPlugin {
                    plugin: field("plugin"),
                    marketplace: field("marketplace"),
                    marketplace_source: field("marketplace_source"),
                });
            }
            "mcp" => match open_config(o, it.config.as_deref()) {
                Ok(config) => kit.mcp.push(KitMcp {
                    name: it.name,
                    config: config.to_string(),
                }),
                Err(e) => unopened.push((it.id, format!("{e:#}"))),
            },
            _ => {}
        }
    }
    Ok((kit, unopened))
}

/// The kit's enabled MCP servers, as the JSON object a spawn carries in `extra_mcp_servers`;
/// `None` when there are none. A server whose config cannot be opened is left out (its install
/// status already says why).
///
/// Never fails a run: a kit it cannot read (a database hiccup, a config that no longer
/// decrypts) is logged and the session starts with the fleet's server alone.
pub async fn agent_servers(o: &Orchestrator) -> Option<String> {
    let read = async {
        let rows: Vec<(String, Option<Vec<u8>>)> = sqlx::query_as(
            "select name, config from kit_items where kind = 'mcp' and enabled order by name",
        )
        .fetch_all(&o.db)
        .await?;
        let mut servers = serde_json::Map::new();
        for (name, sealed) in rows {
            match open_config(o, sealed.as_deref()) {
                Ok(config) => {
                    servers.insert(name, config);
                }
                Err(e) => tracing::warn!("kit MCP server {name} left out of a session: {e:#}"),
            }
        }
        Ok::<_, anyhow::Error>((!servers.is_empty()).then(|| Value::Object(servers).to_string()))
    };
    read.await.unwrap_or_else(|e| {
        tracing::warn!("the kit's MCP servers are left out of a session: {e:#}");
        None
    })
}

/// Held while a kit is read and sent, so two pushes (a connect and a change, two browser tabs)
/// cannot cross: the one that read the older kit would otherwise be sent last, and the node would
/// keep it until the next change. Messages to one node go out in the order they are queued.
static PUSHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Sends the kit to one node (which must be current: see the caller) and marks what it is
/// waiting to hear about as pending.
pub async fn push(o: &Orchestrator, node_id: &str) -> Result<()> {
    let _one_at_a_time = PUSHING.lock().await;
    push_locked(o, node_id).await
}

async fn push_locked(o: &Orchestrator, node_id: &str) -> Result<()> {
    let peer: EndpointId = node_id.parse()?;
    let (kit, unopened) = desired(o).await?;
    // Only what the node is to have is waited on. A disabled item's row stays until the node's
    // report says what became of it (see [`statuses`]).
    sqlx::query(
        "insert into node_kit (node_id, item_id, state, detail, at)
         select $1, id, 'pending', '', now() from kit_items where enabled
         on conflict (node_id, item_id) do update set state = 'pending', detail = '', at = now()",
    )
    .bind(node_id)
    .execute(&o.db)
    .await?;
    for (id, why) in unopened {
        sqlx::query(
            "update node_kit set state = 'failed', detail = $3, at = now() where node_id = $1 and item_id = $2",
        )
        .bind(node_id)
        .bind(id)
        .bind(why)
        .execute(&o.db)
        .await?;
    }
    let revision = crate::web::random_token();
    let parts = kit.split(PART_BUDGET);
    let n = parts.len();
    let bodies: Vec<Body> = parts
        .into_iter()
        .enumerate()
        .map(|(i, content)| {
            Body::Kit(Kit {
                revision: revision.clone(),
                part: i as u32,
                last: i + 1 == n,
                content,
            })
        })
        .collect();
    // A frame the link cannot send would block everything queued behind it, so nothing is sent
    // unless all of it fits. The caps on skills and configs keep this from happening.
    if !bodies.iter().all(fleet_proto::kit::fits) {
        let why =
            "the kit does not fit the messages that carry it; make a skill or a config smaller";
        sqlx::query(
            "update node_kit set state = 'failed', detail = $2, at = now() where node_id = $1 and state = 'pending'",
        )
        .bind(node_id)
        .bind(why)
        .execute(&o.db)
        .await?;
        bail!("{why}");
    }
    for body in bodies {
        o.server.send(peer, body);
    }
    Ok(())
}

/// Sends the kit to every connected node that is current.
pub async fn push_all(o: &Orchestrator) -> Result<()> {
    let _one_at_a_time = PUSHING.lock().await;
    let nodes: Vec<(String, String)> =
        sqlx::query_as("select id, app_version from nodes where online")
            .fetch_all(&o.db)
            .await?;
    for (id, version) in nodes {
        let connected = id
            .parse::<EndpointId>()
            .is_ok_and(|p| o.server.is_connected(p));
        if connected
            && fleet_proto::node_is_current(&version)
            && let Err(e) = push_locked(o, &id).await
        {
            tracing::warn!(node = %id, "sending the kit failed: {e:#}");
        }
    }
    Ok(())
}

/// A node's report on the kit it was sent.
///
/// A report is the whole outcome of one apply. So a disabled item it does not mention is not on
/// the node at all (the kit never had it there, or removed it before), and its row goes; a
/// `removed` row stays, so the owner sees what was taken off. What the node says about itself
/// (`kind = "node"`: its CLI cannot enforce the allowlist, its record of what it installed was
/// unreadable) becomes the node's `kit_note`.
pub async fn statuses(o: &Orchestrator, peer: EndpointId, list: Vec<KitItemStatus>) -> Result<()> {
    let node = peer.to_string();
    let (kinds, names): (Vec<String>, Vec<String>) = list
        .iter()
        .map(|s| (s.kind.clone(), s.name.clone()))
        .unzip();
    sqlx::query(
        "delete from node_kit s using kit_items i
         where s.item_id = i.id and s.node_id = $1 and not i.enabled and s.state <> 'removed'
           and not exists (select 1 from unnest($2::text[], $3::text[]) as r(kind, name)
                           where r.kind = i.kind and r.name = i.name)",
    )
    .bind(&node)
    .bind(&kinds)
    .bind(&names)
    .execute(&o.db)
    .await?;
    let about_node: Vec<&KitItemStatus> = list.iter().filter(|s| s.kind == "node").collect();
    if !about_node.is_empty() {
        let notes: Vec<String> = about_node
            .iter()
            .filter(|s| s.state != "installed")
            .map(|s| s.detail.chars().take(500).collect())
            .collect();
        sqlx::query("update nodes set kit_note = $2 where id = $1")
            .bind(&node)
            .bind((!notes.is_empty()).then(|| notes.join("; ")))
            .execute(&o.db)
            .await?;
    }
    for s in &list {
        if !KINDS.contains(&s.kind.as_str()) {
            continue;
        }
        let state = match s.state.as_str() {
            st @ ("installed" | "failed" | "conflict" | "removed") => st,
            _ => continue,
        };
        let detail: String = s.detail.chars().take(1000).collect();
        sqlx::query(
            "insert into node_kit (node_id, item_id, state, detail, at)
             select $1, id, $4, $5, now() from kit_items where kind = $2 and name = $3
             on conflict (node_id, item_id) do update set
               -- An enabled item the node took off was left out of its kit: its config could not
               -- be opened. The row keeps saying so rather than a bare `removed`.
               state = case when $4 = 'removed' and node_kit.state = 'failed'
                             and exists (select 1 from kit_items k where k.id = node_kit.item_id and k.enabled)
                            then 'failed' else $4 end,
               detail = case when $4 = 'removed' and node_kit.state = 'failed'
                              and exists (select 1 from kit_items k where k.id = node_kit.item_id and k.enabled)
                             then node_kit.detail || '; taken off this node until then' else $5 end,
               at = now()",
        )
        .bind(&node)
        .bind(&s.kind)
        .bind(&s.name)
        .bind(state)
        .bind(detail)
        .execute(&o.db)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- the web UI's view

/// The kit page: items (an MCP server by its summary only), nodes and the install matrix. Skill
/// files are listed (path and size), not sent; [`item_json`] has them.
pub async fn json(o: &Orchestrator) -> Result<Value> {
    let items = items(o).await?;
    let nodes: Vec<(String, String, bool, String, Option<String>)> =
        sqlx::query_as("select id, name, online, app_version, kit_note from nodes order by name")
            .fetch_all(&o.db)
            .await?;
    let status: Vec<(String, i64, String, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("select node_id, item_id, state, detail, at from node_kit")
            .fetch_all(&o.db)
            .await?;
    Ok(json!({
        "items": items.iter().map(|i| json!({
            "id": i.id,
            "kind": i.kind,
            "name": i.name,
            "enabled": i.enabled,
            "spec": i.spec.0,
            "files": i.files.as_ref().map(|f| f.0.iter().map(|(p, t)| json!({"path": p, "bytes": t.len()})).collect::<Vec<_>>()),
            "updated_at": i.updated_at,
        })).collect::<Vec<_>>(),
        "nodes": nodes.iter().map(|(id, name, online, version, note)| json!({
            "id": id,
            "name": name,
            "online": online,
            "version": version,
            "current": fleet_proto::node_is_current(version),
            // Set when the node cannot hold its sessions to the kit, or lost track of it.
            "note": note,
        })).collect::<Vec<_>>(),
        "status": status.iter().map(|(node, item, state, detail, at)| json!({
            "node": node, "item": item, "state": state, "detail": detail, "at": at,
        })).collect::<Vec<_>>(),
    }))
}

/// One item with everything the editor needs: a skill's files in full, an MCP server's config
/// masked (never a secret value).
pub async fn item_json(o: &Orchestrator, id: i64) -> Result<Value> {
    let i: ItemRow = sqlx::query_as(&format!(
        "select {ITEM_COLUMNS} from kit_items where id = $1"
    ))
    .bind(id)
    .fetch_optional(&o.db)
    .await?
    .context("no such kit item")?;
    let config = match i.kind.as_str() {
        "mcp" => Some(mask_config(&open_config(o, i.config.as_deref())?).0),
        _ => None,
    };
    Ok(json!({
        "id": i.id,
        "kind": i.kind,
        "name": i.name,
        "enabled": i.enabled,
        "spec": i.spec.0,
        "files": i.files.map(|f| f.0),
        "config": config,
        "updated_at": i.updated_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_masked_value_is_kept_only_when_nothing_else_changed() {
        let old = json!({
            "command": "npx",
            "args": ["-y", "@good/mcp-server", "--api-key", "a-raw-1"],
            "env": {"API_TOKEN": "t-raw-3", "API_BASE": "https://api.good.example"},
        });
        let (masked, _) = mask_config(&old);
        // Saved untouched: exactly the old config.
        assert_eq!(merge_masked(&masked, &old).unwrap(), old);
        // Everything typed: taken as typed.
        let typed = json!({"command": "npx", "args": ["-y", "@good/mcp-server"], "env": {"API_TOKEN": "t-new"}});
        assert_eq!(merge_masked(&typed, &old).unwrap(), typed);
        let refused = |edited: &Value| {
            let e = merge_masked(edited, &old).unwrap_err().to_string();
            assert!(e.contains("no secret is kept"), "{e}");
        };
        // Another package, the token still masked: it would go to the other package.
        let mut package = masked.clone();
        package["args"][1] = json!("evil-package");
        refused(&package);
        // Another host typed over its mask, the key still masked: it would go to that host.
        let mut host = masked.clone();
        host["env"]["API_BASE"] = json!("https://evil.example");
        refused(&host);
        // A pair of args deleted: the next secret would move beside another flag.
        let mut deleted = masked.clone();
        deleted["args"].as_array_mut().unwrap().drain(2..4);
        refused(&deleted);
        // A changed command or URL, a mask added where there was none.
        let mut cmd = masked.clone();
        cmd["command"] = json!("other");
        refused(&cmd);
        let mut stray = masked.clone();
        stray["env"]["NEW"] = json!(MASK);
        refused(&stray);
        // A URL masked in part is no change when left as shown.
        let http = json!({"type": "http", "url": "https://bot:pw-raw@mcp.example/x", "headers": {"Authorization": "Bearer t-raw-1"}});
        assert_eq!(merge_masked(&mask_config(&http).0, &http).unwrap(), http);
    }

    #[test]
    fn a_skill_is_checked_before_it_is_stored() {
        let ok = BTreeMap::from([
            ("SKILL.md".to_owned(), "---\nname: x\n---".to_owned()),
            ("scripts/a.py".to_owned(), "print(1)".to_owned()),
        ]);
        assert!(check_skill("x", &ok).is_ok());
        assert!(check_skill("../x", &ok).is_err());
        let mut no_skill = ok.clone();
        no_skill.remove("SKILL.md");
        assert!(check_skill("x", &no_skill).is_err());
        for bad in ["../up.md", "/abs.md", "C:/x.md", "a\\b.md", ".fleet-kit"] {
            let mut f = ok.clone();
            f.insert(bad.into(), "x".into());
            assert!(check_skill("x", &f).is_err(), "{bad}");
        }
        let mut big = ok.clone();
        big.insert("big.md".into(), "x".repeat(MAX_SKILL_FILE_BYTES + 1));
        assert!(check_skill("x", &big).is_err());
        let mut total = ok.clone();
        for i in 0..3 {
            total.insert(format!("p{i}.md"), "x".repeat(MAX_SKILL_FILE_BYTES - 1));
        }
        assert!(
            check_skill("x", &total).is_err(),
            "the whole skill is capped too"
        );
        let mut binary = ok.clone();
        binary.insert("b.bin".into(), "a\0b".into());
        assert!(check_skill("x", &binary).is_err());
        let mut secret = ok;
        secret.insert("scripts/.env".into(), "TOKEN=x".into());
        assert!(
            check_skill("x", &secret).is_err(),
            "a secret file never enters the kit"
        );
    }

    #[test]
    fn a_plugin_source_carries_no_credential_and_cannot_pass_for_a_flag() {
        let p = |source: &str| KitPlugin {
            plugin: "p".into(),
            marketplace: "m".into(),
            marketplace_source: source.into(),
        };
        assert!(check_plugin(&p("owner/repo")).is_ok());
        assert!(check_plugin(&p("https://github.com/o/r.git")).is_ok());
        for bad in [
            "",
            "--scope=project",
            "a b",
            "https://ghp_x@github.com/o/r",
            "https://h/r?token=x",
        ] {
            assert!(check_plugin(&p(bad)).is_err(), "{bad:?}");
        }
    }
}
