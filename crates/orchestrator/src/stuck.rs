//! Runs that stopped making progress: an agent in the middle of a turn that has written nothing
//! for a long time is most likely stuck (a command that never returns, a tool that hangs, a
//! process wedged on something it cannot see), and it holds its node slot and its agent while it
//! is.
//!
//! Housekeeping looks at every running task. When a turn is in progress (something was sent in
//! after the last `result` came out) and nothing has come for `FLEET_RUN_SILENT_WARN_MINUTES`, the
//! owner is told once in the task's topic (and the session page shows it); after
//! `FLEET_RUN_SILENT_STOP_MINUTES` the run is stopped, exactly as the owner's Stop would, and a
//! message resumes the task later.
//!
//! What is not silence:
//! - waiting on the owner: an open plan, question or permission holds a turn open on purpose;
//! - between turns: a session that has answered and waits to be written to is idle, and the node
//!   closes those itself;
//! - a node that is away: its run's output waits in the node's outbox, so silence is counted only
//!   on a connected node and only from when its connection began (which also covers the
//!   orchestrator's own restart, after which every node connects anew);
//! - a task already stopped, whose run is only waiting for its node to confirm the exit.
//!
//! The thresholds are generous on purpose: Claude's own commands time out after ten minutes, so
//! half an hour of nothing is not a long build. A run that keeps writing (the same failing
//! command again and again) is not caught here.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::{
    chat::{Notify, esc},
    core::Orchestrator,
};

/// How many of a task's latest lines in and out are looked at to tell whether a turn is in
/// progress. A turn longer than this (many tool calls since the message that started it) counts as
/// in progress, which it is.
const LOOK_BACK: i64 = 40;

/// Whether `data`, a line out of a session, is the `result` that ends a turn.
fn ends_turn(kind: &str, data: &str) -> bool {
    kind == "out"
        && data.contains("result")
        && serde_json::from_str::<serde_json::Value>(data).is_ok_and(|v| v["type"] == "result")
}

/// Whether a turn is in progress, from a task's latest lines in and out, newest first: something
/// was sent in after the last `result`. Other lines after a `result` (Claude's usage report, a
/// background notice) do not reopen a turn; only a message in does.
fn mid_turn<'a>(newest_first: impl IntoIterator<Item = (&'a str, &'a str)>) -> bool {
    for (kind, data) in newest_first {
        if kind == "in" {
            return true;
        }
        if ends_turn(kind, data) {
            return false;
        }
    }
    // Nothing either way in the window: a long turn, or a run that has not said a word yet.
    true
}

/// Called from housekeeping.
pub async fn check(o: &Arc<Orchestrator>) -> Result<()> {
    let warn = o.cfg.run_silent_warn_minutes;
    let stop_after = o.cfg.run_silent_stop_minutes;
    if warn == 0 && stop_after == 0 {
        return Ok(());
    }
    // Per running task on a connected node: its run, and when it last did anything — its last
    // line, its dispatch (a run that never wrote a line), or the node's connection, whichever is
    // latest. Few tasks run at once, and each lookup walks the `(task_id, id)` index backwards.
    let rows: Vec<(String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "select t.id, t.run_id,
                greatest(coalesce(t.dispatched_at, t.updated_at),
                         coalesce(n.connected_at, '-infinity'),
                         coalesce((select at from task_events
                                   where task_id = t.id and kind in ('in', 'out', 'err')
                                   order by id desc limit 1), '-infinity'))
         from tasks t join nodes n on n.id = t.node_id
         where t.running and n.online and t.status <> 'stopped' and t.id <> all($1)
           and not exists (select 1 from human_items h
                           where h.task_id = t.id and h.state = 'open' and h.blocking)",
    )
    .bind(crate::model::BUILTIN_TASKS)
    .fetch_all(&o.db)
    .await?;
    for (task_id, run_id, last_at) in rows {
        let silent = (Utc::now() - last_at).num_minutes();
        let due_warn = warn > 0 && silent >= warn;
        let due_stop = stop_after > 0 && silent >= stop_after;
        if !(due_warn || due_stop) {
            continue;
        }
        let lines: Vec<(String, String)> = sqlx::query_as(
            "select kind, data from task_events where task_id = $1 and kind in ('in', 'out')
             order by id desc limit $2",
        )
        .bind(&task_id)
        .bind(LOOK_BACK)
        .fetch_all(&o.db)
        .await?;
        // Between turns: idle, not stuck.
        if !mid_turn(lines.iter().map(|(k, d)| (k.as_str(), d.as_str()))) {
            continue;
        }
        let done = if due_stop {
            stop(o, &task_id, run_id.as_deref(), silent).await
        } else {
            tell(o, &task_id, last_at, silent, stop_after).await
        };
        if let Err(e) = done {
            tracing::warn!(task = %task_id, "handling a silent run failed: {e:#}");
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
    o.record(
        task_id,
        "silent",
        &format!("No output for {silent} min in the middle of a turn: it may be stuck"),
    )
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

/// Stops the run as the owner's Stop would (its wakeups too), and says why. Only the run that was
/// judged, and only while it is still running and still quiet: one that exited, or a fresh run
/// dispatched since, is not this one.
async fn stop(
    o: &Arc<Orchestrator>,
    task_id: &str,
    run_id: Option<&str>,
    silent: i64,
) -> Result<()> {
    let Some(task) = o.task(task_id).await? else {
        return Ok(());
    };
    if !task.running || task.status == "stopped" || task.run_id.as_deref() != run_id {
        return Ok(());
    }
    // Once per run: a task whose status a stop leaves as it is (a done task written to again)
    // would otherwise be stopped, and the owner told, on every tick until its exit arrives.
    let stopped_before: bool = sqlx::query_scalar(
        "select exists(select 1 from audit_log where action = 'task.stop_silent'
                       and detail->>'task' = $1 and detail->>'run' = $2)",
    )
    .bind(&task.id)
    .bind(run_id)
    .fetch_one(&o.db)
    .await?;
    if stopped_before {
        return Ok(());
    }
    crate::tasks::stop(o, &task, false).await?;
    o.audit(
        "orchestrator",
        "task.stop_silent",
        json!({"task": task.id, "run": run_id, "silent_minutes": silent}),
    )
    .await;
    o.record(
        &task.id,
        "silent",
        &format!("Stopped after {silent} min without output in the middle of a turn"),
    )
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
        assert!(!ends_turn("in", r#"{"type":"result"}"#));
    }

    #[test]
    fn a_turn_is_open_from_a_message_in_until_its_result() {
        let result = r#"{"type":"result"}"#;
        let tool = r#"{"type":"assistant","message":{"content":[]}}"#;
        let usage = r#"{"type":"rate_limit_event","rate_limit_info":{}}"#;
        // Answered, then Claude's usage report: still idle.
        assert!(!mid_turn([("out", usage), ("out", result), ("in", "go")]));
        // Written to again: a turn is open, whatever came before.
        assert!(mid_turn([("in", "more"), ("out", result)]));
        // Working through tools.
        assert!(mid_turn([("out", tool), ("out", tool), ("in", "go")]));
        // Nothing yet at all.
        assert!(mid_turn([]));
    }
}
