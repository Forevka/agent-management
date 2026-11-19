//! The orchestrator: the fleet's single point of coordination, running on the VPS.
//!
//! It keeps all state in Postgres, talks to nodes over the iroh control link, to the owner
//! through Telegram (behind the [`Chat`] trait, so the core is testable without Telegram), and
//! serves a small web UI for plans, projects, secrets, nodes and bots.

// Query results are read into tuples matching each SELECT; naming every one would add types
// that are used exactly once.
#![allow(clippy::type_complexity)]

pub mod admin;
pub mod auth;
pub mod chat;
pub mod config;
pub mod core;
pub mod feeds;
pub mod human;
pub mod memory;
pub mod model;
pub mod prompts;
pub mod secretary;
pub mod tasks;
pub mod tg;
pub mod tools;
pub mod topics;
pub mod vault;
pub mod web;
pub mod workflows;
pub mod working;

#[cfg(test)]
mod e2e;

pub use chat::{Button, Chat, From};
pub use config::Config;
pub use core::Orchestrator;
pub use vault::Vault;

/// Embedded migrations, applied at startup.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
