//! The nightly archive: done tasks move out of the board's way.
//!
//! Once a day, at `FLEET_ARCHIVE_AT` (21:00) in `FLEET_ARCHIVE_TIMEZONE` (Europe/Kyiv), every task
//! that is `done` becomes `archived`. An archived task is hidden from the board and the default
//! lists, and still found by search, by its link and by the status filter. Nothing else about it
//! changes: its transcript, links and attachments stay, and `updated_at` is left alone, because
//! being put away is not something that happened to the work.
//!
//! - **Time zones.** The local time is worked out by Postgres (`at time zone`), whose zone data
//!   knows Kyiv's summer and winter offsets, so 21:00 stays 21:00 across the DST changes rather
//!   than drifting by an hour twice a year.
//! - **Once a day, late rather than never.** The local day of the last run is kept in `kv`. The
//!   archive runs on the first housekeeping tick at or after 21:00 that is on a new day, so an
//!   orchestrator that was down at 21:00 catches up when it comes back that evening.
//! - **Workflows are never archived.** A task turned into a standing automation keeps its topic
//!   open to be triggered again and belongs on the board; it is `done` only between two runs.
//!   Deleting the workflow ends that, and the next night archives it like any other.
//! - **Running again un-archives.** An archived task the owner writes to (or the brain sends a
//!   follow-up to) is claimed by `tasks::dispatch` like any finished task, which sets its status
//!   to `planning` or `running`. It is work again and is shown as such; once done, the next night
//!   puts it away again.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::core::Orchestrator;

/// The local day (`YYYY-MM-DD`) the archive last ran on.
const LAST_DAY_KEY: &str = "archive.last_day";

/// When the archive runs, checked once at startup: the minute of the day and a zone Postgres
/// knows. `None` when it is off, or when the zone is unknown — then it says so once, loudly,
/// instead of failing every housekeeping tick. `Europe/Kyiv` falls back to its older spelling
/// `Europe/Kiev`, which is all a Postgres with zone data from before 2022 knows.
pub async fn schedule(o: &Orchestrator) -> Option<(u32, String)> {
    let at = o.cfg.archive.at_minute?;
    let zone = o.cfg.archive.timezone.clone();
    let mut candidates = vec![zone.clone()];
    if zone == "Europe/Kyiv" {
        candidates.push("Europe/Kiev".into());
    }
    for z in candidates {
        if sqlx::query("select now() at time zone $1")
            .bind(&z)
            .execute(&o.db)
            .await
            .is_ok()
        {
            return Some((at, z));
        }
    }
    tracing::error!(
        "the nightly archive is off: FLEET_ARCHIVE_TIMEZONE {zone:?} is not a time zone Postgres knows"
    );
    None
}

/// Runs the archive if its time has come today. Returns how many tasks it archived, or `None`
/// when it was not due (or is turned off).
pub async fn run_if_due(o: &Arc<Orchestrator>, schedule: &(u32, String)) -> Result<Option<u64>> {
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
    .with_context(|| format!("FLEET_ARCHIVE_TIMEZONE {timezone:?} is not a time zone Postgres knows"))?;
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
    let archived: Vec<String> = sqlx::query_scalar(
        "update tasks set status = 'archived'
         where status = 'done' and not running
           and id <> all($1)
           and not exists (select 1 from workflows w where w.task_id = tasks.id)
         returning id",
    )
    .bind(crate::model::BUILTIN_TASKS)
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
