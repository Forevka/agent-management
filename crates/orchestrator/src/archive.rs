//! The nightly archive: done tasks move out of the board's way.
//!
//! Once a day, at `FLEET_ARCHIVE_AT` (21:00) in `FLEET_ARCHIVE_TIMEZONE` (Europe/Kyiv), every task
//! that is `done` gets an `archived_at`. An archived task is hidden from the board and the default
//! lists, and still found by search, by its link and by the `archived` filter. Nothing else about
//! it changes: it stays `done`, its transcript, links and attachments stay, and `updated_at` is
//! left alone, because being put away is not something that happened to the work.
//!
//! - **A mark, not a status.** Everything that reads a task's status (the plan gate, delivery,
//!   topic cleanup, an older release after a rollback) still sees `done` and treats the task as
//!   finished work. Only the views that list tasks look at `archived_at`.
//! - **Time zones.** The local time is worked out by Postgres (`at time zone`), whose zone data
//!   knows Kyiv's summer and winter offsets, so 21:00 stays 21:00 across the DST changes rather
//!   than drifting by an hour twice a year.
//! - **Once a day, late rather than never.** The local day of the last run is kept in `kv`. The
//!   archive runs on the first housekeeping tick at or after 21:00 that is on a new day, so an
//!   orchestrator that was down at 21:00 catches up when it comes back that evening.
//! - **The evening's work stays visible.** Only tasks finished at least an hour before the run
//!   are archived, so what was done at 20:30 is still in the board's Done column that night and is
//!   put away the next evening.
//! - **Workflows are never archived.** A task turned into a standing automation keeps its topic
//!   open to be triggered again and belongs on the board; it is `done` only between two runs.
//!   Deleting the workflow ends that, and the next night archives it like any other.
//! - **Working on it again un-archives.** An archived task the owner writes to (or the brain sends
//!   a follow-up to) is dispatched like any finished task, planning first, and the dispatch, or the
//!   queue it waits in, clears `archived_at`. Once done, a later night puts it away again.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::core::Orchestrator;

/// The local day (`YYYY-MM-DD`) the archive last ran on.
const LAST_DAY_KEY: &str = "archive.last_day";

/// How long a task must have been finished before a run archives it: an hour.
const GRACE_MINUTES: i32 = 60;

/// When the archive runs: the minute of the day and a zone Postgres knows.
pub type Schedule = (u32, String);

/// Works out when the archive runs. `Ok(None)` when it is off, or when the zone is one Postgres
/// does not know (said once, loudly: a setting to fix, not something a retry mends). `Err` is the
/// database failing, which the caller retries on its next tick. `Europe/Kyiv` falls back to its
/// older spelling `Europe/Kiev`, which is all a Postgres with zone data from before 2022 knows.
pub async fn schedule(o: &Orchestrator) -> Result<Option<Schedule>> {
    let Some(at) = o.cfg.archive.at_minute else {
        return Ok(None);
    };
    let zone = o.cfg.archive.timezone.clone();
    let mut candidates = vec![zone.clone()];
    if zone == "Europe/Kyiv" {
        candidates.push("Europe/Kiev".into());
    }
    for z in candidates {
        let known: bool =
            sqlx::query_scalar("select exists(select 1 from pg_timezone_names where name = $1)")
                .bind(&z)
                .fetch_one(&o.db)
                .await
                .context("checking FLEET_ARCHIVE_TIMEZONE")?;
        if known {
            return Ok(Some((at, z)));
        }
    }
    tracing::error!(
        "the nightly archive is off: FLEET_ARCHIVE_TIMEZONE {zone:?} is not a time zone Postgres knows"
    );
    Ok(None)
}

/// Runs the archive if its time has come today. Returns how many tasks it archived, or `None`
/// when it was not due.
pub async fn run_if_due(o: &Arc<Orchestrator>, schedule: &Schedule) -> Result<Option<u64>> {
    run_at(o, Utc::now(), schedule.0, &schedule.1).await
}

/// The archive decision for a given instant, so a test can say what time it is. `at_minute` is
/// minutes after local midnight in `timezone`.
pub async fn run_at(
    o: &Orchestrator,
    now: DateTime<Utc>,
    at_minute: u32,
    timezone: &str,
) -> Result<Option<u64>> {
    let (minute, day): (i32, String) = sqlx::query_as(
        "select (extract(hour from $1 at time zone $2) * 60 + extract(minute from $1 at time zone $2))::int,
                to_char($1 at time zone $2, 'YYYY-MM-DD')",
    )
    .bind(now)
    .bind(timezone)
    .fetch_one(&o.db)
    .await
    .with_context(|| format!("the local time in {timezone:?}"))?;
    if minute < at_minute as i32 {
        return Ok(None);
    }
    if o.kv_get(LAST_DAY_KEY)
        .await?
        .as_ref()
        .and_then(|v| v.as_str())
        == Some(day.as_str())
    {
        return Ok(None);
    }
    // `updated_at` is the last thing that happened to the task, its finish at the earliest: a
    // task touched since is kept for a later evening rather than put away under the owner's eyes.
    let archived: Vec<String> = sqlx::query_scalar(
        "update tasks set archived_at = $1
         where status = 'done' and not running and archived_at is null
           and updated_at <= $1 - make_interval(mins => $3::int)
           and id <> all($2)
           and not exists (select 1 from workflows w where w.task_id = tasks.id)
         returning id",
    )
    .bind(now)
    .bind(crate::model::BUILTIN_TASKS)
    .bind(GRACE_MINUTES)
    .fetch_all(&o.db)
    .await?;
    o.kv_set(LAST_DAY_KEY, json!(day)).await?;
    if !archived.is_empty() {
        o.audit(
            "orchestrator",
            "task.archive",
            json!({"day": day, "count": archived.len(), "tasks": archived}),
        )
        .await;
        tracing::info!("archived {} done tasks ({day})", archived.len());
    }
    Ok(Some(archived.len() as u64))
}
