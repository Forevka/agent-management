//! Projects the owner assigned to this node, cloned when a task first needs them.
//!
//! The node clones with whatever git login this machine already has (a credential manager, an
//! ssh key): the fleet never holds git credentials. It runs git so that it can never stop to
//! ask anyone (no terminal prompt, no askpass program, no credential dialog, and ssh in batch
//! mode unless the machine chose its own ssh command), for at most [`CLONE_LIMIT`].
//!
//! A clone goes into a temporary directory of its own beside the target
//! (`<root>/.fleet-clone-<slug>-<random>`) and is renamed to `<root>/<slug>` only once git has
//! finished, so a node stopped, killed or rebooted mid-clone never leaves a half-made checkout
//! where a later task would take it for a ready one. What it removes is only ever such a
//! temporary directory, which it made; a directory already at the target is adopted when it is a
//! finished checkout of the same repository and refused otherwise, and never changed.
//!
//! While its project is cloned, a run waits in `Node::preparing`: it counts as running, keeps
//! the messages sent to it, and can be stopped, which stops git first.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use fleet_proto::{
    AgentEventKind, Body, ProjectSetupReport, SpawnAgent, StopAgent,
    node::{SETUP_CLONED, SETUP_CLONING, SETUP_FAILED, SETUP_STOPPED},
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::{Node, config::NodeConfig};

/// How long one clone may take before it is stopped. Large repositories over a slow link take
/// minutes; one that takes longer than this is hung on something nobody will answer.
pub const CLONE_LIMIT: Duration = Duration::from_secs(20 * 60);

/// A clone is not started on a volume with less free space than this: it would fill the disk
/// the agent's work (and maybe Claude Code's sessions) needs. The orchestrator measures only the
/// volumes a node already uses, so the node checks the projects root itself.
pub const MIN_FREE_TO_CLONE: u64 = 1 << 30;

/// How long a failed clone's reason answers the runs that waited for it, rather than each of
/// them trying again at once.
const FAILURE_REUSE: Duration = Duration::from_secs(30);

/// How long git's error output is still read once git has exited or been killed: a helper that
/// outlived it may hold the pipe open.
const STDERR_GRACE: Duration = Duration::from_secs(5);

/// The temporary directories clones are made in: `<root>/.fleet-clone-<slug>-<8 random>`.
const TEMP_PREFIX: &str = ".fleet-clone-";

/// `~/fleet-projects` (`%USERPROFILE%\fleet-projects` on Windows): where clones go when the
/// owner set no projects root for this node.
pub fn default_root() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("fleet-projects"))
}

/// A run whose project is being cloned before it can start.
pub(crate) struct Preparing {
    pub(crate) task_id: String,
    pub(crate) run_id: String,
    pub(crate) project: String,
    /// What its clone is made from: a clone of another URL running for the same project (the
    /// owner changed it meanwhile) is not one this run waits for.
    pub(crate) url: String,
    /// Messages for the run that arrived while it waited: delivered once it has started, and
    /// handed back to the orchestrator if it never does.
    pub(crate) texts: Vec<String>,
    /// Stopped while its own clone runs: it ends once git is gone.
    pub(crate) stopped: bool,
}

/// What the node keeps about clones.
#[derive(Default)]
pub(crate) struct Clones {
    /// One lock per project, so a project is cloned once however many runs wait for it.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The clone running now, per project: what it is made from, and how to stop it.
    running: Mutex<HashMap<String, Running>>,
    /// Clones under way. A self-update waits for none, checked under the agents lock, which is
    /// also held whenever this goes up.
    pub(crate) in_flight: AtomicUsize,
    /// The last failure per project, shared with the runs that waited for that attempt.
    failures: Mutex<HashMap<String, Failure>>,
}

impl Clones {
    /// Forgets recent failures, so the next run of each project clones again.
    pub(crate) fn forget_failures(&self) {
        self.failures.lock().unwrap().clear();
    }
}

struct Running {
    url: String,
    cancel: CancellationToken,
}

struct Failure {
    at: Instant,
    url: String,
    root: PathBuf,
    reason: String,
}

/// Why a project could not be made ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloneError {
    /// A sentence for the owner saying what to do.
    Failed(String),
    /// Every run that wanted it was stopped, and git with them.
    Stopped,
}

