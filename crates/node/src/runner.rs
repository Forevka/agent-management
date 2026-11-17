//! Starts and supervises one Claude Code process per agent.
//!
//! Claude runs headless with stream-json on both sides: stdout is a stream of JSON events that
//! the node forwards verbatim (the orchestrator keeps the transcript), and stdin stays open so
//! the orchestrator can feed the session new user messages (human answers, other agents'
//! messages) without restarting it.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
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

pub struct AgentHandle {
    pub agent_id: String,
    pub task_id: String,
    /// Authenticates this agent's MCP shim on the node's loopback IPC port.
    pub ipc_token: String,
    stdin: mpsc::UnboundedSender<String>,
    stop: CancellationToken,
    after_plan_mode: Option<String>,
}

impl AgentHandle {
    /// Queues a user turn. Returns false if the process is gone.
    pub fn send_user_message(&self, text: &str) -> bool {
        let line = json!({"type":"user","message":{"role":"user","content":text}}).to_string();
        self.stdin.send(line).is_ok()
    }

    pub fn switch_to_after_plan_mode(&self) {
        if let Some(mode) = &self.after_plan_mode {
            let line = json!({
                "type": "control_request",
                "request_id": random_token(),
                "request": {"subtype": "set_permission_mode", "mode": mode}
            })
            .to_string();
            let _ = self.stdin.send(line);
        }
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }
}

/// The full command line for a spawn, separated out so it can be tested without a process.
pub fn build_args(spawn: &SpawnAgent, mcp_config: &Path) -> Vec<String> {
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(model) = &spawn.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(prompt) = &spawn.append_system_prompt {
        args.extend(["--append-system-prompt".into(), prompt.clone()]);
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
    secrets::write(data_dir, &spawn.task_id, &spawn.secret_files)?;

    let mut cmd = Command::new(&program);
    cmd.args(base_args)
        .args(build_args(&spawn, &mcp_path))
        .current_dir(&project_dir)
        .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
        .env(
            "FLEET_SECRETS_DIR",
            secrets::task_dir(data_dir, &spawn.task_id),
        )
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
    #[cfg(windows)]
    {
        // No console window per agent when the node runs in the background.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;

    let (agent_id, task_id) = (spawn.agent_id.clone(), spawn.task_id.clone());
    node.event(
        &agent_id,
        &task_id,
        AgentEventKind::Started {
            pid: child.id().unwrap_or(0),
        },
    );

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
    });

    let stdout = child.stdout.take().context("no stdout")?;
    let n = node.clone();
    let (a, t) = (agent_id.clone(), task_id.clone());
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut session_sent = false;
        while let Ok(Some(mut line)) = lines.next_line().await {
            if !session_sent && let Some(id) = session_id(&line) {
                session_sent = true;
                n.event(&a, &t, AgentEventKind::Session { session_id: id });
            }
            if is_noise(&line) {
                continue;
            }
            truncate(&mut line);
            n.event(&a, &t, AgentEventKind::Output { json: line });
        }
    });
    let stderr = child.stderr.take().context("no stderr")?;
    let n = node.clone();
    let (a, t) = (agent_id.clone(), task_id.clone());
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(mut line)) = lines.next_line().await {
            truncate(&mut line);
            n.event(&a, &t, AgentEventKind::Stderr { line });
        }
    });

    let stop = CancellationToken::new();
    let handle = AgentHandle {
        agent_id: agent_id.clone(),
        task_id: task_id.clone(),
        ipc_token: ipc_token.clone(),
        stdin: stdin_tx,
        stop: stop.clone(),
        after_plan_mode: spawn.after_plan_mode.clone(),
    };
    handle.send_user_message(&spawn.prompt);

    let data_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = stop.cancelled() => {
                let _ = child.kill().await;
                child.wait().await.ok()
            }
        };
        // Let the readers drain so the exit event comes after the last output line.
        let _ = out_task.await;
        let _ = err_task.await;
        secrets::remove(&data_dir, &task_id);
        node.event(
            &agent_id,
            &task_id,
            AgentEventKind::Exited {
                code: status.and_then(|s| s.code()),
            },
        );
        node.agent_exited(&agent_id, &ipc_token);
    });
    Ok(handle)
}

/// The MCP config for one agent: the fleet shim plus any extra servers (e.g. Hindsight).
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
            extra_mcp_servers: Some(r#"{"hindsight":{"type":"http","url":"http://h/mcp"}}"#.into()),
            secret_files: vec![],
        }
    }

    #[test]
    fn args_cover_headless_contract() {
        let args = build_args(&spawn(), Path::new("/tmp/m.json"));
        let joined = args.join(" ");
        for needle in [
            "-p",
            "--input-format stream-json",
            "--output-format stream-json",
            "--permission-mode plan",
            "--permission-prompt-tool mcp__fleet__permission",
            "--mcp-config /tmp/m.json",
            "--model opus",
            "--resume s1",
        ] {
            assert!(joined.contains(needle), "{needle} missing from {joined}");
        }
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
        assert_eq!(v["mcpServers"]["hindsight"]["url"], "http://h/mcp");
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
}
