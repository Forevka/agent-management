//! Starts and supervises one Claude Code process per agent run.
//!
//! Claude runs headless with stream-json on both sides: stdout is a stream of JSON events that
//! the node forwards (the orchestrator keeps the transcript), and stdin stays open so the
//! orchestrator can feed the session new user messages (human answers, other agents'
//! messages) without restarting it. A session that has finished its turn and stays idle is
//! closed; the orchestrator resumes it (`--resume`) when there is something new for it.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use fleet_proto::{AgentEventKind, ClaudeAuth, SpawnAgent, redact::redact_secrets};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use crate::{Node, automemory, config::NodeConfig, secrets, tools};

/// Forwarded output lines are capped so one huge tool result cannot exceed the frame limit.
const MAX_LINE: usize = 512 * 1024;
/// MCP tool calls normally time out quickly; the permission tool waits for a human, possibly
/// for hours, so the agent's MCP client must wait far longer (one week, in ms).
const MCP_TOOL_TIMEOUT_MS: &str = "604800000";
/// Claude loads only the servers of `--mcp-config` (see [`build_args`]).
pub(crate) const STRICT_MCP: &str = "--strict-mcp-config";
/// Claude writes each token as it comes (`stream_event` lines; checked against the real CLI,
/// 2.1.268).
pub(crate) const PARTIAL_MESSAGES: &str = "--include-partial-messages";

/// How long a run's exit waits to send what its last turn wrote to Claude's memory.
const MEMORY_SYNC_AT_EXIT: Duration = Duration::from_secs(10);
/// How often a watched run's block in progress is sent on: often enough to read as typing,
/// rarely enough that a long answer is not a frame per token.
const PARTIAL_EVERY: Duration = Duration::from_millis(100);
/// A block longer than this is not streamed further; its finished line follows anyway. Each
/// partial carries the whole block (see `LiveBlock`), so this bounds what one costs.
const PARTIAL_MAX: usize = 16 * 1024;
/// After a graceful stop, how long Claude may take to finish its turn before it is killed.
const GRACEFUL_STOP_LIMIT: Duration = Duration::from_secs(600);

/// How long a session may sit idle after finishing a turn before the node closes it.
fn idle_exit() -> Duration {
    let minutes = std::env::var("FLEET_IDLE_EXIT_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30u64);
    Duration::from_secs(minutes.max(1) * 60)
}

pub struct AgentHandle {
    pub agent_id: String,
    pub task_id: String,
    pub run_id: String,
    /// Authenticates this agent's `fleet agent` calls and MCP shim on the node's IPC port.
    pub ipc_token: String,
    /// Dropping the sender closes Claude's stdin, which ends the session after its turn.
    stdin: Mutex<Option<mpsc::UnboundedSender<String>>>,
    kill: CancellationToken,
    after_plan_mode: Option<String>,
    activity: Arc<Activity>,
    /// When stdin was closed for a graceful stop; a stop that takes too long becomes a kill.
    closed_at: Arc<Mutex<Option<std::time::Instant>>>,
}

/// What the idle watchdog needs to know.
struct Activity {
    /// Unix seconds of the last output line or input message.
    last: AtomicU64,
    /// The last thing Claude printed was a turn result, and no new input arrived since.
    idle: AtomicBool,
}

impl Activity {
    fn touch(&self, idle: bool) {
        self.last.store(now_secs(), Ordering::Relaxed);
        self.idle.store(idle, Ordering::Relaxed);
    }
}

impl AgentHandle {
    fn send_line(&self, line: String) -> bool {
        self.stdin
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.send(line).is_ok())
    }

    /// Queues a user turn. Returns false if the session no longer takes input.
    pub fn send_user_message(&self, text: &str) -> bool {
        self.activity.touch(false);
        self.send_line(json!({"type":"user","message":{"role":"user","content":text}}).to_string())
    }

    pub fn switch_to_after_plan_mode(&self) {
        if let Some(mode) = &self.after_plan_mode {
            self.send_line(
                json!({
                    "type": "control_request",
                    "request_id": random_token(),
                    "request": {"subtype": "set_permission_mode", "mode": mode}
                })
                .to_string(),
            );
        }
    }

    /// Graceful: close stdin so Claude finishes its turn and exits. Otherwise kill the process
    /// tree at once.
    pub fn stop(&self, graceful: bool) {
        self.stdin.lock().unwrap().take();
        self.closed_at
            .lock()
            .unwrap()
            .get_or_insert_with(std::time::Instant::now);
        if !graceful {
            self.kill.cancel();
        }
    }
}

