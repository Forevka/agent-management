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
    chat::{From, Notify, clip, esc},
    core::Orchestrator,
    human::{self, NewItem},
    jira,
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
    /// Which Jira a project maps to: its key there (`BIOINS`), the board whose active sprint
    /// counts as the owner's, and the credential folder only when it differs from the fleet's
    /// shared one. An empty string clears a field; the credential itself is never part of this.
    ProjectJira {
        project: String,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        board: Option<String>,
        #[serde(default)]
        folder: Option<String>,
    },
    /// A new issue in a project's Jira. Outward-facing, so it runs only once the owner approves.
    JiraCreate {
        project: String,
        summary: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        issue_type: Option<String>,
        #[serde(default)]
        parent: Option<String>,
    },
    /// Changed fields of an existing Jira issue, likewise only once the owner approves.
    JiraUpdate {
        project: String,
        issue: String,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        description: Option<String>,
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
    /// The agent's default Claude model and effort. Unset fields stay as they are; an empty
    /// string clears one (back to the fleet's default).
    AgentSettings {
        agent: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
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
    /// A habit: something the owner kept doing that the fleet does for them from now on (see
    /// `habits.rs`). `decide` needs `project`, `title_like` and `decision`; `do` needs
    /// `schedule` and `brief`. `why` is what it was learned from, in the owner's own terms,
    /// because that is what they read before deciding whether to have it.
    HabitAdd {
        kind: String,
        why: String,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        title_like: Option<String>,
        #[serde(default)]
        decision: Option<String>,
        #[serde(default)]
        schedule: Option<String>,
        #[serde(default)]
        brief: Option<String>,
    },
    HabitPause {
        habit: i64,
    },
    HabitRemove {
        habit: i64,
    },
}

/// An agent setting as given (None: unchanged; empty: back to the default), checked.
pub(crate) fn agent_settings_values(
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(Option<Option<String>>, Option<Option<String>>)> {
    let model = match model.map(str::trim) {
        None => None,
        Some("") => Some(None),
        Some(m) => {
            crate::model::check_model_value("model", m)?;
            Some(Some(m.to_owned()))
        }
    };
    let effort = match effort.map(str::trim) {
        None => None,
        Some("") => Some(None),
        Some(e) => {
            crate::model::check_effort(e)?;
            Some(Some(e.to_owned()))
        }
    };
    Ok((model, effort))
}

/// "model X, effort Y" for an agent's defaults, saying where it falls back to the default.
fn setting_label(model: &Option<String>, effort: &Option<String>) -> String {
    format!(
        "model {}, effort {}",
        model.as_deref().unwrap_or("(fleet default)"),
        effort.as_deref().unwrap_or("(Claude's default)")
    )
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
    // An empty value clears the flag; any other is a model name, by the fleet's one rule.
    for value in [model, fallback_model].into_iter().flatten() {
        if !value.is_empty() {
            crate::model::check_model_value("model", value)?;
        }
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
        !crate::model::is_builtin_agent(&a.id),
        "the orchestrator's and the secretary's agents cannot be changed"
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
        A::ProjectJira {
            project,
            key,
            board,
            folder,
        } => {
            tasks::project(o, project).await?;
            // A field given empty clears it; a field not given stays as it is.
            let text = |v: &Option<String>| v.as_ref().map(|s| s.trim().to_owned());
            let key_val = text(key)
                .map(|k| k.to_uppercase())
                .filter(|k| !k.is_empty());
            let folder_val = text(folder).filter(|f| !f.is_empty());
            let board_val = match text(board).filter(|b| !b.is_empty()) {
                Some(b) => Some(b.parse::<i32>().context("the board must be a number")?),
                None => None,
            };
            sqlx::query(
                "update projects set
                   jira_key    = case when $2 then $3 else jira_key end,
                   jira_board  = case when $4 then $5 else jira_board end,
                   jira_folder = case when $6 then $7 else jira_folder end
                 where slug = $1",
            )
            .bind(project)
            .bind(key.is_some())
            .bind(&key_val)
            .bind(board.is_some())
            .bind(board_val)
            .bind(folder.is_some())
            .bind(&folder_val)
            .execute(&o.db)
            .await?;
            o.audit(
                actor.audit_name(),
                "project.jira",
                json!({"project": project, "key": key_val, "board": board_val, "folder": folder_val}),
            )
            .await;
            Ok(format!(
                "project {project} now points at Jira {}",
                key_val.unwrap_or_else(|| "(cleared)".into())
            ))
        }
        A::JiraCreate {
            project,
            summary,
            description,
            issue_type,
            parent,
        } => {
            let j = jira::for_project(o, project).await?;
            let out = j
                .create(
                    summary,
                    description,
                    issue_type.as_deref().unwrap_or("Task"),
                    parent.as_deref(),
                )
                .await?;
            o.audit(
                actor.audit_name(),
                "jira.create",
                json!({"project": project, "created": out["created"]}),
            )
            .await;
            Ok(format!("created {}", out["created"]))
        }
        A::JiraUpdate {
            project,
            issue,
            summary,
            description,
        } => {
            let j = jira::for_project(o, project).await?;
            j.update(issue, summary.as_deref(), description.as_deref())
                .await?;
            o.audit(
                actor.audit_name(),
                "jira.update",
                json!({"project": project, "issue": issue}),
            )
            .await;
            Ok(format!("updated {issue}"))
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
        A::AgentSettings {
            agent: r,
            model,
            effort,
        } => {
            let a = agent(o, r).await?;
            let (model, effort) = agent_settings_values(model.as_deref(), effort.as_deref())?;
            sqlx::query(
                "update agents set model = case when $2 then $3 else model end, effort = case when $4 then $5 else effort end where id = $1",
            )
            .bind(&a.id)
            .bind(model.is_some())
            .bind(model.clone().flatten())
            .bind(effort.is_some())
            .bind(effort.clone().flatten())
            .execute(&o.db)
            .await?;
            // Absent: unchanged; "": cleared back to the default; else the new value.
            let mut detail = serde_json::Map::new();
            detail.insert("agent".into(), json!(a.id));
            for (k, v) in [("model", &model), ("effort", &effort)] {
                if let Some(v) = v {
                    detail.insert(k.into(), json!(v.clone().unwrap_or_default()));
                }
            }
            o.audit(actor.audit_name(), "agent.settings", Value::Object(detail))
                .await;
            Ok(format!(
                "{} now runs with {}; its running sessions keep theirs until they restart",
                a.name,
                setting_label(&model.unwrap_or(a.model), &effort.unwrap_or(a.effort))
            ))
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
            let cancelled: Vec<HumanItemRow> = sqlx::query_as(
                "update human_items set state = 'cancelled' where node_id = $1 and state = 'open' and kind in ('plan','question','permission') returning *",
            )
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            human::retire_copies(o, cancelled, "cancelled: the agent's node was revoked");
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
        // The owner's yes to a habit is the one thing that creates one: `prepare` has already
        // checked it and written the summary they read.
        A::HabitAdd {
            kind,
            why,
            project,
            title_like,
            decision,
            schedule,
            brief,
        } => {
            let id: i64 = sqlx::query_scalar(
                "insert into habits (kind, why, project, item_kind, title_like, decision, schedule, brief)
                 values ($1, $2, $3, case when $1 = 'decide' then 'plan' end, $4, $5, $6, $7)
                 returning id",
            )
            .bind(kind)
            .bind(why)
            .bind(project)
            .bind(title_like)
            .bind(decision)
            .bind(schedule)
            .bind(brief)
            .fetch_one(&o.db)
            .await?;
            o.audit(
                actor.audit_name(),
                "habit.add",
                json!({"habit": id, "kind": kind, "project": project, "title_like": title_like,
                       "decision": decision, "schedule": schedule}),
            )
            .await;
            // Its card goes where the fleet's standing instructions live, with Pause and Delete.
            if let Ok(Some(row)) =
                sqlx::query_as::<_, crate::model::HabitRow>("select * from habits where id = $1")
                    .bind(id)
                    .fetch_optional(&o.db)
                    .await
            {
                crate::habits::show(o, &row).await;
            }
            Ok(format!("habit H-{id} is on; /habits lists it"))
        }
        A::HabitPause { habit } => {
            crate::habits::pause(o, *habit, "you asked me to pause it").await?;
            Ok(format!("habit H-{habit} paused"))
        }
        A::HabitRemove { habit } => {
            let row: Option<crate::model::HabitRow> =
                sqlx::query_as("delete from habits where id = $1 returning *")
                    .bind(habit)
                    .fetch_optional(&o.db)
                    .await?;
            ensure!(row.is_some(), "no habit H-{habit}");
            o.audit(actor.audit_name(), "habit.delete", json!({"habit": habit}))
                .await;
            Ok(format!("habit H-{habit} deleted"))
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
    let push_error = o.node_settings_changed(&n.id).await.err().map(|e| {
        tracing::warn!("pushing settings to {} failed: {e:#}", n.name);
        format!("{e:#}")
    });
    Ok(SavedNode {
        node: n.name,
        push_error,
    })
}

// ------------------------------------------------- projects assigned to nodes (owner only)

/// Which code is cloned onto which machine is the owner's call alone: these are not
/// `AdminAction`s, so the brain cannot even propose them, and a call made for it is refused.
fn owner_only(actor: Actor) -> Result<()> {
    ensure!(
        actor == Actor::Owner,
        "only the owner decides which projects a node clones (its repository URL, the node's \
         projects root, the assignments), on the web UI; the orchestrator cannot change them"
    );
    Ok(())
}

/// Sets (or with `None` or blank, clears) where a project's code lives. Refuses a URL with
/// credentials in it: nodes clone with their own machine's git login. The nodes it is assigned
/// to hear of it at once, and a clone that failed there is tried again (with the new URL) by the
/// next task.
pub async fn set_repo_url(
    o: &Arc<Orchestrator>,
    actor: Actor,
    slug: &str,
    url: Option<&str>,
) -> Result<String> {
    owner_only(actor)?;
    let p = tasks::project(o, slug).await?;
    let url = url.map(str::trim).filter(|u| !u.is_empty());
    if let Some(u) = url {
        fleet_proto::repo::check_repo_url(u).map_err(anyhow::Error::msg)?;
    }
    sqlx::query("update projects set repo_url = $2 where slug = $1")
        .bind(&p.slug)
        .bind(url)
        .execute(&o.db)
        .await?;
    reset_assignments(o, None, Some(&p.slug)).await?;
    o.audit(
        actor.audit_name(),
        "project.repo_url",
        json!({"project": p.slug, "repo_url": url}),
    )
    .await;
    crate::node_projects::push_project(o, &p.slug).await?;
    // A task waiting for a node may now have one that can clone it.
    if url.is_some() {
        dispatch_queued_soon(o);
    }
    Ok(match url {
        Some(u) => format!(
            "{} is cloned from {u} on the nodes it is assigned to.",
            p.slug
        ),
        None => format!("{} has no repository URL; no node clones it.", p.slug),
    })
}

/// Sets (or with `None` or blank, clears) where a node puts the projects it clones. Must be an
/// absolute path; the node checks it again against its own OS. A clone that failed there is
/// tried again (in the new place) by the next task.
pub async fn set_projects_root(
    o: &Arc<Orchestrator>,
    actor: Actor,
    node_ref: &str,
    root: Option<&str>,
) -> Result<String> {
    owner_only(actor)?;
    let n = node(o, node_ref).await?;
    let root = root.map(str::trim).filter(|r| !r.is_empty());
    if let Some(r) = root {
        fleet_proto::repo::check_projects_root(r).map_err(anyhow::Error::msg)?;
    }
    sqlx::query("update nodes set projects_root = $2 where id = $1")
        .bind(&n.id)
        .bind(root)
        .execute(&o.db)
        .await?;
    reset_assignments(o, Some(&n.id), None).await?;
    o.audit(
        actor.audit_name(),
        "node.projects_root",
        json!({"node": n.id, "projects_root": root}),
    )
    .await;
    crate::node_projects::push(o, &n.id).await?;
    dispatch_queued_soon(o);
    Ok(match root {
        Some(r) => format!("{} clones projects into {r}.", n.name),
        None => format!(
            "{} clones projects into its default, ~/fleet-projects.",
            n.name
        ),
    })
}

/// Assigns a project to a node (`assign`), or takes it back. An assigned project with a
/// repository URL is cloned there when a task for it is placed on that node; taking it back
/// stops that and never deletes anything the node already cloned. Assigning one that is assigned
/// already clears a failed clone, like [`retry_assignment`].
pub async fn assign_project(
    o: &Arc<Orchestrator>,
    actor: Actor,
    node_ref: &str,
    slug: &str,
    assign: bool,
) -> Result<String> {
    owner_only(actor)?;
    let n = node(o, node_ref).await?;
    let p = tasks::project(o, slug).await?;
    let changed = if assign {
        sqlx::query(
            "insert into node_projects (node_id, project) values ($1, $2)
             on conflict (node_id, project) do update set state = null, detail = null
             where node_projects.state is not null",
        )
    } else {
        sqlx::query("delete from node_projects where node_id = $1 and project = $2")
    }
    .bind(&n.id)
    .bind(&p.slug)
    .execute(&o.db)
    .await?
    .rows_affected()
        == 1;
    if changed {
        o.audit(
            actor.audit_name(),
            if assign {
                "node.assign_project"
            } else {
                "node.unassign_project"
            },
            json!({"node": n.id, "project": p.slug}),
        )
        .await;
        crate::node_projects::push(o, &n.id).await?;
        if assign {
            dispatch_queued_soon(o);
        }
    }
    Ok(match (assign, p.repo_url.is_some()) {
        (true, true) => format!(
            "{} is assigned to {}: it clones it when a task needs it there.",
            p.slug, n.name
        ),
        (true, false) => format!(
            "{} is assigned to {}, but has no repository URL yet: set one on the project's page.",
            p.slug, n.name
        ),
        (false, _) => format!(
            "{} is no longer assigned to {}. Nothing on the node was deleted.",
            p.slug, n.name
        ),
    })
}

/// Lets an assignment whose clone failed take tasks again, so the next one tries the clone
/// anew (after the owner fixed what the failure said: a login on the machine, a URL, space).
pub async fn retry_assignment(
    o: &Arc<Orchestrator>,
    actor: Actor,
    node_ref: &str,
    slug: &str,
) -> Result<String> {
    owner_only(actor)?;
    let n = node(o, node_ref).await?;
    let p = tasks::project(o, slug).await?;
    let retried = sqlx::query(
        "update node_projects set state = null, detail = null
         where node_id = $1 and project = $2 and state = 'failed'",
    )
    .bind(&n.id)
    .bind(&p.slug)
    .execute(&o.db)
    .await?
    .rows_affected()
        == 1;
    if !retried {
        return Ok(format!(
            "{} has no failed clone on {}; nothing to retry.",
            p.slug, n.name
        ));
    }
    o.audit(
        actor.audit_name(),
        "node.retry_project",
        json!({"node": n.id, "project": p.slug}),
    )
    .await;
    // The node forgets the failure when it hears its assignments again, so the next task really
    // clones anew rather than being answered with the old reason.
    crate::node_projects::push(o, &n.id).await?;
    dispatch_queued_soon(o);
    Ok(format!(
        "{} will be cloned on {} again when a task needs it.",
        p.slug, n.name
    ))
}

/// Forgets what nodes last reported about assignments (of one node, or of one project): what
/// the owner just changed may well be what made a clone fail.
async fn reset_assignments(
    o: &Orchestrator,
    node_id: Option<&str>,
    project: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "update node_projects set state = null, detail = null
         where state is distinct from 'cloning'
           and ($1::text is null or node_id = $1) and ($2::text is null or project = $2)",
    )
    .bind(node_id)
    .bind(project)
    .execute(&o.db)
    .await?;
    Ok(())
}

/// Tries queued work in the background: a change just made a node able to run more of it.
fn dispatch_queued_soon(o: &Arc<Orchestrator>) {
    let o = o.clone();
    tokio::spawn(async move { tasks::dispatch_queued(&o).await });
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

/// Takes a bot the owner just created for the orchestrator to manage into the pool, and gives it
/// to an agent that has none.
///
/// Returns what to tell the owner. The token is fetched from Telegram rather than typed by
/// anyone, so it never passes through a chat message, a log line or the clipboard.
pub async fn adopt_managed_bot(o: &Orchestrator, bot_user_id: i64, token: &str) -> Result<String> {
    add_bot(o, token).await?;
    let bot: (i64, String) =
        sqlx::query_as("select id, username from bots where tg_user_id = $1 and agent_id is null")
            .bind(bot_user_id)
            .fetch_optional(&o.db)
            .await?
            .context("the new bot is not in the pool, or is already an agent's")?;
    let (bot_id, username) = bot;
    // The agent the ask was raised for comes first: the owner tapped a button that named it, so
    // handing the bot to a different one would not match what they were told. Failing that, the
    // oldest agent still speaking as the orchestrator. Builtin agents are skipped — the brain,
    // the secretary and the retrospective are the fleet's own voice and have no separate face.
    let asked_for: Option<String> = sqlx::query_scalar("select value->>0 from kv where key = $1")
        .bind(crate::tasks::ASKED_FOR)
        .fetch_optional(&o.db)
        .await?
        .flatten();
    let agent: Option<(String, String, String, Option<String>)> = sqlx::query_as(
        "select id, name, role, project from agents
         where bot_id is null and id <> all($1)
         order by (id = $2) desc, created_at limit 1",
    )
    .bind(crate::model::BUILTIN_AGENTS)
    .bind(asked_for.unwrap_or_default())
    .fetch_optional(&o.db)
    .await?;
    let Some((id, name, role, project)) = agent else {
        return Ok(format!(
            "Thanks — @{username} is in the pool and waiting for the next agent that needs one."
        ));
    };
    sqlx::query("update bots set agent_id = $2 where id = $1")
        .bind(bot_id)
        .bind(&id)
        .execute(&o.db)
        .await?;
    sqlx::query("update agents set bot_id = $2 where id = $1")
        .bind(&id)
        .bind(bot_id)
        .execute(&o.db)
        .await?;
    let proj = project.unwrap_or_else(|| "-".into());
    let (display, description) = crate::tasks::bot_profile(&name, &role, &proj);
    if let Err(e) = o.chat.set_bot_profile(bot_id, &display, &description).await {
        tracing::warn!("renaming bot @{username} failed: {e:#}");
    }
    o.audit(
        "owner",
        "bot.adopt",
        json!({"bot": username, "agent": id, "project": proj}),
    )
    .await;
    Ok(format!(
        "Thanks — @{username} is now {name}, the {role} on {proj}. It will speak for itself from its next run."
    ))
}

// ---------------------------------------------------------------- proposals

/// The warning on every proposal that gives a project's agents secret values.
const SECRETS_REACH: &str =
    "⚠️ The orchestrator can direct this project's agents, and so can reach these values.";

/// Whether `s` already looks like a raw Slack id (`C0123ABCD`), not a name to resolve.
fn looks_like_slack_id(s: &str) -> bool {
    s.len() <= 32 && !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A Slack channel id (`C0123ABCD`), or a name (`#code-review`/`code-review`) resolved against
/// every conversation the secretary has already seen (`feed_conversations`, filled in for every
/// conversation the owner is in, mapped or not — unlike `feed_channels`, which only has entries
/// once mapped, so this is the only place a name can be resolved before a channel is mapped).
async fn resolve_slack_channel(o: &Orchestrator, input: &str) -> Result<String> {
    let input = input.trim();
    ensure!(!input.is_empty(), "channel required");
    if looks_like_slack_id(input) {
        return Ok(input.to_owned());
    }
    let needle = input.trim_start_matches('#');
    let matches: Vec<(String, String)> = sqlx::query_as(
        "select conversation_id, name from feed_conversations
         where source = 'slack' and kind in ('channel', 'private')
           and lower(ltrim(name, '#')) = lower($1)",
    )
    .bind(needle)
    .fetch_all(&o.db)
    .await?;
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap().0),
        0 => bail!(
            "no Slack channel named {input:?} is known yet (the secretary learns a channel's \
             name once it sees a message there); give the channel id instead (like C0123ABCD)"
        ),
        _ => bail!("more than one Slack channel is named {input:?}; give the channel id instead"),
    }
}

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
        A::ProjectJira {
            project,
            key,
            board,
            folder,
        } => {
            let p = tasks::project(o, &project).await?;
            let text = |v: Option<String>| v.map(|s| s.trim().to_owned());
            let key = text(key).map(|k| k.to_uppercase());
            let folder = text(folder);
            let board = text(board);
            if let Some(k) = key.as_deref().filter(|k| !k.is_empty()) {
                ensure!(
                    (2..=20).contains(&k.len())
                        && k.starts_with(|c: char| c.is_ascii_uppercase())
                        && k.chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                    "{k:?} is not a Jira project key (like BIOINS or DAP)"
                );
            }
            if let Some(b) = board.as_deref().filter(|b| !b.is_empty()) {
                let n: i64 = b
                    .parse()
                    .with_context(|| format!("board {b:?} is not a number"))?;
                ensure!(n > 0, "the board must be positive");
            }
            // The folder must already hold the credential, or the brain's Jira calls would all
            // fail later with nothing to point at. Only key names are looked at, never values.
            if let Some(f) = folder.as_deref().filter(|f| !f.is_empty()) {
                let have: Vec<String> = sqlx::query_scalar(
                    "select i.key from secret_items i join secret_folders f on f.id = i.folder_id
                     where lower(f.name) = lower($1) and i.key = any($2)",
                )
                .bind(f)
                .bind(vec![
                    jira::URL_KEY.to_owned(),
                    jira::EMAIL_KEY.to_owned(),
                    jira::TOKEN_KEY.to_owned(),
                ])
                .fetch_all(&o.db)
                .await?;
                let missing: Vec<&str> = [jira::URL_KEY, jira::EMAIL_KEY, jira::TOKEN_KEY]
                    .into_iter()
                    .filter(|k| !have.iter().any(|h| h == k))
                    .collect();
                ensure!(
                    missing.is_empty(),
                    "the secret folder {f:?} has no {}; set them there first (the `jira` preset has all three)",
                    missing.join(", ")
                );
            }
            let mut summary = format!("Project {} in Jira:", p.slug);
            match key.as_deref() {
                Some("") => summary.push_str(" no key (its Jira is turned off)."),
                Some(k) => summary.push_str(&format!(" key {k}.")),
                None => {}
            }
            match board.as_deref() {
                Some("") => summary.push_str(" No board: every open sprint counts."),
                Some(b) => summary.push_str(&format!(" Board {b} decides \"my sprint\".")),
                None => {}
            }
            match folder.as_deref() {
                Some("") => summary.push_str(&format!(
                    " Credential from the fleet's shared folder {}.",
                    q(jira::DEFAULT_FOLDER)
                )),
                Some(f) => summary.push_str(&format!(" Credential from folder {}.", q(f))),
                None => {}
            }
            ensure!(
                key.is_some() || board.is_some() || folder.is_some(),
                "nothing to change: give a key, a board or a folder"
            );
            (
                A::ProjectJira {
                    project: p.slug,
                    key,
                    board,
                    folder,
                },
                summary,
            )
        }
        A::JiraCreate {
            project,
            summary,
            description,
            issue_type,
            parent,
        } => {
            let p = tasks::project(o, &project).await?;
            let key = project_jira_key(o, &p.slug).await?;
            let summary_text = summary.trim().to_owned();
            ensure!(!summary_text.is_empty(), "the issue needs a summary");
            check_len("summary", &summary_text, 250)?;
            let issue_type = issue_type
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty());
            if let Some(parent) = parent.as_deref() {
                ensure!(
                    jira::is_own_key(&key, parent),
                    "the parent {parent} is not an issue of {key}"
                );
            }
            let card = format!(
                "Create a {} in {key} (project {}){}. Summary: {}.{}",
                q(issue_type.as_deref().unwrap_or("Task")),
                p.slug,
                parent
                    .as_deref()
                    .map(|x| format!(" under {}", x.to_uppercase()))
                    .unwrap_or_default(),
                q(&summary_text),
                // The agent's own words come last and quoted, like every other proposal.
                if description.trim().is_empty() {
                    String::new()
                } else {
                    format!(" Description: {}.", q(&clip(description.trim(), 700)))
                }
            );
            (
                A::JiraCreate {
                    project: p.slug,
                    summary: summary_text,
                    description,
                    issue_type,
                    parent: parent.map(|x| x.to_uppercase()),
                },
                card,
            )
        }
        A::JiraUpdate {
            project,
            issue,
            summary,
            description,
        } => {
            let p = tasks::project(o, &project).await?;
            let key = project_jira_key(o, &p.slug).await?;
            let issue = issue.trim().to_uppercase();
            ensure!(
                jira::is_own_key(&key, &issue),
                "{issue} is not an issue of {key}, this project's Jira"
            );
            let summary = summary
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            let description = description.filter(|d| !d.trim().is_empty());
            ensure!(
                summary.is_some() || description.is_some(),
                "nothing to change: give a summary or a description"
            );
            if let Some(s) = summary.as_deref() {
                check_len("summary", s, 250)?;
            }
            let mut card = format!("Change {issue} in {key} (project {}).", p.slug);
            if let Some(s) = summary.as_deref() {
                card.push_str(&format!(" New summary: {}.", q(s)));
            }
            if let Some(d) = description.as_deref() {
                card.push_str(&format!(" New description: {}.", q(&clip(d.trim(), 700))));
            }
            (
                A::JiraUpdate {
                    project: p.slug,
                    issue,
                    summary,
                    description,
                },
                card,
            )
        }
        A::ChannelAdd {
            project,
            channel,
            triage,
        } => {
            let p = tasks::project(o, &project).await?;
            let ch = resolve_slack_channel(o, &channel).await?;
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
            let ch = resolve_slack_channel(o, &channel).await?;
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
        A::AgentSettings {
            agent: r,
            model,
            effort,
        } => {
            let a = agent(o, &r).await?;
            let (m, e) = agent_settings_values(model.as_deref(), effort.as_deref())?;
            // The brain proposes within the owner's allow-list, as for a task (the owner's own
            // edits on the web may use any valid value).
            o.cfg.check_allowed(
                m.clone().flatten().as_deref(),
                e.clone().flatten().as_deref(),
            )?;
            ensure!(
                m.is_some() || e.is_some(),
                "nothing to change: give model or effort (an empty value goes back to the default)"
            );
            let before = setting_label(&a.model, &a.effort);
            let after = setting_label(
                &m.clone().unwrap_or(a.model.clone()),
                &e.clone().unwrap_or(a.effort.clone()),
            );
            let summary = format!(
                "Set agent {}'s defaults ({}, project {}): {before} → {after}. Its tasks use them unless a task sets its own. Agent name: {}.",
                a.id,
                a.role,
                a.project.as_deref().unwrap_or("-"),
                q(&a.name)
            );
            (
                A::AgentSettings {
                    agent: a.id,
                    model: m.map(Option::unwrap_or_default),
                    effort: e.map(Option::unwrap_or_default),
                },
                summary,
            )
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
        // A habit answers for the owner from now on, so this is the summary that matters most:
        // it says exactly what will happen without them, and what it was learned from.
        A::HabitAdd {
            kind,
            why,
            project,
            title_like,
            decision,
            schedule,
            brief,
        } => {
            let living: i64 = sqlx::query_scalar("select count(*) from habits where active")
                .fetch_one(&o.db)
                .await?;
            ensure!(
                living < crate::habits::MAX_HABITS,
                "there are already {living} habits; the owner can remove one with /habits"
            );
            let why = why.trim().to_owned();
            ensure!(
                !why.is_empty(),
                "why is required: say what of the owner's own doing this was learned from"
            );
            check_len("why", &why, MAX_PROPOSED_DESCRIPTION)?;
            match kind.as_str() {
                "decide" => {
                    let project = project.context("project required for a decide habit")?;
                    let slug = tasks::project(o, &project).await?.slug;
                    // The fleet's own project is where its secrets reach sessions; the brain may
                    // not even bind a folder there, let alone approve work in it.
                    ensure!(
                        slug != o.cfg.brain_project,
                        "a habit cannot decide for the fleet's own project"
                    );
                    let pattern = title_like
                        .context("title_like required: which tasks this covers")?
                        .trim()
                        .to_owned();
                    crate::habits::validate_title_like(&pattern, &slug)?;
                    ensure!(
                        decision.as_deref() == Some("approve"),
                        "the only decision a habit may make is \"approve\""
                    );
                    // A habit the owner paused covers its shape for good: bringing it back is
                    // their call, not something the next reflection may propose around.
                    let clashes: Option<(i64, bool)> = sqlx::query_as(
                        "select id, active from habits
                          where kind = 'decide' and project = $1
                            and ($2 like title_like or title_like like $2)
                          order by id limit 1",
                    )
                    .bind(&slug)
                    .bind(&pattern)
                    .fetch_optional(&o.db)
                    .await?;
                    if let Some((other, active)) = clashes {
                        bail!(
                            "habit H-{other} already covers that{}; the owner changes it with \
                             /habits",
                            if active { "" } else { " (paused)" }
                        );
                    }
                    // What the owner is really being asked for: how much of this they have
                    // decided themselves, and how much they have never seen. The same bar
                    // reflection uses, so an accepted proposal means what the card says.
                    let shape = crate::habits::SHAPE_SQL.replace("{}", "t.title");
                    let (approved, revised, unseen): (i64, i64, i64) = sqlx::query_as(&format!(
                        "select
                           (select count(*) from human_items h join tasks t on t.id = h.task_id
                             where h.kind = 'plan' and h.state = 'answered'
                               and coalesce((h.answer->>'approved')::bool, false)
                               and h.answered_at > now() - make_interval(days => $3::int)
                               and t.project = $1 and {shape} like $2),
                           (select count(*) from human_items h join tasks t on t.id = h.task_id
                             where h.kind = 'plan' and h.state = 'answered'
                               and not coalesce((h.answer->>'approved')::bool, true)
                               and h.answered_at > now() - make_interval(days => $3::int)
                               and t.project = $1 and {shape} like $2),
                           (select count(*) from tasks t
                             where t.project = $1 and {shape} like $2
                               and not exists (select 1 from human_items h
                                                where h.task_id = t.id and h.kind = 'plan'
                                                  and h.state = 'answered'))"
                    ))
                    .bind(&slug)
                    .bind(&pattern)
                    .bind(crate::habits::WINDOW_DAYS as i32)
                    .fetch_one(&o.db)
                    .await?;
                    ensure!(
                        approved >= crate::habits::REPEATS,
                        "only {approved} plan(s) like {pattern:?} were approved by hand in \
                         {slug} in the last {} days; a habit needs {} of the owner's own \
                         decisions behind it",
                        crate::habits::WINDOW_DAYS,
                        crate::habits::REPEATS
                    );
                    ensure!(
                        revised == 0,
                        "the owner sent {revised} plan(s) like {pattern:?} back for changes; \
                         they have not settled on this"
                    );
                    let summary = format!(
                        "From now on, approve the plan of tasks in project {slug} whose title \
                         looks like {} — you approved {approved} of these by hand in the last {} \
                         days and sent none back{}. Nothing else changes: a plan that names a \
                         blocker, an open question or an unclear acceptance criterion still \
                         waits for you, so does a task with another question of its own open, \
                         and the habit stands aside for good once you send any plan of this \
                         shape back. At most 10 a day, each posted in the task's topic with a \
                         button that stops the task and pauses this. Note that it covers tasks \
                         I create, including ones I create after reading a ticket or a Slack \
                         message — their briefs are not part of what you are approving here. \
                         Learned from: {}.",
                        q(&pattern),
                        crate::habits::WINDOW_DAYS,
                        if unseen > 0 {
                            format!(
                                ", and it also matches {unseen} task(s) of that project whose \
                                 plans you never saw"
                            )
                        } else {
                            String::new()
                        },
                        q(&why)
                    );
                    (
                        A::HabitAdd {
                            kind: "decide".into(),
                            why,
                            project: Some(slug),
                            title_like: Some(pattern),
                            decision: Some("approve".into()),
                            schedule: None,
                            brief: None,
                        },
                        summary,
                    )
                }
                "do" => {
                    let schedule = schedule.context("schedule required for a do habit")?;
                    crate::habits::validate_schedule(&schedule)?;
                    let brief = brief
                        .context("brief required: what to ask for, in the owner's words")?
                        .trim()
                        .to_owned();
                    ensure!(!brief.is_empty(), "brief required");
                    check_len("brief", &brief, MAX_PROPOSED_DESCRIPTION)?;
                    // The project a standing duty belongs to, when it has one: a label the
                    // request carries and the owner reads, so it must name a real project.
                    let project = match project {
                        Some(p) if !p.trim().is_empty() => {
                            Some(tasks::project(o, p.trim()).await?.slug)
                        }
                        _ => None,
                    };
                    // Scheduled work in the fleet's own repository is the retrospective's to
                    // propose, one owner-tapped task at a time, never a standing duty.
                    ensure!(
                        project.as_deref() != Some(o.cfg.brain_project.as_str()),
                        "a standing duty cannot be in the fleet's own project"
                    );
                    // The owner restating a duty must not end in two habits doing it twice.
                    let same: Option<i64> = sqlx::query_scalar(
                        "select id from habits
                          where kind = 'do' and active and schedule = $1
                            and project is not distinct from $2
                          order by id limit 1",
                    )
                    .bind(&schedule)
                    .bind(&project)
                    .fetch_optional(&o.db)
                    .await?;
                    if let Some(id) = same {
                        bail!(
                            "habit H-{id} already does something on this schedule{}; change or pause it instead of adding another",
                            project
                                .as_deref()
                                .map(|p| format!(" in project {p}"))
                                .unwrap_or_default()
                        );
                    }
                    let summary = format!(
                        "From now on, {schedule} ({}), do this without being asked{}: {}. It \
                         reaches me as if you had just asked for it, at most 10 times a day; I \
                         report what I did and what I passed over, which /habits shows, and \
                         /habits pauses it. Learned from: {}.",
                        crate::habits::how_often(&schedule),
                        project
                            .as_deref()
                            .map(|p| format!(" in project {p}"))
                            .unwrap_or_default(),
                        q(&brief),
                        q(&why)
                    );
                    (
                        A::HabitAdd {
                            kind: "do".into(),
                            why,
                            project,
                            title_like: None,
                            decision: None,
                            schedule: Some(schedule),
                            brief: Some(brief),
                        },
                        summary,
                    )
                }
                other => bail!("a habit is \"decide\" or \"do\", not {other:?}"),
            }
        }
        A::HabitPause { habit } => {
            let row = habit_row(o, habit).await?;
            ensure!(row.active, "habit H-{habit} is already paused");
            let summary = format!(
                "Pause habit H-{habit} ({}), so I ask you again instead.",
                q(&crate::habits::describe(&row))
            );
            (A::HabitPause { habit }, summary)
        }
        A::HabitRemove { habit } => {
            let row = habit_row(o, habit).await?;
            let summary = format!(
                "Delete habit H-{habit} ({}).",
                q(&crate::habits::describe(&row))
            );
            (A::HabitRemove { habit }, summary)
        }
    })
}

async fn habit_row(o: &Orchestrator, id: i64) -> Result<crate::model::HabitRow> {
    sqlx::query_as("select * from habits where id = $1")
        .bind(id)
        .fetch_optional(&o.db)
        .await?
        .with_context(|| format!("no habit H-{id}"))
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
        // Only the fields being changed: the owner editing the other one on the web meanwhile
        // does not make the proposal stale.
        A::AgentSettings {
            agent,
            model,
            effort,
        } => {
            let row: Option<(Option<String>, Option<String>)> =
                sqlx::query_as("select model, effort from agents where id = $1")
                    .bind(agent)
                    .fetch_optional(&o.db)
                    .await?;
            match row {
                None => Value::Null,
                Some((m, e)) => {
                    let mut v = serde_json::Map::new();
                    if model.is_some() {
                        v.insert("model".into(), json!(m));
                    }
                    if effort.is_some() {
                        v.insert("effort".into(), json!(e));
                    }
                    Value::Object(v)
                }
            }
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
        // What the project points at: a change approved after the owner repointed its Jira
        // meanwhile would write to the wrong tracker.
        A::ProjectJira { project, .. }
        | A::JiraCreate { project, .. }
        | A::JiraUpdate { project, .. } => {
            let row: Option<(Option<String>, Option<i32>, Option<String>)> = sqlx::query_as(
                "select jira_key, jira_board, jira_folder from projects where slug = $1",
            )
            .bind(project)
            .fetch_optional(&o.db)
            .await?;
            match row {
                Some((key, board, folder)) => json!({"key": key, "board": board, "folder": folder}),
                None => Value::Null,
            }
        }
        // Two habits that cover the same ground would both act, so a proposal goes stale when
        // any habit appears, is paused or is removed in the meantime — a `do` proposal too,
        // which is why this does not filter by kind, project or active. For pause and remove,
        // what that habit is now.
        A::HabitAdd { .. } => {
            let rows: Vec<(
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                bool,
            )> = sqlx::query_as(
                "select id, kind, project, title_like, schedule, active from habits
                     order by id",
            )
            .fetch_all(&o.db)
            .await?;
            json!(rows)
        }
        A::HabitPause { habit } | A::HabitRemove { habit } => {
            let row: Option<(bool, String)> =
                sqlx::query_as("select active, why from habits where id = $1")
                    .bind(habit)
                    .fetch_optional(&o.db)
                    .await?;
            json!(row)
        }
    })
}

/// A project's Jira key, for validating an issue belongs to it without touching the credential.
async fn project_jira_key(o: &Orchestrator, project: &str) -> Result<String> {
    let key: Option<Option<String>> =
        sqlx::query_scalar("select jira_key from projects where slug = $1")
            .bind(project)
            .fetch_optional(&o.db)
            .await?;
    key.flatten().filter(|k| !k.is_empty()).with_context(|| {
        format!("project {project} has no Jira key yet; the owner sets it on the project's page")
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
/// Which pages the orchestrator may send the owner to. A whitelist rather than "any path",
/// because the brain chooses this and the link it produces is a login: the owner should never
/// be sent somewhere that is not a page of this UI. [`crate::web::safe_target`] is the second
/// check, on the way out of the redirect.
fn link_allowed(page: &str) -> bool {
    let Some(rest) = page.strip_prefix('/') else {
        return false;
    };
    // A filtered view is a page too — `/tasks?project=bioins&status=failed` is often exactly
    // the thing worth linking to. The query is allowed to carry only what the UI's own filters
    // use, so it cannot smuggle a quote, a space or a control character.
    let (rest, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    if let Some(q) = query {
        let ok = !q.is_empty()
            && q.len() <= 200
            && q.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '=' | '&' | '-' | '_' | '.' | '%' | '+')
            });
        if !ok {
            return false;
        }
    }
    let mut parts = rest.split('/');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some() {
        return false;
    }
    match (first, second) {
        (
            "" | "projects" | "secrets" | "nodes" | "agents" | "tasks" | "sessions" | "inbox"
            | "habits" | "retro" | "people",
            None,
        ) => true,
        ("projects" | "secrets" | "nodes" | "items" | "tasks" | "people", Some(s)) => {
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
        "page must be one of /, /tasks[/<id>], /sessions, /projects[/<slug>], /inbox, /agents, /nodes[/<node-id>], /secrets[/<folder-id>], /items/<id>, optionally with a filter query such as /tasks?project=bioins&status=failed"
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
        // The owner is asked to do something on that page.
        .send(From::Orchestrator, None, &html, vec![], None, Notify::Sound)
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
            let mut out = Vec::with_capacity(nodes.len());
            for n in &nodes {
                out.push(node_read_json(o, n).await?);
            }
            Value::Array(out)
        }
        "node" => node_read_json(o, &node(o, need()?).await?).await?,
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
        "habits" => crate::habits::json(o, true).await?,
        other => bail!(
            "unknown read {other:?}; use projects, project <slug>, agents [project], bots, nodes, node <id|name>, secrets, secret <folder>, status, items or habits"
        ),
    };
    Ok(serde_json::to_string_pretty(&v)?)
}

pub async fn projects_json(o: &Orchestrator) -> Result<Value> {
    // The Jira key comes along: the brain routes a ticket to the right project by it.
    let projects: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("select slug, name, description, jira_key from projects order by slug")
            .fetch_all(&o.db)
            .await?;
    let nodes: Vec<NodeRow> = sqlx::query_as("select * from nodes")
        .fetch_all(&o.db)
        .await?;
    let mut out = Vec::new();
    for (slug, name, description, jira) in projects {
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
        // The nodes it is assigned to (cloned there on demand), and how each stands.
        let assigned: Vec<Value> = crate::node_projects::project_json(o, &slug)
            .await?
            .iter()
            .map(|a| json!({"node": a["node"], "online": a["online"], "status": a["status"]}))
            .collect();
        out.push(json!({"slug": slug, "name": name, "description": description, "nodes": on, "assigned_nodes": assigned, "open_tasks": open, "jira": jira}));
    }
    Ok(Value::Array(out))
}

pub(crate) async fn project_json(o: &Orchestrator, slug: &str) -> Result<Value> {
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
    // Which Jira this project maps to, so the brain knows its key and board without guessing.
    // The folder is named, never its values: the credential stays in the orchestrator.
    let jira: (Option<String>, Option<i32>, Option<String>) =
        sqlx::query_as("select jira_key, jira_board, jira_folder from projects where slug = $1")
            .bind(&p.slug)
            .fetch_one(&o.db)
            .await?;
    Ok(json!({
        "slug": p.slug,
        "name": p.name,
        "description": p.description,
        "jira": match &jira.0 {
            Some(key) => json!({
                "key": key,
                "board": jira.1,
                "credential_folder": jira.2.clone().unwrap_or_else(|| crate::jira::DEFAULT_FOLDER.to_owned()),
            }),
            None => Value::Null,
        },
        // Who approves plans of its tasks (`plan_gate`); only the owner sets it, on the project
        // page.
        "plan_gate": p.plan_gate,
        // Where its code lives, for nodes it is assigned to that clone it; only the owner sets it.
        "repo_url": p.repo_url,
        // The nodes it is assigned to and how it stands there; a node's words framed as data.
        "assigned_nodes": crate::node_projects::project_json(o, &p.slug)
            .await?
            .iter()
            .map(|a| json!({
                "node": a["node"],
                "online": a["online"],
                "status": a["status"],
                "path": a["path"],
                "detail": a["detail"].as_str().map(|d| tasks::untrusted(d, 600)),
            }))
            .collect::<Vec<_>>(),
        "secret_folders": folders,
        "slack_channels": channels.iter().map(|(s, id, n, t)| json!({"source": s, "channel_id": id, "name": n, "triage": t})).collect::<Vec<_>>(),
        "nodes": on,
        "agents": agents_json(o, Some(&p.slug)).await?,
    }))
}

pub async fn agents_json(o: &Orchestrator, project: Option<&str>) -> Result<Value> {
    let rows: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "select a.id, a.name, a.role, a.project, b.username,
                (select count(*) from tasks t where t.agent_id = a.id and t.running) as running,
                a.model, a.effort
         from agents a left join bots b on b.id = a.bot_id
         where a.id <> all($2) and ($1::text is null or a.project = $1) order by a.project, a.name",
    )
    .bind(project)
    .bind([BRAIN_AGENT, crate::model::SECRETARY_AGENT])
    .fetch_all(&o.db)
    .await?;
    let mut out = Vec::new();
    for (id, name, role, project, bot, running, model, effort) in rows {
        let recent: Vec<String> = sqlx::query_scalar("select id || ': ' || title || ' [' || status || ']' from tasks where agent_id = $1 order by updated_at desc limit 5")
            .bind(&id)
            .fetch_all(&o.db)
            .await?;
        // Its defaults (null: the fleet's model, Claude's default effort).
        out.push(json!({"id": id, "name": name, "role": role, "project": project, "bot": bot, "running": running > 0, "model": model, "effort": effort, "recent_tasks": recent}));
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
        // Free space where its sessions write; `state: full` means new task runs wait elsewhere
        // or in the queue until space is freed.
        "disk": crate::disk::json(n),
    })
}

/// [`node_json`] with the projects assigned to the node and how each stands there (mapped,
/// cloning, failed and why, or waiting for a task to need it).
async fn node_read_json(o: &Orchestrator, n: &NodeRow) -> Result<Value> {
    let mut v = node_json(n);
    v["projects_root"] = json!(n.projects_root);
    let mut assigned = crate::node_projects::node_json(o, n).await?;
    // The node's report quotes git, and git quotes the remote (`remote:` lines): data to read,
    // never instructions.
    for a in &mut assigned {
        if let Some(d) = a["detail"].as_str() {
            a["detail"] = json!(tasks::untrusted(d, 600));
        }
    }
    v["assigned_projects"] = json!(assigned);
    Ok(v)
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
        "select count(*) filter (where status in ('planning','running','waiting')), count(*) filter (where status = 'queued') from tasks where id not in ('brain', 'secretary')",
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
        "nodes": nodes.iter().map(|n| json!({"name": n.name, "online": n.online, "running": n.running.0.len(), "max_agents": n.max_agents, "projects": n.projects.0.len(), "disk": n.disk.as_ref().map(|_| n.disk_state.as_str())})).collect::<Vec<_>>(),
        "tasks": {"active": active, "queued": queued},
        "waiting_on_owner": open,
        "free_bots": free_bots,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_id_shape() {
        assert!(looks_like_slack_id("C0123ABCD"));
        assert!(looks_like_slack_id("C1"));
        assert!(!looks_like_slack_id(""));
        assert!(!looks_like_slack_id("code-review"));
        assert!(!looks_like_slack_id("#code-review"));
        assert!(!looks_like_slack_id(&"C".repeat(33)));
    }

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
            // The pages the new UI added, and the filtered views worth linking to: the point of
            // a link is to land on the exact thing being discussed.
            "/tasks",
            "/tasks/demo-BS-1",
            "/sessions",
            "/inbox",
            "/sessions?project=dap",
            "/tasks?project=bioins&status=failed",
            "/tasks?q=jira%20token",
            "/people",
            "/people/4",
            "/people?tag=night-owl&language=uk",
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
            "/people/4/edit",
            "/people/../login",
            // A query is for the UI's filters, not a place to hide a payload.
            "/tasks?",
            "/tasks?q=<script>",
            "/tasks?q=a b",
            "/tasks?q='",
            "/tasks?q=a\nb",
            &format!("/tasks?q={}", "x".repeat(300)),
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
