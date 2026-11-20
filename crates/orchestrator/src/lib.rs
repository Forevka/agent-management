//! The orchestrator: the fleet's single point of coordination, running on the VPS.
//!
//! It keeps all state in Postgres, talks to nodes over the iroh control link, to the owner
//! through Telegram (behind the [`Chat`] trait, so the core is testable without Telegram), and
//! serves a small web UI for plans, projects, secrets, nodes and bots.

// Query results are read into tuples matching each SELECT; naming every one would add types
// that are used exactly once.
#![allow(clippy::type_complexity)]

pub mod admin;
pub mod api;
pub mod auth;
pub mod chat;
pub mod config;
pub mod core;
pub mod feeds;
pub mod habits;
pub mod human;
pub mod jira;
pub mod links;
pub mod memory;
pub mod model;
pub mod plan_gate;
pub mod preflight;
pub mod prompts;
pub mod retro;
pub mod secretary;
pub mod tasks;
pub mod tg;
pub mod tools;
pub mod topics;
pub mod vault;
pub mod wakeups;
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
///
/// **Tolerates migrations it does not know** (`ignore_missing`), which is what makes rolling
/// back safe. By default sqlx refuses to start against a database that has an applied migration
/// missing from the binary (`VersionMissing`) — so after a release that added a migration failed
/// its health check, the previous release would crash-loop instead of taking over, while the
/// updater announced a successful rollback. Since migrations are add-only, older code works
/// against a newer schema, provided every new column is nullable or has a default (see
/// CLAUDE.md), so it can still insert rows that do not mention it.
///
/// Built through sqlx's public `set_ignore_missing` rather than its `#[doc(hidden)]` fields,
/// which it documents as semver-exempt. The dry run (`preflight`) uses this same migrator, so it
/// accepts and refuses exactly what a real start would.
pub static MIGRATOR: std::sync::LazyLock<sqlx::migrate::Migrator> =
    std::sync::LazyLock::new(|| {
        let mut m = sqlx::migrate!("./migrations");
        m.set_ignore_missing(true);
        m
    });