/// The fleet's arguments for a spawn, which follow the node's own `base_args` (the rest of its
/// configured Claude command); separated out so it can be tested without a process.
pub fn build_args(
    spawn: &SpawnAgent,
    base_args: &[String],
    mcp_config: &Path,
    plugin_dir: &Path,
    system_prompt: Option<&Path>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        // Token-by-token output (`stream_event` lines), streamed only to someone watching the
        // session live and otherwise dropped here; see `LiveBlock`.
        PARTIAL_MESSAGES,
        "--permission-mode",
        &spawn.permission_mode,
        "--permission-prompt-tool",
        &format!("mcp__fleet__{}", tools::PERMISSION_TOOL),
        "--mcp-config",
        &mcp_config.display().to_string(),
        "--plugin-dir",
        &plugin_dir.display().to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // The MCP servers a session gets are exactly the ones in its config: the fleet's own, plus
    // the owner's kit when the orchestrator sent it. Nothing else this machine has configured
    // (user scope, a project's `.mcp.json`, a plugin's own servers, claude.ai connectors) is
    // loaded — checked against the real CLI. The secretary's and the retrospective's spawns ask
    // for it themselves; once is enough.
    if !spawn.extra_args.iter().any(|a| a == STRICT_MCP) {
        args.push(STRICT_MCP.into());
    }
    // A model set in the node's command wins over the fleet's default: the last `--model` would
    // otherwise be ours, and the node's setting would do nothing.
    let node_sets_model = base_args
        .iter()
        .any(|a| a == "--model" || a.starts_with("--model="));
    if let Some(model) = spawn.model.as_ref().filter(|_| !node_sets_model) {
        args.extend(["--model".into(), model.clone()]);
    }
    // A file rather than an argument: multi-line arguments do not survive Windows batch
    // wrappers such as npm's claude.cmd.
    if let Some(path) = system_prompt {
        args.extend([
            "--append-system-prompt-file".into(),
            path.display().to_string(),
        ]);
    }
    if let Some(session) = &spawn.resume_session_id {
        args.extend(["--resume".into(), session.clone()]);
    }
    args.extend(spawn.extra_args.iter().cloned());
    args
}

