//! What a task is about in somebody else's tracker.
//!
//! The fleet's own key (`F-41`) names a task; a link says which outside thing it belongs to —
//! the Jira ticket it came from, the merge request it opened, a pull request, a page. The one
//! that matters most is usually discovered during the work rather than known at the start, which
//! is why an agent can record its own (`fleet agent link`), and why these are rows rather than a
//! field on the task.
//!
//! A link is a *claim*, not a capability: recording `BIOINS-5068` gives nobody access to it. The
//! fleet already decides separately whether an agent may read a project's Jira (see `jira`), and
//! nothing here changes that.

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::core::Orchestrator;

/// The kinds of thing a task can point at. Stored as text, so a tracker nobody has thought of
/// yet needs no migration and an unknown kind degrades to a plain chip.
pub const KINDS: [&str; 4] = ["jira", "mr", "pr", "url"];

#[derive(Debug, Clone, FromRow)]
pub struct LinkRow {
    pub id: i64,
    pub task_id: String,
    pub kind: String,
    pub ref_: String,
    pub url: Option<String>,
    pub added_by: String,
}

/// Reads what a person wrote and works out what they meant.
///
/// `BIOINS-5068` is a ticket, `!940` a merge request, `#482` a pull request, and anything that
/// starts with `http` is just itself. A bare word is refused rather than guessed at: a link
/// nobody can follow and nobody can read is worse than no link.
pub fn classify(raw: &str) -> Result<(String, String)> {
    let s = raw.trim();
    ensure!(!s.is_empty(), "say what to link to");
    ensure!(
        s.len() <= 500,
        "that is too long to be a ticket reference ({} characters)",
        s.len()
    );
    if s.starts_with("http://") || s.starts_with("https://") {
        return Ok(("url".into(), s.to_owned()));
    }
    if let Some(n) = s.strip_prefix('!')
        && n.chars().all(|c| c.is_ascii_digit())
        && !n.is_empty()
    {
        return Ok(("mr".into(), s.to_owned()));
    }
    if let Some(n) = s.strip_prefix('#')
        && n.chars().all(|c| c.is_ascii_digit())
        && !n.is_empty()
    {
        return Ok(("pr".into(), s.to_owned()));
    }
    // A Jira key: letters, a dash, digits. Deliberately not the fleet's own prefix — `F-41`
    // names a task here, and a task cannot be an external ticket for itself.
    if let Some((prefix, n)) = s.split_once('-')
        && !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphanumeric())
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
    {
        if crate::model::parse_task_key(s).is_some() {
            bail!("{s} is this fleet's own task key, not an external ticket");
        }
        return Ok(("jira".into(), s.to_ascii_uppercase()));
    }
    bail!(
        "{s:?} is not something I can link to: a ticket (BIOINS-5068), a merge request (!940), \
         a pull request (#482), or a URL"
    )
}

/// Attaches a link to a task, resolving a URL for it where the fleet knows one.
///
/// Adding the same thing twice is not an error: the second one is simply already there, which is
/// what an agent retrying a step needs.
pub async fn add(o: &Orchestrator, task_id: &str, raw: &str, added_by: &str) -> Result<LinkRow> {
    let (kind, reference) = classify(raw)?;
    let task = o.task(task_id).await?.with_context_task(task_id)?;
    let url = resolve_url(o, &task.project, &kind, &reference).await;
    sqlx::query(
        "insert into task_links (task_id, kind, ref, url, added_by) values ($1, $2, $3, $4, $5)
         on conflict (task_id, kind, ref) do update set url = coalesce(excluded.url, task_links.url)",
    )
    .bind(&task.id)
    .bind(&kind)
    .bind(&reference)
    .bind(&url)
    .bind(added_by)
    .execute(&o.db)
    .await?;
    o.audit(
        added_by,
        "task.link",
        json!({"task": task.id, "kind": kind, "ref": reference}),
    )
    .await;
    Ok(sqlx::query_as(
        "select id, task_id, kind, ref as ref_, url, added_by from task_links
         where task_id = $1 and kind = $2 and ref = $3",
    )
    .bind(&task.id)
    .bind(&kind)
    .bind(&reference)
    .fetch_one(&o.db)
    .await?)
}

