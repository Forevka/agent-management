//! The kit: the owner's Claude Code skills, plugins and MCP servers, kept here, installed on
//! every node, and the allowlist of MCP servers for agent sessions.
//!
//! - **Owner only.** Every change here takes an [`Actor`] and refuses anything but
//!   [`Actor::Owner`], signed in on the web. None of it is an [`crate::admin::AdminAction`],
//!   which is the surface the brain proposes from: what runs in every agent session on every
//!   machine is not something an agent may suggest, even for the owner's approval.
//! - **Secrets in the vault.** An MCP server's env and header values are moved into the `kit`
//!   secret folder the moment they arrive (from an import, or typed into the config editor), and
//!   `kit_items.spec` keeps a `${secret:NAME}` placeholder in their place. They are resolved only
//!   to go to a node over the authenticated link: in the [`Kit`] (installed for the owner's own
//!   Claude Code) and in an agent's spawn (its per-run MCP config). Never logged, never in a
//!   response.
//! - **The import** asks a node what it has ([`capture`]); the answer, raw secrets included,
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
        MAX_SKILL_BYTES, MAX_SKILL_FILE_BYTES, MAX_SKILL_FILES, PART_BUDGET, Parts,
        safe_skill_path, valid_name,
    },
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{admin::Actor, core::Orchestrator};

/// The secret folder that holds the kit's MCP secrets.
pub const KIT_FOLDER: &str = "kit";
/// The preset that marks the kit's secret folder. No folder the owner creates can have it (it is
/// not in `admin::PRESETS`), and the brain cannot propose binding, unsetting or deleting it.
pub const KIT_PRESET: &str = "kit";
/// How long the owner waits for a node to report what it has.
const CAPTURE_WAIT: Duration = Duration::from_secs(60);
/// How long a finished capture (with its raw secrets) is kept for the owner to import from.
const CAPTURE_KEEP: Duration = Duration::from_secs(30 * 60);
const KINDS: [&str; 3] = ["skill", "plugin", "mcp"];

/// Captures asked for and not yet imported. In memory only: they hold raw secret values.
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

// ---------------------------------------------------------------- secrets

/// `${secret:NAME}`.
pub fn placeholder(key: &str) -> String {
    format!("${{secret:{key}}}")
}

