//! The local copy of every connected Jira: each project's tickets in its active sprint, its future
//! sprints and its backlog, for any assignee; the sprints they are in; and the people seen on
//! them (assignees, reporters, commenters). The Jira page and the roadmap read this copy, so the
//! owner sees everything at once and nothing but this module calls Jira on a timer.
//!
//! Projects are read per credential folder (an account on one site; two sites with two accounts
//! is the normal case), each project on its own, so one that fails (a mistyped key, access
//! lost) is named on the page and does not stop the others. Every `FLEET_JIRA_SYNC_MINUTES` an
//! account reads what changed since its last good read (a ticket that closed comes back
//! closed), asks its open sprints how they stand (a sprint can start without any of its tickets
//! changing), and reads who commented on a few of the tickets that changed. Every
//! `FLEET_JIRA_FULL_SYNC_HOURS`, or when the owner asks, it reads everything again in two parts:
//! every ticket of an open sprint, then the open backlog newest first, up to
//! `FLEET_JIRA_MAX_ISSUES` — so a cap never cuts the sprints the owner works from — and only a
//! read Jira had nothing more to give for may mark a ticket gone. Once a day, or when the
//! folder's site changed, it asks who it is there (what "assigned to me" means) and which fields
//! hold sprints and rank on that site.
//!
//! It runs on its own, never inside the housekeeping loop's turn, and one sync at a time: a read
//! the owner asks for while one runs is refused rather than queued.
//!
//! The credential stays here; everything this module stores is what any reader of those tickets
//! sees, and it stores no ticket descriptions and no comments, only who wrote them.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, PgConnection};

use crate::{
    chat::clip,
    core::Orchestrator,
    jira::{self, Jira, JiraUser, MirrorIssue, MirrorRead, SprintInfo},
};

/// Open sprints asked about per account per sync.
const SPRINT_CHECKS: i64 = 10;
/// Minutes added to the incremental window, so a read that ran late misses nothing.
const WINDOW_MARGIN_MINUTES: i64 = 5;

#[derive(Debug, Clone, FromRow)]
struct AccountRow {
    folder: String,
    site: Option<String>,
    account_id: Option<String>,
    sprint_field: Option<String>,
    rank_field: Option<String>,
    fields_checked_at: Option<DateTime<Utc>>,
    last_sync_at: Option<DateTime<Utc>>,
    last_full_sync_at: Option<DateTime<Utc>>,
}

/// Each project with a Jira key, by the folder its credential is in.
async fn projects_by_folder(o: &Orchestrator) -> Result<BTreeMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select coalesce(nullif(jira_folder, ''), $1), slug from projects
         where jira_key is not null order by slug",
    )
    .bind(jira::DEFAULT_FOLDER)
    .fetch_all(&o.db)
    .await?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (folder, slug) in rows {
        out.entry(folder).or_default().push(slug);
    }
    Ok(out)
}

async fn account(o: &Orchestrator, folder: &str) -> Result<AccountRow> {
    Ok(sqlx::query_as(
        "insert into jira_accounts (folder) values ($1)
         on conflict (folder) do update set folder = excluded.folder
         returning folder, site, account_id, sprint_field, rank_field, fields_checked_at,
                   last_sync_at, last_full_sync_at",
    )
    .bind(folder)
    .fetch_one(&o.db)
    .await?)
}

/// From housekeeping: starts the sync of every account that is due, in the background, so a
/// slow Jira never holds up the rest of housekeeping. Nothing starts while a sync runs.
pub fn tick(o: &Arc<Orchestrator>) {
    let o = o.clone();
    tokio::spawn(async move {
        let lock = o.task_lock("~jira-sync");
        let Ok(_held) = lock.try_lock() else {
            return;
        };
        let folders = match projects_by_folder(&o).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("listing the projects to read from Jira failed: {e:#}");
                return;
            }
        };
        let every = chrono::Duration::minutes(o.cfg.jira.sync_minutes);
        for (folder, projects) in folders {
            match account(&o, &folder).await {
                Ok(a) if a.last_sync_at.is_some_and(|at| Utc::now() - at < every) => {}
                Ok(a) => {
                    sync_account(&o, &a, &projects, false).await;
                }
                Err(e) => tracing::warn!("reading Jira ({folder}) failed: {e:#}"),
            }
        }
    });
}