impl Node {
    /// Where to clone `project` from and into, when it is not mapped here but assigned to this
    /// node with a repository URL; `None` otherwise (the spawn then goes ahead, and fails as an
    /// unmapped project always has). The root is `None` when there is neither one set for this
    /// node nor a home directory to default to.
    pub(crate) fn clone_source(&self, project: &str) -> Option<(Option<PathBuf>, String)> {
        if self.config.lock().unwrap().project_dir(project).is_some() {
            return None;
        }
        let assignments = self.assignments.lock().unwrap();
        let a = assignments.as_ref()?;
        let url = a
            .projects
            .iter()
            .find(|p| p.slug == project)?
            .repo_url
            .clone()?;
        let root = match &a.root {
            Some(r) => Some(PathBuf::from(r)),
            None => default_root(),
        };
        Some((root, url))
    }

    /// Clones an assigned project (off the agents lock: it takes minutes), then starts the run
    /// as usual. Until then the run counts as running here, takes the messages sent to it, and
    /// can be stopped.
    pub(crate) fn prepare_then_spawn(
        self: &Arc<Self>,
        spawn: SpawnAgent,
        root: Option<PathBuf>,
        url: String,
    ) {
        // Under the agents lock, so a self-update's "nothing is running" can never fall between
        // this run being accepted and it being counted.
        let replaced = {
            let _agents = self.agents.lock().unwrap();
            self.preparing.lock().unwrap().insert(
                spawn.agent_id.clone(),
                Preparing {
                    task_id: spawn.task_id.clone(),
                    run_id: spawn.run_id.clone(),
                    project: spawn.project.clone(),
                    url: url.clone(),
                    texts: vec![],
                    stopped: false,
                },
            )
        };
        // The orchestrator only respawns an agent it believes is gone: a run of it still waiting
        // here never starts, and ends as a replaced process does.
        if let Some(old) = replaced.filter(|p| p.run_id != spawn.run_id) {
            self.end_waiting(&spawn.agent_id, old, AgentEventKind::Exited { code: None });
        }
        self.send_info();
        let this = self.clone();
        tokio::spawn(async move {
            let result = this
                .clone_project(&spawn.agent_id, &spawn.run_id, &spawn.project, root, &url)
                .await;
            this.finish_waiting(spawn, result);
        });
    }

    /// The clone is done (or failed, or was stopped): start the run if it still waits, or end
    /// it and hand back what it was sent.
    fn finish_waiting(self: &Arc<Self>, spawn: SpawnAgent, result: Result<(), CloneError>) {
        let agents = self.agents.lock().unwrap();
        let waiting = {
            let mut preparing = self.preparing.lock().unwrap();
            match preparing.get(&spawn.agent_id) {
                Some(p) if p.run_id == spawn.run_id => preparing.remove(&spawn.agent_id),
                // Stopped and already reported, or replaced by a newer run, while it waited.
                _ => None,
            }
        };
        let Some(p) = waiting else {
            return;
        };
        let agent_id = spawn.agent_id.clone();
        match result {
            Ok(()) if !p.stopped => self.start_locked(agents, spawn, p.texts),
            Err(CloneError::Failed(reason)) if !p.stopped => {
                drop(agents);
                self.end_waiting(&agent_id, p, AgentEventKind::SpawnFailed { reason });
                self.send_info();
            }
            _ => {
                drop(agents);
                self.end_waiting(&agent_id, p, AgentEventKind::Exited { code: None });
                self.send_info();
            }
        }
    }

    /// Reports a waiting run as ended (`kind`), then hands back each message it was sent, so the
    /// orchestrator delivers them to the task's next run rather than losing them.
    pub(crate) fn end_waiting(&self, agent_id: &str, p: Preparing, kind: AgentEventKind) {
        self.event(agent_id, &p.task_id, &p.run_id, kind);
        for text in p.texts {
            self.event(
                agent_id,
                &p.task_id,
                &p.run_id,
                AgentEventKind::Undelivered { text },
            );
        }
    }

