//! Claude's usage limits, per node: what Claude says about them, holding new runs on a node that
//! is out of usage, and carrying on the work a limit cut off once it resets.
//!
//! Every node runs Claude under its own login, so its limits are its own. Claude reports them in
//! a `rate_limit_event` line (`rate_limit_info`: a `status` of `allowed`, `allowed_warning` or
//! `rejected`, when the window resets, and how much of the 5-hour and weekly windows is used),
//! which the node forwards like any other line of a run. The orchestrator keeps the last one per
//! node (shown on the Nodes page), and on `rejected`:
//!
//! - holds new task runs on that node until the reset (`held`), the way a full disk does: they
//!   would only fail there. The fleet's own sessions are never held, so the owner can still reach
//!   the fleet, and the brain says what it can;
//! - tells the owner once per limit, not once per run it cuts off;
//! - wakes each task whose turn it cut off a little after the reset (an ordinary wakeup, by the
//!   orchestrator), so the work carries on instead of waiting for someone to notice.
//!
//! A turn can also end in a usage-limit error with no `rejected` event before it (an older CLI,
//! or a limit hit between events): its `result` line says so in words, and is treated the same.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use fleet_net::EndpointId;
use serde_json::{Value, json};

use crate::{
    chat::{From, Notify, esc},
    core::Orchestrator,
    model::{NodeRow, TaskRow},
};

/// How long a limit is assumed to last when Claude says it was hit but not until when.
const UNKNOWN_RESET_MINUTES: i64 = 60;
/// How long after the reset a task the limit cut off is woken: the window has reopened by then,
/// and the clocks of the node and the orchestrator need not agree to the second.
const RESUME_AFTER_RESET_SECS: i64 = 120;

/// Whether `node` is out of Claude usage right now, so no new task run should start there.
pub fn held(node: &NodeRow) -> bool {
    node.claude_limited_until.is_some_and(|t| t > Utc::now())
}

/// What the web UI shows of a node's usage: how much of each window is used (0..1), and until
/// when it is out, if it is.
pub fn json(n: &NodeRow) -> Value {
    let info = n.claude_usage.as_ref().map(|j| &j.0);
    let used =
        |window: &str| info.and_then(|i| i["unifiedWindows"][window]["utilization"].as_f64());
    json!({
        "at": n.claude_usage_at,
        "status": info.and_then(|i| i["status"].as_str()),
        "five_hour": used("five_hour"),
        "seven_day": used("seven_day"),
        "limited_until": n.claude_limited_until.filter(|t| *t > Utc::now()),
    })
}

/// A `rate_limit_event` line of a run on `peer`. `task` is the run's task while the run is its
/// current one (a replaced run's cut-off is nobody's to resume).
pub async fn event(
    o: &Arc<Orchestrator>,
    peer: EndpointId,
    task: Option<&TaskRow>,
    line: &Value,
) -> Result<()> {
    let info = &line["rate_limit_info"];
    if !info.is_object() {
        return Ok(());
    }
    let node_id = peer.to_string();
    sqlx::query("update nodes set claude_usage = $2, claude_usage_at = now() where id = $1")
        .bind(&node_id)
        .bind(sqlx::types::Json(info))
        .execute(&o.db)
        .await?;
    if info["status"].as_str() == Some("rejected") {
        let until = reset_of(info).unwrap_or_else(unknown_reset);
        hit(o, &node_id, until, info["rateLimitType"].as_str(), task).await?;
    }
    Ok(())
}

/// The `result` line that ends a turn: when it failed because the usage limit was reached, the
/// same as a `rejected` event.
pub async fn turn_ended(
    o: &Arc<Orchestrator>,
    peer: EndpointId,
    task: Option<&TaskRow>,
    line: &Value,
) -> Result<()> {
    if line["is_error"] != json!(true) || !is_limit_message(line["result"].as_str().unwrap_or("")) {
        return Ok(());
    }
    let node_id = peer.to_string();
    let known: Option<(Option<sqlx::types::Json<Value>>, Option<DateTime<Utc>>)> =
        sqlx::query_as("select claude_usage, claude_limited_until from nodes where id = $1")
            .bind(&node_id)
            .fetch_optional(&o.db)
            .await?;
    let Some((usage, limited)) = known else {
        return Ok(());
    };
    // The reset Claude gave last, if it is still ahead; otherwise a guess.
    let until = limited
        .filter(|t| *t > Utc::now())
        .or_else(|| usage.and_then(|u| reset_of(&u.0)))
        .unwrap_or_else(unknown_reset);
    hit(o, &node_id, until, None, task).await
}

/// Whether a failed turn's text says the usage limit ended it. Claude words it as "Claude AI
/// usage limit reached" or "You've hit your limit · resets 3pm"; an API rate limit (a 429, which
/// Claude retries on its own) is not this and must not hold a node for an hour.
fn is_limit_message(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("usage limit") || (t.contains("hit your") && t.contains("limit"))
}

