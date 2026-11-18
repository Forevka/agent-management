//! Fleet administration: the one implementation of the changes the web UI makes to projects,
//! secrets, nodes, agents and bots, shared by the web UI and the orchestrator brain.
//!
//! - The web UI runs an [`AdminAction`] directly with [`execute`], because the owner is signed in.
//! - The brain can only propose one ([`propose`]). [`prepare`] checks that it can run, resolves
//!   names to ids and writes the summary the owner approves; the owner's tap on Approve runs it
//!   through the same [`execute`] (human item kind `admin_change`, see `human.rs`).
//! - Reads ([`read`]) are free. They never include a secret value or a bot token, and no action
//!   takes one: those are only entered on web pages, which the brain can have the owner sent a
//!   login link to ([`send_link`]). The brain never sees the link itself, so it cannot sign in
//!   and approve its own proposals.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    chat::{From, clip, esc},
    core::Orchestrator,
    human::{self, NewItem},
    model::{AgentRow, BRAIN_AGENT, BRAIN_TASK, HumanItemRow, NodeRow, TaskRow},
    tasks,
};

/// Who a change is made for. Both are the owner; the difference is how it reached us, which
/// decides what is allowed (the brain's proposals are narrower) and what the audit log says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// Signed in on the web UI.
    Owner,
    /// Approved by the owner on a proposal from the orchestrator brain.
    Brain,
}

impl Actor {
    pub fn audit_name(self) -> &'static str {
        match self {
            Actor::Owner => "owner",
            Actor::Brain => "owner (via brain)",
        }
    }
}

/// An `admin_change` older than this is not run any more: the fleet may have moved on.
pub const PROPOSAL_TTL_HOURS: i64 = 24;

/// Secret folder presets (suggested keys); any folder can still hold any keys.
pub const PRESETS: &[(&str, &[&str])] = &[
    ("custom", &[]),
    ("jira", &["JIRA_URL", "JIRA_EMAIL", "JIRA_API_TOKEN"]),
    ("github", &["GITHUB_TOKEN"]),
    ("gitlab", &["GITLAB_URL", "GITLAB_TOKEN"]),
    ("slack", &["SLACK_USER_TOKEN"]),
    ("ssh", &["SSH_HOST", "SSH_USER", "SSH_PRIVATE_KEY"]),
];

const AUTH_MODES: &[&str] = &["default", "oauth_token", "api_key"];

/// A secret folder or bot, by id or by name. The web UI uses ids; the brain usually names things.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Ref {
    Id(i64),
    Name(String),
}

/// A change to the fleet's configuration. Serialized into `admin_change` items, so variants and
/// fields may be added but not renamed while items are open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminAction {
    ProjectCreate {
        slug: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: String,
    },
    /// Unset fields stay as they are; `folders` replaces the bound secret folders.
    ProjectEdit {
        slug: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        folders: Option<Vec<i64>>,
    },
    ProjectBindFolder {
        project: String,
        folder: Ref,
    },
    ProjectUnbindFolder {
        project: String,
        folder: Ref,
    },
    ChannelAdd {
        project: String,
        channel: String,
        #[serde(default)]
        triage: bool,
    },
    ChannelRemove {
        project: String,
        channel: String,
    },
    AgentRename {
        agent: String,
        name: String,
    },
    /// Gives the agent's bot back to the pool; the agent keeps working as the orchestrator bot.
    AgentUnbind {
        agent: String,
    },
    AgentRemove {
        agent: String,
    },
    /// Removes a free bot (and its token) from the pool.
    BotRemove {
        bot: Ref,
    },
    /// Unset fields stay as they are. The token or key of a token auth mode is set on the web.
    ///
    /// The brain cannot give `command`; it gives `model` / `fallback_model` (an empty string
    /// clears the flag), which [`prepare`] merges into the node's current command and stores
    /// as `command`, keeping the program and every other argument.
    NodeSettings {
        node: String,
        #[serde(default)]
        command: Option<Vec<String>>,
        #[serde(default)]
        auth_mode: Option<String>,
        #[serde(default)]
        max_agents: Option<i32>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        fallback_model: Option<String>,
    },
    NodeRevoke {
        node: String,
    },
    FolderCreate {
        name: String,
        #[serde(default)]
        preset: Option<String>,
    },
    FolderDelete {
        folder: Ref,
    },
    SecretUnset {
        folder: Ref,
        key: String,
    },
}

/// The node's rule, so every project created here can be mapped on a node.
pub fn valid_slug(slug: &str) -> bool {
    fleet_node_config::validate_slug(slug).is_ok()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn clean_command(command: &[String]) -> Vec<String> {
    command
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn check_len(what: &str, s: &str, max: usize) -> Result<()> {
    ensure!(
        s.chars().count() <= max,
        "{what} is too long (at most {max} characters)"
    );
    Ok(())
}

/// The brain's own sessions run in the brain project and get its secret folders as `.env`
/// files, so binding a folder there would hand the brain the values. Only the owner, on the
/// web, may do that.
fn guard_brain_secrets(o: &Orchestrator, actor: Actor, project: &str) -> Result<()> {
    ensure!(
        actor != Actor::Brain || project != o.cfg.brain_project,
        "secret folders cannot be bound to the orchestrator's own project ({project}) this way: its values would reach the orchestrator"
    );
    Ok(())
}

/// Longest summary a proposal may have: with the header and the brain's note (clipped to 500)
/// the announcement stays within Telegram's 4096 characters.
const MAX_SUMMARY: usize = 3000;
/// Descriptions the brain proposes are shown in full in the summary, so they are short.
const MAX_PROPOSED_DESCRIPTION: usize = 500;

/// Text that came from the brain, a node, an agent or a third party, as it appears in a summary
/// the owner approves: JSON-quoted, so it shows exactly where it starts and ends and cannot
/// break lines or pose as part of our own wording.
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// The Claude command flags the brain may set; all else in a node's command is the owner's,
/// set on the web. (`--verbose` is not among them: the node always passes it, stream-json
/// output needs it.)
const BRAIN_FLAGS: [&str; 2] = ["--model", "--fallback-model"];

/// Sets or clears `--model` / `--fallback-model` (an empty value clears) in a node's command.
/// The program and every other argument stay as they are, so the brain can never change what
/// runs, only which model Claude uses.
///
/// Only Claude's own arguments are touched: the Claude program is the first element whose file
/// stem is `claude` (a wrapper such as `npx` or a script may come before it, with flags of its
/// own); without one, the first element.
fn merge_claude_flags(
    command: &[String],
    model: Option<&str>,
    fallback_model: Option<&str>,
) -> Result<Vec<String>> {
    for value in [model, fallback_model].into_iter().flatten() {
        ensure!(
            value.is_empty()
                || (value.len() <= 100
                    && !value.starts_with('-')
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-._:[]".contains(c))),
            "{value:?} is not a model name"
        );
    }
    ensure!(
        !command.is_empty(),
        "the node has no command; set it on the web"
    );
    let program = command
        .iter()
        .position(|a| {
            let base = a.rsplit(['/', '\\']).next().unwrap_or_default();
            let stem = base.split('.').next().unwrap_or_default();
            stem.eq_ignore_ascii_case("claude")
        })
        .unwrap_or(0);
    let changing = |flag: &str| match flag {
        "--model" => model.is_some(),
        _ => fallback_model.is_some(),
    };
    // Drop the flags being changed (`--flag value` or `--flag=value`) after the program.
    let mut out = command[..=program].to_vec();
    let mut args = command[program + 1..].iter();
    while let Some(arg) = args.next() {
        let flag = arg.split('=').next().unwrap_or_default();
        if BRAIN_FLAGS.contains(&flag) && changing(flag) {
            if !arg.contains('=') {
                args.next();
            }
            continue;
        }
        out.push(arg.clone());
    }
    for (flag, value) in [("--model", model), ("--fallback-model", fallback_model)] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            out.extend([flag.to_owned(), v.to_owned()]);
        }
    }
    Ok(out)
}
// ---------------------------------------------------------------- lookups

async fn folder(o: &Orchestrator, r: &Ref) -> Result<(i64, String)> {
    let row: Option<(i64, String)> = match r {
        Ref::Id(id) => {
            sqlx::query_as("select id, name from secret_folders where id = $1")
                .bind(id)
                .fetch_optional(&o.db)
                .await?
        }
        Ref::Name(n) => {
            let by_name: Option<(i64, String)> =
                sqlx::query_as("select id, name from secret_folders where name = $1")
                    .bind(n.trim())
                    .fetch_optional(&o.db)
                    .await?;
            match (by_name, n.trim().parse::<i64>()) {
                (Some(row), _) => Some(row),
                (None, Ok(id)) => {
                    sqlx::query_as("select id, name from secret_folders where id = $1")
                        .bind(id)
                        .fetch_optional(&o.db)
                        .await?
                }
                (None, Err(_)) => None,
            }
        }
    };
    row.with_context(|| format!("no secret folder {r:?}"))
}

async fn folder_keys(o: &Orchestrator, id: i64) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("select key from secret_items where folder_id = $1 order by key")
            .bind(id)
            .fetch_all(&o.db)
            .await?,
    )
}