pub fn start(
    node: Arc<Node>,
    cfg: &NodeConfig,
    data_dir: &Path,
    exe: &Path,
    ipc_addr: SocketAddr,
    spawn: SpawnAgent,
) -> Result<AgentHandle> {
    let project_dir = cfg
        .project_dir(&spawn.project)
        .with_context(|| format!("project {:?} is not mapped on this node", spawn.project))?;
    if !project_dir.is_dir() {
        bail!("project directory {} does not exist", project_dir.display());
    }
    let settings = cfg.settings.clone().unwrap_or(fleet_proto::NodeSettings {
        claude_command: vec!["claude".into()],
        auth: ClaudeAuth::Default,
        max_agents: 4,
    });
    let (program, base_args) = settings
        .claude_command
        .split_first()
        .context("claude command is empty")?;
    let program = resolve_program(program)?;

    let agent_dir = data_dir.join("agents").join(sanitize(&spawn.agent_id));
    std::fs::create_dir_all(&agent_dir)?;
    let ipc_token = random_token();
    let role = spawn.role.clone();
    // Secrets are per run: a replaced run's cleanup must not delete the new run's files.
    let secrets_key = format!("{}-{}", spawn.task_id, spawn.run_id);
    // The MCP config holds the kit's resolved secrets, so it is the run's own file, outside the
    // project, removed with the run's other secrets when the process exits. Like them, it is
    // written only once nothing else can fail before the process starts.
    let mcp_text = mcp_config(
        exe,
        ipc_addr,
        &ipc_token,
        &role,
        spawn.extra_mcp_servers.as_deref(),
    )?;
    let mcp_path = secrets::mcp_config_path(data_dir, &secrets_key);
    let system_path = match &spawn.append_system_prompt {
        Some(text) => {
            let p = agent_dir.join("system-prompt.md");
            std::fs::write(&p, text)?;
            Some(p)
        }
        None => None,
    };
    // The kit's MCP secrets are scrubbed from the transcript like the project's.
    let kit_servers = extra_servers(spawn.extra_mcp_servers.as_deref()).unwrap_or_default();
    let redact: Arc<Vec<String>> = Arc::new(
        spawn
            .secret_files
            .iter()
            .flat_map(|f| secrets::parse_env(&f.content).into_values())
            .chain(crate::kit::mcp_secret_values(&kit_servers))
            .collect(),
    );
    let plugin_dir = crate::skill::write_plugin(data_dir, &role)?;

    // `fleet agent ...` in the session's shell finds this executable and its node through
    // these variables; the token is what identifies the agent to the node.
    let mut path =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if let Some(dir) = exe.parent() {
        path.insert(0, dir.to_path_buf());
    }
    // The secretary reads third parties' messages all day, and the retrospective reads a whole
    // day of them: each works in an empty directory of its own, so even the read-only commands
    // Claude allows without asking see nothing there (reads outside the working directory are
    // refused). The project mapping still picks the node.
    let work_dir = if role == crate::skill::SECRETARY_ROLE || role == crate::skill::RETRO_ROLE {
        let d = agent_dir.join("work");
        std::fs::create_dir_all(&d)?;
        d
    } else {
        project_dir.clone()
    };
    let mut args = build_args(
        &spawn,
        base_args,
        &mcp_path,
        &plugin_dir,
        system_path.as_deref(),
    );
    // A claude CLI too old for `--strict-mcp-config` would refuse every run with it. The node
    // leaves out the flag it adds itself (a spawn that asks for it, the secretary's, keeps it:
    // that session must not start wider than it is meant to), and the kit's status says the
    // allowlist is not enforced on this node.
    if !node.strict_mcp_supported(&settings.claude_command)
        && !spawn.extra_args.iter().any(|a| a == STRICT_MCP)
    {
        args.retain(|a| a != STRICT_MCP);
    }
    // Nor for `--include-partial-messages`: such a CLI streams nothing, and its sessions show
    // each block once it is finished.
    if !node.partial_messages_supported(&settings.claude_command) {
        args.retain(|a| a != PARTIAL_MESSAGES);
    }
    // Claude's own memory goes to a folder of the node's, not the one the owner's Claude Code
    // shares for this directory, and what it holds is sent on after every turn (see
    // `automemory`). Not for the sealed sessions (the secretary's and the retrospective's take
    // no settings at all), and not when the node's command already brings settings of its own:
    // two `--settings` would leave which one wins to the CLI.
    let node_settings = base_args
        .iter()
        .any(|a| a == "--settings" || a.starts_with("--settings="));
    if node_settings {
        tracing::info!(
            "the node's claude command has --settings of its own; sessions keep Claude's own \
             memory folder, which is not sent to the fleet's memory"
        );
    }
    let memory_key = automemory::key(&spawn).filter(|_| !node_settings);
    let memory = memory_key.as_deref().and_then(|key| {
        let seed = node
            .claude_home()
            .map(|home| automemory::claude_default(&home, &work_dir));
        let folder = automemory::dir(data_dir, key);
        automemory::prepare(
            data_dir,
            key,
            seed.as_deref(),
            automemory::sends_seeded(&spawn),
        )
        .and_then(|()| automemory::settings_file(&agent_dir, &folder))
        .inspect_err(|e| tracing::warn!("the session keeps Claude's own memory folder: {e:#}"))
        .ok()
    });
    if let Some(path) = &memory {
        args.extend(["--settings".into(), path.display().to_string()]);
    }
    let sync_memory = {
        let (node, data_dir, token) = (node.clone(), data_dir.to_path_buf(), ipc_token.clone());
        let key = memory.as_ref().and(memory_key);
        move || {
            let (node, data_dir, key, token) =
                (node.clone(), data_dir.clone(), key.clone(), token.clone());
            async move {
                if let Some(key) = key {
                    automemory::sync(&node, &data_dir, &key, &token).await;
                }
            }
        }
    };
    let mut cmd = Command::new(&program);
    cmd.args(base_args)
        .args(args)
        .env("PATH", std::env::join_paths(path)?)
        .env("FLEET_IPC_ADDR", ipc_addr.to_string())
        .env("FLEET_IPC_TOKEN", &ipc_token)
        .env("FLEET_ROLE", &role)
        .env(
            "FLEET_SECRETS_DIR",
            secrets::task_dir(data_dir, &secrets_key),
        )
        .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
        .envs(
            node.claude_config_env()
                .map(|d| ("CLAUDE_CONFIG_DIR", d.into_os_string())),
        )
        .current_dir(&work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match &settings.auth {
        ClaudeAuth::Default => {}
        ClaudeAuth::OAuthToken(t) => {
            cmd.env("CLAUDE_CODE_OAUTH_TOKEN", t);
        }
        ClaudeAuth::ApiKey(k) => {
            cmd.env("ANTHROPIC_API_KEY", k);
        }
    }
    // Claude starts helpers (shells, MCP servers, wrapper scripts). They live in their own
    // process group so a stop takes the whole tree down, not just the direct child.
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        // No console window per agent when the node runs in the background.
        cmd.creation_flags(0x0800_0000);
    }
    // The launcher's permission to replace the node's binary is the node's alone: a `fleet node`
    // an agent starts while working on the fleet's own code must not overwrite itself.
    cmd.env_remove(crate::update::SELF_UPDATE_ENV);
    let started = secrets::write(data_dir, &secrets_key, &spawn.secret_files)
        .and_then(|_| secrets::write_mcp_config(data_dir, &secrets_key, &mcp_text))
        .and_then(|()| {
            cmd.spawn()
                .with_context(|| format!("starting {}", program.display()))
        });
    let mut child = match started {
        Ok(c) => c,
        Err(e) => {
            secrets::remove(data_dir, &secrets_key);
            return Err(e);
        }
    };
    let tree = ProcessTree::adopt(&child);

    let (agent_id, task_id, run_id) = (
        spawn.agent_id.clone(),
        spawn.task_id.clone(),
        spawn.run_id.clone(),
    );
    let ev = {
        let (node, a, t, r) = (
            node.clone(),
            agent_id.clone(),
            task_id.clone(),
            run_id.clone(),
        );
        move |kind: AgentEventKind| node.event(&a, &t, &r, kind)
    };
    ev(AgentEventKind::Started {
        pid: child.id().unwrap_or(0),
    });

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    let mut stdin = child.stdin.take().context("no stdin")?;
    tokio::spawn(async move {
        while let Some(line) = stdin_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err()
                || stdin.write_all(b"\n").await.is_err()
            {
                break;
            }
            let _ = stdin.flush().await;
        }
        // Dropping stdin here closes the pipe: Claude ends the session after its turn.
    });

    let activity = Arc::new(Activity {
        last: AtomicU64::new(now_secs()),
        idle: AtomicBool::new(false),
    });
    let stdout = child.stdout.take().context("no stdout")?;
    let (ev_out, act, red) = (ev.clone(), activity.clone(), redact.clone());
    let (live_node, live_task, live_run) = (node.clone(), task_id.clone(), run_id.clone());
    let sync_after_turn = sync_memory.clone();
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut session_sent = false;
        let mut live = LiveBlock::default();
        while let Ok(Some(mut line)) = lines.next_line().await {
            if !session_sent && let Some(id) = session_id(&line) {
                session_sent = true;
                ev_out(AgentEventKind::Session { session_id: id });
            }
            if is_noise(&line) {
                // Claude's token-by-token output, which only someone watching the session
                // live wants (see `LiveBlock`); nothing of it is kept.
                if !live_node.watched(&live_task) {
                    // A block missing a piece must never be streamed: redaction only sees a
                    // secret whole, and one falling into the gap would go out in two halves. It
                    // is forgotten; the next block starts clean.
                    live.reset();
                } else if let Some((kind, text)) = live.feed(&line, &red, Instant::now()) {
                    live_node.partial(fleet_proto::RunPartial {
                        task_id: live_task.clone(),
                        run_id: live_run.clone(),
                        kind,
                        text,
                    });
                }
                continue;
            }
            // A finished line (the whole block, a tool result, the turn's end) supersedes
            // whatever was being written.
            live.reset();
            let turn_ended = is_turn_result(&line);
            act.touch(turn_ended);
            // Whatever the turn wrote to Claude's memory is sent on now, in the background.
            if turn_ended {
                tokio::spawn(sync_after_turn());
            }
            redact_secrets(&mut line, &red);
            truncate(&mut line);
            ev_out(AgentEventKind::Output { json: line });
        }
    });
    let stderr = child.stderr.take().context("no stderr")?;
    let (ev_err, red) = (ev.clone(), redact.clone());
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(mut line)) = lines.next_line().await {
            redact_secrets(&mut line, &red);
            truncate(&mut line);
            ev_err(AgentEventKind::Stderr { line });
        }
    });

    let kill = CancellationToken::new();
    let closed_at = Arc::new(Mutex::new(None));
    let handle = AgentHandle {
        agent_id: agent_id.clone(),
        task_id: task_id.clone(),
        run_id: run_id.clone(),
        ipc_token: ipc_token.clone(),
        stdin: Mutex::new(Some(stdin_tx)),
        kill: kill.clone(),
        after_plan_mode: spawn.after_plan_mode.clone(),
        activity: activity.clone(),
        closed_at: closed_at.clone(),
    };
    handle.send_user_message(&spawn.prompt);

    // Idle watchdog: a session that finished its turn and got nothing new is closed.
    let (watch_node, watch_agent, watch_run, idle) =
        (node.clone(), agent_id.clone(), run_id.clone(), idle_exit());
    let watchdog = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let quiet = Duration::from_secs(
                now_secs().saturating_sub(activity.last.load(Ordering::Relaxed)),
            );
            if activity.idle.load(Ordering::Relaxed) && quiet >= idle {
                tracing::info!(agent = %watch_agent, "closing idle session");
                watch_node.stop_run(&watch_agent, &watch_run, true);
                break;
            }
        }
    });

    let data_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = kill.cancelled() => {
                tree.kill(&mut child).await;
                child.wait().await.ok()
            }
            // A graceful stop that takes too long becomes a kill.
            _ = async {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    if closed_at.lock().unwrap().is_some_and(|t| t.elapsed() > GRACEFUL_STOP_LIMIT) {
                        break;
                    }
                }
            } => {
                tree.kill(&mut child).await;
                child.wait().await.ok()
            }
        };
        // Whatever the child left behind (helpers holding the pipes) goes too, so the readers
        // see end-of-file.
        tree.kill(&mut child).await;
        watchdog.abort();
        let _ = out_task.await;
        let _ = err_task.await;
        // A last look at the run's memory, while the node still knows the run (its token is how
        // the note is sent): a run stopped mid-turn never reached a turn's end. Bounded, so an
        // unreachable orchestrator does not hold up the exit.
        let _ = tokio::time::timeout(MEMORY_SYNC_AT_EXIT, sync_memory()).await;
        secrets::remove(&data_dir, &secrets_key);
        ev(AgentEventKind::Exited {
            code: status.and_then(|s| s.code()),
        });
        node.agent_exited(&agent_id, &ipc_token);
    });
    Ok(handle)
}