    /// A stop for a run still waiting for its clone. If that clone is this run's alone, git is
    /// stopped first and the run ends once it is gone; otherwise the run ends at once and the
    /// clone carries on for the runs that still want it.
    pub(crate) fn stop_waiting(&self, stop: &StopAgent) {
        let agents = self.agents.lock().unwrap();
        let mut preparing = self.preparing.lock().unwrap();
        let Some((project, url)) = preparing
            .get(&stop.agent_id)
            .filter(|p| p.task_id == stop.task_id)
            .filter(|p| stop.run_id.is_empty() || p.run_id == stop.run_id)
            .map(|p| (p.project.clone(), p.url.clone()))
        else {
            return;
        };
        {
            let running = self.clones.running.lock().unwrap();
            if let Some(r) = running.get(&project).filter(|r| r.url == url) {
                let shared = preparing.iter().any(|(a, q)| {
                    a != &stop.agent_id && !q.stopped && q.project == project && q.url == url
                });
                if !shared {
                    r.cancel.cancel();
                    if let Some(p) = preparing.get_mut(&stop.agent_id) {
                        p.stopped = true;
                    }
                    return;
                }
            }
        }
        let Some(p) = preparing.remove(&stop.agent_id) else {
            return;
        };
        drop(preparing);
        drop(agents);
        self.end_waiting(&stop.agent_id, p, AgentEventKind::Exited { code: None });
        self.send_info();
    }

    /// Stops every clone and marks every waiting run stopped; each ends once its git is gone.
    /// Called with the agents lock held (`stop_all`).
    pub(crate) fn stop_all_waiting(&self) {
        for p in self.preparing.lock().unwrap().values_mut() {
            p.stopped = true;
        }
        for r in self.clones.running.lock().unwrap().values() {
            r.cancel.cancel();
        }
    }

    /// Clones `project` into `<root>/<project>` (or adopts a checkout of it already there) and
    /// maps it, once: a second run for the same project waits for the first clone and then
    /// finds it mapped, or shares its failure.
    async fn clone_project(
        &self,
        agent_id: &str,
        run_id: &str,
        project: &str,
        root: Option<PathBuf>,
        url: &str,
    ) -> Result<(), CloneError> {
        let lock = self
            .clones
            .locks
            .lock()
            .unwrap()
            .entry(project.to_owned())
            .or_default()
            .clone();
        let _held = lock.lock().await;
        if self.config.lock().unwrap().project_dir(project).is_some() {
            return Ok(());
        }
        let Some(root) = root else {
            let why = "this machine has no home directory for the default projects root \
                       (~/fleet-projects); set a projects root on the node's page"
                .to_owned();
            self.report_setup(project, SETUP_FAILED, &why);
            return Err(CloneError::Failed(why));
        };
        if let Some(why) = self.recent_failure(project, url, &root) {
            self.report_setup(project, SETUP_FAILED, &why);
            return Err(CloneError::Failed(why));
        }
        // Counted (and made stoppable) under the agents lock, and only while the run that asked
        // still waits: a run stopped while this one queued for the lock needs no clone.
        let cancel = CancellationToken::new();
        {
            let _agents = self.agents.lock().unwrap();
            let waiting = self
                .preparing
                .lock()
                .unwrap()
                .get(agent_id)
                .is_some_and(|p| p.run_id == run_id && !p.stopped);
            if !waiting {
                return Err(CloneError::Stopped);
            }
            self.clones.in_flight.fetch_add(1, Ordering::SeqCst);
            self.clones.running.lock().unwrap().insert(
                project.to_owned(),
                Running {
                    url: url.to_owned(),
                    cancel: cancel.clone(),
                },
            );
        }
        let _in_flight = InFlight {
            clones: &self.clones,
            project,
        };
        self.report_setup(
            project,
            SETUP_CLONING,
            &root.join(project).display().to_string(),
        );
        tracing::info!("cloning {project} from {url} into {}", root.display());
        let result = match ensure(&root, project, url, &cancel).await {
            Ok(dir) => self
                .map_clone(project, &dir)
                .await
                .map_err(CloneError::Failed),
            Err(e) => Err(e),
        };
        match &result {
            Ok(path) => {
                // NodeInfo first, so the orchestrator has the mapping when it reads "cloned".
                self.send_info();
                self.report_setup(project, SETUP_CLONED, path);
            }
            Err(CloneError::Failed(why)) => {
                tracing::warn!("cloning {project} failed: {why}");
                self.clones.failures.lock().unwrap().insert(
                    project.to_owned(),
                    Failure {
                        at: Instant::now(),
                        url: url.to_owned(),
                        root: root.clone(),
                        reason: why.clone(),
                    },
                );
                self.report_setup(project, SETUP_FAILED, why);
            }
            Err(CloneError::Stopped) => {
                tracing::info!("cloning {project} was stopped");
                self.report_setup(project, SETUP_STOPPED, "");
            }
        }
        result.map(|_| ())
    }

