//! The local copy of every connected Jira: each project's tickets in its active sprint, its future
//! sprints and its backlog, for any assignee; the sprints they are in; and the people seen on
//! them (assignees, reporters, commenters). The Jira page and the roadmap read this copy, so the
//! owner sees everything at once and nothing but this module calls Jira on a timer.
//!
//! Projects are read per credential folder (an account on one site; two sites with two accounts
//! is the normal case). Every `FLEET_JIRA_SYNC_MINUTES` an account reads what changed lately in
//! each of its projects (one paged JQL each, a ticket that closed comes back closed), asks its
//! open sprints how they stand (a sprint can start without any of its tickets changing), and
//! reads who commented on a few of the tickets that changed. Every `FLEET_JIRA_FULL_SYNC_HOURS`,
//! or when the owner asks, it reads everything still open again: the only read that may mark a
//! ticket gone, and never one that stopped at the cap or failed. Once a day it asks who it is
//! (what "assigned to me" means on that site) and which fields hold sprints and rank there.
//!
//! The credential stays here; everything this module stores is what any reader of those tickets
//! sees, and it stores no ticket descriptions and no comments, only who wrote them.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::FromRow;

use crate::{
    chat::clip,
    core::Orchestrator,
    jira::{self, Jira, JiraUser, MirrorIssue, SprintInfo},
};

/// Open sprints asked about per account per sync.
const SPRINT_CHECKS: i64 = 10;

#[derive(Debug, Clone, FromRow)]
struct AccountRow {
    folder: String,
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
         returning folder, account_id, sprint_field, rank_field, fields_checked_at,
                   last_sync_at, last_full_sync_at",
    )
    .bind(folder)
    .fetch_one(&o.db)
    .await?)
}

/// From housekeeping: every account whose sync is due.
pub async fn tick(o: &Arc<Orchestrator>) -> Result<()> {
    let folders = projects_by_folder(o).await?;
    if folders.is_empty() {
        return Ok(());
    }
    let lock = o.task_lock("~jira-sync");
    let _held = lock.lock().await;
    let every = chrono::Duration::minutes(o.cfg.jira.sync_minutes);
    for (folder, projects) in folders {
        let a = account(o, &folder).await?;
        if a.last_sync_at.is_some_and(|at| Utc::now() - at < every) {
            continue;
        }
        sync_account(o, &a, &projects, false).await;
    }
    Ok(())
}

/// Reads every account now, in full when `full` (the owner's "Read Jira now"). Returns how each
/// account went.
pub async fn sync_all(o: &Arc<Orchestrator>, full: bool) -> Result<Value> {
    let lock = o.task_lock("~jira-sync");
    let _held = lock.lock().await;
    let mut out = serde_json::Map::new();
    for (folder, projects) in projects_by_folder(o).await? {
        let a = account(o, &folder).await?;
        out.insert(folder, sync_account(o, &a, &projects, full).await);
    }
    Ok(Value::Object(out))
}

