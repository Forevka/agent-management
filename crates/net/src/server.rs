//! Orchestrator side: accepts node connections and keeps one [`Session`] per node.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use fleet_proto::{Body, Envelope, Hello, PROTOCOL_VERSION, Rejected, Welcome};
use iroh::{
    EndpointId,
    endpoint::{Connection, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use tokio::sync::mpsc;

use crate::{
    frame::{read_frame, write_frame},
    session::{Ended, Inbound, Session},
};

/// How long a new connection may take to open its stream and say hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Decides whether a node may connect. Called for every connection, after the version check.
///
/// The iroh handshake has already proven that the peer holds the private key for `node`, so
/// an allowlist of keys is a complete authentication scheme; `hello.join_secret` lets an
/// unknown key in once, when it presents a valid invite.
pub trait Authorizer: Send + Sync + 'static {
    fn authorize<'a>(
        &'a self,
        node: EndpointId,
        hello: &'a Hello,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// A boxed future, so [`Authorizer`] stays object safe while implementations query a database.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

struct Inner {
    incarnation: u64,
    sessions: Mutex<HashMap<EndpointId, Session>>,
    auth: Arc<dyn Authorizer>,
    events: mpsc::UnboundedSender<Inbound>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

impl Server {
    /// Returns the server (register it on a `Router` under [`fleet_proto::ALPN`]) and the
    /// stream of everything nodes say.
    pub fn new(auth: impl Authorizer) -> (Self, mpsc::UnboundedReceiver<Inbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let server = Self {
            inner: Arc::new(Inner {
                incarnation: crate::session::new_incarnation(),
                sessions: Mutex::new(HashMap::new()),
                auth: Arc::new(auth),
                events: tx,
            }),
        };
        (server, rx)
    }

    /// Queues a message for a node. If the node is offline it is delivered on reconnect.
    pub fn send(&self, node: EndpointId, body: Body) {
        self.session(node).send(body);
    }

    pub fn is_connected(&self, node: EndpointId) -> bool {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(&node)
            .is_some_and(|s| s.is_connected())
    }

    pub fn pending(&self, node: EndpointId) -> usize {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(&node)
            .map_or(0, |s| s.pending())
    }

    fn session(&self, node: EndpointId) -> Session {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .entry(node)
            .or_insert_with(|| Session::new(node, self.inner.events.clone()))
            .clone()
    }

    async fn handle(&self, conn: Connection) -> Result<()> {
        let node = conn.remote_id();
        let (mut send, recv, hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let (send, mut recv) = conn.accept_bi().await?;
            let env = read_frame(&mut recv)
                .await?
                .context("stream closed before hello")?;
            let Body::Hello(hello) = env.body else {
                bail!("first frame was not a hello")
            };
            anyhow::Ok((send, recv, hello))
        })
        .await
        .map_err(|_| anyhow!("handshake timed out"))??;

        if hello.protocol_version != PROTOCOL_VERSION {
            let reason = format!(
                "protocol version {} is not supported (orchestrator speaks {PROTOCOL_VERSION}); update the node",
                hello.protocol_version
            );
            return reject(&conn, &mut send, reason).await;
        }
        if let Err(reason) = self.inner.auth.authorize(node, &hello).await {
            return reject(&conn, &mut send, reason).await;
        }

        let session = self.session(node);
        session.adopt_peer(hello.incarnation);
        // The node's acknowledgement only counts if it refers to this orchestrator process; after
        // our own restart our stream starts over at 1.
        let peer_last_received = if hello.last_received_from == self.inner.incarnation {
            hello.last_received_seq
        } else {
            0
        };
        let welcome = Welcome {
            last_received_seq: session.last_received(),
            incarnation: self.inner.incarnation,
        };
        write_frame(
            &mut send,
            &Envelope::control(welcome.last_received_seq, Body::Welcome(welcome)),
        )
        .await?;
        tracing::info!(node = %node.fmt_short(), name = %hello.node_name, "node connected");
        session.emit(Inbound::Connected {
            peer: node,
            hello: Some(hello),
        });

        match session.run(send, recv, peer_last_received).await {
            Ended::Replaced => Ok(()),
            Ended::Closed(result) => {
                tracing::info!(node = %node.fmt_short(), ?result, "node disconnected");
                session.emit(Inbound::Disconnected { peer: node });
                conn.close(0u32.into(), b"bye");
                result
            }
        }
    }
}

async fn reject(conn: &Connection, send: &mut SendStream, reason: String) -> Result<()> {
    tracing::warn!(node = %conn.remote_id().fmt_short(), %reason, "node rejected");
    write_frame(
        send,
        &Envelope::control(
            0,
            Body::Rejected(Rejected {
                reason: reason.clone(),
            }),
        ),
    )
    .await?;
    send.finish()?;
    // Give the frame a moment to leave before the connection is torn down.
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    conn.close(1u32.into(), b"rejected");
    Ok(())
}

impl ProtocolHandler for Server {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        self.handle(conn)
            .await
            .map_err(|e| AcceptError::from_err(std::io::Error::other(format!("{e:#}"))))
    }
}