/// Everything an agent process (or a clone's git) started, so a stop can end all of it. The
/// process must lead its own process group on unix (`process_group(0)`, or `setsid`).
pub(crate) struct ProcessTree {
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

impl ProcessTree {
    pub(crate) fn adopt(child: &tokio::process::Child) -> Self {
        #[cfg(unix)]
        {
            Self {
                pgid: child.id().map(|p| p as i32),
            }
        }
        #[cfg(windows)]
        {
            // A job object with kill-on-close: closing it ends every process in it.
            let job = (|| {
                let mut info = win32job::ExtendedLimitInfo::new();
                info.limit_kill_on_job_close();
                let job = win32job::Job::create_with_limit_info(&info).ok()?;
                job.assign_process(child.raw_handle()? as isize).ok()?;
                Some(job)
            })();
            if job.is_none() {
                tracing::warn!(
                    "could not put the agent in a job object; a stop may leave helper processes running"
                );
            }
            Self { job }
        }
    }

    pub(crate) async fn kill(&self, child: &mut tokio::process::Child) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: killpg only sends a signal; the group was created for this child.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            // SAFETY: the handle belongs to `job`, which outlives this call.
            unsafe {
                windows_terminate_job(job.handle());
            }
        }
        let _ = child.start_kill();
    }
}

