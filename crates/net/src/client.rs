//! Node side: keeps a connection to the orchestrator up, reconnecting with backoff.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use fleet_proto::{ALPN, Body, Envelope, Hello, PROTOCOL_VERSION};
use iroh::{Endpoint, EndpointAddr};
use tokio::sync::mpsc;

use crate::{
    frame::{read_frame, write_frame},
    session::{Ended, Inbound, Session},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub orchestrator: EndpointAddr,
    pub node_name: String,
    pub app_version: String,
    pub os: String,
    /// Invite secret for the first connection. Dropped after the first accepted handshake.
    pub join_secret: Option<String>,
}

/// A node's link to the orchestrator. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    session: Session,
    join_secret: Arc<Mutex<Option<String>>>,
    /// This node process's id in the handshake.
    incarnation: u64,
}

impl Client {
    pub fn new(orchestrator: EndpointAddr) -> (Self, mpsc::UnboundedReceiver<Inbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = Session::new(orchestrator.id, tx);
        (
            Self {
                session,
                join_secret: Arc::new(Mutex::new(None)),
                incarnation: crate::session::new_incarnation(),
            },
            rx,
        )
    }

    /// Queues a message for the orchestrator; delivered once the link is up.
    pub fn send(&self, body: Body) {
        self.session.send(body);
    }

    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    pub fn pending(&self) -> usize {
        self.session.pending()
    }

    /// Connects and stays connected until the orchestrator rejects this node, which is the
    /// only error it returns. Network failures are retried forever with backoff.
    pub async fn run(&self, endpoint: &Endpoint, cfg: ClientConfig) -> Result<()> {
        if cfg.join_secret.is_some() {
            *self.join_secret.lock().unwrap() = cfg.join_secret.clone();
        }
        let mut backoff = MIN_BACKOFF;
        loop {
            match self.connect_once(endpoint, &cfg).await {
                Ok(Attempt::Rejected(reason)) => bail!("orchestrator rejected this node: {reason}"),
                Ok(Attempt::Served) => backoff = MIN_BACKOFF,
                Ok(Attempt::Replaced) => return Ok(()),
                Err(e) => tracing::warn!("link to orchestrator failed: {e:#}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn connect_once(&self, endpoint: &Endpoint, cfg: &ClientConfig) -> Result<Attempt> {
        let (conn, mut send, recv, welcome) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let conn = endpoint.connect(cfg.orchestrator.clone(), ALPN).await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            let hello = Hello {
                protocol_version: PROTOCOL_VERSION,
                node_name: cfg.node_name.clone(),
                app_version: cfg.app_version.clone(),
                os: cfg.os.clone(),
                join_secret: self.join_secret.lock().unwrap().clone(),
                last_received_seq: self.session.last_received(),
                incarnation: self.incarnation,
                last_received_from: self.session.peer_incarnation(),
            };
            write_frame(
                &mut send,
                &Envelope::control(hello.last_received_seq, Body::Hello(hello)),
            )
            .await?;
            let env = read_frame(&mut recv)
                .await?
                .context("orchestrator closed the stream during the handshake")?;
            anyhow::Ok((conn, send, recv, env.body))
        })
        .await
        .map_err(|_| anyhow!("handshake timed out"))??;

        let welcome = match welcome {
            Body::Welcome(w) => w,
            Body::Rejected(r) => {
                let _ = send.finish();
                return Ok(Attempt::Rejected(r.reason));
            }
            other => bail!("unexpected handshake reply: {other:?}"),
        };
        // The key is on the orchestrator's allowlist now; the invite must not be reused.
        self.join_secret.lock().unwrap().take();
        // A restarted orchestrator has a fresh stream: our queued messages are renumbered for it.
        self.session.adopt_peer(welcome.incarnation);
        let welcome_ack = welcome.last_received_seq;
        tracing::info!(orchestrator = %conn.remote_id().fmt_short(), "connected to orchestrator");
        self.session.emit(Inbound::Connected {
            peer: conn.remote_id(),
            hello: None,
        });

        let ended = self.session.run(send, recv, welcome_ack).await;
        conn.close(0u32.into(), b"bye");
        match ended {
            Ended::Replaced => Ok(Attempt::Replaced),
            Ended::Closed(result) => {
                self.session.emit(Inbound::Disconnected {
                    peer: conn.remote_id(),
                });
                result.map(|()| Attempt::Served)
            }
        }
    }
}

enum Attempt {
    Rejected(String),
    /// The connection worked for a while and then ended.
    Served,
    /// Another `run` took over this client's session.
    Replaced,
}