/// Reads every account now, in full when `full`, and waits for it (the roadmap's own refresh,
/// the brain's `roadmap sync`). Waits for a sync already running rather than refusing.
pub async fn sync_all(o: &Arc<Orchestrator>, full: bool) -> Result<Value> {
    let lock = o.task_lock("~jira-sync");
    let _held = lock.lock().await;
    sync_all_locked(o, full).await
}

async fn sync_all_locked(o: &Arc<Orchestrator>, full: bool) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for (folder, projects) in projects_by_folder(o).await? {
        let a = account(o, &folder).await?;
        out.insert(folder, sync_account(o, &a, &projects, full).await);
    }
    Ok(Value::Object(out))
}

/// The owner's "Read Jira now": a full read in the background, answered at once. `false` when
/// one is running already (the page shows it when it is done).
pub fn sync_all_soon(
    o: &Arc<Orchestrator>,
    then: Option<Box<dyn FnOnce(Arc<Orchestrator>) + Send>>,
) -> bool {
    let lock = o.task_lock("~jira-sync");
    let Ok(held) = lock.clone().try_lock_owned() else {
        return false;
    };
    let o = o.clone();
    tokio::spawn(async move {
        let _held = held;
        if let Err(e) = sync_all_locked(&o, true).await {
            tracing::warn!("reading Jira failed: {e:#}");
        }
        drop(_held);
        if let Some(then) = then {
            then(o);
        }
    });
    true
}

/// One account's sync. Never fails: what went wrong is recorded on the account (each failing
/// project named), where the Jira and Integrations pages show it.
async fn sync_account(o: &Orchestrator, a: &AccountRow, projects: &[String], full: bool) -> Value {
    let full = full
        || a.last_full_sync_at.is_none_or(|at| {
            Utc::now() - at >= chrono::Duration::hours(o.cfg.jira.full_sync_hours)
        });
    let began = Utc::now();
    let outcome = sync_account_inner(o, a, projects, full).await;
    let (ok, error, truncated) = match &outcome {
        Ok((errors, truncated)) if errors.is_empty() => (true, None, *truncated),
        Ok((errors, truncated)) => (false, Some(clip(&errors.join("; "), 600)), *truncated),
        Err(e) => (false, Some(clip(&format!("{e:#}"), 600)), false),
    };
    // A full read that ran counts as done even if a project failed in it: that project says
    // so, and the others are not read in full again every few minutes because of it.
    let full_ran = full && outcome.is_ok();
    if let Err(e) = sqlx::query(
        "update jira_accounts set last_sync_at = now(),
                last_full_sync_at = case when $2 then now() else last_full_sync_at end,
                last_ok_at = case when $3 then $6 else last_ok_at end,
                last_error = $4,
                last_error_at = case when $3 then last_error_at else now() end,
                truncated = case when $2 then $5 else truncated end
         where folder = $1",
    )
    .bind(&a.folder)
    .bind(full_ran)
    .bind(ok)
    .bind(&error)
    .bind(truncated)
    .bind(began)
    .execute(&o.db)
    .await
    {
        tracing::warn!("recording the Jira sync of {} failed: {e:#}", a.folder);
    }
    if let Some(e) = &error {
        tracing::warn!("reading Jira ({}) failed: {e}", a.folder);
    }
    json!({"ok": ok, "full": full, "error": error, "truncated": truncated})
}

