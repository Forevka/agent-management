//! The control link between the orchestrator and its nodes, over iroh.
//!
//! Topology is hub and spoke: nodes dial the orchestrator, never each other. Each node has one
//! long-lived [`Session`] on each side. A session outlives individual QUIC connections: messages
//! sent while the peer is offline wait in its outbox and are replayed, in order and exactly once,
//! when the node reconnects (see [`fleet_proto::Envelope`] for the sequence/ack scheme).
//!
//! Outboxes are in memory for now, so a process restart loses unacknowledged messages. Durable
//! outboxes (Postgres on the orchestrator, a local file on nodes) come with the hardening phase.

mod client;
mod endpoint;
mod frame;
mod server;
mod session;
mod ticket;

pub use client::{Client, ClientConfig};
pub use endpoint::{RelayChoice, bind_endpoint};
pub use server::{Authorizer, BoxFuture, Server};
pub use session::{Inbound, Session};
pub use ticket::InviteTicket;

pub use iroh::{EndpointAddr, EndpointId, SecretKey};
