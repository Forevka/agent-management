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
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use fleet_proto::{AgentEventKind, ClaudeAuth, SpawnAgent};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use crate::{Node, config::NodeConfig, secrets, tools};

/// Forwarded output lines are capped so one huge tool result cannot exceed the frame limit.
const MAX_LINE: usize = 512 * 1024;
/// MCP tool calls normally time out quickly; the permission tool waits for a human, possibly
/// for hours, so the agent's MCP client must wait far longer (one week, in ms).
const MCP_TOOL_TIMEOUT_MS: &str = "604800000";
/// After a graceful stop, how long Claude may take to finish its turn before it is killed.
const GRACEFUL_STOP_LIMIT: Duration = Duration::from_secs(600);
/// Secret values shorter than this are not redacted from output: they would match too much.
const MIN_REDACT_LEN: usize = 6;

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

/// The full command line for a spawn, separated out so it can be tested without a process.
pub fn build_args(
    spawn: &SpawnAgent,
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
    if let Some(model) = &spawn.model {
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
    let mcp_path = agent_dir.join("mcp.json");
    std::fs::write(
        &mcp_path,
        mcp_config(
            exe,
            ipc_addr,
            &ipc_token,
            &role,
            spawn.extra_mcp_servers.as_deref(),
        )?,
    )?;
    let system_path = match &spawn.append_system_prompt {
        Some(text) => {
            let p = agent_dir.join("system-prompt.md");
            std::fs::write(&p, text)?;
            Some(p)
        }
        None => None,
    };
    // Secrets are per run: a replaced run's cleanup must not delete the new run's files.
    let secrets_key = format!("{}-{}", spawn.task_id, spawn.run_id);
    secrets::write(data_dir, &secrets_key, &spawn.secret_files)?;
    let redact: Arc<Vec<String>> = Arc::new(
        spawn
            .secret_files
            .iter()
            .flat_map(|f| secrets::parse_env(&f.content).into_values())
            .filter(|v| v.len() >= MIN_REDACT_LEN)
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
    let mut cmd = Command::new(&program);
    cmd.args(base_args)
        .args(build_args(
            &spawn,
            &mcp_path,
            &plugin_dir,
            system_path.as_deref(),
        ))
        .env("PATH", std::env::join_paths(path)?)
        .env("FLEET_IPC_ADDR", ipc_addr.to_string())
        .env("FLEET_IPC_TOKEN", &ipc_token)
        .env("FLEET_ROLE", &role)
        .env(
            "FLEET_SECRETS_DIR",
            secrets::task_dir(data_dir, &secrets_key),
        )
        .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
        .current_dir(&project_dir)
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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
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
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut session_sent = false;
        while let Ok(Some(mut line)) = lines.next_line().await {
            if !session_sent && let Some(id) = session_id(&line) {
                session_sent = true;
                ev_out(AgentEventKind::Session { session_id: id });
            }
            if is_noise(&line) {
                continue;
            }
            act.touch(is_turn_result(&line));
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
        secrets::remove(&data_dir, &secrets_key);
        ev(AgentEventKind::Exited {
            code: status.and_then(|s| s.code()),
        });
        node.agent_exited(&agent_id, &ipc_token);
    });
    Ok(handle)
}

/// Everything an agent process started, so a stop can end all of it.
struct ProcessTree {
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

impl ProcessTree {
    fn adopt(child: &tokio::process::Child) -> Self {
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

    async fn kill(&self, child: &mut tokio::process::Child) {
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
    let mut servers = serde_json::Map::new();
    if let Some(extra) = extra.filter(|e| !e.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(extra).context("extra MCP servers are not a JSON object")?;
        if let serde_json::Value::Object(m) = v {
            servers.extend(m);
        }
    }
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

/// Keeps secret values the session read (e.g. through `fleet secret get`) out of the
/// transcript stored on the orchestrator.
fn redact_secrets(line: &mut String, values: &[String]) {
    for v in values {
        if line.contains(v.as_str()) {
            *line = line.replace(v.as_str(), "[secret]");
        }
        // The value as it appears inside a JSON string (escaped quotes, backslashes, newlines).
        let escaped = serde_json::to_string(v).unwrap_or_default();
        let escaped = escaped.trim_matches('"');
        if escaped != v && line.contains(escaped) {
            *line = line.replace(escaped, "[secret]");
        }
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

fn sanitize(s: &str) -> String {
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
        ] {
            assert!(joined.contains(needle), "{needle} missing from {joined}");
        }
        assert!(
            !args.iter().any(|a| a.contains('\n')),
            "no multi-line arguments"
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
        assert!(is_turn_result(
            r#"{"type": "result", "subtype": "success"}"#
        ));
    }

    #[test]
    fn secrets_are_redacted_raw_and_json_escaped() {
        let values = vec!["hunter22".to_owned(), "line1\nline2\"q".to_owned()];
        let mut a = "token is hunter22!".to_owned();
        redact_secrets(&mut a, &values);
        assert_eq!(a, "token is [secret]!");
        let mut b = serde_json::json!({"out": "key: line1\nline2\"q"}).to_string();
        redact_secrets(&mut b, &values);
        assert!(!b.contains("line1"), "{b}");
    }
}