/// A node by id, or by name when that is unambiguous.
async fn node(o: &Orchestrator, r: &str) -> Result<NodeRow> {
    node_opt(o, r)
        .await?
        .with_context(|| format!("no node {r:?}"))
}

/// [`node`], with "no such node" as `None` (and only that: database errors stay errors).
async fn node_opt(o: &Orchestrator, r: &str) -> Result<Option<NodeRow>> {
    if let Some(n) = o.node(r.trim()).await? {
        return Ok(Some(n));
    }
    let mut rows: Vec<NodeRow> =
        sqlx::query_as("select * from nodes where lower(name) = lower($1)")
            .bind(r.trim())
            .fetch_all(&o.db)
            .await?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(Some(rows.remove(0))),
        _ => bail!("several nodes are named {r:?}; use the node id"),
    }
}

/// An agent by id or name. The orchestrator itself is not administered this way.
async fn agent(o: &Orchestrator, r: &str) -> Result<AgentRow> {
    let a = tasks::find_agent(o, r.trim())
        .await?
        .with_context(|| format!("no agent {r:?}"))?;
    ensure!(
        a.id != BRAIN_AGENT,
        "the orchestrator agent cannot be changed"
    );
    Ok(a)
}

/// A pool bot: (id, username, assigned agent).
async fn bot(o: &Orchestrator, r: &Ref) -> Result<(i64, String, Option<String>)> {
    let q = "select id, username, agent_id from bots";
    let row: Option<(i64, String, Option<String>)> = match r {
        Ref::Id(id) => {
            sqlx::query_as(&format!("{q} where id = $1"))
                .bind(id)
                .fetch_optional(&o.db)
                .await?
        }
        Ref::Name(n) => {
            let n = n.trim().trim_start_matches('@');
            sqlx::query_as(&format!(
                "{q} where lower(username) = lower($1) or id::text = $1 order by (lower(username) = lower($1)) desc limit 1"
            ))
            .bind(n)
            .fetch_optional(&o.db)
            .await?
        }
    };
    row.with_context(|| format!("no bot {r:?} in the pool"))
}

async fn active_tasks(o: &Orchestrator, agent_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from tasks where agent_id = $1 and (running or status in ('queued','planning','running','waiting'))",
    )
    .bind(agent_id)
    .fetch_one(&o.db)
    .await?)
}

/// Nodes show the project list in their tray app; refresh it after a project is created or
/// renamed. The change itself has already succeeded, so a failure here is only logged.
async fn projects_changed(o: &Orchestrator) {
    if let Err(e) = o.push_project_list(None).await {
        tracing::warn!("sending the project list to nodes failed: {e:#}");
    }
}

// ---------------------------------------------------------------- execute

