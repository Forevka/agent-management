//! The node role: runs Claude Code agents for the orchestrator on this host.
//!
//! A node owns three things: its local configuration (which project lives in which directory,
//! how to start Claude), the agent processes it supervises, and a loopback IPC endpoint that the
//! `fleet mcp` shim inside each agent uses to reach the orchestrator.

pub mod agent_cli;
pub mod automemory;
pub mod checkout;
pub mod disk;
/// `node.json` and `status.json` live in their own crate so the tray app can use them without
/// compiling the network stack; re-exported here so `fleet_node::config` keeps working.
pub use fleet_node_config as config;
pub mod ipc;
pub mod kit;
pub mod mcp;
pub mod runner;
pub mod secrets;
pub mod skill;
pub mod tools;
pub mod update;

use std::{
    collections::{HashMap, HashSet},
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
    AgentEvent, AgentEventKind, Body, Kit, KitCapture, KitContent, KitItemStatus, KitSkipped,
    NodeInfo, NodeSettings, ProjectAssignments, ProjectSummary, SpawnAgent, ToolCall, ToolResult,
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
    /// Clones under way, per project (see `checkout`).
    clones: checkout::Clones,
    /// Runs waiting for their project to be cloned, by agent. They count as running (in
    /// `NodeInfo`, and for a self-update), so neither the orchestrator nor an update takes them
    /// for gone. Lock order: `agents`, then this, then `clones`' own locks.
    preparing: Mutex<HashMap<String, checkout::Preparing>>,
    /// Where Claude Code's configuration is, when it is not where the environment says (see
    /// [`Node::use_claude_config_dir`]).
    claude_config: Mutex<Option<kit::KitPaths>>,
    /// The parts of the kit arriving now.
    kit_parts: Mutex<fleet_proto::kit::Parts>,
    /// The newest complete kit not yet applied, and whether an apply is under way: one at a
    /// time, and a kit that arrives meanwhile replaces any other still waiting.
    kit_queue: Mutex<(Option<(KitContent, u64)>, bool)>,
    /// The last whole kit received, to apply again when the node learns something that changes
    /// its status (its CLI cannot enforce the allowlist).
    kit_last: Mutex<Option<KitContent>>,
    /// Counts connects and disconnects. A kit answer (a capture, a status) goes out only on the
    /// connection it was asked on: after a reconnect the orchestrator may be an older release
    /// that cannot decode it, and an undecodable frame drops the link. The orchestrator asks
    /// again on every connect anyway.
    link_gen: AtomicU64,
    /// Whether the configured claude CLI knows `--strict-mcp-config`, for that command, from its
    /// `--help` (asked when the settings arrive). Unknown counts as yes.
    strict_mcp: Mutex<Option<(Vec<String>, Option<kit::CliFlags>)>>,
    /// The tasks someone watches live (`Body::WatchTasks`), whose runs' text is streamed as it is
    /// written (`Body::RunPartial`). Cleared on every disconnect, like `disk_wanted`: only an
    /// orchestrator that asked on the current connection reads a partial.
    watching: Mutex<HashSet<String>>,
}

