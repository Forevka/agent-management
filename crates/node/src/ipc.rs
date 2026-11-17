//! Loopback IPC between agents' `fleet mcp` shims and their node.
//!
//! One request per TCP connection, as a JSON line `{"token", "tool", "args"}`, answered with a
//! JSON line `{"ok", "result"}`. The token, issued per agent process, is the only thing that
//! identifies the caller: an agent cannot speak for another agent even though every agent can
//! reach the port.

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

use crate::Node;

/// Requests are small; a cap keeps a confused client from making the node buffer gigabytes.
const MAX_REQUEST: usize = 4 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcRequest {
    pub token: String,
    pub tool: String,
    /// JSON arguments, as text.
    pub args: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    pub result: String,
}

pub async fn serve(node: Arc<Node>) -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let node = node.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(node, stream).await {
                            tracing::debug!("ipc connection failed: {e:#}");
                        }
                    });
                }
                Err(e) => tracing::warn!("ipc accept failed: {e}"),
            }
        }
    });
    Ok(addr)
}

async fn handle(node: Arc<Node>, stream: TcpStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read.take(MAX_REQUEST as u64));
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp = match serde_json::from_str::<IpcRequest>(&line) {
        Ok(req) => {
            let r = node.call_tool(&req.token, req.tool, req.args).await;
            IpcResponse {
                ok: r.ok,
                result: r.result,
            }
        }
        Err(e) => IpcResponse {
            ok: false,
            result: format!("bad request: {e}"),
        },
    };
    let mut out = serde_json::to_vec(&resp)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    Ok(())
}

/// Client side, used by the shim.
pub async fn call(addr: &str, req: &IpcRequest) -> Result<IpcResponse> {
    let stream = TcpStream::connect(addr).await?;
    let (read, mut write) = stream.into_split();
    let mut out = serde_json::to_vec(req)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}