/// The vault key an MCP server's env or header value is kept under: `SLACK_SLACK_TOKEN`.
fn secret_key(server: &str, field: &str) -> String {
    format!("{server}_{field}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Moves every secret of an MCP server's config out, leaving placeholders: each env and header
/// value, a URL's password and query values, and in `args` a DSN's password, a `NAME=value` or a
/// flag named like a secret ([`fleet_proto::kit::secret_spots`]). A value that already holds a
/// placeholder (the owner wrote `Bearer ${secret:X}`) stays as it is. `taken` are vault keys in
/// use elsewhere: a new key avoids them. Returns `(key, value)` pairs.
pub fn extract_secrets(
    server: &str,
    config: &mut Value,
    taken: &BTreeSet<String>,
) -> Vec<(String, String)> {
    use fleet_proto::kit::SpotAt;
    let mut out: Vec<(String, String)> = Vec::new();
    for spot in fleet_proto::kit::secret_spots(config) {
        if spot.value.contains("${secret:") {
            continue;
        }
        let base = secret_key(server, &spot.name);
        let mut key = base.clone();
        let mut n = 2;
        while taken.contains(&key) || out.iter().any(|(k, _)| *k == key) {
            key = format!("{base}_{n}");
            n += 1;
        }
        let target = match &spot.at {
            SpotAt::Env => config.get_mut("env").and_then(|m| m.get_mut(&spot.name)),
            SpotAt::Header => config
                .get_mut("headers")
                .and_then(|m| m.get_mut(&spot.name)),
            SpotAt::Url => config.get_mut("url"),
            SpotAt::Arg(i) => config.get_mut("args").and_then(|a| a.get_mut(*i)),
        };
        let Some(Value::String(s)) = target else {
            continue;
        };
        let (from, to) = (
            format!("{}{}{}", spot.prefix, spot.value, spot.suffix),
            format!("{}{}{}", spot.prefix, placeholder(&key), spot.suffix),
        );
        if !s.contains(&from) {
            continue;
        }
        *s = s.replacen(&from, &to, 1);
        out.push((key, spot.value));
    }
    out
}

/// The vault keys a config's placeholders name.
pub fn placeholders(config: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk_strings(config, &mut |s| {
        let mut rest = s;
        while let Some(i) = rest.find("${secret:") {
            rest = &rest[i + 9..];
            if let Some(end) = rest.find('}') {
                out.insert(rest[..end].to_owned());
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
    });
    out
}

fn walk_strings(v: &Value, f: &mut impl FnMut(&str)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter().for_each(|x| walk_strings(x, f)),
        Value::Object(m) => m.values().for_each(|x| walk_strings(x, f)),
        _ => {}
    }
}

/// The config with every placeholder replaced by its value, wherever it is (env, headers, args,
/// a URL). A placeholder whose secret is not set is an error naming the key, never a silent "".
pub fn resolve(config: &Value, secrets: &HashMap<String, String>) -> Result<Value> {
    Ok(match config {
        Value::String(s) => {
            let mut out = String::with_capacity(s.len());
            let mut rest = s.as_str();
            while let Some(i) = rest.find("${secret:") {
                out.push_str(&rest[..i]);
                let after = &rest[i + 9..];
                let end = after
                    .find('}')
                    .context("a ${secret: placeholder is not closed")?;
                let key = &after[..end];
                out.push_str(
                    secrets
                        .get(key)
                        .with_context(|| format!("the secret {key} is not set"))?,
                );
                rest = &after[end + 1..];
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| resolve(x, secrets))
                .collect::<Result<_>>()?,
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| Ok((k.clone(), resolve(x, secrets)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// The kit's secret folder: the one folder with the reserved preset [`KIT_PRESET`], created on
/// first use. A folder the owner made, whatever its name, is never taken for it: it is named
/// `kit`, or `kit-…` when the owner already has a folder called that.
async fn kit_folder(o: &Orchestrator) -> Result<i64> {
    if let Some(id) = kit_folder_id(o).await? {
        return Ok(id);
    }
    for name in [
        KIT_FOLDER.to_owned(),
        format!(
            "{KIT_FOLDER}-{}",
            &crate::web::random_token()[..6].to_lowercase()
        ),
    ] {
        let made: Option<i64> = sqlx::query_scalar(
            "insert into secret_folders (name, preset) values ($1, $2)
             on conflict (name) do nothing returning id",
        )
        .bind(&name)
        .bind(KIT_PRESET)
        .fetch_optional(&o.db)
        .await?;
        if let Some(id) = made {
            return Ok(id);
        }
    }
    bail!("could not create the kit's secret folder")
}

async fn kit_folder_id(o: &Orchestrator) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("select id from secret_folders where preset = $1 order by id limit 1")
            .bind(KIT_PRESET)
            .fetch_optional(&o.db)
            .await?,
    )
}

/// Every kit secret, decrypted. Only ever used to resolve configs that go to a node.
async fn secrets(o: &Orchestrator) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "select i.key, i.value from secret_items i join secret_folders f on f.id = i.folder_id
         where f.preset = $1",
    )
    .bind(KIT_PRESET)
    .fetch_all(&o.db)
    .await?;
    rows.into_iter()
        .map(|(k, v)| Ok((k, o.vault.decrypt(&v)?)))
        .collect()
}

/// The names of the kit's secrets.
async fn secret_keys(o: &Orchestrator) -> Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar(
        "select i.key from secret_items i join secret_folders f on f.id = i.folder_id where f.preset = $1",
    )
    .bind(KIT_PRESET)
    .fetch_all(&o.db)
    .await?
    .into_iter()
    .collect())
}

/// Vault keys a new secret of MCP server `name` must not take: every key in the kit's folder
/// (one the owner set by hand is never overwritten) and every key another item refers to, except
/// the server's own, which a new import of it replaces.
async fn taken_keys(o: &Orchestrator, name: &str) -> Result<BTreeSet<String>> {
    let specs: Vec<(String, sqlx::types::Json<Value>)> =
        sqlx::query_as("select name, spec from kit_items where kind = 'mcp'")
            .fetch_all(&o.db)
            .await?;
    let own: BTreeSet<String> = specs
        .iter()
        .filter(|(n, _)| n == name)
        .flat_map(|(_, s)| placeholders(&s.0))
        .collect();
    let others: BTreeSet<String> = specs
        .iter()
        .filter(|(n, _)| n != name)
        .flat_map(|(_, s)| placeholders(&s.0))
        .collect();
    let mut taken: BTreeSet<String> = secret_keys(o).await?.difference(&own).cloned().collect();
    taken.extend(others);
    Ok(taken)
}

/// Stores one kit secret, encrypted, exactly as given: a token is not text to tidy up.
async fn store_secret(o: &Orchestrator, folder: i64, key: &str, value: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "a secret's name is letters, digits and '_'"
    );
    sqlx::query(
        "insert into secret_items (folder_id, key, value) values ($1, $2, $3)
         on conflict (folder_id, key) do update set value = $3",
    )
    .bind(folder)
    .bind(key)
    .bind(o.vault.encrypt(value))
    .execute(&o.db)
    .await?;
    o.audit("owner", "kit.secret.set", json!({"key": key}))
        .await;
    Ok(())
}

/// The longest MCP server config the kit takes: far more than any real one, and small enough
/// that a kit of them always fits its messages.
const MAX_MCP_CONFIG_BYTES: usize = 64 * 1024;

/// Stores an MCP server's config: its raw env and header values go into the vault first, and
/// only the config with placeholders reaches the database.
async fn store_mcp(o: &Orchestrator, name: &str, mut config: Value) -> Result<Vec<String>> {
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
    let taken = taken_keys(o, name).await?;
    let moved = extract_secrets(name, &mut config, &taken);
    if !moved.is_empty() {
        let folder = kit_folder(o).await?;
        for (key, value) in &moved {
            store_secret(o, folder, key, value).await?;
        }
    }
    sqlx::query(
        "insert into kit_items (kind, name, spec) values ('mcp', $1, $2)
         on conflict (kind, name) do update set spec = $2, updated_at = now()",
    )
    .bind(name)
    .bind(sqlx::types::Json(&config))
    .execute(&o.db)
    .await?;
    Ok(moved.into_iter().map(|(k, _)| k).collect())
}

// ---------------------------------------------------------------- skills

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
    let source = p.marketplace_source.trim();
    ensure!(
        !source.is_empty() && !source.starts_with('-') && !source.contains(char::is_whitespace),
        "the marketplace source is `owner/repo`, a git URL or a path"
    );
    ensure!(
        !fleet_proto::kit::has_credentials(source),
        "the marketplace source carries a password; give the node access another way (a git credential helper, an SSH key)"
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

/// An MCP server, created or replaced with this config. Raw env and header values in it go into
/// the vault; placeholders already there stay.
pub async fn save_mcp(
    o: &Arc<Orchestrator>,
    actor: Actor,
    name: &str,
    config: Value,
) -> Result<String> {
    owner_only(actor)?;
    let moved = store_mcp(o, name, config).await?;
    o.audit(
        actor.audit_name(),
        "kit.mcp.save",
        json!({"mcp": name, "secrets_stored": moved}),
    )
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

/// Sets (or replaces) one kit secret. Write-only, like every other secret.
pub async fn set_secret(
    o: &Arc<Orchestrator>,
    actor: Actor,
    key: &str,
    value: &str,
) -> Result<String> {
    owner_only(actor)?;
    let folder = kit_folder(o).await?;
    store_secret(o, folder, key, value).await?;
    changed(o).await;
    Ok(format!("{key} is set"))
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

/// Deletes an item. The nodes remove what they installed of it when the new kit reaches them.
/// Its secrets stay in the `kit` folder until the owner removes them there.
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

/// Drops captures nobody imported from, with their raw secrets. Housekeeping calls it.
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

/// What a capture offers, with every secret value replaced by the placeholder it would get.
async fn candidates(o: &Orchestrator, id: &str) -> Result<Value> {
    let have = in_kit(o).await?;
    let names: Vec<String> = o
        .kit_captures
        .lock()
        .unwrap()
        .ready
        .get(id)
        .map(|r| r.content.mcp.iter().map(|m| m.name.clone()).collect())
        .unwrap_or_default();
    let mut taken = HashMap::new();
    for name in names {
        let t = taken_keys(o, &name).await?;
        taken.insert(name, t);
    }
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
        "plugins": r.content.plugins.iter().map(|p| json!({
            "name": p.id(),
            "in_kit": has("plugin", &p.id()),
            // A source with a password is never shown, and cannot be imported (`check_plugin`).
            "marketplace_source": if fleet_proto::kit::has_credentials(&p.marketplace_source) {
                "(its source carries a password; it cannot be imported)".to_owned()
            } else {
                p.marketplace_source.clone()
            },
            "importable": !fleet_proto::kit::has_credentials(&p.marketplace_source),
        })).collect::<Vec<_>>(),
        "mcp": r.content.mcp.iter().map(|m| {
            let mut config: Value = serde_json::from_str(&m.config).unwrap_or(Value::Null);
            let none = BTreeSet::new();
            let secrets: Vec<String> = extract_secrets(&m.name, &mut config, taken.get(&m.name).unwrap_or(&none))
                .into_iter().map(|(k, _)| k).collect();
            json!({"name": m.name, "in_kit": has("mcp", &m.name), "config": config, "secrets": secrets})
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
/// capture is gone afterwards, raw secrets and all.
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
    // All of it checked before anything is stored, so a bad item leaves nothing half imported,
    // and the capture stays for another try.
    let checked = (|| -> Result<()> {
        for s in r
            .content
            .skills
            .iter()
            .filter(|s| pick(&sel.skills, &s.name))
        {
            let files: BTreeMap<String, String> = s
                .files
                .iter()
                .map(|f| (f.path.clone(), f.text.clone()))
                .collect();
            check_skill(&s.name, &files).with_context(|| format!("skill {}", s.name))?;
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
            ensure!(
                valid_name(&m.name),
                "{:?} is not a usable MCP server name",
                m.name
            );
            let v: Value = serde_json::from_str(&m.config)
                .with_context(|| format!("the config of {} is not JSON", m.name))?;
            ensure!(
                v.is_object(),
                "the config of {} is not a JSON object",
                m.name
            );
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
        let files: BTreeMap<String, String> = s
            .files
            .iter()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        store_skill(o, &s.name, &files)
            .await
            .with_context(|| format!("skill {}", s.name))?;
        done.push(format!("skill {}", s.name));
    }
    for p in r
        .content
        .plugins
        .iter()
        .filter(|p| pick(&sel.plugins, &p.id()))
    {
        store_plugin(o, p)
            .await
            .with_context(|| format!("plugin {}", p.id()))?;
        done.push(format!("plugin {}", p.id()));
    }
    for m in r.content.mcp.iter().filter(|m| pick(&sel.mcp, &m.name)) {
        let config: Value = serde_json::from_str(&m.config)
            .with_context(|| format!("the config of {} is not JSON", m.name))?;
        store_mcp(o, &m.name, config)
            .await
            .with_context(|| format!("MCP server {}", m.name))?;
        done.push(format!("MCP server {}", m.name));
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
    updated_at: chrono::DateTime<chrono::Utc>,
}

async fn items(o: &Orchestrator) -> Result<Vec<ItemRow>> {
    Ok(sqlx::query_as(
        "select id, kind, name, enabled, spec, files, updated_at from kit_items order by kind, name",
    )
    .fetch_all(&o.db)
    .await?)
}

/// The enabled kit with its secrets resolved, and the MCP items that could not be resolved
/// (with why), which are left out.
async fn desired(o: &Orchestrator) -> Result<(KitContent, Vec<(i64, String)>)> {
    let secrets = secrets(o).await?;
    let mut kit = KitContent::default();
    let mut unresolved = Vec::new();
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
            "mcp" => match resolve(&it.spec.0, &secrets) {
                Ok(config) => kit.mcp.push(KitMcp {
                    name: it.name,
                    config: config.to_string(),
                }),
                Err(e) => unresolved.push((it.id, format!("{e:#}"))),
            },
            _ => {}
        }
    }
    Ok((kit, unresolved))
}

/// The kit's enabled MCP servers, secrets resolved, as the JSON object a spawn carries in
/// `extra_mcp_servers`; `None` when there are none. A server whose secret is not set is left
/// out (its install status already says why).
///
/// Never fails a run: a kit it cannot read (a database hiccup, a secret that no longer decrypts)
/// is logged and the session starts with the fleet's server alone.
pub async fn agent_servers(o: &Orchestrator) -> Option<String> {
    let read = async {
        let specs: Vec<(String, sqlx::types::Json<Value>)> = sqlx::query_as(
            "select name, spec from kit_items where kind = 'mcp' and enabled order by name",
        )
        .fetch_all(&o.db)
        .await?;
        if specs.is_empty() {
            return Ok::<_, anyhow::Error>(None);
        }
        let secrets = secrets(o).await?;
        let mut servers = serde_json::Map::new();
        for (name, spec) in specs {
            match resolve(&spec.0, &secrets) {
                Ok(config) => {
                    servers.insert(name, config);
                }
                Err(e) => tracing::warn!("kit MCP server {name} left out of a session: {e:#}"),
            }
        }
        Ok((!servers.is_empty()).then(|| Value::Object(servers).to_string()))
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
    let (kit, unresolved) = desired(o).await?;
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
    for (id, why) in unresolved {
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
/// `removed` row stays, so the owner sees what was taken off.
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
    for s in list {
        // What the node says about itself: whether its claude CLI enforces the allowlist.
        if s.kind == "node" && s.name == "allowlist" {
            let note =
                (s.state != "installed").then(|| s.detail.chars().take(500).collect::<String>());
            sqlx::query("update nodes set kit_note = $2 where id = $1")
                .bind(&node)
                .bind(note)
                .execute(&o.db)
                .await?;
            continue;
        }
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
               -- An enabled item the node took off was left out of its kit: its secret is not
               -- set. The row keeps saying so rather than a bare `removed`.
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

/// The kit page: items, nodes, the install matrix and the names of the kit's secrets. Skill
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
    let keys: Vec<String> = sqlx::query_scalar(
        "select i.key from secret_items i join secret_folders f on f.id = i.folder_id where f.preset = $1 order by i.key",
    )
    .bind(KIT_PRESET)
    .fetch_all(&o.db)
    .await?;
    let set: BTreeSet<&String> = keys.iter().collect();
    Ok(json!({
        "items": items.iter().map(|i| {
            let missing: Vec<String> = if i.kind == "mcp" {
                placeholders(&i.spec.0).into_iter().filter(|k| !set.contains(k)).collect()
            } else {
                vec![]
            };
            json!({
                "id": i.id,
                "kind": i.kind,
                "name": i.name,
                "enabled": i.enabled,
                "spec": i.spec.0,
                "files": i.files.as_ref().map(|f| f.0.iter().map(|(p, t)| json!({"path": p, "bytes": t.len()})).collect::<Vec<_>>()),
                "missing_secrets": missing,
                "updated_at": i.updated_at,
            })
        }).collect::<Vec<_>>(),
        "nodes": nodes.iter().map(|(id, name, online, version, note)| json!({
            "id": id,
            "name": name,
            "online": online,
            "version": version,
            "current": fleet_proto::node_is_current(version),
            // Set when the node cannot hold its sessions to the kit (its claude CLI is too old).
            "note": note,
        })).collect::<Vec<_>>(),
        "status": status.iter().map(|(node, item, state, detail, at)| json!({
            "node": node, "item": item, "state": state, "detail": detail, "at": at,
        })).collect::<Vec<_>>(),
        "secrets": keys,
    }))
}

/// One item with everything the editor needs: a skill's files in full, an MCP server's config
/// (placeholders, never values).
pub async fn item_json(o: &Orchestrator, id: i64) -> Result<Value> {
    let i: ItemRow = sqlx::query_as(
        "select id, kind, name, enabled, spec, files, updated_at from kit_items where id = $1",
    )
    .bind(id)
    .fetch_optional(&o.db)
    .await?
    .context("no such kit item")?;
    Ok(json!({
        "id": i.id,
        "kind": i.kind,
        "name": i.name,
        "enabled": i.enabled,
        "spec": i.spec.0,
        "files": i.files.map(|f| f.0),
        "updated_at": i.updated_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_env_and_header_value_leaves_for_the_vault() {
        let mut config = json!({
            "type": "http",
            "url": "https://mcp.example/api",
            "headers": {"Authorization": "Bearer xoxp-raw-1", "X-Team": "${secret:TEAM}"},
            "env": {"SLACK_TOKEN": "xoxp-raw-2", "DEBUG": "1"},
            "args": ["--flag"],
        });
        let taken = BTreeSet::from(["SLACK_SLACK_TOKEN".to_owned()]);
        let moved = extract_secrets("slack", &mut config, &taken);
        let keys: Vec<&str> = moved.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["SLACK_DEBUG", "SLACK_SLACK_TOKEN_2", "SLACK_AUTHORIZATION"]
        );
        let stored = config.to_string();
        for raw in ["xoxp-raw-1", "xoxp-raw-2"] {
            assert!(!stored.contains(raw), "{raw} survived in {stored}");
        }
        assert_eq!(
            config["env"]["SLACK_TOKEN"],
            "${secret:SLACK_SLACK_TOKEN_2}"
        );
        assert_eq!(
            config["headers"]["X-Team"], "${secret:TEAM}",
            "a placeholder the owner wrote stays"
        );
        assert_eq!(
            placeholders(&config),
            BTreeSet::from([
                "SLACK_DEBUG".to_owned(),
                "SLACK_SLACK_TOKEN_2".to_owned(),
                "SLACK_AUTHORIZATION".to_owned(),
                "TEAM".to_owned()
            ])
        );

        // Resolved only for a node, everywhere a placeholder is, and never as an empty string.
        let mut secrets: HashMap<String, String> = moved.into_iter().collect();
        assert!(
            resolve(&config, &secrets)
                .unwrap_err()
                .to_string()
                .contains("TEAM"),
            "a missing secret is named"
        );
        secrets.insert("TEAM".into(), "t1".into());
        let resolved = resolve(&config, &secrets).unwrap();
        assert_eq!(resolved["headers"]["Authorization"], "Bearer xoxp-raw-1");
        assert_eq!(resolved["headers"]["X-Team"], "t1");
        assert_eq!(resolved["env"]["SLACK_TOKEN"], "xoxp-raw-2");
        let in_url = json!({"url": "https://h/?k=${secret:TEAM}&x=1"});
        assert_eq!(
            resolve(&in_url, &secrets).unwrap()["url"],
            "https://h/?k=t1&x=1"
        );
        assert!(resolve(&json!({"url": "${secret:TEAM"}), &secrets).is_err());

        // A token in a flag or a URL goes too, and comes back where it was.
        let mut config = json!({
            "command": "npx",
            "args": ["-y", "srv", "--api-key", "a-raw-1", "--token=t-raw-2", "--port", "80"],
            "url": "https://h/mcp?team=x&access_token=q-raw-3",
        });
        let original = config.clone();
        let moved = extract_secrets("srv", &mut config, &BTreeSet::new());
        let stored = config.to_string();
        for raw in ["a-raw-1", "t-raw-2", "q-raw-3"] {
            assert!(!stored.contains(raw), "{raw} survived in {stored}");
        }
        assert_eq!(config["args"][4], "--token=${secret:SRV_TOKEN}");
        assert_eq!(config["args"][6], "80", "an ordinary flag stays");
        let secrets: HashMap<String, String> = moved.into_iter().collect();
        assert_eq!(resolve(&config, &secrets).unwrap(), original);
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
    fn a_plugin_source_cannot_pass_for_a_flag() {
        let p = |source: &str| KitPlugin {
            plugin: "p".into(),
            marketplace: "m".into(),
            marketplace_source: source.into(),
        };
        assert!(check_plugin(&p("owner/repo")).is_ok());
        assert!(check_plugin(&p("https://github.com/o/r.git")).is_ok());
        for bad in ["", "--scope=project", "a b"] {
            assert!(check_plugin(&p(bad)).is_err(), "{bad:?}");
        }
    }
}