/// One account's sync. Never fails: what went wrong is recorded on the account, where the Jira
/// and Integrations pages show it.
async fn sync_account(o: &Orchestrator, a: &AccountRow, projects: &[String], full: bool) -> Value {
    let full = full
        || a.last_full_sync_at.is_none_or(|at| {
            Utc::now() - at >= chrono::Duration::hours(o.cfg.jira.full_sync_hours)
        });
    let outcome = sync_account_inner(o, a, projects, full).await;
    let (ok, error, truncated) = match &outcome {
        Ok(t) => (true, None, *t),
        Err(e) => (false, Some(clip(&format!("{e:#}"), 400)), false),
    };
    if let Err(e) = sqlx::query(
        "update jira_accounts set last_sync_at = now(),
                last_full_sync_at = case when $2 and $3 then now() else last_full_sync_at end,
                last_ok_at = case when $3 then now() else last_ok_at end,
                last_error = $4,
                last_error_at = case when $3 then last_error_at else now() end,
                truncated = case when $2 and $3 then $5 else truncated end
         where folder = $1",
    )
    .bind(&a.folder)
    .bind(full)
    .bind(ok)
    .bind(&error)
    .bind(truncated)
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

/// Returns whether a full read stopped at the cap.
async fn sync_account_inner(
    o: &Orchestrator,
    a: &AccountRow,
    projects: &[String],
    full: bool,
) -> Result<bool> {
    let acc = jira::for_folder(o, &a.folder).await?;
    let site = acc.site().to_owned();
    // Once a day: who we are there, and which fields hold sprints and rank on that site.
    let (mut sprint_field, mut rank_field) = (a.sprint_field.clone(), a.rank_field.clone());
    if a.account_id.is_none()
        || a.fields_checked_at
            .is_none_or(|at| Utc::now() - at >= chrono::Duration::hours(24))
    {
        let me = acc.myself().await?;
        let (sf, rf) = acc.discover_fields().await?;
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
    // Twice the interval, so a sync that ran late misses nothing.
    let since = (!full).then(|| o.cfg.jira.sync_minutes * 2);
    let mut truncated = false;
    let mut changed: Vec<(String, String)> = Vec::new();
    for project in projects {
        let j = jira::for_project(o, project).await?;
        let began: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
            .fetch_one(&o.db)
            .await?;
        let (issues, more) = j
            .mirror_issues(
                since,
                sprint_field.as_deref(),
                rank_field.as_deref(),
                o.cfg.jira.max_issues,
            )
            .await?;
        for i in &issues {
            store_issue(o, project, &site, i).await?;
            changed.push((project.clone(), i.key.clone()));
        }
        if full && !more {
            // Not in a complete read of everything open: closed outside a sprint, moved away,
            // or deleted. Off the page, still in the table.
            sqlx::query(
                "update jira_issues set gone_at = now()
                 where project = $1 and site = $2 and last_seen < $3 and gone_at is null",
            )
            .bind(project)
            .bind(&site)
            .bind(began)
            .execute(&o.db)
            .await?;
        }
        truncated |= full && more;
    }
    refresh_sprints(o, &acc, &site).await;
    read_commenters(o, &changed).await;
    Ok(truncated)
}

async fn store_issue(o: &Orchestrator, project: &str, site: &str, i: &MirrorIssue) -> Result<()> {
    for s in &i.sprints {
        store_sprint(o, site, s).await?;
    }
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
    .execute(&o.db)
    .await?;
    for u in [&i.assignee, &i.reporter].into_iter().flatten() {
        store_user(o, project, site, u).await?;
    }
    Ok(())
}

async fn store_sprint(o: &Orchestrator, site: &str, s: &SprintInfo) -> Result<()> {
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
    .execute(&o.db)
    .await?;
    Ok(())
}

async fn store_user(o: &Orchestrator, project: &str, site: &str, u: &JiraUser) -> Result<()> {
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
    .execute(&o.db)
    .await?;
    Ok(())
}

/// Open sprints asked how they stand: one that started (or ended) changes no ticket, so the
/// ticket reads alone would not notice. A site whose account may not read the agile API is
/// corrected by the next full read instead.
async fn refresh_sprints(o: &Orchestrator, acc: &Jira, site: &str) {
    let ids: Vec<i64> = sqlx::query_scalar(
        "select sprint_id from jira_sprints where site = $1 and state in ('active', 'future')
         order by starts_at nulls last limit $2",
    )
    .bind(site)
    .bind(SPRINT_CHECKS)
    .fetch_all(&o.db)
    .await
    .unwrap_or_default();
    for id in ids {
        match acc.sprint_info(id).await {
            Ok(s) => {
                if let Err(e) = store_sprint(o, site, &s).await {
                    tracing::warn!("storing sprint {id} failed: {e:#}");
                }
            }
            Err(e) => {
                tracing::debug!("asking Jira about sprint {id} failed: {e:#}");
                break;
            }
        }
    }
}

/// Who commented on the tickets that changed, a few per sync: people the ticket fields alone
/// would never name.
async fn read_commenters(o: &Orchestrator, changed: &[(String, String)]) {
    for (project, key) in changed.iter().take(o.cfg.jira.comment_fetches as usize) {
        let Ok(j) = jira::for_project(o, project).await else {
            continue;
        };
        let site = j.site().to_owned();
        match j.comment_authors(key, 20).await {
            Ok(list) => {
                for u in list {
                    let stored = async {
                        store_user(o, project, &site, &u).await?;
                        sqlx::query(
                            "insert into jira_comment_authors (jira_key, account_id, project)
                             values ($1, $2, $3)
                             on conflict (jira_key, account_id) do update set last_at = now()",
                        )
                        .bind(key)
                        .bind(&u.account_id)
                        .bind(project)
                        .execute(&o.db)
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

/// How each account's last read went, for the Jira and Integrations pages.
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
        "select folder, site, account_name, last_sync_at, last_full_sync_at, last_ok_at,
                last_error, last_error_at, truncated
         from jira_accounts order by folder",
    )
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
       order by r.sprint_id desc limit 1) r on true
    left join tasks t on t.id = r.task_id
    where i.gone_at is null
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
                  s.starts_at nulls last, i.sprint_id nulls last, i.rank nulls last, i.jira_key
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
               and s.state in ('active', 'future')
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
         group by status order by max(status_category), status",
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