/// Applies a change for `actor` (who the audit log names, and what is allowed). Returns a short
/// description of what happened.
pub async fn execute(o: &Arc<Orchestrator>, actor: Actor, action: &AdminAction) -> Result<String> {
    use AdminAction as A;
    match action {
        A::ProjectCreate {
            slug,
            name,
            description,
        } => {
            ensure!(valid_slug(slug), "invalid slug");
            let name = name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(slug);
            sqlx::query("insert into projects (slug, name, description) values ($1, $2, $3)")
                .bind(slug)
                .bind(name)
                .bind(description)
                .execute(&o.db)
                .await
                .context("a project with this slug exists")?;
            o.audit(
                actor.audit_name(),
                "project.create",
                json!({"project": slug}),
            )
            .await;
            projects_changed(o).await;
            Ok(format!("created project {slug}"))
        }
        A::ProjectEdit {
            slug,
            name,
            description,
            folders,
        } => {
            if folders.is_some() {
                guard_brain_secrets(o, actor, slug)?;
            }
            // All or nothing: a folder deleted since the proposal must not leave the project
            // renamed with its bindings half replaced.
            let mut tx = o.db.begin().await?;
            let n = sqlx::query(
                "update projects set name = coalesce($2, name), description = coalesce($3, description) where slug = $1",
            )
            .bind(slug)
            .bind(name)
            .bind(description)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            ensure!(n == 1, "unknown project {slug:?}");
            if let Some(folders) = folders {
                sqlx::query("delete from project_secrets where project = $1")
                    .bind(slug)
                    .execute(&mut *tx)
                    .await?;
                for id in folders {
                    sqlx::query("insert into project_secrets (project, folder_id) values ($1, $2) on conflict do nothing")
                        .bind(slug)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            tx.commit().await?;
            o.audit(actor.audit_name(), "project.edit", json!({"project": slug}))
                .await;
            projects_changed(o).await;
            Ok(format!("updated project {slug}"))
        }
        A::ProjectBindFolder { project, folder: f } => {
            guard_brain_secrets(o, actor, project)?;
            tasks::project(o, project).await?;
            let (id, name) = folder(o, f).await?;
            sqlx::query("insert into project_secrets (project, folder_id) values ($1, $2) on conflict do nothing")
                .bind(project)
                .bind(id)
                .execute(&o.db)
                .await?;
            o.audit(
                actor.audit_name(),
                "project.edit",
                json!({"project": project, "bind_folder": id}),
            )
            .await;
            Ok(format!("bound secret folder {name} to project {project}"))
        }
        A::ProjectUnbindFolder { project, folder: f } => {
            let (id, name) = folder(o, f).await?;
            sqlx::query("delete from project_secrets where project = $1 and folder_id = $2")
                .bind(project)
                .bind(id)
                .execute(&o.db)
                .await?;
            o.audit(
                actor.audit_name(),
                "project.edit",
                json!({"project": project, "unbind_folder": id}),
            )
            .await;
            Ok(format!(
                "unbound secret folder {name} from project {project}"
            ))
        }
        A::ChannelAdd {
            project,
            channel,
            triage,
        } => {
            let ch = channel.trim();
            ensure!(!ch.is_empty(), "channel id required");
            sqlx::query(
                "insert into feed_channels (source, channel_id, project, triage) values ('slack', $1, $2, $3)
                 on conflict (source, channel_id) do update set project = $2, triage = $3",
            )
            .bind(ch)
            .bind(project)
            .bind(triage)
            .execute(&o.db)
            .await?;
            o.audit(
                actor.audit_name(),
                "project.edit",
                json!({"project": project, "channel": ch}),
            )
            .await;
            Ok(format!("Slack channel {ch} now feeds project {project}"))
        }
        A::ChannelRemove { project, channel } => {
            sqlx::query("delete from feed_channels where source = 'slack' and channel_id = $1 and project = $2")
                .bind(channel)
                .bind(project)
                .execute(&o.db)
                .await?;
            o.audit(
                actor.audit_name(),
                "project.edit",
                json!({"project": project, "unchannel": channel}),
            )
            .await;
            Ok(format!(
                "Slack channel {channel} no longer feeds project {project}"
            ))
        }
        A::AgentRename { agent: r, name } => {
            let a = agent(o, r).await?;
            let name = name.trim();
            tasks::check_agent_name(name)?;
            if let Some(other) = tasks::find_agent(o, name).await?
                && other.id != a.id
            {
                bail!("an agent named {name} already exists");
            }
            sqlx::query("update agents set name = $2 where id = $1")
                .bind(&a.id)
                .bind(name)
                .execute(&o.db)
                .await
                .context("that name is taken")?;
            if let Some(bot_id) = a.bot_id {
                let (display, description) =
                    tasks::bot_profile(name, &a.role, a.project.as_deref().unwrap_or("-"));
                if let Err(e) = o.chat.set_bot_profile(bot_id, &display, &description).await {
                    tracing::warn!("renaming bot {bot_id} failed: {e:#}");
                }
            }
            o.audit(
                actor.audit_name(),
                "agent.rename",
                json!({"agent": a.id, "from": a.name, "to": name}),
            )
            .await;
            Ok(format!("renamed agent {} to {name}", a.name))
        }
        A::AgentUnbind { agent: r } => {
            let a = agent(o, r).await?;
            let bot_id = a.bot_id.with_context(|| format!("{} has no bot", a.name))?;
            let mut tx = o.db.begin().await?;
            sqlx::query("update agents set bot_id = null where id = $1")
                .bind(&a.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("update bots set agent_id = null where id = $1")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            o.audit(
                actor.audit_name(),
                "agent.unbind",
                json!({"agent": a.id, "bot": bot_id}),
            )
            .await;
            Ok(format!("{}'s bot is back in the pool", a.name))
        }
        A::AgentRemove { agent: r } => {
            let a = agent(o, r).await?;
            // Past tasks keep their history but lose the agent (the foreign key forbids
            // dangling ids), so they can no longer be resumed.
            let mut tx = o.db.begin().await?;
            // Locking the agent row holds off a new task for it (its foreign key check waits)
            // between the check below and the delete.
            sqlx::query("select id from agents where id = $1 for update")
                .bind(&a.id)
                .fetch_optional(&mut *tx)
                .await?
                .with_context(|| format!("agent {} is gone", a.name))?;
            let active: i64 = sqlx::query_scalar(
                "select count(*) from tasks where agent_id = $1 and (running or status in ('queued','planning','running','waiting'))",
            )
            .bind(&a.id)
            .fetch_one(&mut *tx)
            .await?;
            ensure!(
                active == 0,
                "{} has {active} active task(s); stop them first",
                a.name
            );
            sqlx::query("update bots set agent_id = null where agent_id = $1")
                .bind(&a.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("update tasks set agent_id = null where agent_id = $1")
                .bind(&a.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("delete from agents where id = $1")
                .bind(&a.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            o.audit(
                actor.audit_name(),
                "agent.remove",
                json!({"agent": a.id, "name": a.name}),
            )
            .await;
            Ok(format!("removed agent {}", a.name))
        }
        A::BotRemove { bot: r } => {
            let (id, username, _) = bot(o, r).await?;
            let n = sqlx::query("delete from bots where id = $1 and agent_id is null")
                .bind(id)
                .execute(&o.db)
                .await?
                .rows_affected();
            if n == 0 {
                return Ok(format!(
                    "nothing removed: @{username} is assigned to an agent"
                ));
            }
            // Stop sending as it and polling it: its token is gone from the pool.
            o.chat.remove_bot(id).await;
            o.audit(actor.audit_name(), "bot.remove", json!({"bot": username}))
                .await;
            Ok(format!("removed @{username} from the pool"))
        }
        // The brain's model / fallback_model were merged into `command` by `prepare`;
        // the precondition checked before this runs ensures the command it was merged into is
        // still the node's.
        A::NodeSettings {
            node: r,
            command,
            auth_mode,
            max_agents,
            ..
        } => {
            let saved = save_node_settings(
                o,
                actor,
                r,
                command.as_deref(),
                auth_mode.as_deref(),
                *max_agents,
                None,
            )
            .await?;
            Ok(match saved.push_error {
                None => format!("saved {}'s settings and pushed them to it", saved.node),
                Some(e) => format!(
                    "saved {}'s settings, but pushing them failed ({e}); the node gets them when it reconnects",
                    saved.node
                ),
            })
        }
        A::NodeRevoke { node: r } => {
            // The web UI passes ids of nodes that may already be gone: an unknown one is cleaned
            // up by id all the same (its runs below), but a lookup error is an error.
            let (id, name) = match node_opt(o, r).await? {
                Some(n) => (n.id, n.name),
                None => (r.trim().to_owned(), r.trim().to_owned()),
            };
            // The node, its runs and the prompts its agents were blocked on all go at once, as
            // when a node restarts: nothing may stay "running" on a node that can never report.
            let mut tx = o.db.begin().await?;
            let removed = sqlx::query("delete from nodes where id = $1")
                .bind(&id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            let ended: Vec<TaskRow> = sqlx::query_as(
                "update tasks set running = false where node_id = $1 and running returning *",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
            sqlx::query(
                "update human_items set state = 'cancelled' where node_id = $1 and state = 'open' and kind in ('plan','question','permission')",
            )
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            // Ask the node to stop those runs, after the delete so it cannot be readmitted. (A
            // node that comes back is refused, and then stops all its agents itself.)
            let key = id.parse::<fleet_net::EndpointId>().ok();
            if let Some(key) = key {
                for t in &ended {
                    if let Some(agent) = &t.agent_id {
                        o.server.send(
                            key,
                            fleet_proto::Body::StopAgent(fleet_proto::StopAgent {
                                agent_id: agent.clone(),
                                task_id: t.id.clone(),
                                graceful: false,
                                run_id: t.run_id.clone().unwrap_or_default(),
                            }),
                        );
                    }
                }
            }
            if removed == 0 && ended.is_empty() {
                if let Some(key) = key {
                    o.server.disconnect(key);
                }
                return Ok(format!("nothing to revoke: there is no node {}", q(r)));
            }
            o.audit(actor.audit_name(), "node.revoke", json!({"node": id}))
                .await;
            // In the background: the owner's tap (or the web request) is answered now, and the
            // follow-ups may wait on other tasks' locks.
            let (o2, n) = (o.clone(), ended.len());
            tokio::spawn(async move {
                // Disconnect once the node has acknowledged the stops (disconnecting drops what
                // is still queued), or after a few seconds if it does not.
                if let Some(key) = key {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                    while o2.server.pending(key) > 0 && tokio::time::Instant::now() < deadline {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    o2.server.disconnect(key);
                }
                o2.runs_lost(&ended, "its node was revoked").await;
                // Work waiting for a node (including a brain with messages) can go elsewhere.
                tasks::dispatch_queued(&o2).await;
            });
            Ok(format!("revoked node {name}; {n} run(s) on it ended"))
        }
        A::FolderCreate { name, preset } => {
            let id = create_folder(o, actor, name, preset.as_deref().unwrap_or("custom")).await?;
            Ok(format!("created secret folder {} (id {id})", name.trim()))
        }
        A::FolderDelete { folder: f } => {
            let (id, name) = folder(o, f).await?;
            sqlx::query("delete from secret_folders where id = $1")
                .bind(id)
                .execute(&o.db)
                .await?;
            o.audit(
                actor.audit_name(),
                "secrets.folder.delete",
                json!({"folder": id}),
            )
            .await;
            Ok(format!("deleted secret folder {name}"))
        }
        A::SecretUnset { folder: f, key } => {
            let (id, name) = folder(o, f).await?;
            sqlx::query("delete from secret_items where folder_id = $1 and key = $2")
                .bind(id)
                .bind(key)
                .execute(&o.db)
                .await?;
            o.audit(
                actor.audit_name(),
                "secrets.unset",
                json!({"folder": id, "key": key}),
            )
            .await;
            Ok(format!("deleted {key} from secret folder {name}"))
        }
    }
}

/// Creates a secret folder; returns its id.
pub async fn create_folder(
    o: &Orchestrator,
    actor: Actor,
    name: &str,
    preset: &str,
) -> Result<i64> {
    let name = name.trim();
    ensure!(!name.is_empty(), "name required");
    ensure!(
        PRESETS.iter().any(|(p, _)| *p == preset),
        "unknown preset {preset:?}"
    );
    let id: i64 = sqlx::query_scalar(
        "insert into secret_folders (name, preset) values ($1, $2) returning id",
    )
    .bind(name)
    .bind(preset)
    .fetch_one(&o.db)
    .await
    .context("a folder with this name exists")?;
    o.audit(
        actor.audit_name(),
        "secrets.folder.create",
        json!({"folder": name}),
    )
    .await;
    Ok(id)
}

// Secret values and tokens: web UI only, so these are not [`AdminAction`]s.

/// Stores one secret value, encrypted.
pub async fn set_secret(o: &Orchestrator, folder_id: i64, key: &str, value: &str) -> Result<()> {
    ensure!(valid_key(key), "invalid key");
    let value = value.replace("\r\n", "\n");
    sqlx::query("insert into secret_items (folder_id, key, value) values ($1, $2, $3) on conflict (folder_id, key) do update set value = $3")
        .bind(folder_id)
        .bind(key)
        .bind(o.vault.encrypt(value.trim_end_matches('\n')))
        .execute(&o.db)
        .await?;
    o.audit(
        "owner",
        "secrets.set",
        json!({"folder": folder_id, "key": key}),
    )
    .await;
    Ok(())
}

/// What saving a node's settings did.
pub struct SavedNode {
    pub node: String,
    /// The settings are stored either way; the node also gets them when it next connects.
    pub push_error: Option<String>,
}

/// Validates and stores a node's settings, then pushes them to the node once. Unset fields stay
/// as they are. `secret` (the Claude token or API key) comes from the web only and is stored in
/// the same statement, so nothing is half saved when validation fails.
pub async fn save_node_settings(
    o: &Orchestrator,
    actor: Actor,
    node_ref: &str,
    command: Option<&[String]>,
    auth_mode: Option<&str>,
    max_agents: Option<i32>,
    secret: Option<&str>,
) -> Result<SavedNode> {
    let n = node(o, node_ref).await?;
    // The brain only ever changes its three flags; everything else must be the node's own
    // command as it is now (`prepare` built it that way; this keeps it so).
    if actor == Actor::Brain
        && let Some(c) = command
    {
        let base = |cmd: &[String]| merge_claude_flags(cmd, Some(""), Some(""));
        ensure!(
            base(&clean_command(c))? == base(&n.claude_command.0)?,
            "the orchestrator may only change --model and --fallback-model"
        );
    }
    let command = clean_command(command.unwrap_or(&n.claude_command.0));
    ensure!(
        !command.is_empty(),
        "the command needs at least the program"
    );
    let mode = auth_mode.unwrap_or(&n.auth_mode);
    ensure!(AUTH_MODES.contains(&mode), "bad auth mode");
    let max = max_agents.unwrap_or(n.max_agents).clamp(1, 64);
    sqlx::query(
        "update nodes set claude_command = $2, auth_mode = $3, max_agents = $4, auth_secret = coalesce($5, auth_secret) where id = $1",
    )
    .bind(&n.id)
    .bind(sqlx::types::Json(&command))
    .bind(mode)
    .bind(max)
    .bind(secret.map(|s| o.vault.encrypt(s)))
    .execute(&o.db)
    .await?;
    o.audit(
        actor.audit_name(),
        "node.settings",
        json!({"node": n.id, "auth_mode": mode}),
    )
    .await;
    let push_error = o.push_node_settings(&n.id).await.err().map(|e| {
        tracing::warn!("pushing settings to {} failed: {e:#}", n.name);
        format!("{e:#}")
    });
    Ok(SavedNode {
        node: n.name,
        push_error,
    })
}

/// Registers an agent bot token in the pool.
pub async fn add_bot(o: &Orchestrator, token: &str) -> Result<()> {
    let token = token.trim();
    let (tg_id, username) = o.chat.inspect_bot(token).await?;
    let id: i64 = sqlx::query_scalar(
        "insert into bots (token, tg_user_id, username) values ($1, $2, $3)
         on conflict (tg_user_id) do update set token = $1, username = $3 returning id",
    )
    .bind(o.vault.encrypt(token))
    .bind(tg_id)
    .bind(&username)
    .fetch_one(&o.db)
    .await?;
    o.chat.add_bot(id, token).await?;
    o.audit("owner", "bot.add", json!({"bot": username})).await;
    Ok(())
}

// ---------------------------------------------------------------- proposals

/// The warning on every proposal that gives a project's agents secret values.
const SECRETS_REACH: &str =
    "⚠️ The orchestrator can direct this project's agents, and so can reach these values.";

/// Checks that a proposed change can run, resolves names to ids, and describes it for the owner.
/// The description is written here from the parsed action, never taken from the proposer. It
/// says what changes first; free text (names and descriptions, written by the brain, a node, an
/// agent or Slack) comes last and JSON-quoted. One change per proposal, so each summary says one
/// thing.
pub async fn prepare(o: &Orchestrator, action: AdminAction) -> Result<(AdminAction, String)> {
    use AdminAction as A;
    Ok(match action {
        A::ProjectCreate {
            slug,
            name,
            description,
        } => {
            let slug = slug.trim().to_owned();
            ensure!(
                valid_slug(&slug),
                "invalid slug {slug:?}: use 1-40 characters of a-z, 0-9 and -"
            );
            let exists: bool =
                sqlx::query_scalar("select exists(select 1 from projects where slug = $1)")
                    .bind(&slug)
                    .fetch_one(&o.db)
                    .await?;
            ensure!(!exists, "project {slug} already exists");
            let name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
            let description = description.trim().to_owned();
            check_len("name", name.as_deref().unwrap_or_default(), 100)?;
            check_len("description", &description, MAX_PROPOSED_DESCRIPTION)?;
            let mut summary = format!(
                "Create project {slug}. Name: {}.",
                q(name.as_deref().unwrap_or(&slug))
            );
            if !description.is_empty() {
                summary.push_str(&format!(" Description: {}.", q(&description)));
            }
            (
                A::ProjectCreate {
                    slug,
                    name,
                    description,
                },
                summary,
            )
        }
        A::ProjectEdit {
            slug,
            name,
            description,
            folders,
        } => {
            let p = tasks::project(o, &slug).await?;
            ensure!(
                folders.is_none() || (name.is_none() && description.is_none()),
                "one change per proposal: propose the secret folders separately from the name and description"
            );
            if let Some(mut ids) = folders {
                guard_brain_secrets(o, Actor::Brain, &p.slug)?;
                ids.sort_unstable();
                ids.dedup();
                let mut names = Vec::new();
                for id in &ids {
                    let (_, name) = folder(o, &Ref::Id(*id)).await?;
                    names.push(format!("#{id} {}", q(&name)));
                }
                let summary = format!(
                    "{SECRETS_REACH} Set the secret folders project {}'s agents get to exactly: {}.",
                    p.slug,
                    if names.is_empty() {
                        "none".into()
                    } else {
                        names.join(", ")
                    }
                );
                (
                    A::ProjectEdit {
                        slug: p.slug,
                        name: None,
                        description: None,
                        folders: Some(ids),
                    },
                    summary,
                )
            } else {
                let name = name.map(|n| n.trim().to_owned());
                let description = description.map(|d| d.trim().to_owned());
                let mut summary = format!("Edit project {}.", p.slug);
                if let Some(n) = &name {
                    ensure!(!n.is_empty(), "the name cannot be empty");
                    check_len("name", n, 100)?;
                    summary.push_str(&format!(" Name: {} → {}.", q(&p.name), q(n)));
                }
                if let Some(d) = &description {
                    check_len("description", d, MAX_PROPOSED_DESCRIPTION)?;
                    summary.push_str(&format!(" New description: {}.", q(d)));
                }
                ensure!(
                    name.is_some() || description.is_some(),
                    "nothing to change: give a name or a description"
                );
                (
                    A::ProjectEdit {
                        slug: p.slug,
                        name,
                        description,
                        folders: None,
                    },
                    summary,
                )
            }
        }
        A::ProjectBindFolder { project, folder: f } => {
            let p = tasks::project(o, &project).await?;
            guard_brain_secrets(o, Actor::Brain, &p.slug)?;
            let (id, name) = folder(o, &f).await?;
            ensure!(
                !folder_bound(o, &p.slug, id).await?,
                "folder {name} is already bound to {}",
                p.slug
            );
            let keys = folder_keys(o, id).await?;
            let summary = format!(
                "{SECRETS_REACH} Give project {}'s agents secret folder #{id} (keys: {}). Folder name: {}.",
                p.slug,
                key_list(&keys),
                q(&name)
            );
            (
                A::ProjectBindFolder {
                    project: p.slug,
                    folder: Ref::Id(id),
                },
                summary,
            )
        }
        A::ProjectUnbindFolder { project, folder: f } => {
            let p = tasks::project(o, &project).await?;
            let (id, name) = folder(o, &f).await?;
            ensure!(
                folder_bound(o, &p.slug, id).await?,
                "folder {name} is not bound to {}",
                p.slug
            );
            let summary = format!(
                "Stop giving project {}'s agents secret folder #{id}. Folder name: {}.",
                p.slug,
                q(&name)
            );
            (
                A::ProjectUnbindFolder {
                    project: p.slug,
                    folder: Ref::Id(id),
                },
                summary,
            )
        }
        A::ChannelAdd {
            project,
            channel,
            triage,
        } => {
            let p = tasks::project(o, &project).await?;
            let ch = channel.trim().to_owned();
            ensure!(
                !ch.is_empty() && ch.len() <= 32 && ch.chars().all(|c| c.is_ascii_alphanumeric()),
                "give the Slack channel id (like C0123ABCD), not its name"
            );
            let current: Option<Option<String>> = sqlx::query_scalar(
                "select project from feed_channels where source = 'slack' and channel_id = $1",
            )
            .bind(&ch)
            .fetch_optional(&o.db)
            .await?;
            let moved = match current {
                Some(Some(prev)) if prev != p.slug => format!(" It moves from project {prev}."),
                _ => String::new(),
            };
            let mode = if triage {
                "triage: the orchestrator sees new messages and may propose tasks or replies"
            } else {
                "mirror only"
            };
            let summary = format!(
                "Mirror Slack channel {ch} into project {} ({mode}).{moved}",
                p.slug
            );
            (
                A::ChannelAdd {
                    project: p.slug,
                    channel: ch,
                    triage,
                },
                summary,
            )
        }
        A::ChannelRemove { project, channel } => {
            let p = tasks::project(o, &project).await?;
            let ch = channel.trim().to_owned();
            let name: Option<String> = sqlx::query_scalar(
                "select name from feed_channels where source = 'slack' and channel_id = $1 and project = $2",
            )
            .bind(&ch)
            .bind(&p.slug)
            .fetch_optional(&o.db)
            .await?;
            let name = name.with_context(|| format!("channel {ch} does not feed {}", p.slug))?;
            let mut summary = format!("Stop mirroring Slack channel {ch} into project {}.", p.slug);
            if !name.is_empty() {
                summary.push_str(&format!(" Channel name: {}.", q(&name)));
            }
            (
                A::ChannelRemove {
                    project: p.slug,
                    channel: ch,
                },
                summary,
            )
        }
        A::AgentRename { agent: r, name } => {
            let a = agent(o, &r).await?;
            let name = name.trim().to_owned();
            tasks::check_agent_name(&name)?;
            if let Some(other) = tasks::find_agent(o, &name).await? {
                ensure!(other.id == a.id, "an agent named {name} already exists");
            }
            let summary = format!(
                "Rename agent {} ({}, project {}); its bot's Telegram name follows. Name: {} → {}.",
                a.id,
                a.role,
                a.project.as_deref().unwrap_or("-"),
                q(&a.name),
                q(&name)
            );
            (A::AgentRename { agent: a.id, name }, summary)
        }
        A::AgentUnbind { agent: r } => {
            let a = agent(o, &r).await?;
            let bot_id = a.bot_id.with_context(|| format!("{} has no bot", a.name))?;
            let (_, username, _) = bot(o, &Ref::Id(bot_id)).await?;
            let summary = format!(
                "Give bot {} back to the pool, away from agent {} (project {}). The agent stays and posts as the orchestrator bot. Agent name: {}.",
                q(&format!("@{username}")),
                a.id,
                a.project.as_deref().unwrap_or("-"),
                q(&a.name)
            );
            (A::AgentUnbind { agent: a.id }, summary)
        }
        A::AgentRemove { agent: r } => {
            let a = agent(o, &r).await?;
            let active = active_tasks(o, &a.id).await?;
            ensure!(
                active == 0,
                "{} has {active} active task(s); stop them first",
                a.name
            );
            let past: i64 = sqlx::query_scalar("select count(*) from tasks where agent_id = $1")
                .bind(&a.id)
                .fetch_one(&o.db)
                .await?;
            let bot = match a.bot_id {
                Some(b) => format!(
                    " Bot {} goes back to the pool.",
                    q(&format!("@{}", bot(o, &Ref::Id(b)).await?.1))
                ),
                None => String::new(),
            };
            let summary = format!(
                "Remove agent {} ({}, project {}).{bot} Its {past} past task(s) keep their history but can no longer be resumed. Agent name: {}.",
                a.id,
                a.role,
                a.project.as_deref().unwrap_or("-"),
                q(&a.name)
            );
            (A::AgentRemove { agent: a.id }, summary)
        }
        A::BotRemove { bot: r } => {
            let (id, username, assigned) = bot(o, &r).await?;
            ensure!(
                assigned.is_none(),
                "@{username} is assigned to an agent; unbind or remove the agent first"
            );
            let summary = format!(
                "Remove bot #{id} from the pool (its token is deleted). Bot: {}.",
                q(&format!("@{username}"))
            );
            (A::BotRemove { bot: Ref::Id(id) }, summary)
        }
        A::NodeSettings {
            node: r,
            command,
            auth_mode,
            max_agents,
            model,
            fallback_model,
        } => {
            ensure!(
                command.is_none(),
                "the orchestrator cannot set a node's whole Claude command; propose model or fallback_model, or ask the owner to change it on the node's web page"
            );
            let n = node(o, &r).await?;
            let mut changes = Vec::new();
            if let Some(m) = &auth_mode {
                ensure!(
                    AUTH_MODES.contains(&m.as_str()),
                    "auth mode must be one of {}",
                    AUTH_MODES.join(", ")
                );
                let mut change = format!("authentication {} → {m}", n.auth_mode);
                if m != "default" && n.auth_secret.is_none() {
                    change.push_str(
                        " (no token is stored for this node yet: until one is set on its web page, Claude's own login is used)",
                    );
                }
                changes.push(change);
            }
            if let Some(m) = max_agents {
                ensure!((1..=64).contains(&m), "max agents must be 1-64");
                changes.push(format!("max agents {} → {m}", n.max_agents));
            }
            let mut merged = None;
            if model.is_some() || fallback_model.is_some() {
                let current = &n.claude_command.0;
                let new = merge_claude_flags(
                    current,
                    model.as_deref().map(str::trim),
                    fallback_model.as_deref().map(str::trim),
                )?;
                ensure!(
                    &new != current,
                    "the node's Claude command already has these flags"
                );
                // The whole command, before and after: only the brain's flags differ.
                changes.push(format!(
                    "Claude command {} → {}",
                    json!(current),
                    json!(new)
                ));
                merged = Some(new);
            }
            ensure!(
                !changes.is_empty(),
                "nothing to change: give auth_mode, max_agents, model or fallback_model"
            );
            let summary = format!(
                "Change node {} settings: {}. New agent sessions on it use them. Node name: {}.",
                short_id(&n.id),
                changes.join("; "),
                q(&n.name)
            );
            (
                A::NodeSettings {
                    node: n.id,
                    command: merged,
                    auth_mode,
                    max_agents,
                    model: None,
                    fallback_model: None,
                },
                summary,
            )
        }
        A::NodeRevoke { node: r } => {
            let n = node(o, &r).await?;
            let running: i64 =
                sqlx::query_scalar("select count(*) from tasks where node_id = $1 and running")
                    .bind(&n.id)
                    .fetch_one(&o.db)
                    .await?;
            let runs_brain: bool = sqlx::query_scalar(
                "select exists(select 1 from tasks where id = $1 and node_id = $2 and running)",
            )
            .bind(BRAIN_TASK)
            .bind(&n.id)
            .fetch_one(&o.db)
            .await?;
            let brain = &o.cfg.brain_project;
            let others: Vec<NodeRow> = sqlx::query_as("select * from nodes where id <> $1")
                .bind(&n.id)
                .fetch_all(&o.db)
                .await?;
            let elsewhere = others.iter().any(|m| m.has_project(brain));
            let brain_note = match (runs_brain || n.has_project(brain), elsewhere) {
                (false, _) => String::new(),
                (true, true) => format!(
                    " The orchestrator's own session can run there: if it does, it ends and continues on another node with project {brain} at the next message."
                ),
                (true, false) => format!(
                    " The orchestrator's own session runs there and no other node has project {brain}: the orchestrator stays unreachable until one does."
                ),
            };
            let summary = format!(
                "Revoke node {}: it is disconnected and can only rejoin with a new invite. Its {running} running agent run(s) end; their Claude sessions stay on that machine, so a message to such a task starts it afresh on another node with its project.{brain_note} Node name: {}.",
                short_id(&n.id),
                q(&n.name)
            );
            (A::NodeRevoke { node: n.id }, summary)
        }
        A::FolderCreate { name, preset } => {
            let name = name.trim().to_owned();
            ensure!(!name.is_empty(), "name required");
            check_len("name", &name, 100)?;
            let preset = preset.unwrap_or_else(|| "custom".into());
            let keys = PRESETS
                .iter()
                .find(|(p, _)| *p == preset)
                .map(|(_, k)| k)
                .with_context(|| {
                    format!(
                        "unknown preset {preset:?}; use one of {}",
                        PRESETS
                            .iter()
                            .map(|(p, _)| *p)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            // By name only: a folder whose id happens to look like the new name is no clash.
            let taken: bool =
                sqlx::query_scalar("select exists(select 1 from secret_folders where name = $1)")
                    .bind(&name)
                    .fetch_one(&o.db)
                    .await?;
            ensure!(!taken, "a folder named {name} exists");
            let summary = format!(
                "Create an empty secret folder (preset {preset}{}); its values are then set on the web page. Folder name: {}.",
                if keys.is_empty() {
                    String::new()
                } else {
                    format!(": {}", keys.join(", "))
                },
                q(&name)
            );
            (
                A::FolderCreate {
                    name,
                    preset: Some(preset),
                },
                summary,
            )
        }
        A::FolderDelete { folder: f } => {
            let (id, name) = folder(o, &f).await?;
            let keys = folder_keys(o, id).await?;
            let projects: Vec<String> = sqlx::query_scalar(
                "select project from project_secrets where folder_id = $1 order by project",
            )
            .bind(id)
            .fetch_all(&o.db)
            .await?;
            let summary = format!(
                "Delete secret folder #{id} and its values (keys: {}). Used by: {}. Folder name: {}.",
                key_list(&keys),
                if projects.is_empty() {
                    "no project".into()
                } else {
                    projects.join(", ")
                },
                q(&name)
            );
            (
                A::FolderDelete {
                    folder: Ref::Id(id),
                },
                summary,
            )
        }
        A::SecretUnset { folder: f, key } => {
            let (id, name) = folder(o, &f).await?;
            let key = key.trim().to_owned();
            ensure!(
                folder_keys(o, id).await?.contains(&key),
                "folder {name} has no key {key}"
            );
            let summary = format!(
                "Delete the value of {key} in secret folder #{id}. Folder name: {}.",
                q(&name)
            );
            (
                A::SecretUnset {
                    folder: Ref::Id(id),
                    key,
                },
                summary,
            )
        }
    })
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(10)]
}

fn key_list(keys: &[String]) -> String {
    if keys.is_empty() {
        "none yet".into()
    } else {
        keys.join(", ")
    }
}

/// The state a proposal was written against. It is stored with the proposal and compared again
/// when the owner approves: the owner approved a change to *that* state, so if it moved in the
/// meantime the change is not run. Covers what the change touches and what its summary shows.
async fn snapshot(o: &Orchestrator, action: &AdminAction) -> Result<Value> {
    use AdminAction as A;
    let ref_id = |r: &Ref| match r {
        Ref::Id(id) => Some(*id),
        Ref::Name(_) => None,
    };
    Ok(match action {
        A::ProjectCreate { slug, .. } => {
            let exists: bool =
                sqlx::query_scalar("select exists(select 1 from projects where slug = $1)")
                    .bind(slug)
                    .fetch_one(&o.db)
                    .await?;
            json!({"exists": exists})
        }
        A::ProjectEdit { slug, .. }
        | A::ProjectBindFolder { project: slug, .. }
        | A::ProjectUnbindFolder { project: slug, .. } => {
            let row: Option<(String, String)> =
                sqlx::query_as("select name, description from projects where slug = $1")
                    .bind(slug)
                    .fetch_optional(&o.db)
                    .await?;
            let folders: Vec<i64> = sqlx::query_scalar(
                "select folder_id from project_secrets where project = $1 order by folder_id",
            )
            .bind(slug)
            .fetch_all(&o.db)
            .await?;
            let keys = match action {
                A::ProjectBindFolder { folder, .. } => match ref_id(folder) {
                    Some(id) => folder_keys(o, id).await?,
                    None => vec![],
                },
                _ => vec![],
            };
            json!({"project": row, "folders": folders, "keys": keys})
        }
        A::ChannelAdd { channel, .. } | A::ChannelRemove { channel, .. } => {
            let row: Option<(Option<String>, bool, String)> = sqlx::query_as(
                "select project, triage, name from feed_channels where source = 'slack' and channel_id = $1",
            )
            .bind(channel.trim())
            .fetch_optional(&o.db)
            .await?;
            json!(row)
        }
        // A rename or unbind does not care how much work the agent did meanwhile; a removal
        // does (its summary counts the tasks that stop being resumable).
        A::AgentRename { agent, .. } | A::AgentUnbind { agent } => {
            let row: Option<(String, Option<i64>, Option<String>)> =
                sqlx::query_as("select name, bot_id, project from agents where id = $1")
                    .bind(agent)
                    .fetch_optional(&o.db)
                    .await?;
            json!(row)
        }
        A::AgentRemove { agent } => {
            let row: Option<(String, Option<i64>, Option<String>, i64)> = sqlx::query_as(
                "select name, bot_id, project, (select count(*) from tasks t where t.agent_id = a.id) from agents a where id = $1",
            )
            .bind(agent)
            .fetch_optional(&o.db)
            .await?;
            json!(row)
        }
        A::BotRemove { bot } => {
            let row: Option<(String, Option<String>)> = match ref_id(bot) {
                Some(id) => {
                    sqlx::query_as("select username, agent_id from bots where id = $1")
                        .bind(id)
                        .fetch_optional(&o.db)
                        .await?
                }
                None => None,
            };
            json!(row)
        }
        // Only the fields being changed: the owner storing the node's token after a proposal
        // to switch its auth mode (the documented flow) must not make that proposal stale.
        A::NodeSettings {
            node,
            command,
            auth_mode,
            max_agents,
            ..
        } => match o.node(node).await? {
            None => Value::Null,
            Some(n) => json!({
                "command": command.as_ref().map(|_| &n.claude_command.0),
                "auth_mode": auth_mode.as_ref().map(|_| &n.auth_mode),
                "max_agents": max_agents.map(|_| n.max_agents),
            }),
        },
        A::NodeRevoke { node } => json!({"exists": o.node(node).await?.is_some()}),
        A::FolderCreate { name, .. } => {
            let exists: bool =
                sqlx::query_scalar("select exists(select 1 from secret_folders where name = $1)")
                    .bind(name.trim())
                    .fetch_one(&o.db)
                    .await?;
            json!({"exists": exists})
        }
        A::FolderDelete { folder } => match ref_id(folder) {
            Some(id) => folders_json(o, Some(id)).await?,
            None => Value::Null,
        },
        A::SecretUnset { folder, .. } => match ref_id(folder) {
            Some(id) => json!(folder_keys(o, id).await?),
            None => Value::Null,
        },
    })
}

async fn folder_bound(o: &Orchestrator, project: &str, folder_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select exists(select 1 from project_secrets where project = $1 and folder_id = $2)",
    )
    .bind(project)
    .bind(folder_id)
    .fetch_one(&o.db)
    .await?)
}

/// The brain's `admin_propose`: `{"action": "...", "params": {...}, "note": "..."}`. Queues the
/// change for the owner; nothing changes until they approve.
pub async fn propose(o: &Orchestrator, args: &Value) -> Result<String> {
    let name = args["action"].as_str().context("action required")?;
    let mut fields = args
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    fields.insert("action".into(), json!(name));
    let action: AdminAction = serde_json::from_value(Value::Object(fields))
        .with_context(|| format!("bad parameters for {name}"))?;
    let (action, summary) = prepare(o, action).await?;
    // The owner approves exactly what the summary says, so it is never cut: a change too long
    // to show in one Telegram message (with the note) is refused instead.
    ensure!(
        summary.chars().count() <= MAX_SUMMARY,
        "this change is too long to show the owner in one message ({} characters, at most {MAX_SUMMARY}); shorten or split it",
        summary.chars().count()
    );
    let note = args["note"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| clip(n, 1000));
    let precondition = snapshot(o, &action).await?;
    let row = human::create(
        o,
        NewItem {
            task: None,
            project: Some(o.cfg.brain_project.clone()),
            agent_id: Some(BRAIN_AGENT.into()),
            kind: "admin_change",
            blocking: true,
            payload: json!({"action": action, "summary": summary, "note": note, "precondition": precondition}),
            node_id: None,
            call_id: None,
        },
    )
    .await?;
    // An item the owner was never shown would wait forever: withdraw it and say so.
    let shown: Option<i64> = sqlx::query_scalar("select tg_msg from human_items where id = $1")
        .bind(row.id)
        .fetch_one(&o.db)
        .await?;
    if shown.is_none() {
        sqlx::query("update human_items set state = 'cancelled' where id = $1 and state = 'open'")
            .bind(row.id)
            .execute(&o.db)
            .await?;
        bail!(
            "the proposal could not be shown to the owner in Telegram, so it was withdrawn; try again later"
        );
    }
    Ok(format!(
        "Proposed as Q-{id}: {summary}\nNothing changes until the owner approves. The outcome arrives as a message starting with [Admin change Q-{id}].",
        id = row.id
    ))
}

/// Runs an approved `admin_change` item. A failure is reported, not retried: the owner approved
/// this exact change once, and the brain can propose again. So is a change whose state moved
/// since it was proposed: the owner approved it against the state its summary described.
pub async fn run_approved(o: &Arc<Orchestrator>, row: &HumanItemRow) -> Result<(String, Value)> {
    let action: AdminAction = serde_json::from_value(row.payload.0["action"].clone())
        .context("the stored change is unreadable")?;
    let unchanged = match (
        snapshot(o, &action).await,
        row.payload.0.get("precondition"),
    ) {
        (Ok(now), Some(then)) => &now == then,
        (Err(e), _) => {
            tracing::warn!("checking Q-{} against the fleet failed: {e:#}", row.id);
            false
        }
        (Ok(_), None) => false,
    };
    if !unchanged {
        return Ok((
            "not run: the state changed since this was proposed; ask again".into(),
            json!({"approved": true, "stale": true}),
        ));
    }
    Ok(match execute(o, Actor::Brain, &action).await {
        Ok(done) => (
            format!("approved: {done}"),
            json!({"approved": true, "result": done}),
        ),
        Err(e) => (
            format!("approved, but it failed: {e:#}"),
            json!({"approved": true, "error": format!("{e:#}")}),
        ),
    })
}

// ---------------------------------------------------------------- web links

/// Login links the brain may have sent to the owner per hour.
const MAX_LINKS_PER_HOUR: i64 = 5;

/// Pages a link may point at: the web UI's own pages, never the login route.
fn link_allowed(page: &str) -> bool {
    let Some(rest) = page.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.split('/');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some() {
        return false;
    }
    match (first, second) {
        ("" | "projects" | "secrets" | "nodes" | "agents", None) => true,
        ("projects" | "secrets" | "nodes" | "items" | "tasks", Some(s)) => {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }
        _ => false,
    }
}

/// Sends the owner a single-use login link to a web page (to enter a token or secret value).
/// The link goes straight to Telegram: the brain never holds a web session.
pub async fn send_link(o: &Orchestrator, page: &str, text: Option<&str>) -> Result<String> {
    let page = page.trim();
    ensure!(
        link_allowed(page),
        "page must be one of /, /projects[/<slug>], /secrets[/<folder-id>], /nodes[/<node-id>], /agents, /items/<id>, /tasks/<id>"
    );
    let text = clip(
        text.map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("this page may help."),
        500,
    );
    // Each link is a login; a brain gone wrong must not flood the owner with them. Counted and
    // recorded under one lock, so links asked for at the same moment cannot all slip through.
    let mut tx = o.db.begin().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtext('fleet.web_link'))")
        .execute(&mut *tx)
        .await?;
    let recent: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'web.link' and at > now() - interval '1 hour'",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        recent < MAX_LINKS_PER_HOUR,
        "the owner already got {MAX_LINKS_PER_HOUR} links in the last hour; wait, or ask them to send /web"
    );
    sqlx::query(
        "insert into audit_log (actor, action, detail) values ('orchestrator', 'web.link', $1)",
    )
    .bind(json!({"page": page, "text": text}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let url = crate::web::magic_link(o, page).await?;
    // Labelled as the orchestrator's words: the owner should not take it for the fleet's own.
    let html = format!(
        "🔗 <i>Orchestrator says:</i> {}\n<a href=\"{}\">Open {}</a> (single-use login link)",
        esc(&text),
        esc(&url),
        esc(page)
    );
    o.chat
        .send(From::Orchestrator, None, &html, vec![], None)
        .await?;
    Ok(format!(
        "Sent the owner a login link to {page} in Telegram (you do not get the link)."
    ))
}

// ---------------------------------------------------------------- reads

/// The brain's `admin_read`. Secret folders show key names only; bots show usernames only.
pub async fn read(o: &Orchestrator, what: &str, target: Option<&str>) -> Result<String> {
    let target = target.map(str::trim).filter(|t| !t.is_empty());
    let need = || target.with_context(|| format!("say which {what}"));
    let v = match what {
        "projects" => projects_json(o).await?,
        "project" => project_json(o, need()?).await?,
        "agents" => agents_json(o, target).await?,
        "bots" => {
            let rows: Vec<(i64, String, Option<String>)> =
                sqlx::query_as("select id, username, agent_id from bots order by id")
                    .fetch_all(&o.db)
                    .await?;
            let free = rows.iter().filter(|r| r.2.is_none()).count();
            json!({
                "free": free,
                "bots": rows.iter().map(|(id, u, a)| json!({"id": id, "username": u, "agent": a})).collect::<Vec<_>>(),
            })
        }
        "nodes" => {
            let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
                .fetch_all(&o.db)
                .await?;
            Value::Array(nodes.iter().map(node_json).collect())
        }
        "node" => node_json(&node(o, need()?).await?),
        "secrets" => folders_json(o, None).await?,
        "secret" | "folder" => {
            let (id, _) = folder(o, &Ref::Name(need()?.to_owned())).await?;
            folders_json(o, Some(id))
                .await?
                .get(0)
                .cloned()
                .unwrap_or_default()
        }
        "status" => status_json(o).await?,
        "items" => {
            let items = human::open_items(o).await?;
            Value::Array(
                items
                    .iter()
                    .map(|i| json!({"id": format!("Q-{}", i.id), "kind": i.kind, "project": i.project, "task": i.task_id, "blocking": i.blocking, "about": clip(&crate::web::item_summary(i), 200)}))
                    .collect(),
            )
        }
        other => bail!(
            "unknown read {other:?}; use projects, project <slug>, agents [project], bots, nodes, node <id|name>, secrets, secret <folder>, status or items"
        ),
    };
    Ok(serde_json::to_string_pretty(&v)?)
}

pub async fn projects_json(o: &Orchestrator) -> Result<Value> {
    let projects: Vec<(String, String, String)> =
        sqlx::query_as("select slug, name, description from projects order by slug")
            .fetch_all(&o.db)
            .await?;
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes")
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
    Ok(Value::Array(out))
}

async fn project_json(o: &Orchestrator, slug: &str) -> Result<Value> {
    let p = tasks::project(o, slug).await?;
    let folder_ids: Vec<i64> = sqlx::query_scalar(
        "select folder_id from project_secrets where project = $1 order by folder_id",
    )
    .bind(&p.slug)
    .fetch_all(&o.db)
    .await?;
    let mut folders = Vec::new();
    for id in folder_ids {
        folders.extend(
            folders_json(o, Some(id))
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    let channels: Vec<(String, String, String, bool)> = sqlx::query_as(
        "select source, channel_id, name, triage from feed_channels where project = $1 order by name",
    )
    .bind(&p.slug)
    .fetch_all(&o.db)
    .await?;
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await?;
    let on: Vec<Value> = nodes
        .iter()
        .filter_map(|n| {
            n.projects.0.iter().find(|m| m.project == p.slug).map(
                |m| json!({"node": n.name, "node_id": n.id, "path": m.path, "online": n.online}),
            )
        })
        .collect();
    Ok(json!({
        "slug": p.slug,
        "name": p.name,
        "description": p.description,
        "secret_folders": folders,
        "slack_channels": channels.iter().map(|(s, id, n, t)| json!({"source": s, "channel_id": id, "name": n, "triage": t})).collect::<Vec<_>>(),
        "nodes": on,
        "agents": agents_json(o, Some(&p.slug)).await?,
    }))
}

pub async fn agents_json(o: &Orchestrator, project: Option<&str>) -> Result<Value> {
    let rows: Vec<(String, String, String, Option<String>, Option<String>, i64)> = sqlx::query_as(
        "select a.id, a.name, a.role, a.project, b.username,
                (select count(*) from tasks t where t.agent_id = a.id and t.running) as running
         from agents a left join bots b on b.id = a.bot_id
         where a.id <> $2 and ($1::text is null or a.project = $1) order by a.project, a.name",
    )
    .bind(project)
    .bind(BRAIN_AGENT)
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
    Ok(Value::Array(out))
}

/// A node's settings. The stored token or key is never shown, only whether there is one.
fn node_json(n: &NodeRow) -> Value {
    json!({
        "id": n.id,
        "name": n.name,
        "os": n.os,
        "version": n.app_version,
        "online": n.online,
        "last_seen": n.last_seen.map(|t| t.to_rfc3339()),
        "claude_command": n.claude_command.0,
        "auth_mode": n.auth_mode,
        "auth_secret_set": n.auth_secret.is_some(),
        "max_agents": n.max_agents,
        "running_agents": n.running.0.len(),
        "projects": n.projects.0.iter().map(|m| json!({"project": m.project, "path": m.path})).collect::<Vec<_>>(),
    })
}

/// Secret folders with their key names and the projects using them. Never values.
async fn folders_json(o: &Orchestrator, only: Option<i64>) -> Result<Value> {
    let rows: Vec<(i64, String, String, Vec<String>, Vec<String>)> = sqlx::query_as(
        "select f.id, f.name, f.preset,
                coalesce((select array_agg(i.key order by i.key) from secret_items i where i.folder_id = f.id), '{}'),
                coalesce((select array_agg(ps.project order by ps.project) from project_secrets ps where ps.folder_id = f.id), '{}')
         from secret_folders f where ($1::bigint is null or f.id = $1) order by f.name",
    )
    .bind(only)
    .fetch_all(&o.db)
    .await?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|(id, name, preset, keys, projects)| {
                json!({"id": id, "name": name, "preset": preset, "keys": keys, "projects": projects})
            })
            .collect(),
    ))
}

async fn status_json(o: &Orchestrator) -> Result<Value> {
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await?;
    let (active, queued): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where status in ('planning','running','waiting')), count(*) filter (where status = 'queued') from tasks where id <> 'brain'",
    )
    .fetch_one(&o.db)
    .await?;
    let open: i64 = sqlx::query_scalar("select count(*) from human_items where state = 'open'")
        .fetch_one(&o.db)
        .await?;
    let free_bots: i64 = sqlx::query_scalar("select count(*) from bots where agent_id is null")
        .fetch_one(&o.db)
        .await?;
    Ok(json!({
        "nodes": nodes.iter().map(|n| json!({"name": n.name, "online": n.online, "running": n.running.0.len(), "max_agents": n.max_agents, "projects": n.projects.0.len()})).collect::<Vec<_>>(),
        "tasks": {"active": active, "queued": queued},
        "waiting_on_owner": open,
        "free_bots": free_bots,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip_through_json() {
        let a: AdminAction = serde_json::from_value(
            json!({"action": "channel_add", "project": "demo", "channel": "C1"}),
        )
        .unwrap();
        assert_eq!(
            a,
            AdminAction::ChannelAdd {
                project: "demo".into(),
                channel: "C1".into(),
                triage: false
            }
        );
        let f: AdminAction =
            serde_json::from_value(json!({"action": "folder_delete", "folder": 3})).unwrap();
        assert_eq!(f, AdminAction::FolderDelete { folder: Ref::Id(3) });
        let f: AdminAction =
            serde_json::from_value(json!({"action": "folder_delete", "folder": "Jira"})).unwrap();
        assert_eq!(
            f,
            AdminAction::FolderDelete {
                folder: Ref::Name("Jira".into())
            }
        );
        let back: AdminAction = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(back, a);
        assert!(serde_json::from_value::<AdminAction>(json!({"action": "secret_set"})).is_err());
    }

    #[test]
    fn links_only_reach_web_pages() {
        for ok in [
            "/",
            "/projects",
            "/projects/demo",
            "/secrets/3",
            "/nodes/abc",
            "/agents",
            "/items/5",
        ] {
            assert!(link_allowed(ok), "{ok}");
        }
        for bad in [
            "",
            "projects",
            "/login/x",
            "//evil.example",
            "/projects/../login",
            "/secrets/3/x",
            "/agents/x",
            "https://evil.example",
            "/items/",
        ] {
            assert!(!link_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn the_brain_only_changes_its_flags_in_a_node_command() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let current = v(&[
            r"C:\tools\claude.exe",
            "--add-dir",
            "D:\\shared",
            "--model",
            "sonnet",
        ]);
        // Set the model: the program and the owner's other arguments stay as they are.
        assert_eq!(
            merge_claude_flags(&current, Some("opus"), None).unwrap(),
            v(&[
                r"C:\tools\claude.exe",
                "--add-dir",
                "D:\\shared",
                "--model",
                "opus"
            ])
        );
        // Clear the model and add a fallback; `--flag=value` forms too.
        let eq_form = v(&["claude", "--model=opus", "--fallback-model=haiku", "-p"]);
        assert_eq!(
            merge_claude_flags(&eq_form, Some(""), Some("sonnet")).unwrap(),
            v(&["claude", "-p", "--fallback-model", "sonnet"])
        );
        // A wrapper's own flags, before the Claude program, are not Claude's to change.
        let wrapped = v(&[
            "run.sh",
            "--model",
            "w",
            "/opt/bin/claude.cmd",
            "--model",
            "x",
        ]);
        assert_eq!(
            merge_claude_flags(&wrapped, Some("opus"), None).unwrap(),
            v(&[
                "run.sh",
                "--model",
                "w",
                "/opt/bin/claude.cmd",
                "--model",
                "opus"
            ])
        );
        // No `claude` element (an npx package): Claude's flags follow the first element.
        let npx = v(&["npx", "--verbose", "@anthropic-ai/claude-code"]);
        assert_eq!(
            merge_claude_flags(&npx, Some("opus"), None).unwrap(),
            v(&[
                "npx",
                "--verbose",
                "@anthropic-ai/claude-code",
                "--model",
                "opus"
            ])
        );
        for bad in [
            "--dangerously-skip-permissions",
            "opus --add-dir /",
            "opus;rm",
            "-x",
        ] {
            assert!(
                merge_claude_flags(&current, Some(bad), None).is_err(),
                "{bad}"
            );
        }
        assert!(merge_claude_flags(&[], Some("opus"), None).is_err());
    }
    #[test]
    fn slugs() {
        assert!(valid_slug("web-2"));
        assert!(!valid_slug("Web") && !valid_slug("") && !valid_slug("a b"));
        assert!(!valid_slug("-web"), "nodes refuse a leading '-'");
    }
}