    /// The reason the same clone failed a moment ago, if it did.
    fn recent_failure(&self, project: &str, url: &str, root: &Path) -> Option<String> {
        let mut failures = self.clones.failures.lock().unwrap();
        failures.retain(|_, f| f.at.elapsed() < FAILURE_REUSE);
        failures
            .get(project)
            .filter(|f| f.url == url && f.root == root)
            .map(|f| f.reason.clone())
    }

    /// Maps a finished clone, and returns the directory as mapped.
    async fn map_clone(&self, project: &str, dir: &Path) -> Result<String, String> {
        // Under `node.json.lock`, like every writer of the file (the tray app maps projects
        // too); off the runtime, since that lock may wait on another process.
        let data_dir = self.data_dir.clone();
        let (slug, at) = (project.to_owned(), dir.to_path_buf());
        let (saved, ()) = tokio::task::spawn_blocking(move || {
            NodeConfig::update(&data_dir, |c| c.map_project(&slug, &at))
        })
        .await
        .map_err(|e| format!("mapping {project} failed: {e}"))?
        .map_err(|e| {
            format!(
                "{project} was cloned into {}, but mapping it failed: {e:#}",
                dir.display()
            )
        })?;
        let path = {
            let mut cfg = self.config.lock().unwrap();
            cfg.projects = saved.projects;
            cfg.project_dir(project)
        };
        tracing::info!("{project} is ready in {}", dir.display());
        Ok(path
            .unwrap_or_else(|| dir.to_path_buf())
            .display()
            .to_string())
    }

    /// Tells the orchestrator how setting up an assigned project goes, if it can read that: it
    /// sent assignments on this connection, and `project` is among them.
    fn report_setup(&self, project: &str, state: &str, detail: &str) {
        let assigned = self.assignments_heard.load(Ordering::Acquire)
            && self
                .assignments
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|a| a.projects.iter().any(|p| p.slug == project));
        if assigned {
            self.client
                .send(Body::ProjectSetupReport(ProjectSetupReport {
                    project: project.to_owned(),
                    state: state.to_owned(),
                    detail: detail.to_owned(),
                }));
        }
    }
}

/// A clone under way, for as long as this lives.
struct InFlight<'a> {
    clones: &'a Clones,
    project: &'a str,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.clones.running.lock().unwrap().remove(self.project);
        self.clones.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What to do with the directory an assigned project would be cloned into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Nothing is there: clone into it.
    Clone,
    /// A checkout of the same repository is already there: map it as it is.
    Adopt,
    /// Something else is there, which is never touched. Why, for the owner.
    Refuse(String),
}

/// Decides from what is at `dir`: whether it exists, and, when it is a git checkout, its
/// `origin`. Kept free of the file system so every case is tested.
pub fn decide(dir: &Path, exists: bool, origin: Option<&str>, url: &str) -> Target {
    match (exists, origin) {
        (false, _) => Target::Clone,
        (true, Some(o)) if same_repo(o, url) => Target::Adopt,
        // The other origin is not quoted: a URL someone set up by hand may carry a token.
        (true, Some(_)) => Target::Refuse(format!(
            "{} is already a git checkout of another repository, not {url}. Nothing was changed: \
             map the project by hand, or set another projects root for this node.",
            dir.display()
        )),
        (true, None) => Target::Refuse(format!(
            "{} already exists and is not a git checkout of {url}. Nothing was changed: move it \
             away, map the project by hand, or set another projects root for this node.",
            dir.display()
        )),
    }
}

