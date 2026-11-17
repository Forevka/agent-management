//! Reliable, ordered, exactly-once message delivery to one peer across reconnects.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, bail};
use fleet_proto::{Body, Envelope, Hello};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream},
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::frame::{read_frame, write_frame};

/// How often an idle link sends a heartbeat.
pub(crate) const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
/// A link that has received nothing (not even a heartbeat) for this long is considered dead.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(35);

/// What a session reports to its owner.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A connection to `peer` completed its handshake. On the orchestrator, `hello` is the
    /// node's handshake; on a node it is `None`.
    Connected {
        peer: EndpointId,
        hello: Option<Hello>,
    },
    /// A reliable message, delivered exactly once and in the order it was sent.
    Message { peer: EndpointId, body: Body },
    /// The current connection to `peer` ended. Messages sent from now on are queued.
    Disconnected { peer: EndpointId },
}

struct State {
    /// Sequence number for the next message we send.
    next_seq: u64,
    /// Sent (or waiting to be sent) and not yet acknowledged, in sequence order.
    outbox: VecDeque<(u64, Body)>,
    /// Highest peer sequence number delivered to our owner.
    last_received: u64,
    /// We delivered something the peer has not seen an ack for yet.
    ack_dirty: bool,
    /// The connection currently driving this session, if any: its generation and the token
    /// that stops it when a newer connection takes over.
    current: Option<(u64, CancellationToken)>,
    /// Generation number for the next connection.
    next_generation: u64,
    /// The peer process our sequence numbers are counted against.
    peer_incarnation: Option<u64>,
}

impl State {
    fn apply_ack(&mut self, ack: u64) {
        while self.outbox.front().is_some_and(|(seq, _)| *seq <= ack) {
            self.outbox.pop_front();
        }
    }
}

/// One peer's message stream. Cheap to clone; all clones share the same state.
#[derive(Clone)]
pub struct Session {
    peer: EndpointId,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    inbound: mpsc::UnboundedSender<Inbound>,
}

/// How a connection driven by [`Session::run`] ended.
#[derive(Debug)]
pub(crate) enum Ended {
    /// A newer connection for the same peer took over; it owns the session now.
    Replaced,
    /// The connection closed or failed; the session is idle until the next one.
    Closed(Result<()>),
}

impl Session {
    pub fn new(peer: EndpointId, inbound: mpsc::UnboundedSender<Inbound>) -> Self {
        Self {
            peer,
            state: Arc::new(Mutex::new(State {
                next_seq: 1,
                outbox: VecDeque::new(),
                last_received: 0,
                ack_dirty: false,
                current: None,
                next_generation: 1,
                peer_incarnation: None,
            })),
            wake: Arc::new(Notify::new()),
            inbound,
        }
    }

    pub fn peer(&self) -> EndpointId {
        self.peer
    }

    /// Queues a message for reliable delivery. Never blocks and never fails: if the peer is
    /// offline the message waits until it reconnects.
    pub fn send(&self, body: Body) {
        assert!(
            !body.is_control(),
            "control frames are not sent through the outbox"
        );
        {
            let mut st = self.state.lock().unwrap();
            let seq = st.next_seq;
            st.next_seq += 1;
            st.outbox.push_back((seq, body));
        }
        self.wake.notify_one();
    }

    /// Highest peer sequence number delivered so far (sent in the handshake).
    pub fn last_received(&self) -> u64 {
        self.state.lock().unwrap().last_received
    }

    /// Messages not yet acknowledged by the peer.
    pub fn pending(&self) -> usize {
        self.state.lock().unwrap().outbox.len()
    }

    /// Records which peer process we talk to. If it is a different one than before (the peer
    /// restarted and lost its state), both streams start over: undelivered messages are
    /// renumbered from 1 and nothing is considered received yet.
    pub(crate) fn adopt_peer(&self, incarnation: u64) {
        let mut st = self.state.lock().unwrap();
        if st.peer_incarnation == Some(incarnation) {
            return;
        }
        if st.peer_incarnation.is_some() {
            tracing::info!(peer = %self.peer.fmt_short(), "peer restarted; resetting the message stream");
        }
        st.last_received = 0;
        let mut seq = 1;
        for (s, _) in st.outbox.iter_mut() {
            *s = seq;
            seq += 1;
        }
        st.next_seq = seq;
        st.peer_incarnation = Some(incarnation);
    }

    /// The peer incarnation `last_received` counts, or 0 before the first handshake.
    pub(crate) fn peer_incarnation(&self) -> u64 {
        self.state.lock().unwrap().peer_incarnation.unwrap_or(0)
    }

    /// Drops the current connection (e.g. the node was revoked). Queued messages stay queued.
    pub fn disconnect(&self) {
        if let Some((_, token)) = self.state.lock().unwrap().current.take() {
            token.cancel();
        }
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().unwrap().current.is_some()
    }

