//! The orchestrator's shared state and its handling of node events.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use fleet_net::{EndpointId, Inbound, Server};
use fleet_proto::{
    AgentEvent, AgentEventKind, Body, ClaudeAuth, Hello, NodeInfo, NodeSettings, ToolResult,
};
use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::{
    chat::{Chat, From, esc},
    config::Config,
    model::{NodeRow, TaskRow, status},
    vault::Vault,
};

pub struct Orchestrator {
    pub db: PgPool,
    pub vault: Vault,
    pub server: Server,
    pub chat: Arc<dyn Chat>,
    pub cfg: Config,
    /// Posts approved Slack replies; set once the feed source is configured.
    pub feed_poster: tokio::sync::RwLock<Option<Arc<dyn crate::feeds::ReplyPoster>>>,
    pub auth: crate::auth::DbAuth,
    /// The address put in invite tickets (set once the endpoint is bound).
    pub invite_addr: std::sync::RwLock<Option<fleet_net::EndpointAddr>>,
}

impl Orchestrator {
    pub fn new(
        db: PgPool,
        vault: Vault,
        server: Server,
        chat: Arc<dyn Chat>,
        cfg: Config,
        auth: crate::auth::DbAuth,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            vault,
            server,
            chat,
            cfg,
            feed_poster: Default::default(),
            auth,
            invite_addr: Default::default(),
        })
    }

    /// Loads pool bots into the chat client. Called once at startup.
    pub async fn load_bots(&self) -> Result<()> {
        let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as("select id, token from bots")
            .fetch_all(&self.db)
            .await?;
        for (id, token) in rows {
            match self.vault.decrypt(&token) {
                Ok(t) => {
                    if let Err(e) = self.chat.add_bot(id, &t).await {
                        tracing::warn!("bot {id} unusable: {e:#}");
                    }
                }
                Err(e) => tracing::warn!("bot {id} token cannot be decrypted: {e:#}"),
            }
        }
        Ok(())
    }

    /// Consumes node events forever.
    pub async fn run_node_events(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Inbound>) {
        // All nodes start offline; connections re-mark them.
        let _ = sqlx::query("update nodes set online = false")
            .execute(&self.db)
            .await;
        while let Some(ev) = events.recv().await {
            let this = self.clone();
            // Events of one node must stay ordered (output lines, exit after output), so they are
            // handled inline; handlers only do short database work and queue chat sends.
            if let Err(e) = this.handle_node_event(ev).await {
                tracing::error!("node event failed: {e:#}");
            }
        }
    }

    async fn handle_node_event(self: &Arc<Self>, ev: Inbound) -> Result<()> {
        match ev {
            Inbound::Connected { peer, hello } => {
                if let Some(h) = hello {
                    self.node_connected(peer, h).await?;
                }
            }
            Inbound::Disconnected { peer } => {
                sqlx::query("update nodes set online = false, last_seen = now() where id = $1")
                    .bind(peer.to_string())
                    .execute(&self.db)
                    .await?;
            }
            Inbound::Message { peer, body } => match body {
                Body::NodeInfo(info) => self.node_info(peer, info).await?,
                Body::AgentEvent(ev) => self.agent_event(peer, ev).await?,
                Body::ToolCall(call) => {
                    let this = self.clone();
                    // Tool calls may wait on Telegram or the database for a while; never block the
                    // event stream on them.
                    tokio::spawn(async move {
                        let call_id = call.call_id.clone();
                        let result = match crate::tools::handle(&this, peer, call).await {
                            Ok(Some(r)) => Some(r),
                            Ok(None) => None,
                            Err(e) => Some(ToolResult {
                                call_id: call_id.clone(),
                                ok: false,
                                result: format!("{e:#}"),
                            }),
                        };
                        if let Some(r) = result {
                            this.server.send(peer, Body::ToolResult(r));
                        }
                    });
                }
                other => {
                    tracing::warn!(node = %peer.fmt_short(), "unexpected message from node: {other:?}")
                }
            },
        }
        Ok(())
    }

    async fn node_connected(self: &Arc<Self>, peer: EndpointId, hello: Hello) -> Result<()> {
        let id = peer.to_string();
        sqlx::query(
            "insert into nodes (id, name, os, app_version, online, last_seen) values ($1, $2, $3, $4, true, now())
             on conflict (id) do update set name = $2, os = $3, app_version = $4, online = true, last_seen = now()",
        )
        .bind(&id)
        .bind(&hello.node_name)
        .bind(&hello.os)
        .bind(&hello.app_version)
        .execute(&self.db)
        .await?;
        self.push_node_settings(&id).await?;
        Ok(())
    }

    /// Sends a node how to start Claude. Called on connect and after edits in the web UI.
    pub async fn push_node_settings(&self, node_id: &str) -> Result<()> {
        let node = self.node(node_id).await?.context("unknown node")?;
        let auth = match (node.auth_mode.as_str(), &node.auth_secret) {
            ("oauth_token", Some(s)) => ClaudeAuth::OAuthToken(self.vault.decrypt(s)?),
            ("api_key", Some(s)) => ClaudeAuth::ApiKey(self.vault.decrypt(s)?),
            _ => ClaudeAuth::Default,
        };
        let settings = NodeSettings {
            claude_command: node.claude_command.0.clone(),
            auth,
            max_agents: node.max_agents.max(1) as u32,
        };
        self.server
            .send(node_id.parse()?, Body::NodeSettings(settings));
        Ok(())
    }

    async fn node_info(self: &Arc<Self>, peer: EndpointId, info: NodeInfo) -> Result<()> {
        let id = peer.to_string();
        sqlx::query("update nodes set projects = $2, running = $3, max_agents = $4, last_seen = now() where id = $1")
            .bind(&id)
            .bind(sqlx::types::Json(&info.projects))
            .bind(sqlx::types::Json(&info.running_agents))
            .bind(info.max_agents as i32)
            .execute(&self.db)
            .await?;
        // A task we think is running whose agent the node no longer runs has ended (e.g. the node
        // restarted while we were disconnected and the exit event was lost with its outbox).
        sqlx::query("update tasks set running = false where node_id = $1 and running and not (agent_id = any($2))")
            .bind(&id)
            .bind(&info.running_agents)
            .execute(&self.db)
            .await?;
        // Capacity or projects may have changed: try queued work.
        crate::tasks::dispatch_queued(self).await;
        Ok(())
    }

    async fn agent_event(self: &Arc<Self>, peer: EndpointId, ev: AgentEvent) -> Result<()> {
        let AgentEvent {
            agent_id,
            task_id,
            kind,
        } = ev;
        match kind {
            AgentEventKind::Started { .. } => {
                sqlx::query(
                    "update tasks set running = true, node_id = $2, updated_at = now(),
                     status = case when status = 'queued' then 'planning' else status end where id = $1",
                )
                .bind(&task_id)
                .bind(peer.to_string())
                .execute(&self.db)
                .await?;
                sqlx::query(
                    "update agents set last_active = now(), preferred_node = $2 where id = $1",
                )
                .bind(&agent_id)
                .bind(peer.to_string())
                .execute(&self.db)
                .await?;
            }
            AgentEventKind::Session { session_id } => {
                sqlx::query("update tasks set session_id = $2 where id = $1")
                    .bind(&task_id)
                    .bind(&session_id)
                    .execute(&self.db)
                    .await?;
            }
            AgentEventKind::Output { json } => {
                self.record(&task_id, "out", &json).await?;
            }
            AgentEventKind::Stderr { line } => {
                self.record(&task_id, "err", &line).await?;
            }
            AgentEventKind::Exited { code } => {
                self.record(&task_id, "exit", &format!("{code:?}")).await?;
                let task = self.task(&task_id).await?;
                sqlx::query("update tasks set running = false, updated_at = now() where id = $1")
                    .bind(&task_id)
                    .execute(&self.db)
                    .await?;
                // Cancel questions the dead session can no longer receive answers for.
                sqlx::query("update human_items set state = 'cancelled' where task_id = $1 and state = 'open' and kind in ('plan','question','permission')")
                    .bind(&task_id)
                    .execute(&self.db)
                    .await?;
                if let Some(task) = task
                    && task.status != status::DONE
                    && task.id != crate::model::BRAIN_TASK
                {
                    let new_status = if code == Some(0) {
                        status::STOPPED
                    } else {
                        status::FAILED
                    };
                    sqlx::query("update tasks set status = $2 where id = $1")
                        .bind(&task.id)
                        .bind(new_status)
                        .execute(&self.db)
                        .await?;
                    let note = format!(
                        "⏹ Agent session ended ({}). Reply in this topic to resume it.",
                        code.map_or("killed".into(), |c| format!("exit code {c}"))
                    );
                    self.post_task(&task, &note).await;
                }
                crate::tasks::dispatch_queued(self).await;
            }
            AgentEventKind::SpawnFailed { reason } => {
                self.record(&task_id, "spawn_failed", &reason).await?;
                sqlx::query("update tasks set running = false, status = 'failed', updated_at = now() where id = $1 and status <> 'done'")
                    .bind(&task_id)
                    .execute(&self.db)
                    .await?;
                if let Some(task) = self.task(&task_id).await? {
                    self.post_task(
                        &task,
                        &format!("⚠️ Could not start the agent: {}", esc(&reason)),
                    )
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn record(&self, task_id: &str, kind: &str, data: &str) -> Result<()> {
        sqlx::query("insert into task_events (task_id, kind, data) values ($1, $2, $3)")
            .bind(task_id)
            .bind(kind)
            .bind(data)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Posts in a task's topic as the task's agent (or the orchestrator if it has no bot).
    pub async fn post_task(&self, task: &TaskRow, html: &str) {
        let from = self.task_sender(task).await;
        if let Err(e) = self
            .chat
            .send(from, task.topic_id, html, vec![], None)
            .await
        {
            tracing::warn!("posting to {} failed: {e:#}", task.id);
        }
    }

    pub async fn task_sender(&self, task: &TaskRow) -> From {
        let Some(agent) = &task.agent_id else {
            return From::Orchestrator;
        };
        match sqlx::query_scalar::<_, Option<i64>>("select bot_id from agents where id = $1")
            .bind(agent)
            .fetch_optional(&self.db)
            .await
        {
            Ok(Some(Some(bot))) => From::Bot(bot),
            _ => From::Orchestrator,
        }
    }

    pub async fn node(&self, id: &str) -> Result<Option<NodeRow>> {
        Ok(sqlx::query_as("select * from nodes where id = $1")
            .bind(id)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn task(&self, id: &str) -> Result<Option<TaskRow>> {
        Ok(sqlx::query_as("select * from tasks where id = $1")
            .bind(id)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn audit(&self, actor: &str, action: &str, detail: serde_json::Value) {
        let _ = sqlx::query("insert into audit_log (actor, action, detail) values ($1, $2, $3)")
            .bind(actor)
            .bind(action)
            .bind(detail)
            .execute(&self.db)
            .await;
    }

    /// Periodic housekeeping: reminders and retention.
    pub async fn run_housekeeping(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            if let Err(e) = crate::human::remind(&self).await {
                tracing::warn!("reminders failed: {e:#}");
            }
            if let Err(e) = self.prune().await {
                tracing::warn!("retention pruning failed: {e:#}");
            }
        }
    }

    async fn prune(&self) -> Result<()> {
        let days = self.cfg.retention_days as f64;
        for table in ["task_events", "feed_messages", "audit_log"] {
            let col = if table == "task_events" || table == "audit_log" {
                "at"
            } else {
                "created_at"
            };
            sqlx::query(&format!(
                "delete from {table} where {col} < now() - make_interval(days => $1::int)"
            ))
            .bind(days as i32)
            .execute(&self.db)
            .await?;
        }
        sqlx::query("delete from magic_links where expires_at < now()")
            .execute(&self.db)
            .await?;
        sqlx::query("delete from web_sessions where expires_at < now()")
            .execute(&self.db)
            .await?;
        Ok(())
    }
}