#[cfg(windows)]
unsafe fn windows_terminate_job(handle: isize) {
    unsafe extern "system" {
        fn TerminateJobObject(job: isize, exit_code: u32) -> i32;
    }
    unsafe {
        TerminateJobObject(handle, 1);
    }
}

/// The MCP config for one agent: the fleet's permission hook plus any extra servers.
pub fn mcp_config(
    exe: &Path,
    ipc: SocketAddr,
    token: &str,
    role: &str,
    extra: Option<&str>,
) -> Result<String> {
    let mut servers = extra_servers(extra)?;
    // The fleet's own server last, so nothing sent along can take its name.
    servers.insert(
        "fleet".into(),
        json!({
            "type": "stdio",
            "command": exe.display().to_string(),
            "args": ["mcp"],
            "env": {"FLEET_IPC_ADDR": ipc.to_string(), "FLEET_IPC_TOKEN": token, "FLEET_ROLE": role}
        }),
    );
    Ok(serde_json::to_string_pretty(
        &json!({ "mcpServers": servers }),
    )?)
}

/// The extra MCP servers of a spawn (the owner's kit), as a JSON object.
fn extra_servers(extra: Option<&str>) -> Result<serde_json::Map<String, serde_json::Value>> {
    match extra.filter(|e| !e.trim().is_empty()) {
        None => Ok(serde_json::Map::new()),
        Some(extra) => match serde_json::from_str(extra) {
            Ok(serde_json::Value::Object(m)) => Ok(m),
            _ => bail!("extra MCP servers are not a JSON object"),
        },
    }
}

fn resolve_program(program: &str) -> Result<PathBuf> {
    // `which` honours PATHEXT on Windows, so "claude" finds claude.exe or claude.cmd.
    which::which(program).with_context(|| {
        format!("{program:?} not found; set the node's Claude command to a full path")
    })
}

