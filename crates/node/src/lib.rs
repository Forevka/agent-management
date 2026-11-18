//! The node role: runs Claude Code agents for the orchestrator on this host.
//!
//! A node owns three things: its local configuration (which project lives in which directory,
//! how to start Claude), the agent processes it supervises, and a loopback IPC endpoint that the
//! `fleet mcp` shim inside each agent uses to reach the orchestrator.

pub mod agent_cli;
pub mod config;
pub mod ipc;
pub mod mcp;
pub mod runner;
pub mod secrets;
pub mod skill;
pub mod tools;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use fleet_net::{Client, Inbound};
use fleet_proto::{AgentEvent, AgentEventKind, Body, NodeInfo, SpawnAgent, ToolCall, ToolResult};
use tokio::sync::{mpsc, oneshot};

use crate::{config::NodeConfig, runner::AgentHandle};

/// Ordinary tool calls answer within seconds; one that does not is reported as failed so the
/// agent's command returns instead of hanging. Permission prompts wait for the human instead.
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A tool call waiting for the orchestrator's answer.
struct PendingCall {
    reply: oneshot::Sender<ToolResult>,
    agent_id: String,
    run_id: String,
    /// Set for the permission tool, so an approved `ExitPlanMode` can switch the session's mode.
    permission_tool: Option<String>,
    /// Kept for permission prompts: they are sent again after every reconnect, because a
    /// restarted orchestrator may have lost them (it answers duplicates idempotently).
    resend: Option<ToolCall>,
}

pub struct Node {
    data_dir: PathBuf,
    /// This executable; agents start it as their MCP server (`fleet mcp`).
    exe: PathBuf,
    client: Client,
    config: Mutex<NodeConfig>,
    agents: Mutex<HashMap<String, AgentHandle>>,
    pending: Mutex<HashMap<String, PendingCall>>,
    next_call: AtomicU64,
    /// Makes call ids unique across node restarts, so a stale answer can never match a new call.
    call_prefix: String,
    ipc_addr: Mutex<Option<std::net::SocketAddr>>,
}

impl Node {
    pub fn new(data_dir: PathBuf, exe: PathBuf, client: Client) -> Result<Arc<Self>> {
        let config = NodeConfig::load(&data_dir)?;
        Ok(Arc::new(Self {
            data_dir,
            exe,
            client,
            config: Mutex::new(config),
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_call: AtomicU64::new(1),
            call_prefix: runner::random_token()[..8].to_owned(),
            ipc_addr: Mutex::new(None),
        }))
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Processes link events until the link's event channel closes.
    pub async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Inbound>) -> Result<()> {
        let addr = ipc::serve(self.clone()).await?;
        *self.ipc_addr.lock().unwrap() = Some(addr);
        tokio::spawn(self.clone().watch_config());

        while let Some(ev) = events.recv().await {
            match ev {
                Inbound::Connected { .. } => {
                    self.send_info();
                    self.resend_pending_permissions();
                }
                Inbound::Disconnected { .. } => {}
                Inbound::Message { body, .. } => self.handle(body),
            }
        }
        Ok(())
    }

    fn handle(self: &Arc<Self>, body: Body) {
        match body {
            Body::SpawnAgent(spawn) => self.spawn_agent(*spawn),
            Body::StopAgent(stop) => {
                // Only the run of that task (and that run, when named): the agent may have
                // moved on to another task, or the task to a newer run.
                if let Some(h) = self
                    .agents
                    .lock()
                    .unwrap()
                    .get(&stop.agent_id)
                    .filter(|h| h.task_id == stop.task_id)
                    .filter(|h| stop.run_id.is_empty() || h.run_id == stop.run_id)
                {
                    h.stop(stop.graceful);
                }
            }
            Body::SendToAgent {
                agent_id,
                task_id,
                text,
            } => {
                let delivered = self
                    .agents
                    .lock()
                    .unwrap()
                    .get(&agent_id)
                    .filter(|h| h.task_id == task_id)
                    .is_some_and(|h| h.send_user_message(&text));
                if !delivered {
                    // Hand the text back so the orchestrator can resume the session with it.
                    self.event(
                        &agent_id,
                        &task_id,
                        "",
                        AgentEventKind::Undelivered { text },
                    );
                }
            }
            Body::SendToRun {
                agent_id,
                task_id,
                run_id,
                text,
            } => {
                // Only the run it was meant for may take it: a newer run of the same agent is a
                // different session, and a message for a replaced run must go back.
                let delivered = self
                    .agents
                    .lock()
                    .unwrap()
                    .get(&agent_id)
                    .filter(|h| h.task_id == task_id && h.run_id == run_id)
                    .is_some_and(|h| h.send_user_message(&text));
                if !delivered {
                    self.event(
                        &agent_id,
                        &task_id,
                        &run_id,
                        AgentEventKind::Undelivered { text },
                    );
                }
            }
            Body::ToolResult(result) => self.resolve(result),
            Body::NodeSettings(settings) => {
                let mut cfg = self.config.lock().unwrap();
                cfg.settings = Some(settings);
                if let Err(e) = cfg.save(&self.data_dir) {
                    tracing::warn!("could not save node settings: {e:#}");
                }
            }
            other => tracing::warn!("unexpected message for a node: {other:?}"),
        }
    }