/// Whether two remote URLs name the same repository: equal once a trailing `/` or `.git` is
/// dropped, slashes are one kind and case is ignored (hosts are case-insensitive, and so are the
/// paths of the hosting services and of Windows).
pub fn same_repo(a: &str, b: &str) -> bool {
    fn norm(u: &str) -> String {
        let u = u.trim().replace('\\', "/");
        let u = u.trim_end_matches('/');
        let u = u.strip_suffix(".git").unwrap_or(u).trim_end_matches('/');
        u.to_ascii_lowercase()
    }
    norm(a) == norm(b)
}

/// Makes `<root>/<slug>` a checkout of `url`, cloning it if nothing is there, and returns it.
/// `cancel` stops git (and everything it started) and removes what it made.
pub(crate) async fn ensure(
    root: &Path,
    slug: &str,
    url: &str,
    cancel: &CancellationToken,
) -> Result<PathBuf, CloneError> {
    let failed = CloneError::Failed;
    // Checked again here, whatever the orchestrator checked: a URL with a token in it never
    // reaches git, and nor does anything git would read as an option or a command.
    fleet_proto::repo::check_repo_url(url).map_err(failed)?;
    crate::config::validate_slug(slug).map_err(|e| failed(format!("{e:#}")))?;
    if !root.is_absolute() {
        return Err(failed(format!(
            "the projects root {} is not an absolute path on this machine; set one in the node's page",
            root.display()
        )));
    }
    let url = url.trim();
    std::fs::create_dir_all(root).map_err(|e| {
        failed(format!(
            "cannot create the projects root {}: {e}",
            root.display()
        ))
    })?;
    let machine_ssh = machine_chose_ssh().await;
    let dir = root.join(slug);
    let exists = std::fs::symlink_metadata(&dir).is_ok();
    let origin = if exists && dir.join(".git").exists() {
        git_origin(&dir, machine_ssh).await
    } else {
        None
    };
    match decide(&dir, exists, origin.as_deref(), url) {
        // A checkout made by hand and cut short already names its origin but has no commit
        // checked out: not something to start work in.
        Target::Adopt if !has_head(&dir, machine_ssh).await => Err(failed(format!(
            "{} looks like an unfinished clone of {url} (no commit is checked out). Nothing was \
             changed: delete it and the next task clones it again.",
            dir.display()
        ))),
        Target::Adopt => Ok(dir),
        Target::Refuse(why) => Err(failed(why)),
        Target::Clone => {
            if let Ok(free) = crate::disk::free_at(root)
                && free < MIN_FREE_TO_CLONE
            {
                return Err(failed(format!(
                    "only {} MB is free where {} is; free some space or set another projects \
                     root for this node, and the next task will clone it.",
                    free >> 20,
                    root.display()
                )));
            }
            remove_stale_temps(root, slug);
            let tmp = root.join(format!("{TEMP_PREFIX}{slug}-{}", temp_suffix()));
            clone(url, &tmp, machine_ssh, cancel).await?;
            if let Err(why) = rename_into_place(&tmp, &dir).await {
                remove_ours(&tmp);
                return Err(failed(why));
            }
            Ok(dir)
        }
    }
}

/// Eight random lowercase letters and digits.
fn temp_suffix() -> String {
    crate::runner::random_token()
        .to_ascii_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect()
}

