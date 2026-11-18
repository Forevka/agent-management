//! Ends the tray when the node that launched it stops.
//!
//! The watched pid can be swapped: when a restarted node launches the tray while the old
//! instance is still running, the single-instance hook hands the new pid to the running tray
//! instead of letting it exit with the old node.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use tauri::{AppHandle, Manager};

use crate::{
    DataDir,
    process::{self, Liveness},
    state,
};

const POLL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Watch {
    /// 0 means "not launched by a node, never exit on our own".
    pid: u32,
    /// Opened once per pid, so a pid reused after the node exits cannot pass for the node.
    #[cfg(windows)]
    handle: Option<process::Process>,
    /// Elsewhere there is no handle to hold, so the pid's start stamp is taken when it is set
    /// and must still match: a different stamp means the node is gone and the pid reused.
    #[cfg(not(windows))]
    started: Option<u64>,
    /// Set once an inconclusive check has been logged, so the log does not fill every 2 s.
    logged_unknown: bool,
}

impl Watch {
    fn check(&mut self) -> Liveness {
        if self.pid == 0 {
            return Liveness::Running;
        }
        #[cfg(windows)]
        {
            if self.handle.is_none() {
                match process::Process::open(self.pid) {
                    Ok(p) => self.handle = Some(p),
                    Err(l) => return l,
                }
            }
            match &self.handle {
                Some(p) if p.has_exited() => Liveness::Gone,
                _ => Liveness::Running,
            }
        }
        #[cfg(not(windows))]
        {
            match process::liveness(self.pid) {
                Liveness::Running
                    if fleet_node_config::process::is_same(self.pid, self.started)
                        == Some(false) =>
                {
                    Liveness::Gone
                }
                other => other,
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct NodePid(Arc<Mutex<Watch>>);

impl NodePid {
    pub fn set(&self, pid: u32) {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if w.pid == pid {
            return;
        }
        *w = Watch {
            pid,
            ..Watch::default()
        };
        #[cfg(windows)]
        {
            w.handle = process::Process::open(pid).ok();
        }
        #[cfg(not(windows))]
        {
            w.started = fleet_node_config::process::start_time(pid);
        }
    }

    pub fn is_set(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).pid != 0
    }

    fn pid(&self) -> u32 {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).pid
    }
}

pub fn spawn(app: AppHandle, node: NodePid) {
    std::thread::spawn(move || {
        loop {
            let data_dir = app.state::<DataDir>().get();
            let result = node.0.lock().unwrap_or_else(|e| e.into_inner()).check();
            match result {
                // A restarted node writes its pid to status.json before it launches the tray,
                // and may have done so before the old pid ended: follow it instead of leaving.
                Liveness::Gone if follow_status_pid(&node, &data_dir) => {}
                Liveness::Gone => {
                    app.exit(0);
                    return;
                }
                Liveness::Unknown => log_unknown_once(&node, &data_dir),
                Liveness::Running => {}
            }
            std::thread::sleep(POLL);
        }
    });
}

fn log_unknown_once(node: &NodePid, data_dir: &Path) {
    let mut w = node.0.lock().unwrap_or_else(|e| e.into_inner());
    if !w.logged_unknown {
        w.logged_unknown = true;
        let msg = format!(
            "cannot check whether node pid {} is running (access denied?); the tray stays up",
            w.pid
        );
        let _ = state::log(data_dir, &msg);
    }
}

/// Switches to the node named in `status.json` when that is a different, running process: a
/// newer node has taken over. Returns whether it switched. Only consulted once the watched node
/// is gone, so a live node is never swapped for another.
///
/// When the status carries the node's start stamp, the pid must still be that process; a pid
/// reused by something else after that node stopped is not followed. Without a stamp (an older
/// node, or a platform without one) the pid's liveness is all there is to go on.
fn follow_status_pid(node: &NodePid, data_dir: &Path) -> bool {
    let watched = node.pid();
    if watched == 0 {
        return false;
    }
    let Some(status) = state::read_status(data_dir) else {
        return false;
    };
    let verified = status.pid_started.is_none() || status.same_process() == Some(true);
    match status.pid {
        Some(pid) if pid != watched && verified && process::liveness(pid) == Liveness::Running => {
            node.set(pid);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_never_exits() {
        assert_eq!(Watch::default().check(), Liveness::Running);
    }

    #[test]
    fn watches_the_pid_it_was_given() {
        let node = NodePid::default();
        node.set(std::process::id());
        assert!(node.is_set());
        assert_eq!(node.0.lock().unwrap().check(), Liveness::Running);

        node.set(process::tests::dead_pid());
        assert_eq!(node.0.lock().unwrap().check(), Liveness::Gone);
    }

    #[test]
    fn follows_a_restarted_node_through_status_json() {
        let dir = std::env::temp_dir().join(format!("fleet-tray-follow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write_pid = |pid: u32| {
            std::fs::write(
                state::Status::path(&dir),
                format!(r#"{{"connected":false,"pid":{pid}}}"#),
            )
            .unwrap()
        };
        let node = NodePid::default();
        let old = process::tests::dead_pid();
        node.set(old);

        // The status still names the old node: nothing to follow, the tray would exit.
        write_pid(old);
        assert!(!follow_status_pid(&node, &dir));
        // A dead pid in the status is no reason to stay either.
        write_pid(process::tests::dead_pid());
        assert!(!follow_status_pid(&node, &dir));
        // A running pid whose start stamp is not the one the node wrote: the pid was reused by
        // another process after that node stopped. Not followed.
        let me = std::process::id();
        let started = fleet_node_config::process::start_time(me).unwrap();
        let write_stamped = |started: u64| {
            std::fs::write(
                state::Status::path(&dir),
                format!(r#"{{"connected":true,"pid":{me},"pid_started":{started}}}"#),
            )
            .unwrap()
        };
        write_stamped(started + 1);
        assert!(!follow_status_pid(&node, &dir));
        // The node that really wrote it: watch it instead.
        write_stamped(started);
        assert!(follow_status_pid(&node, &dir));
        assert_eq!(node.pid(), me);
        assert_eq!(node.0.lock().unwrap().check(), Liveness::Running);
        // A live node is never swapped: only a gone one makes the watcher look at the file.
        write_pid(process::tests::dead_pid());
        assert_eq!(node.0.lock().unwrap().check(), Liveness::Running);

        // Without a stamp (an older node), liveness alone decides.
        let unstamped = NodePid::default();
        unstamped.set(old);
        write_pid(me);
        assert!(follow_status_pid(&unstamped, &dir));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
