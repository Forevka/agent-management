//! Projects the owner assigned to nodes, which a node clones when a task first needs one there.
//!
//! A clean machine joined as a node needs no project checked out by hand: the owner records a
//! project's git URL and assigns the project to the node in the web UI. A task for it may then be
//! placed on that node (after the nodes that already have it mapped), and the node clones it
//! with the machine's own git login, maps it and starts the run (`fleet_node::checkout`).
//! Nothing assigned means nothing is ever cloned, and unassigning never deletes anything.
//!
//! Only the owner changes any of it (`admin::set_repo_url`, `set_projects_root`,
//! `assign_project`): which code lands on which machine is not something the brain may propose.
//! Nodes hear their assignments in [`Body::ProjectAssignments`], which goes only to a node that
//! [`node_is_current`] — so only such a node is offered work it would have to clone.

use anyhow::Result;
use fleet_net::EndpointId;
use fleet_proto::{
    AssignedProject, Body, ProjectAssignments, ProjectSetupReport, node::SETUP_CLONED,
    node::SETUP_CLONING, node::SETUP_FAILED, node_is_current,
};
use serde_json::{Value, json};

use crate::{core::Orchestrator, model::NodeRow};

/// One assignment, with what the owner needs to see of it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Assignment {
    pub node_id: String,
    pub project: String,
    pub name: String,
    pub repo_url: Option<String>,
    pub assigned_at: chrono::DateTime<chrono::Utc>,
    /// The node's last report: cloning, cloned or failed (`None`: nothing reported yet).
    pub state: Option<String>,
    pub detail: Option<String>,
}

/// Every assignment, or one node's.
pub async fn list(o: &Orchestrator, node_id: Option<&str>) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as(
        "select np.node_id, np.project, p.name, p.repo_url, np.assigned_at, np.state, np.detail
         from node_projects np join projects p on p.slug = np.project
         where $1::text is null or np.node_id = $1
         order by np.node_id, np.project",
    )
    .bind(node_id)
    .fetch_all(&o.db)
    .await?)
}

/// Node id -> the projects assigned to it that it could clone (they have a repository URL).
/// What `tasks::node_load` gives placement.
pub async fn clonable(o: &Orchestrator) -> Result<std::collections::HashMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select np.node_id, np.project from node_projects np join projects p on p.slug = np.project
         where p.repo_url is not null",
    )
    .fetch_all(&o.db)
    .await?;
    let mut out = std::collections::HashMap::<String, Vec<String>>::new();
    for (node, project) in rows {
        out.entry(node).or_default().push(project);
    }
    Ok(out)
}

/// What a node is told: its projects root and every project assigned to it.
pub async fn assignments(o: &Orchestrator, node_id: &str) -> Result<ProjectAssignments> {
    let root: Option<String> = sqlx::query_scalar("select projects_root from nodes where id = $1")
        .bind(node_id)
        .fetch_optional(&o.db)
        .await?
        .flatten();
    Ok(ProjectAssignments {
        root,
        projects: list(o, Some(node_id))
            .await?
            .into_iter()
            .map(|a| AssignedProject {
                slug: a.project,
                name: a.name,
                repo_url: a.repo_url,
            })
            .collect(),
    })
}

/// Sends a node its assignments now, if it is connected and reads them. One that is not
/// connected gets them on its next connect ([`on_connect`]).
pub async fn push(o: &Orchestrator, node_id: &str) -> Result<()> {
    let Some(n) = o.node(node_id).await? else {
        return Ok(());
    };
    let Ok(peer) = n.id.parse::<EndpointId>() else {
        return Ok(());
    };
    if !node_is_current(&n.app_version) || !o.server.is_connected(peer) {
        return Ok(());
    }
    o.server.send(
        peer,
        Body::ProjectAssignments(assignments(o, node_id).await?),
    );
    Ok(())
}

/// [`push`] to every node `project` is assigned to (its repository URL changed).
pub async fn push_project(o: &Orchestrator, project: &str) -> Result<()> {
    let nodes: Vec<String> =
        sqlx::query_scalar("select node_id from node_projects where project = $1")
            .bind(project)
            .fetch_all(&o.db)
            .await?;
    for n in nodes {
        push(o, &n).await?;
    }
    Ok(())
}

/// On every connect of a node that reads them (checked against the version in its hello, which
/// is the one it runs now).
pub async fn on_connect(o: &Orchestrator, peer: EndpointId, app_version: &str) -> Result<()> {
    if node_is_current(app_version) {
        o.server.send(
            peer,
            Body::ProjectAssignments(assignments(o, &peer.to_string()).await?),
        );
    }
    Ok(())
}

/// Stores a node's report about one of its assignments. A report for a project that is not
/// assigned to it (unassigned meanwhile), or in a state this build does not know, is dropped.
pub async fn reported(o: &Orchestrator, peer: EndpointId, r: ProjectSetupReport) -> Result<()> {
    if ![SETUP_CLONING, SETUP_CLONED, SETUP_FAILED].contains(&r.state.as_str()) {
        tracing::warn!(node = %peer.fmt_short(), "unknown project setup state {:?}", r.state);
        return Ok(());
    }
    let detail: String = r.detail.chars().take(2000).collect();
    if r.state == SETUP_FAILED {
        tracing::warn!(node = %peer.fmt_short(), "setting up {} failed: {detail}", r.project);
    }
    sqlx::query(
        "update node_projects set state = $3, detail = $4 where node_id = $1 and project = $2",
    )
    .bind(peer.to_string())
    .bind(&r.project)
    .bind(&r.state)
    .bind(&detail)
    .execute(&o.db)
    .await?;
    Ok(())
}

/// How an assignment stands on its node, for the owner (web UI) and the brain (`admin_read`):
/// `mapped` (with its path), `cloning`, `cloned`, `failed` (with the reason), `waiting` (clones
/// when a task needs it), `no_url` (cannot be cloned until the project has a repository URL) or
/// `node_behind` (the node must update before it clones anything).
pub fn assignment_json(a: &Assignment, n: &NodeRow) -> Value {
    let mapped = n.projects.0.iter().find(|m| m.project == a.project);
    let status = match (mapped, a.state.as_deref()) {
        (Some(_), _) => "mapped",
        (None, Some(s @ (SETUP_CLONING | SETUP_FAILED))) => s,
        _ if a.repo_url.is_none() => "no_url",
        _ if !node_is_current(&n.app_version) => "node_behind",
        _ => "waiting",
    };
    json!({
        "project": a.project,
        "name": a.name,
        "repo_url": a.repo_url,
        "status": status,
        "path": mapped.map(|m| m.path.clone()),
        "state": a.state,
        "detail": a.detail,
        "assigned_at": a.assigned_at,
    })
}

/// A node's assignments as [`assignment_json`] shows them.
pub async fn node_json(o: &Orchestrator, n: &NodeRow) -> Result<Vec<Value>> {
    Ok(list(o, Some(&n.id))
        .await?
        .iter()
        .map(|a| assignment_json(a, n))
        .collect())
}
