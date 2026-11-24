//! Runs that stopped making progress: an agent in the middle of a turn that has written nothing
//! for a long time is most likely stuck (a command that never returns, a tool that hangs, a
//! process wedged on something it cannot see), and it holds its node slot and its agent while it
//! is.
//!
//! Housekeeping looks at every running task's last line. When a turn is in progress (the last
//! thing in or out of the session is not the `result` that ends a turn) and nothing has come for
//! `FLEET_RUN_SILENT_WARN_MINUTES`, the owner is told once in the task's topic (and the session
//! page shows it); after `FLEET_RUN_SILENT_STOP_MINUTES` the run is stopped, exactly as the
//! owner's Stop would, and a message resumes the task later.
//!
//! Silence is not judged while the task waits on the owner (an open plan, question or
//! permission holds a turn open on purpose), nor between turns (a session that has answered and
//! waits to be written to is idle, not stuck; the node closes those itself). The thresholds are
//! generous on purpose: Claude's own commands time out after ten minutes, so half an hour of
//! nothing is not a long build.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::{
    chat::{Notify, esc},
    core::Orchestrator,
};

/// Whether `data`, the last line out of a session, is the `result` that ends a turn.
fn ends_turn(kind: &str, data: &str) -> bool {
    kind == "out"
        && data.contains("result")
        && serde_json::from_str::<serde_json::Value>(data).is_ok_and(|v| v["type"] == "result")
}

/// Called from housekeeping.
pub async fn check(o: &Arc<Orchestrator>) -> Result<()> {
    let warn = o.cfg.run_silent_warn_minutes;
    let stop_after = o.cfg.run_silent_stop_minutes;
    if warn == 0 && stop_after == 0 {
        return Ok(());
    }
    // Per running task: when its current run last did anything (its start counts, so a run that
    // never wrote a line is judged from its dispatch), and the last line in or out of it, which
    // says whether a turn is in progress. Few tasks run at once, and each lookup walks the
    // `(task_id, id)` index backwards.
    let rows: Vec<(String, DateTime<Utc>, Option<String>, Option<String>)> = sqlx::query_as(
        "select t.id,
                greatest(coalesce(t.dispatched_at, t.updated_at),
                         coalesce((select at from task_events
                                   where task_id = t.id and kind in ('in', 'out', 'err')
                                   order by id desc limit 1), '-infinity')),
                last.kind, last.data
         from tasks t
         left join lateral (select kind, data from task_events
                            where task_id = t.id and kind in ('in', 'out')
                            order by id desc limit 1) last on true
         where t.running and t.id <> all($1)
           and not exists (select 1 from human_items h
                           where h.task_id = t.id and h.state = 'open' and h.blocking)",
    )
    .bind(crate::model::BUILTIN_TASKS)
    .fetch_all(&o.db)
    .await?;
    for (task_id, last_at, kind, data) in rows {
        // Between turns: idle, not stuck.
        if let (Some(kind), Some(data)) = (&kind, &data)
            && ends_turn(kind, data)
        {
            continue;
        }
        let silent = (Utc::now() - last_at).num_minutes();
        if stop_after > 0 && silent >= stop_after {
            if let Err(e) = stop(o, &task_id, silent).await {
                tracing::warn!(task = %task_id, "stopping a silent run failed: {e:#}");
            }
        } else if warn > 0
            && silent >= warn
            && let Err(e) = tell(o, &task_id, last_at, silent, stop_after).await
        {
            tracing::warn!(task = %task_id, "warning about a silent run failed: {e:#}");
        }
    }
    Ok(())
}

/// Tells the owner once per silence: a run that wakes up and goes quiet again is told again.
async fn tell(
    o: &Arc<Orchestrator>,
    task_id: &str,
    last_at: DateTime<Utc>,
    silent: i64,
    stop_after: i64,
) -> Result<()> {
    let told: bool = sqlx::query_scalar(
        "select exists(select 1 from task_events where task_id = $1 and kind = 'silent' and at >= $2)",
    )
    .bind(task_id)
    .bind(last_at)
    .fetch_one(&o.db)
    .await?;
    if told {
        return Ok(());
    }
    let Some(task) = o.task(task_id).await? else {
        return Ok(());
    };
    sqlx::query("insert into task_events (task_id, kind, data) values ($1, 'silent', $2)")
        .bind(task_id)
        .bind(format!(
            "No output for {silent} min in the middle of a turn: it may be stuck"
        ))
        .execute(&o.db)
        .await?;
    let then = if stop_after > 0 {
        format!(
            " It is stopped after {stop_after} min of silence; stop it sooner from its page if it is."
        )
    } else {
        " Stop it from its page if it is.".to_owned()
    };
    o.post_task(
        &task,
        &format!(
            "⚠️ <b>{}</b>: no output from its agent for {silent} min in the middle of a turn. It may be stuck (a command that never returns, a tool that hangs).{}",
            esc(&task.key()),
            esc(&then)
        ),
        Notify::Sound,
    )
    .await;
    Ok(())
}

/// Stops the run as the owner's Stop would (its wakeups too), and says why.
async fn stop(o: &Arc<Orchestrator>, task_id: &str, silent: i64) -> Result<()> {
    let Some(task) = o.task(task_id).await? else {
        return Ok(());
    };
    crate::tasks::stop(o, &task, false).await?;
    o.audit(
        "orchestrator",
        "task.stop_silent",
        json!({"task": task.id, "silent_minutes": silent}),
    )
    .await;
    sqlx::query("insert into task_events (task_id, kind, data) values ($1, 'silent', $2)")
        .bind(&task.id)
        .bind(format!(
            "Stopped after {silent} min without output in the middle of a turn"
        ))
        .execute(&o.db)
        .await?;
    o.post_task(
        &task,
        &format!(
            "🛑 <b>{}</b> stopped: no output from its agent for {silent} min in the middle of a turn. Write to it to carry on.",
            esc(&task.key())
        ),
        Notify::Sound,
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_result_ends_a_turn() {
        assert!(ends_turn("out", r#"{"type":"result","subtype":"success"}"#));
        // The e2e's fake writes with spaces after colons; the real CLI does not.
        assert!(ends_turn(
            "out",
            r#"{"type": "result", "subtype": "success"}"#
        ));
        assert!(!ends_turn(
            "out",
            r#"{"type":"assistant","message":{"content":[]}}"#
        ));
        // A tool's output that mentions a result is still mid-turn.
        assert!(!ends_turn(
            "out",
            r#"{"type":"user","message":{"content":"result: ok"}}"#
        ));
        // What was sent in starts a turn.
        assert!(!ends_turn("in", r#"{"type":"result"}"#));
    }
}