/// Removes a link. Returns whether there was one.
pub async fn remove(o: &Orchestrator, task_id: &str, raw: &str, actor: &str) -> Result<bool> {
    let (kind, reference) = classify(raw)?;
    let task = o.task(task_id).await?.with_context_task(task_id)?;
    let gone = sqlx::query("delete from task_links where task_id = $1 and kind = $2 and ref = $3")
        .bind(&task.id)
        .bind(&kind)
        .bind(&reference)
        .execute(&o.db)
        .await?
        .rows_affected()
        > 0;
    if gone {
        o.audit(
            actor,
            "task.unlink",
            json!({"task": task.id, "kind": kind, "ref": reference}),
        )
        .await;
    }
    Ok(gone)
}

/// Every link of one task, oldest first.
pub async fn of_task(o: &Orchestrator, task_id: &str) -> Result<Vec<LinkRow>> {
    Ok(sqlx::query_as(
        "select id, task_id, kind, ref as ref_, url, added_by from task_links
         where task_id = $1 order by id",
    )
    .bind(task_id)
    .fetch_all(&o.db)
    .await?)
}

/// The links of many tasks at once, for a page that shows a lot of them.
pub async fn of_tasks(o: &Orchestrator, ids: &[String]) -> Result<Vec<LinkRow>> {
    Ok(sqlx::query_as(
        "select id, task_id, kind, ref as ref_, url, added_by from task_links
         where task_id = any($1) order by id",
    )
    .bind(ids)
    .fetch_all(&o.db)
    .await?)
}

pub fn json(l: &LinkRow) -> Value {
    json!({"id": l.id, "kind": l.kind, "ref": l.ref_, "url": l.url, "added_by": l.added_by})
}

/// Where a reference points, when the fleet can work it out.
///
/// Only Jira is resolvable today: the project's own site is configured, so a ticket key is enough
/// to build a URL. A merge request or pull request names a number in a repository the
/// orchestrator has never been told about, so it stays a readable chip until someone links the
/// URL itself.
async fn resolve_url(
    o: &Orchestrator,
    project: &str,
    kind: &str,
    reference: &str,
) -> Option<String> {
    match kind {
        "url" => Some(reference.to_owned()),
        "jira" => {
            let jira = crate::jira::for_project(o, project).await.ok()?;
            Some(format!("https://{}/browse/{reference}", jira.site()))
        }
        _ => None,
    }
}

/// `context` for the one error every function here can hit, said the same way each time.
trait TaskContext {
    fn with_context_task(self, id: &str) -> Result<crate::model::TaskRow>;
}

impl TaskContext for Option<crate::model::TaskRow> {
    fn with_context_task(self, id: &str) -> Result<crate::model::TaskRow> {
        self.ok_or_else(|| anyhow::anyhow!("no task {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn a_reference_is_read_as_what_it_looks_like() {
        assert_eq!(
            classify("BIOINS-5068").unwrap(),
            ("jira".into(), "BIOINS-5068".into())
        );
        // Said in lower case, stored the way the tracker writes it.
        assert_eq!(
            classify(" dap-356 ").unwrap(),
            ("jira".into(), "DAP-356".into())
        );
        assert_eq!(classify("!940").unwrap(), ("mr".into(), "!940".into()));
        assert_eq!(classify("#482").unwrap(), ("pr".into(), "#482".into()));
        let url = "https://gitlab.example.com/g/p/-/merge_requests/940";
        assert_eq!(classify(url).unwrap(), ("url".into(), url.into()));
    }

    #[test]
    fn the_fleets_own_key_is_not_an_external_ticket() {
        let e = classify("F-41").unwrap_err().to_string();
        assert!(e.contains("own task key"), "{e}");
    }

    #[test]
    fn a_reference_nobody_could_follow_is_refused_rather_than_guessed() {
        for bad in [
            "",
            "   ",
            "the jira ticket",
            "!",
            "#",
            "#abc",
            "-5068",
            "ABC-",
        ] {
            assert!(classify(bad).is_err(), "{bad:?} should not be a link");
        }
    }
}