/// Each failing project's error, and whether a full read stopped at the cap.
async fn sync_account_inner(
    o: &Orchestrator,
    a: &AccountRow,
    projects: &[String],
    full: bool,
) -> Result<(Vec<String>, bool)> {
    let acc = jira::for_folder(o, &a.folder).await?;
    let site = acc.site().to_owned();
    // Once a day, or as soon as the folder points elsewhere: who we are there, and which fields
    // hold sprints and rank on that site.
    let (mut sprint_field, mut rank_field) = (a.sprint_field.clone(), a.rank_field.clone());
    if a.account_id.is_none()
        || a.site.as_deref() != Some(site.as_str())
        || a.fields_checked_at
            .is_none_or(|at| Utc::now() - at >= chrono::Duration::hours(24))
    {
        let me = acc
            .myself()
            .await
            .context("asking Jira who this account is")?;
        let (sf, rf) = acc
            .discover_fields()
            .await
            .context("reading the site's fields")?;
        sqlx::query(
            "update jira_accounts set site = $2, account_id = $3, account_name = $4,
                    sprint_field = $5, rank_field = $6, fields_checked_at = now()
             where folder = $1",
        )
        .bind(&a.folder)
        .bind(&site)
        .bind(&me.account_id)
        .bind(&me.display_name)
        .bind(&sf)
        .bind(&rf)
        .execute(&o.db)
        .await?;
        (sprint_field, rank_field) = (sf, rf);
    }
    let mut errors = Vec::new();
    let mut truncated = false;
    for project in projects {
        // Since the start of this project's last good read (plus a margin), so a gap — a restart,
        // a deploy, a run of failed reads — misses nothing; never less than twice the interval.
        // Per project: one that keeps failing does not widen the others' reads.
        let last_ok: Option<DateTime<Utc>> =
            sqlx::query_scalar("select ok_at from jira_project_reads where project = $1")
                .bind(project)
                .fetch_optional(&o.db)
                .await?
                .flatten();
        let since = last_ok
            .map(|at| (Utc::now() - at).num_minutes() + WINDOW_MARGIN_MINUTES)
            .unwrap_or(i64::MAX)
            .max(o.cfg.jira.sync_minutes * 2)
            .min(o.cfg.jira.full_sync_hours * 60 + WINDOW_MARGIN_MINUTES);
        // A project never read well is read in full.
        let full = full || last_ok.is_none();
        let began = Utc::now();
        let read = async {
            let j = jira::for_project(o, project).await?;
            read_project(
                o,
                &j,
                project,
                &site,
                full,
                since,
                sprint_field.as_deref(),
                rank_field.as_deref(),
            )
            .await
        }
        .await;
        let error = match read {
            Ok(t) => {
                truncated |= t;
                None
            }
            Err(e) => {
                let e = clip(&format!("{e:#}"), 300);
                errors.push(format!("{project}: {e}"));
                Some(e)
            }
        };
        sqlx::query(
            "insert into jira_project_reads (project, ok_at, error, error_at)
             values ($1, case when $3::text is null then $2::timestamptz end, $3,
                     case when $3::text is not null then now() end)
             on conflict (project) do update set
                ok_at = case when $3::text is null then $2::timestamptz
                             else jira_project_reads.ok_at end,
                error = $3,
                error_at = case when $3::text is null then jira_project_reads.error_at
                                else now() end",
        )
        .bind(project)
        .bind(began)
        .bind(&error)
        .execute(&o.db)
        .await?;
    }
    refresh_sprints(o, &acc, &site).await;
    read_commenters(o, projects, o.cfg.jira.sync_minutes * 2).await;
    Ok((errors, truncated))
}