    pub(crate) fn emit(&self, event: Inbound) {
        // The owner dropping its receiver means it is shutting down; nothing to do then.
        let _ = self.inbound.send(event);
    }

    /// Drives the session over one connection's control stream until it ends.
    ///
    /// `peer_last_received` comes from the handshake; everything newer in the outbox is
    /// replayed first, which is what makes delivery survive reconnects.
    pub(crate) async fn run(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        peer_last_received: u64,
    ) -> Ended {
        let cancel = CancellationToken::new();
        let generation = {
            let mut st = self.state.lock().unwrap();
            let generation = st.next_generation;
            st.next_generation += 1;
            if let Some((_, old)) = st.current.replace((generation, cancel.clone())) {
                old.cancel();
            }
            st.apply_ack(peer_last_received);
            // The peer may have lost our last ack along with the old connection.
            st.ack_dirty = true;
            generation
        };
        self.wake.notify_one();

        let result = tokio::select! {
            r = self.write_loop(&mut send, peer_last_received) => r,
            r = self.read_loop(&mut recv) => r,
            _ = cancel.cancelled() => {
                // Cancelled either by a newer connection (which installed itself as current) or
                // by `disconnect` (which left no current connection).
                let mut st = self.state.lock().unwrap();
                if st.current.as_ref().is_some_and(|(g, _)| *g != generation) {
                    return Ended::Replaced;
                }
                st.current = None;
                drop(st);
                let _ = send.finish();
                return Ended::Closed(Err(anyhow::anyhow!("disconnected by the local side")));
            }
        };

        let mut st = self.state.lock().unwrap();
        if st.current.as_ref().is_some_and(|(g, _)| *g == generation) {
            st.current = None;
            drop(st);
            let _ = send.finish();
            Ended::Closed(result)
        } else {
            Ended::Replaced
        }
    }

    async fn write_loop(&self, send: &mut SendStream, peer_last_received: u64) -> Result<()> {
        // Everything with a higher sequence number than this has not been written on this
        // connection yet.
        let mut sent_upto = peer_last_received;
        let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
        heartbeat.tick().await;
        loop {
            let (frames, ack) = {
                let mut st = self.state.lock().unwrap();
                let frames: Vec<Envelope> = st
                    .outbox
                    .iter()
                    .filter(|(seq, _)| *seq > sent_upto)
                    .map(|(seq, body)| Envelope {
                        seq: *seq,
                        ack: st.last_received,
                        body: body.clone(),
                    })
                    .collect();
                let ack = (frames.is_empty() && st.ack_dirty).then_some(st.last_received);
                st.ack_dirty = false;
                (frames, ack)
            };
            for env in &frames {
                write_frame(send, env).await?;
                sent_upto = env.seq;
            }
            if let Some(ack) = ack {
                write_frame(send, &Envelope::control(ack, Body::Ack)).await?;
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = heartbeat.tick() => {
                    let ack = self.last_received();
                    write_frame(send, &Envelope::control(ack, Body::Heartbeat)).await?;
                }
            }
        }
    }

    async fn read_loop(&self, recv: &mut RecvStream) -> Result<()> {
        loop {
            let env = match tokio::time::timeout(IDLE_TIMEOUT, read_frame(recv)).await {
                Err(_) => bail!(
                    "no traffic from {} for {IDLE_TIMEOUT:?}",
                    self.peer.fmt_short()
                ),
                Ok(r) => match r? {
                    Some(env) => env,
                    None => return Ok(()),
                },
            };
            let deliver = {
                let mut st = self.state.lock().unwrap();
                st.apply_ack(env.ack);
                if env.is_control() {
                    match env.body {
                        Body::Heartbeat | Body::Ack => None,
                        other => bail!("unexpected control frame mid-stream: {other:?}"),
                    }
                } else if env.seq <= st.last_received {
                    // A replay of something we already delivered: re-ack, do not deliver twice.
                    st.ack_dirty = true;
                    None
                } else if env.seq == st.last_received + 1 {
                    st.last_received = env.seq;
                    st.ack_dirty = true;
                    Some(env.body)
                } else {
                    bail!(
                        "sequence gap from {}: expected {}, got {}",
                        self.peer.fmt_short(),
                        st.last_received + 1,
                        env.seq
                    );
                }
            };
            if let Some(body) = deliver {
                self.emit(Inbound::Message {
                    peer: self.peer,
                    body,
                });
                self.wake.notify_one();
            }
        }
    }
}

/// A random, non-zero id for this process's side of every session.
pub(crate) fn new_incarnation() -> u64 {
    use rand::RngCore;
    rand::rng().next_u64() | 1
}
