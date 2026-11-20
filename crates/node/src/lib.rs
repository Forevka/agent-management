//! The node role: runs Claude Code agents for the orchestrator on this host.
//!
//! A node owns three things: its local configuration (which project lives in which directory,
//! how to start Claude), the agent processes it supervises, and a loopback IPC endpoint that the
//! `fleet mcp` shim inside each agent uses to reach the orchestrator.

pub mod agent_cli;
pub mod checkout;
pub mod disk;
/// `node.json` and `status.json` live in their own crate so the tray app can use them without
/// compiling the network stack; re-exported here so `fleet_node::config` keeps working.
pub use fleet_node_config as config;
pub mod ipc;
pub mod mcp;
pub mod runner;
pub mod secrets;
pub mod skill;
pub mod tools;
pub mod update;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use fleet_net::{Client, Inbound};
use fleet_proto::{
    AgentEvent, AgentEventKind, Body, NodeInfo, NodeSettings, ProjectAssignments,
    ProjectSetupReport, ProjectSummary, SpawnAgent, ToolCall, ToolResult,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::{NodeConfig, status::Status},
    runner::AgentHandle,
};

/// Ordinary tool calls answer within seconds; one that does not is reported as failed so the
/// agent's command returns instead of hanging. Permission prompts wait for the human instead.
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the node reports its free disk space (besides on every connect). A busy session
/// writes transcripts fast, but not gigabytes in minutes, so this sees a disk filling up in time.
const DISK_REPORT_EVERY: Duration = Duration::from_secs(300);
/// How long one measurement may take before the node stops waiting for it.
const DISK_MEASURE_LIMIT: Duration = Duration::from_secs(120);

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

/// A run whose project is being cloned before it can start (see `Node::prepare_then_spawn`).
struct Preparing {
    task_id: String,
    run_id: String,
    /// Messages for the run that arrived while it waited: delivered once it has started, as they
    /// would have been had it been running.
    texts: Vec<String>,
}

/// What `status.json` reports besides the node's identity.
#[derive(Default)]
struct LinkStatus {
    connected: bool,
    /// The orchestrator's projects, once it has sent them.
    projects: Option<Vec<ProjectSummary>>,
}