/// When the limit in `info` resets, if Claude said and it is still ahead: the event's own
/// `resetsAt`, else the latest of the windows it rejected.
fn reset_of(info: &Value) -> Option<DateTime<Utc>> {
    let at = |v: &Value| v.as_i64().and_then(|s| DateTime::from_timestamp(s, 0));
    let own = at(&info["resetsAt"]);
    let windows = info["unifiedWindows"].as_object().and_then(|w| {
        w.values()
            .filter(|v| v["status"].as_str() == Some("rejected"))
            .filter_map(|v| at(&v["resetsAt"]))
            .max()
    });
    own.or(windows).filter(|t| *t > Utc::now())
}

fn unknown_reset() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::minutes(UNKNOWN_RESET_MINUTES)
}

/// `node_id` is out of usage until `until`: hold it, tell the owner if this is news, and have the
/// task whose turn was cut off carry on after the reset.
async fn hit(
    o: &Arc<Orchestrator>,
    node_id: &str,
    until: DateTime<Utc>,
    kind: Option<&str>,
    task: Option<&TaskRow>,
) -> Result<()> {
    // News only when the node was not already held: one limit cuts off every run on the node,
    // and each says so. Decided by the update itself, so two at once still make one notice.
    let fresh: Option<String> = sqlx::query_scalar(
        "update nodes set claude_limited_until = $2
         where id = $1 and (claude_limited_until is null or claude_limited_until <= now())
         returning name",
    )
    .bind(node_id)
    .bind(until)
    .fetch_optional(&o.db)
    .await?;
    if let Some(name) = fresh {
        tracing::info!(node = %name, %until, "Claude usage limit reached");
        o.audit(
            "orchestrator",
            "claude.limit",
            json!({"node": name, "until": until, "window": kind}),
        )
        .await;
        let window = match kind {
            Some("five_hour") => " (the 5-hour window)",
            Some("seven_day") => " (the weekly window)",
            Some("seven_day_opus") => " (the weekly Opus window)",
            Some("seven_day_sonnet") => " (the weekly Sonnet window)",
            _ => "",
        };
        let html = format!(
            "⏸ <b>{}</b> reached its Claude usage limit{window}. New task runs there wait until {}; work it cut off carries on then.",
            esc(&name),
            esc(&local_time(o, until).await)
        );
        if let Err(e) = o
            .chat
            .send(From::Orchestrator, None, &html, vec![], None, Notify::Sound)
            .await
        {
            tracing::warn!("telling the owner about {name}'s usage limit failed: {e:#}");
        }
    } else {
        // Already held: a later reset (another window ran out too) moves the hold, never an
        // earlier one.
        sqlx::query(
            "update nodes set claude_limited_until = greatest(claude_limited_until, $2) where id = $1",
        )
        .bind(node_id)
        .bind(until)
        .execute(&o.db)
        .await?;
    }
    if let Some(task) = task.filter(|t| !crate::model::is_builtin_task(&t.id)) {
        resume_later(o, task, until).await?;
    }
    Ok(())
}

/// What a task cut off by the limit is told when it is woken.
const RESUME_NOTE: &str = "Your last turn was cut off by the Claude usage limit on your machine, \
     which has reset now. Carry on from where you stopped: check what you had already done first, \
     since nothing after the cut-off happened.";

/// Wakes `task` a little after `until`, through an ordinary wakeup armed by the orchestrator.
/// One per task: a limit that cuts off several turns of one task in a row is still one cut-off.
async fn resume_later(o: &Orchestrator, task: &TaskRow, until: DateTime<Utc>) -> Result<()> {
    let at = until + chrono::Duration::seconds(RESUME_AFTER_RESET_SECS);
    let moved = sqlx::query(
        "update wakeups set fires_at = greatest(fires_at, $2)
         where task_id = $1 and created_by = $3 and state = 'armed' and note = $4",
    )
    .bind(&task.id)
    .bind(at)
    .bind(crate::wakeups::BY_ORCHESTRATOR)
    .bind(RESUME_NOTE)
    .execute(&o.db)
    .await?
    .rows_affected();
    if moved == 0 {
        sqlx::query(
            "insert into wakeups (task_id, note, kind, fires_at, state, created_by, expires_at)
             values ($1, $2, 'timer', $3, 'armed', $4, $3 + interval '1 day')",
        )
        .bind(&task.id)
        .bind(RESUME_NOTE)
        .bind(at)
        .bind(crate::wakeups::BY_ORCHESTRATOR)
        .execute(&o.db)
        .await?;
    }
    Ok(())
}

/// What a turn cost, from the `result` line that ends it, added to its run. Claude counts
/// `total_cost_usd` up across a process's turns (verified with 2.1.331: 0.0067, then 0.0113 after a
/// second turn), so the run keeps the largest it has seen; `usage` is the turn's own tokens, so
/// those are added. The cost is at API prices, which on a subscription is a measure of use
/// rather than a bill.
pub async fn record_turn(o: &Orchestrator, run_id: &str, line: &Value) -> Result<()> {
    let u = &line["usage"];
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    sqlx::query(
        "update runs set
            cost_usd = greatest(coalesce(cost_usd, 0), coalesce($2, 0)),
            input_tokens = coalesce(input_tokens, 0) + $3,
            output_tokens = coalesce(output_tokens, 0) + $4,
            cache_read_tokens = coalesce(cache_read_tokens, 0) + $5,
            cache_write_tokens = coalesce(cache_write_tokens, 0) + $6,
            turns = coalesce(turns, 0) + 1
         where run_id = $1",
    )
    .bind(run_id)
    .bind(line["total_cost_usd"].as_f64())
    .bind(n("input_tokens"))
    .bind(n("output_tokens"))
    .bind(n("cache_read_input_tokens"))
    .bind(n("cache_creation_input_tokens"))
    .execute(&o.db)
    .await?;
    Ok(())
}

