//! Watching a session live: the web UI's session page.
//!
//! The transcript itself is the task's `task_events`, recorded as node events arrive; a page
//! following a session is woken by [`crate::core::Orchestrator::events`] rather than polling. What
//! a run is writing before Claude finishes a block (its token-by-token output) is never stored: a
//! node streams it only for tasks someone is watching (`Body::WatchTasks`), and it reaches the
//! pages that watch them through [`Live::partials`].

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use fleet_net::EndpointId;
use fleet_proto::{Body, RunPartial};
use tokio::sync::broadcast;

use crate::core::Orchestrator;

/// How many partials a slow page may fall behind by before it skips ahead. Each is the whole
/// block so far, so a skipped one loses nothing the next does not carry.
const PARTIALS_BUFFER: usize = 256;

pub struct Live {
    /// Task -> how many pages watch it now.
    watchers: Mutex<HashMap<String, usize>>,
    /// Every watched run's block in progress, as nodes send them, with the newest transcript
    /// line recorded when it arrived: the line that finishes the block is recorded after it, so
    /// a page that already has a later line knows the partial is stale.
    pub partials: broadcast::Sender<(i64, RunPartial)>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            watchers: Mutex::default(),
            partials: broadcast::channel(PARTIALS_BUFFER).0,
        }
    }
}

impl Live {
    /// The tasks someone watches, sorted so two pushes of the same set look the same.
    pub fn watched(&self) -> Vec<String> {
        let mut v: Vec<String> = self.watchers.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

/// A page watching a task. Dropping it (the page went away) ends the watch.
pub struct Watch {
    o: Arc<Orchestrator>,
    task: String,
}

/// Starts watching `task`. The first watcher of a task tells the nodes to stream it.
pub fn watch(o: &Arc<Orchestrator>, task: &str) -> Watch {
    let first = {
        let mut w = o.live.watchers.lock().unwrap();
        let n = w.entry(task.to_owned()).or_default();
        *n += 1;
        *n == 1
    };
    if first {
        push_soon(o);
    }
    Watch {
        o: o.clone(),
        task: task.to_owned(),
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let last = {
            let mut w = self.o.live.watchers.lock().unwrap();
            match w.get_mut(&self.task) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    w.remove(&self.task);
                    true
                }
                None => false,
            }
        };
        if last {
            push_soon(&self.o);
        }
    }
}

/// Sends the watched set to every connected node, in the background: a page opening or closing
/// never waits for it. The set is read when it is sent, not when this was called, so two pushes
/// overtaking each other still leave each node with the set as it is.
fn push_soon(o: &Arc<Orchestrator>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let o = o.clone();
    rt.spawn(async move {
        if let Err(e) = push(&o).await {
            tracing::warn!("telling nodes which sessions are watched failed: {e:#}");
        }
    });
}

async fn push(o: &Orchestrator) -> Result<()> {
    let nodes: Vec<(String, Option<String>)> =
        sqlx::query_as("select id, app_version from nodes where online")
            .fetch_all(&o.db)
            .await?;
    let tasks = o.live.watched();
    for (id, version) in nodes {
        if let Ok(peer) = id.parse::<EndpointId>() {
            send(
                o,
                peer,
                version.as_deref().unwrap_or_default(),
                tasks.clone(),
            );
        }
    }
    Ok(())
}

/// A node that just connected is told what is watched now, even nothing: a set queued for it
/// while it was away is replayed first, and may name a page that has closed since.
pub fn on_connect(o: &Orchestrator, peer: EndpointId, app_version: &str) {
    send(o, peer, app_version, o.live.watched());
}

/// Only to a node level with this build: an older one cannot decode the message, and an
/// undecodable frame drops its link. Such a node is offered the update on connect; until then
/// its sessions show each block once it is finished, as they always did.
fn send(o: &Orchestrator, peer: EndpointId, app_version: &str, tasks: Vec<String>) {
    if fleet_proto::node_is_current(app_version) && o.server.is_connected(peer) {
        o.server.send(peer, Body::WatchTasks(tasks));
    }
}

/// A node's partial, passed to whichever pages watch its task. Nobody listening is not an error:
/// the page may have closed a moment ago.
pub fn partial(o: &Orchestrator, p: RunPartial) {
    let _ = o.live.partials.send((*o.events.borrow(), p));
}