fn session_id(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    (v.get("type")?.as_str()? == "system" && v.get("subtype")?.as_str()? == "init")
        .then(|| v.get("session_id")?.as_str().map(str::to_owned))
        .flatten()
}

fn is_turn_result(line: &str) -> bool {
    line.starts_with(r#"{"type":"result""#)
        || serde_json::from_str::<serde_json::Value>(line)
            .is_ok_and(|v| v.get("type").and_then(|t| t.as_str()) == Some("result"))
}

/// Token-level streaming deltas and status pings: Claude emits one per token, which would
/// flood the link and the transcript table while adding nothing the full messages do not have.
fn is_noise(line: &str) -> bool {
    // Cheap prefix checks first; most lines are deltas.
    if line.starts_with(r#"{"type":"stream_event""#) {
        return true;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("stream_event") => true,
        Some("system") => matches!(
            v.get("subtype").and_then(|s| s.as_str()),
            Some("status" | "thinking_tokens")
        ),
        _ => false,
    }
}

/// The content block a run is writing, built from Claude's `stream_event` lines, for someone
/// watching the session live.
///
/// What goes out is always the whole block so far, redacted, minus a tail as long as the longest
/// secret: redaction only replaces a value it sees whole, so a secret still being written would
/// otherwise show its beginning. Redacting first and cutting after is what makes that hold — a
/// complete value anywhere is already `[secret]`, and an incomplete one can only be at the end.
#[derive(Default)]
struct LiveBlock {
    /// `text`, `thinking` or `tool`; `None` between blocks (or for a block not worth showing).
    kind: Option<String>,
    text: String,
    sent_at: Option<Instant>,
    /// How much of the block went out last time, so an unchanged block is not sent again.
    sent_len: usize,
}

impl LiveBlock {
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Takes one `stream_event` line; returns what to send now, if anything.
    fn feed(&mut self, line: &str, secrets: &[String], now: Instant) -> Option<(String, String)> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        // A subagent's own stream (Task tool) is not the session's voice.
        if !v["parent_tool_use_id"].is_null() {
            return None;
        }
        let ev = &v["event"];
        match ev["type"].as_str()? {
            "content_block_start" => {
                self.reset();
                let block = &ev["content_block"];
                match block["type"].as_str()? {
                    "text" => self.kind = Some("text".into()),
                    // That it thinks, not what: the page shows a loader, and a long thought
                    // would cost the most to stream.
                    "thinking" => {
                        self.kind = Some("thinking".into());
                        return self.take(secrets, now, true);
                    }
                    // The tool's name, at once: what the agent is about to do is the most
                    // useful thing to see while it writes the call's input.
                    "tool_use" => {
                        self.kind = Some("tool".into());
                        self.text = block["name"].as_str().unwrap_or_default().to_owned();
                        return self.take(secrets, now, true);
                    }
                    _ => {}
                }
                None
            }
            "content_block_delta" => {
                let delta = &ev["delta"];
                let piece = match (self.kind.as_deref(), delta["type"].as_str()?) {
                    (Some("text"), "text_delta") => delta["text"].as_str()?,
                    _ => return None,
                };
                if self.text.len() >= PARTIAL_MAX {
                    return None;
                }
                self.text.push_str(piece);
                self.take(secrets, now, false)
            }
            "content_block_stop" | "message_stop" => {
                self.reset();
                None
            }
            _ => None,
        }
    }

    fn take(&mut self, secrets: &[String], now: Instant, always: bool) -> Option<(String, String)> {
        let kind = self.kind.clone()?;
        if !always
            && self
                .sent_at
                .is_some_and(|t| now.duration_since(t) < PARTIAL_EVERY)
        {
            return None;
        }
        let mut text = self.text.clone();
        redact_secrets(&mut text, secrets);
        let hold = secrets
            .iter()
            .map(String::len)
            .filter(|&l| l >= fleet_proto::redact::MIN_REDACT_LEN)
            .max()
            .unwrap_or(0);
        let mut cut = text.len().saturating_sub(hold);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        if !always && text.len() <= self.sent_len {
            return None;
        }
        self.sent_at = Some(now);
        self.sent_len = text.len();
        Some((kind, text))
    }
}

fn truncate(line: &mut String) {
    if line.len() > MAX_LINE {
        let mut cut = MAX_LINE;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str("…[truncated]");
    }
}

pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 18];
    rand::rng().fill_bytes(&mut b);
    data_encoding::BASE64URL_NOPAD.encode(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn() -> SpawnAgent {
        SpawnAgent {
            agent_id: "a1".into(),
            task_id: "p-1".into(),
            project: "p".into(),
            role: "orchestrator".into(),
            prompt: "hi".into(),
            append_system_prompt: Some("role".into()),
            resume_session_id: Some("s1".into()),
            permission_mode: "plan".into(),
            after_plan_mode: Some("auto".into()),
            model: Some("opus".into()),
            extra_args: vec![],
            extra_mcp_servers: Some(r#"{"other":{"type":"http","url":"http://h/mcp"}}"#.into()),
            secret_files: vec![],
            run_id: "r1".into(),
        }
    }

    #[test]
    fn args_cover_headless_contract() {
        let args = build_args(
            &spawn(),
            &[],
            Path::new("/tmp/m.json"),
            Path::new("/tmp/plugin"),
            Some(Path::new("/tmp/sys.md")),
        );
        let joined = args.join(" ");
        for needle in [
            "-p",
            "--input-format stream-json",
            "--output-format stream-json",
            "--permission-mode plan",
            "--permission-prompt-tool mcp__fleet__permission",
            "--mcp-config /tmp/m.json",
            "--plugin-dir /tmp/plugin",
            "--append-system-prompt-file /tmp/sys.md",
            "--model opus",
            "--resume s1",
            "--strict-mcp-config",
        ] {
            assert!(joined.contains(needle), "{needle} missing from {joined}");
        }
        assert!(
            !args.iter().any(|a| a.contains('\n')),
            "no multi-line arguments"
        );
    }

    #[test]
    fn a_model_in_the_node_command_wins() {
        for base in [
            vec!["--model".to_owned(), "sonnet".to_owned()],
            vec!["--model=sonnet".to_owned()],
        ] {
            let args = build_args(&spawn(), &base, Path::new("/m"), Path::new("/p"), None);
            assert!(
                !args.iter().any(|a| a == "--model"),
                "fleet default skipped: {args:?}"
            );
        }
        // Without one, the fleet's default model is passed.
        let args = build_args(
            &spawn(),
            &["--add-dir".into(), "x".into()],
            Path::new("/m"),
            Path::new("/p"),
            None,
        );
        assert!(args.join(" ").contains("--model opus"));
    }

    #[test]
    fn only_the_configured_mcp_servers_load_and_the_fleet_keeps_its_name() {
        // Every session is held to its own config, once, even when the spawn asks for it too
        // (the secretary's does).
        let mut s = spawn();
        let count = |args: &[String]| args.iter().filter(|a| *a == STRICT_MCP).count();
        assert_eq!(
            count(&build_args(&s, &[], Path::new("/m"), Path::new("/p"), None)),
            1
        );
        s.extra_args = vec![STRICT_MCP.into(), "--setting-sources".into(), String::new()];
        assert_eq!(
            count(&build_args(&s, &[], Path::new("/m"), Path::new("/p"), None)),
            1
        );

        // The kit's servers come along, secrets resolved; one named `fleet` cannot replace the
        // fleet's own permission server.
        let kit = r#"{"slack":{"command":"npx","env":{"SLACK_TOKEN":"xoxp-123456"}},"fleet":{"command":"evil"}}"#;
        let text = mcp_config(
            Path::new("/bin/fleet"),
            "127.0.0.1:9".parse().unwrap(),
            "tok",
            "worker",
            Some(kit),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let servers = v["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 2, "{servers:?}");
        assert_eq!(servers["fleet"]["args"], json!(["mcp"]));
        assert_eq!(servers["slack"]["env"]["SLACK_TOKEN"], "xoxp-123456");
        assert!(
            mcp_config(
                Path::new("/f"),
                "127.0.0.1:9".parse().unwrap(),
                "t",
                "w",
                Some("[1]")
            )
            .is_err()
        );
    }

    #[test]
    fn mcp_config_merges_extra_servers() {
        let text = mcp_config(
            Path::new("/bin/fleet"),
            "127.0.0.1:9".parse().unwrap(),
            "tok",
            "worker",
            spawn().extra_mcp_servers.as_deref(),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["fleet"]["env"]["FLEET_IPC_TOKEN"], "tok");
        assert_eq!(v["mcpServers"]["other"]["url"], "http://h/mcp");
    }

    #[test]
    fn session_id_comes_from_init_event() {
        assert_eq!(
            session_id(r#"{"type":"system","subtype":"init","session_id":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(session_id(r#"{"type":"assistant"}"#), None);
        assert_eq!(session_id("not json"), None);
    }

    #[test]
    fn streaming_noise_is_dropped() {
        assert!(is_noise(r#"{"type":"stream_event","event":{}}"#));
        assert!(is_noise(
            r#"{"type": "system", "subtype": "thinking_tokens"}"#
        ));
        assert!(!is_noise(r#"{"type":"system","subtype":"init"}"#));
        assert!(!is_noise(r#"{"type":"assistant","message":{}}"#));
        assert!(!is_noise("not json"));
        assert!(is_noise(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"one"}}}"#
        ));
        assert!(is_turn_result(
            r#"{"type": "result", "subtype": "success"}"#
        ));
    }

    fn stream(event: serde_json::Value) -> String {
        json!({"type": "stream_event", "event": event, "parent_tool_use_id": null}).to_string()
    }

    fn start(kind: &str) -> String {
        stream(json!({"type": "content_block_start", "index": 0,
                      "content_block": {"type": kind, "text": "", "name": "Bash"}}))
    }

    fn delta(text: &str) -> String {
        stream(json!({"type": "content_block_delta", "index": 0,
                      "delta": {"type": "text_delta", "text": text}}))
    }

    #[test]
    fn a_live_block_streams_the_text_so_far_at_a_bounded_rate() {
        let mut b = LiveBlock::default();
        let t0 = Instant::now();
        assert_eq!(b.feed(&start("text"), &[], t0), None);
        assert_eq!(
            b.feed(&delta("Looking"), &[], t0),
            Some(("text".into(), "Looking".into()))
        );
        // Too soon after the last one: kept, and sent whole with the next.
        assert_eq!(
            b.feed(&delta(" at"), &[], t0 + Duration::from_millis(10)),
            None
        );
        assert_eq!(
            b.feed(&delta(" it"), &[], t0 + PARTIAL_EVERY * 2),
            Some(("text".into(), "Looking at it".into()))
        );
        // The block ends; a delta without a block is not text of anything.
        assert_eq!(
            b.feed(
                &stream(json!({"type": "content_block_stop", "index": 0})),
                &[],
                t0
            ),
            None
        );
        assert_eq!(b.feed(&delta("stray"), &[], t0 + PARTIAL_EVERY * 4), None);
        // A tool call shows its name at once.
        assert_eq!(
            b.feed(&start("tool_use"), &[], t0 + PARTIAL_EVERY * 4),
            Some(("tool".into(), "Bash".into()))
        );
        // Thinking shows that it happens, never what it says.
        let mut b = LiveBlock::default();
        assert_eq!(
            b.feed(&start("thinking"), &[], t0),
            Some(("thinking".into(), String::new()))
        );
        let thought = stream(json!({"type": "content_block_delta", "index": 0,
                                    "delta": {"type": "thinking_delta", "thinking": "hmm"}}));
        assert_eq!(b.feed(&thought, &[], t0 + PARTIAL_EVERY * 2), None);
        // A block that missed a piece (the watch lapsed: the runner resets it) stays dropped
        // until the next one starts, so no text from either side of the gap is ever joined.
        let mut b = LiveBlock::default();
        b.feed(&start("text"), &[], t0);
        assert!(b.feed(&delta("before "), &[], t0).is_some());
        b.reset();
        assert_eq!(b.feed(&delta("after"), &[], t0 + PARTIAL_EVERY * 2), None);
        assert_eq!(b.feed(&start("text"), &[], t0), None);
        assert_eq!(
            b.feed(&delta("fresh"), &[], t0 + PARTIAL_EVERY * 4),
            Some(("text".into(), "fresh".into()))
        );
        // A subagent's stream is not the session's.
        let mut b = LiveBlock::default();
        let sub = json!({"type": "stream_event", "parent_tool_use_id": "toolu_1",
                         "event": {"type": "content_block_start", "index": 0,
                                   "content_block": {"type": "tool_use", "name": "Bash"}}});
        assert_eq!(b.feed(&sub.to_string(), &[], t0), None);
    }

    #[test]
    fn no_part_of_a_secret_leaves_in_a_live_block() {
        let secret = "s3cr3t-value-xyz".to_owned();
        let secrets = vec![secret.clone()];
        let mut b = LiveBlock::default();
        let mut now = Instant::now();
        b.feed(&start("text"), &secrets, now);
        let mut sent = Vec::new();
        // The secret arrives a character at a time, between ordinary words, and every
        // intermediate state is offered.
        let text = format!("the key is {secret} and that is all of it, said plainly");
        for c in text.chars() {
            now += PARTIAL_EVERY;
            if let Some((_, t)) = b.feed(&delta(&c.to_string()), &secrets, now) {
                sent.push(t);
            }
        }
        assert!(!sent.is_empty());
        for t in &sent {
            for n in 3..=secret.len() {
                assert!(
                    !t.contains(&secret[..n]),
                    "{:?} leaks {:?}",
                    t,
                    &secret[..n]
                );
            }
        }
        assert!(sent.last().unwrap().contains("[secret]"), "{sent:?}");
    }
}