    fn spawn_agent(self: &Arc<Self>, spawn: SpawnAgent) {
        let (agent_id, task_id) = (spawn.agent_id.clone(), spawn.task_id.clone());
        if let Some(old) = self.agents.lock().unwrap().remove(&agent_id) {
            // The orchestrator only respawns an agent it believes is gone; stop a stale process.
            old.stop(false);
        }
        let run_id = spawn.run_id.clone();
        let cfg = self.config.lock().unwrap().clone();
        let ipc_addr = self
            .ipc_addr
            .lock()
            .unwrap()
            .expect("ipc server starts before the link");
        match runner::start(
            self.clone(),
            &cfg,
            &self.data_dir,
            &self.exe,
            ipc_addr,
            spawn,
        ) {
            Ok(handle) => {
                self.agents.lock().unwrap().insert(agent_id, handle);
                self.send_info();
            }
            Err(e) => self.event(
                &agent_id,
                &task_id,
                &run_id,
                AgentEventKind::SpawnFailed {
                    reason: format!("{e:#}"),
                },
            ),
        }
    }

    /// How many agent processes this node runs now.
    pub fn running(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    /// Stops every agent and waits (up to `limit`) for them to exit, which also removes their
    /// secrets files. For when this node must not keep working: the orchestrator refused it.
    pub async fn stop_all(&self, limit: std::time::Duration) {
        for h in self.agents.lock().unwrap().values() {
            h.stop(false);
        }
        let deadline = tokio::time::Instant::now() + limit;
        while !self.agents.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Called by the runner when an agent process is gone.
    pub(crate) fn agent_exited(&self, agent_id: &str, token: &str) {
        let mut agents = self.agents.lock().unwrap();
        // A respawn may already have replaced this handle; only remove our own.
        if agents.get(agent_id).is_some_and(|h| h.ipc_token == token) {
            agents.remove(agent_id);
        }
        drop(agents);
        // Fail any tool call the dead process was waiting on, so nothing leaks.
        let live: Vec<String> = self
            .agents
            .lock()
            .unwrap()
            .values()
            .map(|h| h.run_id.clone())
            .collect();
        self.pending
            .lock()
            .unwrap()
            .retain(|_, p| live.contains(&p.run_id));
        self.send_info();
    }

    /// Stops one run (if it is still the agent's current one).
    pub(crate) fn stop_run(&self, agent_id: &str, run_id: &str, graceful: bool) {
        if let Some(h) = self
            .agents
            .lock()
            .unwrap()
            .get(agent_id)
            .filter(|h| h.run_id == run_id)
        {
            h.stop(graceful);
        }
    }

    pub(crate) fn event(&self, agent_id: &str, task_id: &str, run_id: &str, kind: AgentEventKind) {
        self.client.send(Body::AgentEvent(AgentEvent {
            agent_id: agent_id.into(),
            task_id: task_id.into(),
            kind,
            run_id: run_id.into(),
        }));
    }

    fn resend_pending_permissions(&self) {
        let calls: Vec<ToolCall> = self
            .pending
            .lock()
            .unwrap()
            .values()
            .filter_map(|p| p.resend.clone())
            .collect();
        for call in calls {
            self.client.send(Body::ToolCall(call));
        }
    }

    fn send_info(&self) {
        let (projects, max_agents) = {
            let cfg = self.config.lock().unwrap();
            (
                cfg.projects.clone(),
                cfg.settings.as_ref().map_or(4, |s| s.max_agents),
            )
        };
        let (running_agents, running_runs) = {
            let agents = self.agents.lock().unwrap();
            (
                agents.keys().cloned().collect(),
                agents.values().map(|h| h.run_id.clone()).collect(),
            )
        };
        self.client.send(Body::NodeInfo(NodeInfo {
            projects,
            max_agents,
            running_agents,
            running_runs,
        }));
    }

    /// Forwards an agent's tool call to the orchestrator and waits for the answer.
    pub(crate) async fn call_tool(
        &self,
        ipc_token: &str,
        tool: String,
        args: String,
    ) -> ToolResult {
        let Some((agent_id, task_id, run_id)) = self
            .agents
            .lock()
            .unwrap()
            .values()
            .find(|h| h.ipc_token == ipc_token)
            .map(|h| (h.agent_id.clone(), h.task_id.clone(), h.run_id.clone()))
        else {
            return ToolResult {
                call_id: String::new(),
                ok: false,
                result: "unknown agent token".into(),
            };
        };
        let call_id = format!(
            "{}-{}",
            self.call_prefix,
            self.next_call.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        let permission_tool = (tool == tools::PERMISSION_TOOL)
            .then(|| serde_json::from_str::<serde_json::Value>(&args).ok())
            .flatten()
            .and_then(|v| {
                v.get("tool_name")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned)
            });
        let is_permission = tool == tools::PERMISSION_TOOL;
        let call = ToolCall {
            call_id: call_id.clone(),
            agent_id: agent_id.clone(),
            task_id,
            tool,
            args,
        };
        self.pending.lock().unwrap().insert(
            call_id.clone(),
            PendingCall {
                reply: tx,
                agent_id,
                run_id,
                permission_tool,
                resend: is_permission.then(|| call.clone()),
            },
        );
        self.client.send(Body::ToolCall(call));
        let failed = |why: &str| ToolResult {
            call_id: call_id.clone(),
            ok: false,
            result: why.into(),
        };
        if is_permission {
            return rx
                .await
                .unwrap_or_else(|_| failed("agent stopped before the answer arrived"));
        }
        match tokio::time::timeout(TOOL_CALL_TIMEOUT, rx).await {
            Ok(r) => r.unwrap_or_else(|_| failed("agent stopped before the answer arrived")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&call_id);
                failed(
                    "the orchestrator did not answer in time (it may be restarting); try again in a minute",
                )
            }
        }
    }

    fn resolve(&self, result: ToolResult) {
        let Some(pending) = self.pending.lock().unwrap().remove(&result.call_id) else {
            // A replayed result for a call that already finished, or for an agent that exited.
            return;
        };
        if result.ok
            && pending.permission_tool.as_deref() == Some("ExitPlanMode")
            && is_allow(&result.result)
        {
            // The human approved the plan. Switch the session out of plan mode before Claude
            // sees the approval, so its next tool calls already run in the execution mode.
            if let Some(h) = self.agents.lock().unwrap().get(&pending.agent_id) {
                h.switch_to_after_plan_mode();
            }
        }
        let _ = pending.reply.send(result);
    }

    /// Re-reads the local config every few seconds so `fleet project map` takes effect
    /// without restarting the service.
    async fn watch_config(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Ok(fresh) = NodeConfig::load(&self.data_dir) else {
                continue;
            };
            let changed = {
                let mut cfg = self.config.lock().unwrap();
                let changed = cfg.projects != fresh.projects;
                cfg.projects = fresh.projects;
                if fresh.settings.is_some() {
                    cfg.settings = fresh.settings;
                }
                changed
            };
            if changed {
                self.send_info();
            }
        }
    }
}

fn is_allow(decision: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(decision)
        .ok()
        .and_then(|v| {
            v.get("behavior")
                .and_then(|b| b.as_str())
                .map(|b| b == "allow")
        })
        .unwrap_or(false)
}