/// Whether `name` is one of this slug's temporary clone directories: exactly
/// `.fleet-clone-<slug>-` and eight random characters, so another slug's (`<slug>-x`) never
/// matches.
fn is_our_temp(name: &str, slug: &str) -> bool {
    name.strip_prefix(TEMP_PREFIX)
        .and_then(|r| r.strip_prefix(slug))
        .and_then(|r| r.strip_prefix('-'))
        .is_some_and(|r| {
            r.len() == 8
                && r.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Removes what an earlier clone of this slug left behind when the node was stopped mid-way.
/// Only directories named as this node names its own temporary clones; called under the
/// project's clone lock, so never one in use.
fn remove_stale_temps(root: &Path, slug: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_str().is_some_and(|n| is_our_temp(n, slug)) {
            tracing::info!("removing an unfinished clone, {}", e.path().display());
            remove_ours(&e.path());
        }
    }
}

/// Moves a finished clone to its place. A directory that appeared there meanwhile is left
/// alone; on Windows a scanner may hold a fresh file briefly, so a refused rename is retried.
async fn rename_into_place(tmp: &Path, dir: &Path) -> Result<(), String> {
    let mut tries = 0;
    loop {
        if std::fs::symlink_metadata(dir).is_ok() {
            return Err(format!(
                "{} appeared while the project was being cloned; nothing there was changed. \
                 Map it by hand, or remove it and the next task clones it again.",
                dir.display()
            ));
        }
        match std::fs::rename(tmp, dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < 20 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => {
                return Err(format!(
                    "the clone could not be moved to {}: {e}",
                    dir.display()
                ));
            }
        }
    }
}

/// `git remote get-url origin` of a checkout, if it has one.
async fn git_origin(dir: &Path, machine_ssh: bool) -> Option<String> {
    let mut cmd = git(machine_ssh);
    cmd.arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|o| !o.is_empty())
}

/// Whether a checkout has a commit checked out.
async fn has_head(dir: &Path, machine_ssh: bool) -> bool {
    let mut cmd = git(machine_ssh);
    cmd.arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        tokio::time::timeout(Duration::from_secs(30), cmd.status()).await,
        Ok(Ok(s)) if s.success()
    )
}

/// Whether this machine chose how git runs ssh (`GIT_SSH_COMMAND`, `GIT_SSH`, or
/// `core.sshCommand` in its git config): then that choice is left exactly as it is, since it is
/// often what makes ssh work at all (a Windows OpenSSH with its agent, a password manager's).
async fn machine_chose_ssh() -> bool {
    let set = |v: &str| std::env::var_os(v).is_some_and(|v| !v.is_empty());
    if set("GIT_SSH_COMMAND") || set("GIT_SSH") {
        return true;
    }
    let mut cmd = git(true);
    cmd.args(["config", "--get", "core.sshCommand"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Ok(Ok(out)) => out.status.success() && !out.stdout.trim_ascii().is_empty(),
        _ => false,
    }
}

async fn clone(
    url: &str,
    tmp: &Path,
    machine_ssh: bool,
    cancel: &CancellationToken,
) -> Result<(), CloneError> {
    let by_hand = format!(
        "Log in to git on this machine (run `git clone {url}` once by hand) and the next task will clone it."
    );
    let mut cmd = git(machine_ssh);
    cmd.arg("clone")
        .arg("--")
        .arg(url)
        .arg(tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Its own session, and so its own process group with no controlling terminal: a stop or the
    // time limit ends every process git started (the transport helpers, ssh), not only git, and
    // an ssh the machine set up to ask on a terminal fails at once instead of being stopped
    // in the background waiting for one.
    #[cfg(unix)]
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the parent, which is all a
    // `pre_exec` hook may do between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| {
        CloneError::Failed(format!(
            "git could not be started on this machine ({e}): install git, make sure the node finds it \
             on its PATH, and log in once; the next task will clone the project."
        ))
    })?;
    // On Windows a job object: `git.exe` on the PATH is often a launcher for the real one.
    let tree = crate::runner::ProcessTree::adopt(&child);
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let mut read = tokio::spawn(async move {
        // The end is where the reason is; a chatty transport must not grow this without bound.
        let mut tail = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 16 << 10 {
                tail.drain(..tail.len() - (8 << 10));
            }
        }
        tail
    });
    enum End {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Stopped,
    }
    let end = tokio::select! {
        status = child.wait() => End::Exited(status),
        _ = tokio::time::sleep(CLONE_LIMIT) => End::TimedOut,
        _ = cancel.cancelled() => End::Stopped,
    };
    let succeeded = matches!(&end, End::Exited(Ok(s)) if s.success());
    if !succeeded {
        tree.kill(&mut child).await;
        let _ = child.wait().await;
    }
    let stderr = match tokio::time::timeout(STDERR_GRACE, &mut read).await {
        Ok(Ok(tail)) => String::from_utf8_lossy(&tail).into_owned(),
        _ => {
            read.abort();
            String::new()
        }
    };
    // On Windows, closing the job ends whatever is still in it.
    drop(tree);
    if succeeded {
        return Ok(());
    }
    // Everything that could write there is gone; the directory is this clone's own.
    remove_ours(tmp);
    Err(match end {
        End::Stopped => CloneError::Stopped,
        End::TimedOut => CloneError::Failed(format!(
            "cloning {url} took over {} minutes and was stopped. {by_hand}",
            CLONE_LIMIT.as_secs() / 60
        )),
        End::Exited(Err(e)) => CloneError::Failed(format!("waiting for git failed: {e}")),
        End::Exited(Ok(_)) => CloneError::Failed(format!(
            "git could not clone {url} on this machine: {}. {by_hand}",
            last_lines(&stderr)
        )),
    })
}