pub struct Node {
    data_dir: PathBuf,
    /// This executable; agents start it as their MCP server (`fleet mcp`).
    exe: PathBuf,
    client: Client,
    /// The name this node joined with, shown by the tray app.
    node_name: String,
    /// This process's start stamp, written next to its pid so a reused pid is not taken for it.
    started: Option<u64>,
    /// Held while `status.json` is written, so writes land in the order of the events.
    status: Mutex<LinkStatus>,
    config: Mutex<NodeConfig>,
    agents: Mutex<HashMap<String, AgentHandle>>,
    pending: Mutex<HashMap<String, PendingCall>>,
    next_call: AtomicU64,
    /// Makes call ids unique across node restarts, so a stale answer can never match a new call.
    call_prefix: String,
    ipc_addr: Mutex<Option<std::net::SocketAddr>>,
    /// The release this node should move to. One task (`update_loop`, started by the first
    /// notice) works towards whatever this holds, so a newer notice redirects it instead of
    /// starting a second download beside it, and a repeated one (every reconnect sends one) is a
    /// no-op.
    update_target: tokio::sync::watch::Sender<Option<fleet_proto::UpdateAvailable>>,
    update_started: AtomicBool,
    /// Set while a disk report is being measured, so a slow walk is never started twice. Stays
    /// set until the measurement really returns, even after the node stopped waiting for it, so
    /// a hung drive ties up one blocking thread rather than a new one every few minutes.
    measuring_disk: AtomicBool,
    /// The orchestrator on the current connection asked for disk reports
    /// (`Body::DiskReportWanted`). An orchestrator from before them never asks — it could not
    /// decode a report, and an undecodable frame drops the link — so a node never sends one
    /// unasked. Cleared on every disconnect: the next connection may be to an older orchestrator
    /// (a rollback).
    disk_wanted: AtomicBool,
    /// A report was asked for while one was being measured: measure again when it is done.
    disk_again: AtomicBool,
    /// The projects the owner assigned to this node, as the orchestrator last sent them
    /// (`Body::ProjectAssignments`). Kept across reconnects: a spawn sent before one may arrive
    /// after it.
    assignments: Mutex<Option<ProjectAssignments>>,
    /// The orchestrator on the current connection sent assignments, so it reads setup reports.
    /// Cleared on every disconnect, like `disk_wanted`: the next one may be an older release.
    assignments_heard: AtomicBool,
    /// One lock per project, so a project is cloned once however many spawns wait for it.
    clones: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Runs waiting for their project to be cloned, by agent. They count as running (in
    /// `NodeInfo`, and for a self-update), so neither the orchestrator nor an update takes them
    /// for gone. Lock order: `agents` before this.
    preparing: Mutex<HashMap<String, Preparing>>,
}

impl Node {
    pub fn new(
        data_dir: PathBuf,
        exe: PathBuf,
        client: Client,
        node_name: String,
    ) -> Result<Arc<Self>> {
        let config = NodeConfig::load(&data_dir)?;
        let node = Arc::new(Self {
            data_dir,
            exe,
            client,
            node_name,
            started: config::process::start_time(std::process::id()),
            status: Mutex::new(LinkStatus::default()),
            config: Mutex::new(config),
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_call: AtomicU64::new(1),
            call_prefix: runner::random_token()[..8].to_owned(),
            ipc_addr: Mutex::new(None),
            update_target: tokio::sync::watch::Sender::new(None),
            update_started: AtomicBool::new(false),
            measuring_disk: AtomicBool::new(false),
            disk_again: AtomicBool::new(false),
            disk_wanted: AtomicBool::new(false),
            assignments: Mutex::new(None),
            assignments_heard: AtomicBool::new(false),
            clones: Mutex::new(HashMap::new()),
            preparing: Mutex::new(HashMap::new()),
        });
        // Replaces whatever a previous node process left, so the tray sees this pid at once.
        node.update_status(|_| {});
        Ok(node)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Moves this node to the release the orchestrator named, in the background: download and
    /// check it, swap it in once no agent is running, then exit so the supervisor starts the new
    /// binary. Returns whether an update is now under way. See `update`.
    ///
    /// `allow_downgrade` is for a refusal's hint only: an orchestrator rolled back to an older
    /// protocol may need its nodes to follow it down. A notice on a live link never moves a node
    /// backwards — a notice queued while it was off may be older than what was installed since.
    pub fn start_update(
        self: &Arc<Self>,
        u: fleet_proto::UpdateAvailable,
        allow_downgrade: bool,
    ) -> bool {
        use fleet_proto::{BUILD_VERSION, release_version};
        if u.version == BUILD_VERSION {
            // The orchestrator runs what this node runs — it may have been rolled back after
            // offering something newer. Whatever was pending is no longer wanted.
            self.update_target.send_if_modified(|t| t.take().is_some());
            return false;
        }
        if !update::enabled(&self.exe) {
            tracing::info!(
                "{} is available; this node does not replace its own binary (no supervisor asked it to, \
                 or its directory is not writable), so it is updated by hand or by its host",
                u.version
            );
            return false;
        }
        let Some(to) = release_version(&u.version) else {
            return false;
        };
        if !allow_downgrade
            && let Some(running) = release_version(BUILD_VERSION)
            && to <= running
        {
            return false;
        }
        if update::rejected(&self.exe, &u.version) {
            tracing::warn!(
                "{} is available, but it was rolled back on this machine before; waiting for the next release",
                u.version
            );
            return false;
        }
        self.update_target.send_if_modified(|t| {
            let changed = t.as_ref() != Some(&u);
            *t = Some(u);
            changed
        });
        if !self.update_started.swap(true, Ordering::SeqCst) {
            tokio::spawn(self.clone().update_loop());
        }
        true
    }

    /// Works towards `update_target` until it is installed (the process then exits), retrying a
    /// failed attempt with backoff — a release's binaries appear minutes after the orchestrator
    /// starts offering it — and starting over whenever a newer target arrives.
    async fn update_loop(self: Arc<Self>) {
        let mut rx = self.update_target.subscribe();
        let mut backoff = update::RETRY_MIN;
        loop {
            let target = rx.borrow_and_update().clone();
            if let Some(u) = target {
                match self.update_to(&u, &mut rx).await {
                    // A newer target arrived while this one waited: go straight to it.
                    Ok(()) => {
                        backoff = update::RETRY_MIN;
                        continue;
                    }
                    Err(e) => tracing::warn!(
                        "update to {} failed: {e:#}; trying again in {} min",
                        u.version,
                        backoff.as_secs() / 60
                    ),
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {
                        backoff = (backoff * 2).min(update::RETRY_MAX);
                        continue;
                    }
                    changed = rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        backoff = update::RETRY_MIN;
                        continue;
                    }
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// One attempt at `u`. Exits the process once it is swapped in; returns `Ok` when a newer
    /// target replaced `u` while it waited, and an error when the attempt failed.
    async fn update_to(
        &self,
        u: &fleet_proto::UpdateAvailable,
        rx: &mut tokio::sync::watch::Receiver<Option<fleet_proto::UpdateAvailable>>,
    ) -> Result<()> {
        use std::time::{Duration, Instant};
        tracing::info!("updating {} -> {}", fleet_proto::BUILD_VERSION, u.version);
        let staged = update::prepare(u, &self.exe).await?;
        let superseded =
            |rx: &mut tokio::sync::watch::Receiver<_>| rx.has_changed().unwrap_or(true);
        // Nobody's work is cut off unless it has to be: wait for the agents to finish, up to
        // the deadline — a day while some run waits on the owner, so an update never throws away
        // a pending plan or question — after which the runs are stopped. The next message to
        // their tasks picks them up again from their sessions.
        let started = Instant::now();
        let mut last_said = None::<Instant>;
        loop {
            if superseded(rx) {
                let _ = std::fs::remove_file(&staged);
                return Ok(());
            }
            // A run waiting for its project's clone is about to start: it counts.
            let running = {
                let agents = self.agents.lock().unwrap();
                agents.len() + self.preparing.lock().unwrap().len()
            };
            if running == 0 {
                // What this node still owes the orchestrator (a finished run's last events) is
                // in memory only; give it a moment to go out before the process ends.
                let flush_until = Instant::now() + Duration::from_secs(20);
                while self.client.pending() > 0 && Instant::now() < flush_until {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                if superseded(rx) {
                    let _ = std::fs::remove_file(&staged);
                    return Ok(());
                }
                // Held across the swap and the exit, so no agent can start in between (see
                // `spawn_agent`).
                let agents = self.agents.lock().unwrap();
                if agents.is_empty() && self.preparing.lock().unwrap().is_empty() {
                    update::swap(&self.exe, &staged)?;
                    tracing::info!("now {}; restarting into it", u.version);
                    self.mark_stopped();
                    // The supervisor (the Windows launcher loop) starts the new binary.
                    std::process::exit(0);
                }
                continue;
            }
            // A permission prompt (a plan to approve, a question) is a wait on the owner; an
            // ordinary tool call is not.
            let waiting_on_owner = self
                .pending
                .lock()
                .unwrap()
                .values()
                .any(|p| p.resend.is_some());
            let deadline = if waiting_on_owner {
                update::OWNER_DEADLINE
            } else {
                update::DEADLINE
            };
            if started.elapsed() >= deadline {
                tracing::info!(
                    "{running} agent(s) still running after {} h; stopping them to update",
                    deadline.as_secs() / 3600
                );
                self.stop_all(Duration::from_secs(30)).await;
                // Not straight back to stopping: give the swap its turn first.
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            if last_said.is_none_or(|t| t.elapsed() >= Duration::from_secs(600)) {
                tracing::info!(
                    "{} is ready; waiting for {running} agent(s) to finish",
                    u.version
                );
                last_said = Some(Instant::now());
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }

    /// Whether the link to the orchestrator is up.
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    /// Changes the reported link state and rewrites `status.json`.
    fn update_status(&self, change: impl FnOnce(&mut LinkStatus)) {
        let mut s = self.status.lock().unwrap();
        change(&mut s);
        let status = Status {
            connected: s.connected,
            node_name: self.node_name.clone(),
            pid: Some(std::process::id()),
            pid_started: self.started,
            updated_at: config::status::now_utc(),
            projects: s.projects.clone(),
        };
        if let Err(e) = status.write(&self.data_dir) {
            tracing::warn!("could not write the status file: {e:#}");
        }
    }

    /// Marks this node disconnected in `status.json` on a clean exit. Left alone if a newer node
    /// process has already taken the file over.
    pub fn mark_stopped(&self) {
        let ours = Status::read(&self.data_dir).and_then(|s| s.pid) == Some(std::process::id());
        if ours {
            self.update_status(|s| s.connected = false);
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Processes link events until the link's event channel closes.
    pub async fn run(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<Inbound>) -> Result<()> {
        let addr = ipc::serve(self.clone()).await?;
        *self.ipc_addr.lock().unwrap() = Some(addr);
        tokio::spawn(self.clone().watch_config());
        tokio::spawn(self.clone().report_disk_periodically());

        while let Some(ev) = events.recv().await {
            match ev {
                Inbound::Connected { .. } => {
                    self.update_status(|s| s.connected = true);
                    self.send_info();
                    self.resend_pending_permissions();
                    // No disk report yet: the orchestrator asks for one if it can read it.
                }
                Inbound::Disconnected { .. } => {
                    self.disk_wanted.store(false, Ordering::Release);
                    self.assignments_heard.store(false, Ordering::Release);
                    self.update_status(|s| s.connected = false);
                }
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
                let agents = self.agents.lock().unwrap();
                if let Some(h) = agents
                    .get(&stop.agent_id)
                    .filter(|h| h.task_id == stop.task_id)
                    .filter(|h| stop.run_id.is_empty() || h.run_id == stop.run_id)
                {
                    h.stop(stop.graceful);
                    return;
                }
                // A run still waiting for its clone never starts; it ends as a killed one does.
                let mut preparing = self.preparing.lock().unwrap();
                let Some(run_id) = preparing
                    .get(&stop.agent_id)
                    .filter(|p| p.task_id == stop.task_id)
                    .filter(|p| stop.run_id.is_empty() || p.run_id == stop.run_id)
                    .map(|p| p.run_id.clone())
                else {
                    return;
                };
                preparing.remove(&stop.agent_id);
                drop(preparing);
                drop(agents);
                self.event(
                    &stop.agent_id,
                    &stop.task_id,
                    &run_id,
                    AgentEventKind::Exited { code: None },
                );
                self.send_info();
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
                // different session, and a message for a replaced run must go back. A run waiting
                // for its clone keeps it for when it starts.
                let delivered = {
                    let agents = self.agents.lock().unwrap();
                    agents
                        .get(&agent_id)
                        .filter(|h| h.task_id == task_id && h.run_id == run_id)
                        .is_some_and(|h| h.send_user_message(&text))
                        || self
                            .preparing
                            .lock()
                            .unwrap()
                            .get_mut(&agent_id)
                            .filter(|p| p.task_id == task_id && p.run_id == run_id)
                            .map(|p| p.texts.push(text.clone()))
                            .is_some()
                };
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
                let projects_changed = {
                    let mut cfg = self.config.lock().unwrap();
                    match apply_settings(&mut cfg, &self.data_dir, &settings) {
                        Ok(changed) => changed,
                        Err(e) => {
                            // Still use them for this run; node.json is left as it is rather
                            // than overwritten from memory.
                            tracing::warn!("could not save node settings: {e:#}");
                            cfg.settings = Some(settings);
                            false
                        }
                    }
                };
                if projects_changed {
                    self.send_info();
                }
            }
            Body::ProjectList(projects) => self.update_status(|s| s.projects = Some(projects)),
            Body::DiskReportWanted => {
                self.disk_wanted.store(true, Ordering::Release);
                self.report_disk();
            }
            Body::UpdateAvailable(u) => {
                self.start_update(u, false);
            }
            Body::ProjectAssignments(a) => {
                *self.assignments.lock().unwrap() = Some(a);
                self.assignments_heard.store(true, Ordering::Release);
            }
            other => tracing::warn!("unexpected message for a node: {other:?}"),
        }
    }

    fn spawn_agent(self: &Arc<Self>, spawn: SpawnAgent) {
        match self.clone_source(&spawn.project) {
            Some((root, url)) => self.prepare_then_spawn(spawn, root, url),
            None => self.start_agent(spawn, false),
        }
    }

    /// Where to clone `project` from and into, when it is not mapped here but assigned to this
    /// node with a repository URL; `None` otherwise (the spawn then goes ahead, and fails as an
    /// unmapped project always has). The root is `None` when there is neither one set for this
    /// node nor a home directory to default to.
    fn clone_source(&self, project: &str) -> Option<(Option<PathBuf>, String)> {
        if self.config.lock().unwrap().project_dir(project).is_some() {
            return None;
        }
        let assignments = self.assignments.lock().unwrap();
        let a = assignments.as_ref()?;
        let url = a
            .projects
            .iter()
            .find(|p| p.slug == project)?
            .repo_url
            .clone()?;
        let root = match &a.root {
            Some(r) => Some(PathBuf::from(r)),
            None => checkout::default_root(),
        };
        Some((root, url))
    }

    /// Clones an assigned project (off the agents lock: it takes minutes), then starts the run
    /// as usual. Until then the run counts as running here, takes the messages sent to it, and
    /// can be stopped.
    fn prepare_then_spawn(self: &Arc<Self>, spawn: SpawnAgent, root: Option<PathBuf>, url: String) {
        // Replaces a run of the same agent still waiting: the orchestrator only respawns an
        // agent it believes is gone, so that run never starts.
        self.preparing.lock().unwrap().insert(
            spawn.agent_id.clone(),
            Preparing {
                task_id: spawn.task_id.clone(),
                run_id: spawn.run_id.clone(),
                texts: vec![],
            },
        );
        self.send_info();
        let this = self.clone();
        tokio::spawn(async move {
            match this.clone_project(&spawn.project, root, &url).await {
                Ok(()) => this.start_agent(spawn, true),
                Err(reason) => {
                    let ours = {
                        let mut preparing = this.preparing.lock().unwrap();
                        let ours = preparing
                            .get(&spawn.agent_id)
                            .is_some_and(|p| p.run_id == spawn.run_id);
                        if ours {
                            preparing.remove(&spawn.agent_id);
                        }
                        ours
                    };
                    if ours {
                        this.event(
                            &spawn.agent_id,
                            &spawn.task_id,
                            &spawn.run_id,
                            AgentEventKind::SpawnFailed { reason },
                        );
                        this.send_info();
                    }
                }
            }
        });
    }

    /// Clones `project` into `<root>/<project>` (or adopts a checkout of it already there) and
    /// maps it, once: a second spawn for the same project waits for the first clone and then
    /// finds it mapped. The error says what the owner can do about it.
    async fn clone_project(
        &self,
        project: &str,
        root: Option<PathBuf>,
        url: &str,
    ) -> Result<(), String> {
        let lock = self
            .clones
            .lock()
            .unwrap()
            .entry(project.to_owned())
            .or_default()
            .clone();
        let _held = lock.lock().await;
        if self.config.lock().unwrap().project_dir(project).is_some() {
            return Ok(());
        }
        let result = self.clone_and_map(project, root, url).await;
        match &result {
            Ok(dir) => {
                // NodeInfo first, so the orchestrator has the mapping when it reads "cloned".
                self.send_info();
                self.report_setup(project, fleet_proto::node::SETUP_CLONED, dir);
            }
            Err(why) => self.report_setup(project, fleet_proto::node::SETUP_FAILED, why),
        }
        result.map(|_| ())
    }

    /// [`clone_project`](Self::clone_project) without the lock: returns the mapped directory.
    async fn clone_and_map(
        &self,
        project: &str,
        root: Option<PathBuf>,
        url: &str,
    ) -> Result<String, String> {
        let root = root.ok_or(
            "this machine has no home directory for the default projects root \
             (~/fleet-projects); set a projects root on the node's page",
        )?;
        self.report_setup(
            project,
            fleet_proto::node::SETUP_CLONING,
            &root.join(project).display().to_string(),
        );
        tracing::info!("cloning {project} from {url} into {}", root.display());
        let dir = checkout::ensure(&root, project, url).await?;
        // Under `node.json.lock`, like every writer of the file (the tray app maps projects
        // too); off the runtime, since that lock may wait on another process.
        let data_dir = self.data_dir.clone();
        let (slug, at) = (project.to_owned(), dir.clone());
        let (saved, ()) = tokio::task::spawn_blocking(move || {
            NodeConfig::update(&data_dir, |c| c.map_project(&slug, &at))
        })
        .await
        .map_err(|e| format!("mapping {project} failed: {e}"))?
        .map_err(|e| {
            format!(
                "{project} was cloned into {}, but mapping it failed: {e:#}",
                dir.display()
            )
        })?;
        let path = {
            let mut cfg = self.config.lock().unwrap();
            cfg.projects = saved.projects;
            cfg.project_dir(project)
        };
        tracing::info!("{project} is ready in {}", dir.display());
        Ok(path.unwrap_or(dir).display().to_string())
    }

    /// Tells the orchestrator how setting up an assigned project goes, if it can read that: it
    /// sent assignments on this connection, and `project` is among them.
    fn report_setup(&self, project: &str, state: &str, detail: &str) {
        let assigned = self.assignments_heard.load(Ordering::Acquire)
            && self
                .assignments
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|a| a.projects.iter().any(|p| p.slug == project));
        if assigned {
            self.client
                .send(Body::ProjectSetupReport(ProjectSetupReport {
                    project: project.to_owned(),
                    state: state.to_owned(),
                    detail: detail.to_owned(),
                }));
        }
    }

    /// Starts a run's agent process and registers it. `prepared`: the run waited for a clone,
    /// so it starts only if it is still waiting (not stopped or replaced meanwhile), and then
    /// gets the messages that arrived in the meantime.
    fn start_agent(self: &Arc<Self>, spawn: SpawnAgent, prepared: bool) {
        let (agent_id, task_id) = (spawn.agent_id.clone(), spawn.task_id.clone());
        let run_id = spawn.run_id.clone();
        let cfg = self.config.lock().unwrap().clone();
        let ipc_addr = self
            .ipc_addr
            .lock()
            .unwrap()
            .expect("ipc server starts before the link");
        // Held from before the process starts until it is registered, so a self-update (which
        // swaps the binary and exits while holding this lock once it finds no agent) can never
        // exit between the two and kill an agent nobody knew had started. `runner::start` is
        // synchronous and takes no node lock; what it spawns only waits for this one briefly.
        let mut agents = self.agents.lock().unwrap();
        let mut texts = vec![];
        if prepared {
            let mut preparing = self.preparing.lock().unwrap();
            match preparing.get(&agent_id) {
                Some(p) if p.run_id == run_id => {
                    texts = preparing
                        .remove(&agent_id)
                        .map(|p| p.texts)
                        .unwrap_or_default();
                }
                // Stopped (and reported as ended) or replaced by a newer run while it waited.
                _ => return,
            }
        } else {
            // A newer run of an agent whose earlier one still waits for a clone: that one must
            // not start over this one when its clone is done.
            self.preparing.lock().unwrap().remove(&agent_id);
        }
        if let Some(old) = agents.remove(&agent_id) {
            // The orchestrator only respawns an agent it believes is gone; stop a stale process.
            old.stop(false);
        }
        match runner::start(
            self.clone(),
            &cfg,
            &self.data_dir,
            &self.exe,
            ipc_addr,
            spawn,
        ) {
            Ok(handle) => {
                for text in &texts {
                    handle.send_user_message(text);
                }
                agents.insert(agent_id, handle);
                drop(agents);
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
        let waiting: Vec<(String, Preparing)> = {
            let agents = self.agents.lock().unwrap();
            for h in agents.values() {
                h.stop(false);
            }
            // Runs still waiting for a clone never start; they end as killed ones do.
            self.preparing.lock().unwrap().drain().collect()
        };
        for (agent_id, p) in waiting {
            self.event(
                &agent_id,
                &p.task_id,
                &p.run_id,
                AgentEventKind::Exited { code: None },
            );
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
        // A run waiting for its project's clone is running as far as the orchestrator is
        // concerned: it must not take it for one that ended.
        let (running_agents, running_runs) = {
            let agents = self.agents.lock().unwrap();
            let preparing = self.preparing.lock().unwrap();
            (
                agents.keys().chain(preparing.keys()).cloned().collect(),
                agents
                    .values()
                    .map(|h| h.run_id.clone())
                    .chain(preparing.values().map(|p| p.run_id.clone()))
                    .collect(),
            )
        };
        self.client.send(Body::NodeInfo(NodeInfo {
            projects,
            max_agents,
            running_agents,
            running_runs,
        }));
    }

    async fn report_disk_periodically(self: Arc<Self>) {
        let mut tick = tokio::time::interval(DISK_REPORT_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; every connect reports anyway.
        tick.tick().await;
        loop {
            tick.tick().await;
            self.report_disk();
        }
    }

    /// Measures free disk space (off the async runtime: sizing the session folders walks a
    /// tree) and sends it. Only while connected: messages queue while the link is down, and a
    /// pile of stale reports is worth less than the fresh one every connect sends.
    fn report_disk(self: &Arc<Self>) {
        if !self.disk_wanted.load(Ordering::Acquire) || !self.status.lock().unwrap().connected {
            return;
        }
        let Some(claude_dir) = disk::claude_dir() else {
            tracing::warn!("cannot tell where Claude Code keeps its sessions; no disk report");
            return;
        };
        if self.measuring_disk.swap(true, Ordering::AcqRel) {
            // One is under way, maybe with mappings from before a change: measure again after.
            self.disk_again.store(true, Ordering::Release);
            return;
        }
        self.disk_again.store(false, Ordering::Release);
        let projects = self.config.lock().unwrap().projects.clone();
        let this = self.clone();
        tokio::spawn(async move {
            let mut measuring =
                tokio::task::spawn_blocking(move || disk::report(&claude_dir, &projects));
            // A dead network drive or a hard NFS mount can block a probe for ever. Stop waiting
            // (the orchestrator stops holding on a report that has gone stale meanwhile), but
            // start no other measurement until this one returns: each would block a new thread
            // on the same drive.
            match tokio::time::timeout(DISK_MEASURE_LIMIT, &mut measuring).await {
                Ok(measured) => {
                    this.measuring_disk.store(false, Ordering::Release);
                    match measured {
                        Ok(report) => this.client.send(Body::DiskReport(report)),
                        Err(e) => tracing::warn!("measuring free disk space failed: {e}"),
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        "measuring free disk space took over {}s; a mapped drive may be hung",
                        DISK_MEASURE_LIMIT.as_secs()
                    );
                    let _ = measuring.await;
                    this.measuring_disk.store(false, Ordering::Release);
                }
            }
            if this.disk_again.swap(false, Ordering::AcqRel) {
                this.report_disk();
            }
        });
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
            let changed = {
                // Loaded under the lock, so a settings push in between cannot be overwritten
                // with an older copy of the file.
                let mut cfg = self.config.lock().unwrap();
                let Ok(fresh) = NodeConfig::load(&self.data_dir) else {
                    continue;
                };
                let changed = cfg.projects != fresh.projects;
                cfg.projects = fresh.projects;
                if fresh.settings.is_some() {
                    cfg.settings = fresh.settings;
                }
                changed
            };
            if changed {
                self.send_info();
                // A newly mapped project may sit on a disk the last report did not cover.
                self.report_disk();
            }
        }
    }
}

/// Saves settings pushed by the orchestrator into `node.json` and returns whether the mapped
/// projects in memory changed.
///
/// It starts from the file, not from memory: memory only catches up with `fleet project map`
/// or the tray app every few seconds, and saving it would undo a mapping made in between. The
/// file is changed under `node.json.lock`, which those writers hold too, and the fresh projects
/// are adopted into memory under the caller's config lock.
fn apply_settings(cfg: &mut NodeConfig, data_dir: &Path, settings: &NodeSettings) -> Result<bool> {
    let (fresh, ()) = NodeConfig::update(data_dir, |c| {
        c.settings = Some(settings.clone());
        Ok(())
    })?;
    let changed = cfg.projects != fresh.projects;
    *cfg = fresh;
    Ok(changed)
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

#[cfg(test)]
mod tests {
    use fleet_proto::ClaudeAuth;

    use super::*;

    fn settings(max_agents: u32) -> NodeSettings {
        NodeSettings {
            claude_command: vec!["claude".into()],
            auth: ClaudeAuth::Default,
            max_agents,
        }
    }

    #[test]
    fn settings_keep_projects_mapped_on_disk_since_the_last_poll() {
        let dir = std::env::temp_dir().join(format!("fleet-node-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("repo");
        std::fs::create_dir_all(&project).unwrap();

        // In memory: nothing mapped yet. On disk: `fleet project map` (or the tray) just
        // mapped a project, and the 5 s poll has not picked it up.
        let mut memory = NodeConfig::default();
        let mut on_disk = NodeConfig::default();
        on_disk.map_project("demo", &project).unwrap();
        on_disk.save(&dir).unwrap();

        let changed = apply_settings(&mut memory, &dir, &settings(3)).unwrap();
        assert!(changed, "the fresh projects are reported");
        let saved = NodeConfig::load(&dir).unwrap();
        assert_eq!(saved.projects, on_disk.projects, "the mapping survives");
        assert_eq!(saved.settings, Some(settings(3)));
        assert_eq!(memory, saved, "memory adopts what was saved");

        // Nothing changed on disk this time: the projects are unchanged.
        assert!(!apply_settings(&mut memory, &dir, &settings(5)).unwrap());
        assert_eq!(NodeConfig::load(&dir).unwrap().settings, Some(settings(5)));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn status_file_tracks_the_link_and_a_clean_exit_leaves_a_newer_node_alone() {
        let dir = std::env::temp_dir().join(format!("fleet-node-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let orchestrator = fleet_net::EndpointAddr::new(fleet_net::SecretKey::generate().public());
        let (client, _events) = Client::new(orchestrator);
        let node = Node::new(dir.clone(), PathBuf::from("fleet"), client, "pc".into()).unwrap();

        // Written at once, with this process's pid and start stamp.
        let status = Status::read(&dir).unwrap();
        assert!(!status.connected);
        assert_eq!(status.node_name, "pc");
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(
            status.pid_started,
            config::process::start_time(std::process::id())
        );
        node.update_status(|s| s.connected = true);
        assert!(Status::read(&dir).unwrap().connected);

        // Our own file: a clean exit marks it disconnected.
        node.mark_stopped();
        assert!(!Status::read(&dir).unwrap().connected);

        // A newer node process has taken the file over: it is left as that node wrote it.
        let foreign = r#"{"connected":true,"node_name":"pc","pid":4000000001}"#;
        std::fs::write(Status::path(&dir), foreign).unwrap();
        node.mark_stopped();
        assert_eq!(
            std::fs::read_to_string(Status::path(&dir)).unwrap(),
            foreign
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_leave_an_unreadable_node_json_alone() {
        let dir = std::env::temp_dir().join(format!("fleet-node-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(NodeConfig::path(&dir), "{broken").unwrap();
        let mut memory = NodeConfig::default();
        assert!(apply_settings(&mut memory, &dir, &settings(3)).is_err());
        assert_eq!(
            std::fs::read_to_string(NodeConfig::path(&dir)).unwrap(),
            "{broken"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