/// What a task's runs cost all together, for its page and its board card: `None` when no run
/// has reported a turn yet.
pub fn cost_json(row: Option<&(String, f64, i64, i64)>) -> Value {
    match row {
        Some((_, usd, tokens, turns)) => json!({"usd": usd, "tokens": tokens, "turns": turns}),
        None => Value::Null,
    }
}

/// Per task of `ids`: its runs' cost, tokens (all four kinds) and turns.
pub async fn cost_of_tasks(
    o: &Orchestrator,
    ids: &[String],
) -> Result<Vec<(String, f64, i64, i64)>> {
    Ok(sqlx::query_as(
        "select task_id, coalesce(sum(cost_usd), 0)::float8,
                coalesce(sum(coalesce(input_tokens, 0) + coalesce(output_tokens, 0)
                             + coalesce(cache_read_tokens, 0) + coalesce(cache_write_tokens, 0)), 0)::bigint,
                coalesce(sum(turns), 0)::bigint
         from runs where task_id = any($1) and turns is not null group by task_id",
    )
    .bind(ids)
    .fetch_all(&o.db)
    .await?)
}

/// Called from housekeeping: a node whose limit has reset is no longer held, the owner hears so
/// (silently), and queued work is looked at again.
pub async fn release_lapsed(o: &Arc<Orchestrator>) -> Result<()> {
    let names: Vec<String> = sqlx::query_scalar(
        "update nodes set claude_limited_until = null
         where claude_limited_until <= now() returning name",
    )
    .fetch_all(&o.db)
    .await?;
    if names.is_empty() {
        return Ok(());
    }
    o.audit(
        "orchestrator",
        "claude.limit_reset",
        json!({"nodes": names}),
    )
    .await;
    let html = format!(
        "▶️ <b>{}</b>: Claude usage is back. Queued work starts.",
        esc(&names.join(", "))
    );
    if let Err(e) = o
        .chat
        .send(
            From::Orchestrator,
            None,
            &html,
            vec![],
            None,
            Notify::Silent,
        )
        .await
    {
        tracing::warn!("telling the owner a usage limit reset failed: {e:#}");
    }
    crate::tasks::dispatch_queued(o).await;
    Ok(())
}

/// `at` as the owner reads it: a time of day in their zone, with the day when it is not today.
async fn local_time(o: &Orchestrator, at: DateTime<Utc>) -> String {
    let tz = &o.cfg.secretary.timezone;
    let shown: Option<String> = sqlx::query_scalar(
        "select case when ($1 at time zone $2)::date = (now() at time zone $2)::date
                     then to_char($1 at time zone $2, 'HH24:MI')
                     else to_char($1 at time zone $2, 'Dy DD Mon HH24:MI') end",
    )
    .bind(at)
    .bind(tz)
    .fetch_optional(&o.db)
    .await
    .ok()
    .flatten();
    match shown {
        Some(s) if tz == "UTC" => format!("{s} UTC"),
        Some(s) => s,
        None => at.format("%Y-%m-%d %H:%M UTC").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_usage_limit_is_a_limit() {
        assert!(is_limit_message("Claude AI usage limit reached|1795483800"));
        assert!(is_limit_message(
            "You've hit your limit · resets 3pm (Europe/Kyiv)"
        ));
        assert!(is_limit_message("You've hit your 5-hour limit"));
        // Claude retries an API rate limit on its own; it says nothing about the node's usage.
        assert!(!is_limit_message(
            "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}"
        ));
        assert!(!is_limit_message("Execution error"));
    }

    #[test]
    fn a_reset_comes_from_the_event_or_the_window_it_rejected() {
        let soon = (Utc::now() + chrono::Duration::hours(2)).timestamp();
        let later = (Utc::now() + chrono::Duration::days(3)).timestamp();
        let own = json!({"status": "rejected", "resetsAt": soon});
        assert_eq!(reset_of(&own).unwrap().timestamp(), soon);
        let windows = json!({"status": "rejected", "unifiedWindows": {
            "five_hour": {"status": "allowed", "resetsAt": soon},
            "seven_day": {"status": "rejected", "resetsAt": later},
        }});
        assert_eq!(reset_of(&windows).unwrap().timestamp(), later);
        // A reset in the past is no reset: the caller guesses instead of holding nothing.
        assert!(reset_of(&json!({"resetsAt": 1_000_000})).is_none());
        assert!(reset_of(&json!({})).is_none());
    }
}