impl Node {
    pub fn new(
        data_dir: PathBuf,
        exe: PathBuf,
        client: Client,
        node_name: String,
    ) -> Result<Arc<Self>> {
        let config = NodeConfig::load(&data_dir)?;
        // A run's MCP config (it holds the kit's secrets) is deleted when the run exits; one a
        // crashed node process left behind belongs to no run now, so it goes at start.
        let _ = std::fs::remove_dir_all(data_dir.join("mcp"));
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
            clones: checkout::Clones::default(),
            preparing: Mutex::new(HashMap::new()),
            claude_config: Mutex::new(None),
            kit_parts: Mutex::new(Default::default()),
            kit_queue: Mutex::new((None, false)),
            kit_last: Mutex::new(None),
            link_gen: AtomicU64::new(0),
            strict_mcp: Mutex::new(None),
            watching: Mutex::new(HashSet::new()),
        });
        // Replaces whatever a previous node process left, so the tray sees this pid at once.
        node.update_status(|_| {});
        Ok(node)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Points this node at a Claude Code config dir other than its environment's, as if
    /// `CLAUDE_CONFIG_DIR` were set to it: the kit reads and installs there, and the `claude`
    /// processes the node starts (the CLI and the agents) are given it. For a test that must
    /// never touch the developer's own `~/.claude`.
    pub fn use_claude_config_dir(&self, dir: PathBuf) {
        *self.claude_config.lock().unwrap() = Some(kit::KitPaths::at(dir));
    }

    fn kit_paths(&self) -> Option<kit::KitPaths> {
        self.claude_config
            .lock()
            .unwrap()
            .clone()
            .or_else(kit::KitPaths::from_env)
    }

    /// Whether `command` (the node's claude command) takes `--strict-mcp-config`. Yes until the
    /// CLI was asked and said otherwise: a CLI that cannot even be asked fails its runs anyway.
    pub(crate) fn strict_mcp_supported(&self, command: &[String]) -> bool {
        self.strict_mcp_known(command) != Some(false)
    }

    fn strict_mcp_known(&self, command: &[String]) -> Option<bool> {
        self.cli_flags(command).map(|f| f.strict_mcp)
    }

    /// Whether `command` takes `--include-partial-messages`, on the same terms as
    /// [`Self::strict_mcp_supported`]: without it a session only shows finished blocks.
    pub(crate) fn partial_messages_supported(&self, command: &[String]) -> bool {
        self.cli_flags(command).map(|f| f.partial_messages) != Some(false)
    }

    fn cli_flags(&self, command: &[String]) -> Option<kit::CliFlags> {
        match &*self.strict_mcp.lock().unwrap() {
            Some((c, known)) if c.as_slice() == command => *known,
            _ => None,
        }
    }

    /// Asks the configured claude CLI, once per command, whether it knows `--strict-mcp-config`.
    ///
    /// In a task of its own: a `--help` that hangs must not hold up the link's events (spawns,
    /// stops, tool answers). Until it answers the CLI counts as able, and whatever it answers,
    /// "could not tell" included, is kept for that command, so a reconnect does not ask again.
    /// An answer of "no" sends the last kit through again, so the node's status says so at once.
    async fn probe_cli(self: Arc<Self>) {
        let Some(command) = self
            .config
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .map(|s| s.claude_command.clone())
        else {
            return;
        };
        {
            let mut known = self.strict_mcp.lock().unwrap();
            if matches!(&*known, Some((c, _)) if *c == command) {
                return;
            }
            *known = Some((command.clone(), None));
        }
        let known = kit::probe_cli_flags(&command, self.kit_paths().as_ref()).await;
        {
            let mut cached = self.strict_mcp.lock().unwrap();
            // The settings may have changed meanwhile; an answer for an old command is dropped.
            if matches!(&*cached, Some((c, _)) if *c == command) {
                *cached = Some((command, known));
            }
        }
        if known.is_some_and(|f| !f.partial_messages) {
            tracing::warn!(
                "the claude CLI here has no --include-partial-messages: a session watched on the web shows each block once it is finished"
            );
        }
        if known.is_some_and(|f| !f.strict_mcp) {
            tracing::warn!(
                "the claude CLI here has no --strict-mcp-config: agent sessions run without the kit's MCP allowlist until it is updated"
            );
            let last = self.kit_last.lock().unwrap().clone();
            if let Some(kit) = last {
                self.queue_kit(kit);
            }
        }
    }

    /// Whether the connection `generation` an answer was asked on is still the live one.
    fn same_link(&self, generation: u64) -> bool {
        self.link_gen.load(Ordering::Acquire) == generation && self.client.is_connected()
    }

    /// The `CLAUDE_CONFIG_DIR` the agents must be given, when the node was pointed elsewhere.
    /// The Claude config folder this node's sessions use: the one it was pointed at, else
    /// `CLAUDE_CONFIG_DIR` or `~/.claude`. Where Claude's own per-project memory is found.
    pub(crate) fn claude_home(&self) -> Option<PathBuf> {
        self.kit_paths().map(|p| p.config_dir)
    }

    pub(crate) fn claude_config_env(&self) -> Option<PathBuf> {
        self.claude_config
            .lock()
            .unwrap()
            .as_ref()
            .filter(|p| p.set_env)
            .map(|p| p.config_dir.clone())
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
            // A run waiting for its project's clone is about to start, and a clone half done
            // would be left for the next start to clear up: both count. So does a kit being
            // applied: an exit between its renames, or in the middle of `claude plugin install`,
            // would leave a skill or a plugin half installed.
            let running = {
                let agents = self.agents.lock().unwrap();
                agents.len()
                    + self.preparing.lock().unwrap().len()
                    + self.clones.in_flight.load(Ordering::SeqCst)
                    + usize::from(self.kit_queue.lock().unwrap().1)
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
                // `spawn_agent`), and no kit apply either (`queue_kit` takes the kit queue's lock
                // to start one). Lock order: `agents`, then `kit_queue`.
                let agents = self.agents.lock().unwrap();
                let kit = self.kit_queue.lock().unwrap();
                if agents.is_empty()
                    && self.preparing.lock().unwrap().is_empty()
                    && self.clones.in_flight.load(Ordering::SeqCst) == 0
                    && !kit.1
                {
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
                    self.link_gen.fetch_add(1, Ordering::AcqRel);
                    self.update_status(|s| s.connected = true);
                    self.send_info();
                    self.resend_pending_permissions();
                    // No disk report yet: the orchestrator asks for one if it can read it.
                }
                Inbound::Disconnected { .. } => {
                    self.link_gen.fetch_add(1, Ordering::AcqRel);
                    self.disk_wanted.store(false, Ordering::Release);
                    self.assignments_heard.store(false, Ordering::Release);
                    self.watching.lock().unwrap().clear();
                    self.update_status(|s| s.connected = false);
                }
                Inbound::Message { body, .. } => {
                    let settings = matches!(body, Body::NodeSettings(_));
                    self.handle(body);
                    // Whether this CLI can hold a session to the kit's MCP servers decides its
                    // arguments; asked off the event loop (see `probe_cli`).
                    if settings {
                        tokio::spawn(self.clone().probe_cli());
                    }
                }
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
                drop(agents);
                // A run still waiting for its clone never starts; it ends as a killed one does,
                // once the git it alone was waiting for is stopped.
                self.stop_waiting(&stop);
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
                // Whatever made a clone fail may be what the owner just changed (or retried):
                // the next run tries again rather than being told the old reason.
                self.clones.forget_failures();
                *self.assignments.lock().unwrap() = Some(a);
                self.assignments_heard.store(true, Ordering::Release);
            }
            Body::KitCaptureRequest(r) => self.kit_capture(r.id),
            Body::Kit(k) => self.kit_received(k),
            Body::WatchTasks(tasks) => {
                *self.watching.lock().unwrap() = tasks.into_iter().collect();
            }
            other => tracing::warn!("unexpected message for a node: {other:?}"),
        }
    }

    fn spawn_agent(self: &Arc<Self>, spawn: SpawnAgent) {
        match self.clone_source(&spawn.project) {
            Some((root, url)) => self.prepare_then_spawn(spawn, root, url),
            None => self.start_agent(spawn),
        }
    }

    /// Starts a run whose project is here.
    fn start_agent(self: &Arc<Self>, spawn: SpawnAgent) {
        // Held from before the process starts until it is registered, so a self-update (which
        // swaps the binary and exits while holding this lock once it finds no agent) can never
        // exit between the two and kill an agent nobody knew had started.
        let agents = self.agents.lock().unwrap();
        // A newer run of an agent whose earlier one still waits for a clone: that one never
        // starts, and ends as a replaced process does.
        let replaced = self.preparing.lock().unwrap().remove(&spawn.agent_id);
        let agent_id = spawn.agent_id.clone();
        self.start_locked(agents, spawn, vec![]);
        if let Some(old) = replaced {
            self.end_waiting(&agent_id, old, AgentEventKind::Exited { code: None });
        }
    }

    /// Starts a run's agent process and registers it, with the agents lock held from before the
    /// process starts until it is registered. `texts` arrived for the run while it waited for its
    /// clone: they follow its first prompt, or go back to the orchestrator if it cannot start.
    /// `runner::start` is synchronous and takes no node lock; what it spawns only waits for this
    /// one briefly.
    fn start_locked(
        self: &Arc<Self>,
        mut agents: std::sync::MutexGuard<'_, HashMap<String, AgentHandle>>,
        spawn: SpawnAgent,
        texts: Vec<String>,
    ) {
        let (agent_id, task_id) = (spawn.agent_id.clone(), spawn.task_id.clone());
        let run_id = spawn.run_id.clone();
        let cfg = self.config.lock().unwrap().clone();
        let ipc_addr = self
            .ipc_addr
            .lock()
            .unwrap()
            .expect("ipc server starts before the link");
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
            Err(e) => {
                drop(agents);
                self.event(
                    &agent_id,
                    &task_id,
                    &run_id,
                    AgentEventKind::SpawnFailed {
                        reason: format!("{e:#}"),
                    },
                );
                for text in texts {
                    self.event(
                        &agent_id,
                        &task_id,
                        &run_id,
                        AgentEventKind::Undelivered { text },
                    );
                }
                self.send_info();
            }
        }
    }

    /// How many agent processes this node runs now.
    pub fn running(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    /// Stops every agent and waits (up to `limit`) for them to exit, which also removes their
    /// secrets files. For when this node must not keep working: the orchestrator refused it.
    pub async fn stop_all(&self, limit: std::time::Duration) {
        {
            let agents = self.agents.lock().unwrap();
            for h in agents.values() {
                h.stop(false);
            }
            // Runs still waiting for a clone never start: their git is stopped, and each ends as a
            // killed run does once it is gone.
            self.stop_all_waiting();
        }
        let deadline = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < deadline {
            {
                let agents = self.agents.lock().unwrap();
                if agents.is_empty() && self.preparing.lock().unwrap().is_empty() {
                    break;
                }
            }
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

    /// Whether someone watches this task's session live.
    pub(crate) fn watched(&self, task_id: &str) -> bool {
        self.watching.lock().unwrap().contains(task_id)
    }

    /// Sends a watched run's block in progress — checked again here, so a watch ended (or a
    /// link lost) since the caller looked sends nothing an orchestrator did not ask for.
    pub(crate) fn partial(&self, p: fleet_proto::RunPartial) {
        if self.watched(&p.task_id) {
            self.client.send(Body::RunPartial(p));
        }
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

    /// Answers a [`Body::KitCaptureRequest`]: what this machine's Claude Code has, read off the
    /// async runtime (it walks the skill directories), in parts that each fit a frame.
    fn kit_capture(self: &Arc<Self>, id: String) {
        let paths = self.kit_paths();
        let generation = self.link_gen.load(Ordering::Acquire);
        let this = self.clone();
        tokio::spawn(async move {
            let (content, skipped) = match paths {
                None => (
                    KitContent::default(),
                    vec![KitSkipped {
                        kind: "config".into(),
                        name: String::new(),
                        reason:
                            "cannot tell where Claude Code keeps its configuration on this machine"
                                .into(),
                    }],
                ),
                Some(paths) => tokio::task::spawn_blocking(move || kit::capture(&paths))
                    .await
                    .unwrap_or_else(|e| {
                        (
                            KitContent::default(),
                            vec![KitSkipped {
                                kind: "config".into(),
                                name: String::new(),
                                reason: format!("reading the configuration failed: {e}"),
                            }],
                        )
                    }),
            };
            if !this.same_link(generation) {
                tracing::info!("a kit capture was asked on a connection that is gone; not sent");
                return;
            }
            let parts = content.split(fleet_proto::kit::PART_BUDGET);
            let n = parts.len();
            let mut skipped = Some(skipped);
            for (i, part) in parts.into_iter().enumerate() {
                let mut c = KitCapture {
                    id: id.clone(),
                    part: i as u32,
                    last: i + 1 == n,
                    content: part,
                    skipped: skipped.take().unwrap_or_default(),
                };
                // A frame the link cannot send would block everything behind it. The caps make
                // this unlikely; if it happens anyway, the part goes out empty, saying so.
                if !fleet_proto::kit::fits(&Body::KitCapture(c.clone())) {
                    c.content = KitContent::default();
                    c.skipped = vec![KitSkipped {
                        kind: "capture".into(),
                        name: String::new(),
                        reason: "part of what this machine has was too large to send".into(),
                    }];
                }
                this.client.send(Body::KitCapture(c));
            }
        });
    }

    /// Collects a [`Body::Kit`]'s parts; once the kit is whole, it is applied (one apply at a
    /// time, the newest kit next).
    fn kit_received(self: &Arc<Self>, k: Kit) {
        let Some(whole) =
            self.kit_parts
                .lock()
                .unwrap()
                .add(&k.revision, k.part, k.last, k.content)
        else {
            return;
        };
        *self.kit_last.lock().unwrap() = Some(whole.clone());
        self.queue_kit(whole);
    }

    /// Queues a whole kit for applying, answered on the connection that is live now.
    fn queue_kit(self: &Arc<Self>, kit: KitContent) {
        let generation = self.link_gen.load(Ordering::Acquire);
        let start = {
            let mut q = self.kit_queue.lock().unwrap();
            q.0 = Some((kit, generation));
            !std::mem::replace(&mut q.1, true)
        };
        if start {
            tokio::spawn(self.clone().kit_apply_loop());
        }
    }

    async fn kit_apply_loop(self: Arc<Self>) {
        loop {
            let (kit, generation) = {
                let mut q = self.kit_queue.lock().unwrap();
                match q.0.take() {
                    Some(k) => k,
                    None => {
                        q.1 = false;
                        return;
                    }
                }
            };
            let statuses = match self.kit_paths() {
                Some(paths) => {
                    let command = self
                        .config
                        .lock()
                        .unwrap()
                        .settings
                        .as_ref()
                        .map(|s| s.claude_command.clone())
                        .unwrap_or_else(|| vec!["claude".into()]);
                    let strict = self.strict_mcp_known(&command);
                    kit::apply(&paths, &self.data_dir, &command, strict, &kit).await
                }
                None => {
                    let failed = |kind: &str, name: String| KitItemStatus {
                        kind: kind.into(),
                        name,
                        state: "failed".into(),
                        detail:
                            "cannot tell where Claude Code keeps its configuration on this machine"
                                .into(),
                    };
                    kit.skills
                        .iter()
                        .map(|s| failed("skill", s.name.clone()))
                        .chain(kit.plugins.iter().map(|p| failed("plugin", p.id())))
                        .chain(kit.mcp.iter().map(|m| failed("mcp", m.name.clone())))
                        .collect()
                }
            };
            // The answer goes only to the connection that sent the kit; a new connection gets
            // the kit again, and its own answer.
            if !self.same_link(generation) {
                continue;
            }
            let mut body = Body::KitStatus(statuses);
            if !fleet_proto::kit::fits(&body)
                && let Body::KitStatus(list) = &mut body
            {
                for s in list.iter_mut() {
                    s.detail.clear();
                }
            }
            self.client.send(body);
        }
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