/// One project's read: what changed lately, or (full) its open sprints and then its backlog,
/// stored in one transaction. Returns whether the full read stopped at the cap.
#[allow(clippy::too_many_arguments)]
async fn read_project(
    o: &Orchestrator,
    j: &Jira,
    project: &str,
    site: &str,
    full: bool,
    since: i64,
    sprint_field: Option<&str>,
    rank_field: Option<&str>,
) -> Result<bool> {
    let cap = o.cfg.jira.max_issues;
    let began: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
        .fetch_one(&o.db)
        .await?;
    let mut issues = Vec::new();
    let mut complete = true;
    if full {
        if sprint_field.is_some() {
            let (s, c) = j
                .mirror_issues(MirrorRead::OpenSprints, sprint_field, rank_field, cap)
                .await?;
            complete &= c;
            issues.extend(s);
        }
        let room = cap.saturating_sub(issues.len()).max(1);
        let (b, c) = j
            .mirror_issues(MirrorRead::Backlog, sprint_field, rank_field, room)
            .await?;
        complete &= c;
        let seen: HashSet<String> = issues.iter().map(|i: &MirrorIssue| i.key.clone()).collect();
        issues.extend(b.into_iter().filter(|i| !seen.contains(&i.key)));
    } else {
        let (r, _) = j
            .mirror_issues(MirrorRead::Recent(since), sprint_field, rank_field, cap)
            .await?;
        issues = r;
    }
    let mut tx = o.db.begin().await?;
    let mut sprints = HashSet::new();
    let mut users = HashSet::new();
    for i in &issues {
        for s in &i.sprints {
            if sprints.insert(s.id) {
                store_sprint(&mut tx, site, s).await?;
            }
        }
        store_issue(&mut tx, project, site, i).await?;
        for u in [&i.assignee, &i.reporter].into_iter().flatten() {
            if users.insert(u.account_id.clone()) {
                store_user(&mut tx, project, site, u).await?;
            }
        }
    }
    if full && complete {
        // Not in a complete read of everything open: closed outside a sprint, moved away, or
        // deleted. Off the page, still in the table. By project, whatever site it was read from
        // before (a project moved to another site leaves nothing behind).
        sqlx::query(
            "update jira_issues set gone_at = now()
             where project = $1 and last_seen < $2 and gone_at is null",
        )
        .bind(project)
        .bind(began)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(full && !complete)
}

async fn store_issue(
    c: &mut PgConnection,
    project: &str,
    site: &str,
    i: &MirrorIssue,
) -> Result<()> {
    sqlx::query(
        "insert into jira_issues (project, site, jira_key, jira_id, summary, status, status_category,
                                  issue_type, priority, labels, parent_key, assignee_id,
                                  assignee_name, reporter_id, reporter_name, sprint_id, rank,
                                  jira_created, jira_updated)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
         on conflict (project, jira_key) do update set
            site = excluded.site, jira_id = excluded.jira_id, summary = excluded.summary,
            status = excluded.status, status_category = excluded.status_category,
            issue_type = excluded.issue_type, priority = excluded.priority,
            labels = excluded.labels, parent_key = excluded.parent_key,
            assignee_id = excluded.assignee_id, assignee_name = excluded.assignee_name,
            reporter_id = excluded.reporter_id, reporter_name = excluded.reporter_name,
            sprint_id = excluded.sprint_id, rank = excluded.rank,
            jira_created = excluded.jira_created, jira_updated = excluded.jira_updated,
            last_seen = now(), gone_at = null",
    )
    .bind(project)
    .bind(site)
    .bind(&i.key)
    .bind(&i.id)
    .bind(&i.summary)
    .bind(&i.status)
    .bind(&i.status_category)
    .bind(&i.issue_type)
    .bind(&i.priority)
    .bind(&i.labels)
    .bind(&i.parent)
    .bind(i.assignee.as_ref().map(|u| &u.account_id))
    .bind(i.assignee.as_ref().map(|u| &u.display_name))
    .bind(i.reporter.as_ref().map(|u| &u.account_id))
    .bind(i.reporter.as_ref().map(|u| &u.display_name))
    .bind(i.sprint.as_ref().map(|s| s.id))
    .bind(&i.rank)
    .bind(i.created)
    .bind(i.updated)
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn store_sprint(c: &mut PgConnection, site: &str, s: &SprintInfo) -> Result<()> {
    sqlx::query(
        "insert into jira_sprints (site, sprint_id, board_id, name, state, starts_at, ends_at, goal)
         values ($1, $2, $3, $4, $5, $6, $7, $8)
         on conflict (site, sprint_id) do update set
            board_id = coalesce(excluded.board_id, jira_sprints.board_id), name = excluded.name,
            state = excluded.state, starts_at = excluded.starts_at, ends_at = excluded.ends_at,
            goal = excluded.goal, last_seen = now()",
    )
    .bind(site)
    .bind(s.id)
    .bind(s.board_id)
    .bind(&s.name)
    .bind(&s.state)
    .bind(s.starts_at)
    .bind(s.ends_at)
    .bind(&s.goal)
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn store_user(c: &mut PgConnection, project: &str, site: &str, u: &JiraUser) -> Result<()> {
    sqlx::query(
        "insert into jira_users (account_id, display_name, time_zone, avatar_url, sites, projects)
         values ($1, $2, $3, $4, array[$5], array[$6])
         on conflict (account_id) do update set
            display_name = excluded.display_name,
            time_zone = coalesce(excluded.time_zone, jira_users.time_zone),
            avatar_url = coalesce(excluded.avatar_url, jira_users.avatar_url),
            sites = case when $5 = any(jira_users.sites) then jira_users.sites
                         else jira_users.sites || $5 end,
            projects = case when $6 = any(jira_users.projects) then jira_users.projects
                            else jira_users.projects || $6 end,
            last_seen = now()",
    )
    .bind(&u.account_id)
    .bind(clip(&u.display_name, 200))
    .bind(&u.time_zone)
    .bind(&u.avatar_url)
    .bind(site)
    .bind(project)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Open sprints asked how they stand: one that started (or ended) changes no ticket, so the
/// ticket reads alone would not notice. The least recently checked first, so one that keeps
/// failing (deleted, on a board the account cannot see) never stops the others being checked;
/// a sprint Jira no longer has is closed here.
async fn refresh_sprints(o: &Orchestrator, acc: &Jira, site: &str) {
    let ids: Vec<i64> = sqlx::query_scalar(
        "select sprint_id from jira_sprints where site = $1 and state in ('active', 'future')
         order by last_seen limit $2",
    )
    .bind(site)
    .bind(SPRINT_CHECKS)
    .fetch_all(&o.db)
    .await
    .unwrap_or_default();
    for id in ids {
        let stored = match acc.sprint_info(id).await {
            Ok(s) => match o.db.acquire().await {
                Ok(mut c) => store_sprint(&mut c, site, &s).await,
                Err(e) => Err(e.into()),
            },
            Err(e) => {
                tracing::debug!("asking Jira about sprint {id} failed: {e:#}");
                let gone = format!("{e:#}").contains("answered 404");
                sqlx::query(
                    "update jira_sprints set last_seen = now(),
                            state = case when $3 then 'closed' else state end
                     where site = $1 and sprint_id = $2",
                )
                .bind(site)
                .bind(id)
                .bind(gone)
                .execute(&o.db)
                .await
                .map(|_| ())
                .map_err(Into::into)
            }
        };
        if let Err(e) = stored {
            tracing::warn!("storing sprint {id} failed: {e:#}");
        }
    }
}

/// Who commented on tickets that changed lately, a few per sync: people the ticket fields alone
/// would never name. Only tickets updated within `minutes`, so a full read does not reread the
/// same lowest keys every time.
async fn read_commenters(o: &Orchestrator, projects: &[String], minutes: i64) {
    let changed: Vec<(String, String, String)> = sqlx::query_as(
        "select project, site, jira_key from jira_issues
         where project = any($1) and gone_at is null
           and jira_updated > now() - make_interval(mins => $2::int)
         order by jira_updated desc limit $3",
    )
    .bind(projects)
    .bind(minutes.clamp(1, i32::MAX as i64) as i32)
    .bind(o.cfg.jira.comment_fetches)
    .fetch_all(&o.db)
    .await
    .unwrap_or_default();
    let mut clients: BTreeMap<String, Jira> = BTreeMap::new();
    for (project, site, key) in changed {
        if !clients.contains_key(&project) {
            match jira::for_project(o, &project).await {
                Ok(j) => {
                    clients.insert(project.clone(), j);
                }
                Err(_) => continue,
            }
        }
        let j = &clients[&project];
        match j.comment_authors(&key, 20).await {
            Ok(list) => {
                for u in list {
                    let stored = async {
                        let mut c = o.db.acquire().await?;
                        store_user(&mut c, &project, &site, &u).await?;
                        sqlx::query(
                            "insert into jira_comment_authors (site, jira_key, account_id, project)
                             values ($1, $2, $3, $4)
                             on conflict (site, jira_key, account_id) do update set last_at = now()",
                        )
                        .bind(&site)
                        .bind(&key)
                        .bind(&u.account_id)
                        .bind(&project)
                        .execute(&mut *c)
                        .await?;
                        anyhow::Ok(())
                    }
                    .await;
                    if let Err(e) = stored {
                        tracing::warn!("storing a commenter of {key} failed: {e:#}");
                    }
                }
            }
            Err(e) => tracing::debug!("reading the commenters of {key} failed: {e:#}"),
        }
    }
}

/// How each account in use went at its last read, for the Jira and Integrations pages.
pub async fn accounts_json(o: &Orchestrator) -> Result<Value> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<String>,
        Option<DateTime<Utc>>,
        bool,
    )> = sqlx::query_as(
        "select a.folder, a.site, a.account_name, a.last_sync_at, a.last_full_sync_at, a.last_ok_at,
                a.last_error, a.last_error_at, a.truncated
         from jira_accounts a
         where exists (select 1 from projects p where p.jira_key is not null
                       and coalesce(nullif(p.jira_folder, ''), $1) = a.folder)
         order by a.folder",
    )
    .bind(jira::DEFAULT_FOLDER)
    .fetch_all(&o.db)
    .await?;
    Ok(Value::Object(
        rows.into_iter()
            .map(|r| {
                (
                    r.0,
                    json!({
                        "site": r.1, "account": r.2, "at": r.3, "full_at": r.4, "ok_at": r.5,
                        "error": r.6, "error_at": r.7, "truncated": r.8,
                        // A read that succeeds clears the error, so one standing is the last read's.
                        "ok": r.6.is_none(),
                    }),
                )
            })
            .collect(),
    ))
}

/// What the Jira page asks for. `sprint`: active, future, backlog, all, or a sprint id;
/// `assignee`: any, me, none, or an account id; `status`: open (not done), todo, progress, done,
/// any, or a status name. Each reaches SQL as a bind.
#[derive(Debug, Clone)]
pub struct IssueFilter {
    pub project: Option<String>,
    pub sprint: String,
    pub assignee: String,
    pub status: String,
    pub q: Option<String>,
    pub limit: i64,
}

#[derive(Debug, FromRow)]
struct IssueRow {
    project: String,
    site: String,
    jira_key: String,
    summary: String,
    status: Option<String>,
    status_category: Option<String>,
    issue_type: Option<String>,
    priority: Option<String>,
    labels: Vec<String>,
    parent_key: Option<String>,
    assignee_id: Option<String>,
    assignee_name: Option<String>,
    reporter_name: Option<String>,
    sprint_id: Option<i64>,
    sprint_name: Option<String>,
    sprint_state: Option<String>,
    sprint_starts: Option<DateTime<Utc>>,
    sprint_ends: Option<DateTime<Utc>>,
    jira_updated: Option<DateTime<Utc>>,
    me: Option<bool>,
    item_id: Option<i64>,
    item_state: Option<String>,
    task_id: Option<String>,
    task_key: Option<i64>,
    task_status: Option<String>,
}

/// The tickets the page shows, with their sprint, their roadmap item and its task. `$1` project,
/// `$2` sprint, `$3` assignee, `$4` status, `$5` words (escaped, see `search`).
const ISSUE_WHERE: &str = "
    from jira_issues i
    join projects p on p.slug = i.project
    left join jira_accounts a on a.folder = coalesce(nullif(p.jira_folder, ''), 'jira')
    left join jira_sprints s on s.site = i.site and s.sprint_id = i.sprint_id
    left join lateral (
       select r.id, r.state, r.task_id from roadmap_items r
       where r.project = i.project and r.jira_key = i.jira_key
         -- the current fleet sprint only: an item of an old one is history
         and r.sprint_id = (select max(id) from sprints where starts_on <= current_date)
       limit 1) r on true
    left join tasks t on t.id = r.task_id
    where i.gone_at is null and p.jira_key is not null
      and ($1::text is null or i.project = $1)
      and coalesce(case $2::text
            when 'active' then s.state = 'active'
            when 'future' then s.state = 'future'
            when 'backlog' then i.sprint_id is null
            when 'all' then true
            else i.sprint_id::text = $2 end, false)
      and coalesce(case $3::text
            when 'any' then true
            when 'me' then i.assignee_id = a.account_id
            when 'none' then i.assignee_id is null
            else i.assignee_id = $3 end, false)
      and coalesce(case $4::text
            when 'any' then true
            when 'open' then coalesce(i.status_category, '') <> 'done'
            when 'todo' then i.status_category = 'new'
            when 'progress' then i.status_category = 'indeterminate'
            when 'done' then i.status_category = 'done'
            else i.status = $4 end, false)
      and not exists (
        select 1 from unnest($5::text[]) w
        where not coalesce((
             i.jira_key ilike '%' || w || '%'
          or i.summary ilike '%' || w || '%'
          or coalesce(i.assignee_name, '') ilike '%' || w || '%'
          or coalesce(i.reporter_name, '') ilike '%' || w || '%'
          or coalesce(i.status, '') ilike '%' || w || '%'
          or coalesce(s.name, '') ilike '%' || w || '%'
          or array_to_string(i.labels, ' ') ilike '%' || w || '%'), false))";

/// The Jira page: the tickets that match, grouped by the order they are worked in (the active
/// sprint, the future sprints by start, then the backlog), with what the page filters by and how
/// each account's last read went. Ticket text and names are Jira's: shown as text.
pub async fn issues_json(o: &Orchestrator, f: &IssueFilter) -> Result<Value> {
    let words = f.q.as_deref().map(crate::search::words).unwrap_or_default();
    let limit = f.limit.clamp(1, 1000);
    let rows: Vec<IssueRow> = sqlx::query_as(&format!(
        "select i.project, i.site, i.jira_key, i.summary, i.status, i.status_category,
                i.issue_type, i.priority, i.labels, i.parent_key, i.assignee_id, i.assignee_name,
                i.reporter_name, i.sprint_id, s.name as sprint_name, s.state as sprint_state,
                s.starts_at as sprint_starts, s.ends_at as sprint_ends, i.jira_updated,
                (i.assignee_id = a.account_id) as me,
                r.id as item_id, r.state as item_state, r.task_id, t.key_no as task_key,
                t.status as task_status
         {ISSUE_WHERE}
         order by case s.state when 'active' then 0 when 'future' then 1 else 2 end,
                  s.starts_at nulls last, i.sprint_id nulls last, i.rank collate \"C\" nulls last, i.jira_key
         limit $6"
    ))
    .bind(&f.project)
    .bind(&f.sprint)
    .bind(&f.assignee)
    .bind(&f.status)
    .bind(&words)
    .bind(limit + 1)
    .fetch_all(&o.db)
    .await?;
    let more = rows.len() as i64 > limit;
    let issues: Vec<Value> = rows
        .iter()
        .take(limit as usize)
        .map(|r| {
            json!({
                "project": r.project, "key": r.jira_key,
                "url": format!("https://{}/browse/{}", r.site, r.jira_key),
                "summary": r.summary, "status": r.status, "status_category": r.status_category,
                "type": r.issue_type, "priority": r.priority, "labels": r.labels,
                "parent": r.parent_key,
                "assignee": r.assignee_id.as_ref().map(|id| json!({"account_id": id, "name": r.assignee_name})),
                "me": r.me.unwrap_or(false),
                "reporter": r.reporter_name,
                "sprint": r.sprint_id.map(|id| json!({
                    "id": id, "name": r.sprint_name, "state": r.sprint_state,
                    "starts_at": r.sprint_starts, "ends_at": r.sprint_ends,
                })),
                "updated": r.jira_updated,
                "roadmap": r.item_id.map(|id| json!({
                    "item": id, "state": r.item_state,
                    "task": r.task_id.as_ref().map(|t| json!({
                        "id": t,
                        "key": r.task_key.map_or_else(|| t.clone(), crate::model::task_key),
                        "status": r.task_status,
                    })),
                })),
            })
        })
        .collect();

    // What the filters offer, for the chosen project (or all), whatever else is chosen.
    #[allow(clippy::type_complexity)]
    let sprints: Vec<(i64, String, String, Option<DateTime<Utc>>, Option<DateTime<Utc>>, i64)> =
        sqlx::query_as(
            "select s.sprint_id, s.name, s.state, s.starts_at, s.ends_at, count(i.id)
             from jira_sprints s join jira_issues i on i.site = s.site and i.sprint_id = s.sprint_id
             where i.gone_at is null and ($1::text is null or i.project = $1)
               and s.state in ('active', 'future') and coalesce(i.status_category, '') <> 'done'
             group by s.sprint_id, s.name, s.state, s.starts_at, s.ends_at
             order by case s.state when 'active' then 0 else 1 end, s.starts_at nulls last, s.sprint_id",
        )
        .bind(&f.project)
        .fetch_all(&o.db)
        .await?;
    let assignees: Vec<(Option<String>, Option<String>, i64, Option<bool>)> = sqlx::query_as(
        "select i.assignee_id, max(i.assignee_name), count(*), bool_or(i.assignee_id = a.account_id)
         from jira_issues i join projects p on p.slug = i.project
         left join jira_accounts a on a.folder = coalesce(nullif(p.jira_folder, ''), 'jira')
         where i.gone_at is null and ($1::text is null or i.project = $1)
         group by i.assignee_id order by count(*) desc limit 60",
    )
    .bind(&f.project)
    .fetch_all(&o.db)
    .await?;
    let statuses: Vec<(Option<String>, Option<String>, i64)> = sqlx::query_as(
        "select status, max(status_category), count(*) from jira_issues
         where gone_at is null and ($1::text is null or project = $1)
         group by status
         order by case max(status_category) when 'new' then 0 when 'indeterminate' then 1 else 2 end,
                  status",
    )
    .bind(&f.project)
    .fetch_all(&o.db)
    .await?;
    #[allow(clippy::type_complexity)]
    let projects: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
    )> = sqlx::query_as(
        "select p.slug, p.name, p.color, p.jira_key,
                    coalesce(nullif(p.jira_folder, ''), $1), a.site
             from projects p
             left join jira_accounts a on a.folder = coalesce(nullif(p.jira_folder, ''), $1)
             where p.jira_key is not null order by p.slug",
    )
    .bind(jira::DEFAULT_FOLDER)
    .fetch_all(&o.db)
    .await?;
    Ok(json!({
        "projects": projects.iter().map(|p| json!({
            "slug": p.0, "name": p.1, "color": crate::admin::project_color(&p.0, p.2.as_deref()),
            "key": p.3, "folder": p.4, "site": p.5,
        })).collect::<Vec<_>>(),
        "sprints": sprints.iter().map(|s| json!({
            "id": s.0, "name": s.1, "state": s.2, "starts_at": s.3, "ends_at": s.4, "count": s.5,
        })).collect::<Vec<_>>(),
        "assignees": assignees.iter().map(|a| json!({
            "account_id": a.0, "name": a.1, "count": a.2, "me": a.3.unwrap_or(false),
        })).collect::<Vec<_>>(),
        "statuses": statuses.iter().map(|s| json!({"name": s.0, "category": s.1, "count": s.2}))
            .collect::<Vec<_>>(),
        "issues": issues,
        "more": more,
        "sync": accounts_json(o).await?,
    }))
}