/// Git that never waits for a person: no terminal prompt, no askpass program (an empty
/// `GIT_ASKPASS` also turns off `core.askPass` and `SSH_ASKPASS`), no credential manager dialog,
/// ssh never asking for anything, and ssh in batch mode unless the machine chose its own ssh
/// command (`machine_ssh`), which is kept. No console window of its own on Windows.
fn git(machine_ssh: bool) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GCM_INTERACTIVE", "never")
        .kill_on_drop(true);
    if !machine_ssh {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

/// Removes a directory this node made (a temporary clone).
fn remove_ours(dir: &Path) {
    if std::fs::symlink_metadata(dir).is_ok()
        && let Err(e) = std::fs::remove_dir_all(dir)
    {
        tracing::warn!(
            "could not remove the unfinished clone {}: {e}",
            dir.display()
        );
    }
}

/// The end of git's error output, where the reason is: its last few lines, at most ~600 chars.
fn last_lines(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(4)..].join(" / ");
    if tail.is_empty() {
        return "git gave no reason".into();
    }
    match tail.char_indices().rev().nth(599) {
        Some((i, _)) => format!("…{}", &tail[i..]),
        None => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_directory_is_cloned_into_adopted_or_left_alone() {
        let dir = Path::new("/w/demo");
        let url = "https://github.com/o/demo.git";
        assert_eq!(decide(dir, false, None, url), Target::Clone);
        assert_eq!(
            decide(dir, true, Some("https://GitHub.com/o/demo/"), url),
            Target::Adopt
        );
        let Target::Refuse(why) = decide(
            dir,
            true,
            Some("https://user:tok3n@github.com/o/other.git"),
            url,
        ) else {
            panic!("another repository is refused");
        };
        assert!(why.contains("another repository"), "{why}");
        assert!(
            !why.contains("tok3n"),
            "another origin is never quoted: {why}"
        );
        let Target::Refuse(why) = decide(dir, true, None, url) else {
            panic!("a directory that is no checkout is refused");
        };
        assert!(why.contains("not a git checkout"), "{why}");
    }

    #[test]
    fn remotes_compare_without_their_decoration() {
        assert!(same_repo("git@github.com:o/r.git", "git@github.com:o/r"));
        assert!(same_repo(r"C:\tmp\bare.git", "c:/tmp/bare.git/"));
        assert!(!same_repo("https://h/o/r", "https://h/o/r2"));
    }

    #[test]
    fn git_s_reason_is_its_last_lines() {
        let err = "Cloning into 'x'...\n\nremote: nope\nfatal: Authentication failed\n";
        assert_eq!(
            last_lines(err),
            "Cloning into 'x'... / remote: nope / fatal: Authentication failed"
        );
        assert_eq!(last_lines(""), "git gave no reason");
        assert!(last_lines(&"x".repeat(5000)).chars().count() <= 601);
    }

    #[test]
    fn only_this_slugs_own_temporary_clones_are_ever_removed() {
        assert!(is_our_temp(".fleet-clone-demo-ab12cd34", "demo"));
        for not in [
            ".fleet-clone-demo-x-ab12cd34",
            ".fleet-clone-demo-ab12cd3",
            ".fleet-clone-demo-AB12CD34",
            ".fleet-clone-demo",
            "demo",
            "fleet-clone-demo-ab12cd34",
        ] {
            assert!(!is_our_temp(not, "demo"), "{not}");
        }
        // Another slug that starts the same never matches.
        assert!(!is_our_temp(".fleet-clone-demo-x-ab12cd34", "demo"));
        assert!(is_our_temp(".fleet-clone-demo-x-ab12cd34", "demo-x"));
        assert_eq!(temp_suffix().len(), 8);
        assert!(is_our_temp(
            &format!(".fleet-clone-a-{}", temp_suffix()),
            "a"
        ));
    }

    fn run_git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn commit(dir: &Path) -> bool {
        run_git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        )
    }

    fn failed(r: Result<PathBuf, CloneError>) -> String {
        match r {
            Err(CloneError::Failed(why)) => why,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_existing_directory_is_adopted_or_refused_and_never_changed() {
        let root = std::env::temp_dir().join(format!("fleet-checkout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let url = "https://example.invalid/o/demo.git";
        let go = CancellationToken::new();

        // A checkout of the same repository is mapped as it is.
        let same = root.join("same");
        std::fs::create_dir_all(&same).unwrap();
        assert!(run_git(&same, &["init", "-q"]), "git is needed");
        assert!(run_git(&same, &["remote", "add", "origin", url]));
        // Until a commit is checked out it is an unfinished clone, and left alone.
        let e = failed(ensure(&root, "same", url, &go).await);
        assert!(e.contains("unfinished clone"), "{e}");
        assert!(commit(&same));
        assert_eq!(ensure(&root, "same", url, &go).await, Ok(same.clone()));

        // A checkout of something else, and a plain directory: refused, untouched.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert!(run_git(&other, &["init", "-q"]));
        assert!(run_git(
            &other,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/o/other.git"
            ]
        ));
        let e = failed(ensure(&root, "other", url, &go).await);
        assert!(e.contains("another repository"), "{e}");
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("keep.txt"), "mine").unwrap();
        let e = failed(ensure(&root, "plain", url, &go).await);
        assert!(e.contains("not a git checkout"), "{e}");
        assert_eq!(
            std::fs::read_to_string(plain.join("keep.txt")).unwrap(),
            "mine"
        );

        // A clone that fails says what to do and leaves nothing behind, not even its temporary
        // directory; one left by an earlier, interrupted clone of the slug is cleared first, and
        // another slug's is not touched.
        let stale = root.join(".fleet-clone-absent-ab12cd34");
        let foreign = root.join(".fleet-clone-absent-x-ab12cd34");
        std::fs::create_dir_all(stale.join(".git")).unwrap();
        std::fs::create_dir_all(&foreign).unwrap();
        let missing = root.join("no-such-origin.git").display().to_string();
        let e = failed(ensure(&root, "absent", &missing, &go).await);
        assert!(e.contains("git could not clone"), "{e}");
        assert!(e.contains("Log in to git on this machine"), "{e}");
        assert!(!root.join("absent").exists());
        assert!(
            !stale.exists(),
            "an interrupted clone's leftovers are removed"
        );
        assert!(foreign.exists(), "another slug's are not");
        let left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| is_our_temp(n, "absent"))
            .collect();
        assert!(left.is_empty(), "{left:?}");

        // A real clone lands in place, from its temporary directory.
        let bare = root.join("origin.git");
        assert!(run_git(&root, &["init", "-q", "--bare", "origin.git"]));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        assert!(run_git(&work, &["init", "-q"]));
        assert!(commit(&work));
        assert!(run_git(
            &work,
            &[
                "push",
                "-q",
                &bare.display().to_string(),
                "HEAD:refs/heads/main"
            ]
        ));
        assert!(run_git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]));
        let cloned = ensure(&root, "fresh", &bare.display().to_string(), &go)
            .await
            .unwrap();
        assert_eq!(cloned, root.join("fresh"));
        assert!(has_head(&cloned, true).await);

        // A stopped clone is no clone: nothing is left, and it says it was stopped.
        let stopped = CancellationToken::new();
        stopped.cancel();
        assert_eq!(
            ensure(&root, "halted", &bare.display().to_string(), &stopped).await,
            Err(CloneError::Stopped)
        );
        assert!(!root.join("halted").exists());

        // Refused before git ever runs.
        let e = failed(
            ensure(
                &root,
                "absent",
                "https://u:tok@example.invalid/o/r.git",
                &go,
            )
            .await,
        );
        assert!(e.contains("credentials"), "{e}");
        assert!(
            ensure(Path::new("relative"), "absent", url, &go)
                .await
                .is_err()
        );
        assert!(ensure(&root, "../escape", url, &go).await.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
