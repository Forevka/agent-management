//! `fleet mcp`: a stdio MCP server that Claude Code starts inside each agent session.
//!
//! It is deliberately thin: it lists the fleet tools and forwards every call to the node over
//! loopback IPC; the orchestrator does the real work. MCP over stdio is newline-delimited
//! JSON-RPC 2.0, and only the handful of methods a tools-only server needs are implemented.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::Mutex,
};

use crate::{
    ipc::{self, IpcRequest},
    tools,
};

const FALLBACK_PROTOCOL: &str = "2025-06-18";

pub struct ShimEnv {
    pub ipc_addr: String,
    pub token: String,
    pub role: String,
}

impl ShimEnv {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            ipc_addr: std::env::var("FLEET_IPC_ADDR")
                .context("FLEET_IPC_ADDR not set; `fleet mcp` is started by the node")?,
            token: std::env::var("FLEET_IPC_TOKEN").context("FLEET_IPC_TOKEN not set")?,
            role: std::env::var("FLEET_ROLE").unwrap_or_else(|_| "worker".into()),
        })
    }
}

pub async fn serve_stdio(env: ShimEnv) -> Result<()> {
    serve(env, tokio::io::stdin(), tokio::io::stdout()).await
}

pub async fn serve<R, W>(env: ShimEnv, input: R, output: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let env = Arc::new(env);
    let out = Arc::new(Mutex::new(output));
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write(&out, json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}})).await?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notifications (initialized, cancelled, ...) need no answer.
            continue;
        };
        match method.as_str() {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(FALLBACK_PROTOCOL);
                let result = json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fleet", "version": fleet_proto::BUILD_VERSION}
                });
                write(&out, json!({"jsonrpc":"2.0","id":id,"result":result})).await?;
            }
            "ping" => write(&out, json!({"jsonrpc":"2.0","id":id,"result":{}})).await?,
            "tools/list" => {
                write(
                    &out,
                    json!({"jsonrpc":"2.0","id":id,"result":{"tools": tools::list()}}),
                )
                .await?;
            }
            "tools/call" => {
                // Calls run concurrently: the permission tool can wait hours for a human while
                // the session keeps making other calls.
                let (env, out) = (env.clone(), out.clone());
                tokio::spawn(async move {
                    let name = params
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    let req = IpcRequest {
                        token: env.token.clone(),
                        tool: name,
                        args: args.to_string(),
                    };
                    let (ok, text) = match ipc::call(&env.ipc_addr, &req).await {
                        Ok(r) => (r.ok, r.result),
                        Err(e) => (false, format!("fleet node unreachable: {e:#}")),
                    };
                    let result = json!({"content":[{"type":"text","text":text}],"isError":!ok});
                    let _ = write(&out, json!({"jsonrpc":"2.0","id":id,"result":result})).await;
                });
            }
            other => {
                let err = json!({"code":-32601,"message":format!("method not found: {other}")});
                write(&out, json!({"jsonrpc":"2.0","id":id,"error":err})).await?;
            }
        }
    }
    Ok(())
}

async fn write<W: AsyncWrite + Unpin>(out: &Mutex<W>, v: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&v)?;
    bytes.push(b'\n');
    let mut w = out.lock().await;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Drives the shim like Claude Code does, against a fake node IPC endpoint.
    #[tokio::test]
    async fn initialize_list_and_call() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                let (r, mut w) = s.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                let req: IpcRequest = serde_json::from_str(&line).unwrap();
                let ok = req.token == "tok";
                let resp = json!({"ok": ok, "result": format!("{}:{}", req.tool, req.args)});
                w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
            }
        });
        let (mut client, server_io) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let env = ShimEnv {
            ipc_addr: addr,
            token: "tok".into(),
            role: "implementor".into(),
        };
        tokio::spawn(serve(env, server_read, server_write));

        let send = |v: Value| format!("{v}\n");
        client.write_all(send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).as_bytes()).await.unwrap();
        client
            .write_all(
                send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).as_bytes(),
            )
            .await
            .unwrap();
        client
            .write_all(send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).as_bytes())
            .await
            .unwrap();
        client.write_all(send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"permission","arguments":{"text":"hi"}}})).as_bytes()).await.unwrap();
        client
            .write_all(send(json!({"jsonrpc":"2.0","id":4,"method":"nope"})).as_bytes())
            .await
            .unwrap();

        let mut lines = BufReader::new(client).lines();
        let mut by_id = std::collections::HashMap::new();
        while by_id.len() < 4 {
            let v: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            by_id.insert(v["id"].as_i64().unwrap(), v);
        }
        assert_eq!(by_id[&1]["result"]["protocolVersion"], "2025-06-18");
        let names: Vec<&str> = by_id[&2]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["permission"]);
        assert_eq!(by_id[&3]["result"]["isError"], false);
        assert_eq!(
            by_id[&3]["result"]["content"][0]["text"],
            r#"permission:{"text":"hi"}"#
        );
        assert_eq!(by_id[&4]["error"]["code"], -32601);
    }
}
