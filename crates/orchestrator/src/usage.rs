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

/// A `rate_limit_event` line of a run on `peer`: kept as the node's latest word on its usage.
/// `rejected` holds the node (unless Claude is drawing on extra usage, so work still runs); an
/// all-clear lifts a hold early (extra usage bought, the node's login changed). Resuming a task is
/// left to the turn's own end ([`turn_ended`]): only a turn that actually failed was cut off.
pub async fn event(o: &Arc<Orchestrator>, peer: EndpointId, line: &Value) -> Result<()> {
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
    let overage = info["isUsingOverage"] == json!(true);
    match info["status"].as_str() {
        Some("rejected") if !overage => {
            let until = reset_of(info).unwrap_or_else(unknown_reset);
            hit(o, &node_id, until, info["rateLimitType"].as_str(), None).await?;
        }
        Some("allowed" | "allowed_warning") if !any_rejected(info) => {
            lift(o, &node_id).await?;
        }
        _ => {}
    }
    Ok(())
}

/// The `result` line that ends a turn. A turn that failed because the usage limit was reached
/// (its text says so, or it failed while the node is known to be out of usage) holds the node,
/// and the task carries on after the reset.
pub async fn turn_ended(
    o: &Arc<Orchestrator>,
    peer: EndpointId,
    task: Option<&TaskRow>,
    line: &Value,
) -> Result<()> {
    if line["is_error"] != json!(true) {
        return Ok(());
    }
    let text = line["result"].as_str().unwrap_or("");
    let node_id = peer.to_string();
    let known: Option<(Option<sqlx::types::Json<Value>>, Option<DateTime<Utc>>)> =
        sqlx::query_as("select claude_usage, claude_limited_until from nodes where id = $1")
            .bind(&node_id)
            .fetch_optional(&o.db)
            .await?;
    let Some((usage, limited)) = known else {
        return Ok(());
    };
    let limited = limited.filter(|t| *t > Utc::now());
    if !is_limit_message(text) && limited.is_none() {
        return Ok(());
    }
    // The reset the message names, else the hold already in place, else the one Claude gave
    // last, else a guess.
    let until = reset_in_message(text)
        .or(limited)
        .or_else(|| usage.and_then(|u| reset_of(&u.0)))
        .unwrap_or_else(unknown_reset);
    hit(o, &node_id, until, None, task).await
}

/// Whether a failed turn's text says the subscription's usage limit ended it. Claude words it as
/// "Claude AI usage limit reached|<reset>" or "You've hit your limit · resets 3pm". Neither an API
/// rate limit (a 429, which Claude retries on its own) nor an API key's workspace spend limit is
/// this, and neither may hold a node.
fn is_limit_message(text: &str) -> bool {
    let t = text.to_lowercase();
    if t.contains("workspace") || t.contains("api usage limit") {
        return false;
    }
    t.contains("usage limit reached") || (t.contains("hit your") && t.contains("limit"))
}

/// The reset some versions of Claude put in the message itself: `...usage limit reached|<unix>`.
fn reset_in_message(text: &str) -> Option<DateTime<Utc>> {
    let (_, tail) = text.rsplit_once('|')?;
    let secs: i64 = tail.trim().parse().ok()?;
    DateTime::from_timestamp(secs, 0).filter(|t| *t > Utc::now())
}

/// When the limit in `info` resets, if Claude said and it is still ahead: the latest of the
/// event's own `resetsAt` and every window it rejected, since the node is out until all of them
/// have reset.
fn reset_of(info: &Value) -> Option<DateTime<Utc>> {
    let at = |v: &Value| v.as_i64().and_then(|s| DateTime::from_timestamp(s, 0));
    let windows = info["unifiedWindows"]
        .as_object()
        .into_iter()
        .flat_map(|w| {
            w.values()
                .filter(|v| v["status"].as_str() == Some("rejected"))
                .filter_map(|v| at(&v["resetsAt"]))
        });
    at(&info["resetsAt"])
        .into_iter()
        .chain(windows)
        .max()
        .filter(|t| *t > Utc::now())
}

/// Whether any window in `info` is still rejected, which an overall `allowed` does not undo.
fn any_rejected(info: &Value) -> bool {
    info["unifiedWindows"]
        .as_object()
        .is_some_and(|w| w.values().any(|v| v["status"].as_str() == Some("rejected")))
}

fn unknown_reset() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::minutes(UNKNOWN_RESET_MINUTES)
}

/// Posts to the owner without holding up the caller: [`event`] and [`turn_ended`] run inside the
/// loop that handles every node's events, which must not wait on Telegram.
fn tell_owner(o: &Arc<Orchestrator>, html: String, notify: Notify) {
    let o = o.clone();
    tokio::spawn(async move {
        if let Err(e) = o
            .chat
            .send(From::Orchestrator, None, &html, vec![], None, notify)
            .await
        {
            tracing::warn!("telling the owner about Claude usage failed: {e:#}");
        }
    });
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
        tell_owner(o, html, Notify::Sound);
    } else {
        // Already held: a later reset (another window ran out too) moves the hold, never an
        // earlier one; the owner hears quietly when it moves by more than a few minutes, since
        // the time they were given is then wrong.
        let old: Option<DateTime<Utc>> =
            sqlx::query_scalar("select claude_limited_until from nodes where id = $1")
                .bind(node_id)
                .fetch_optional(&o.db)
                .await?
                .flatten();
        let name: Option<String> = sqlx::query_scalar(
            "update nodes set claude_limited_until = greatest(claude_limited_until, $2)
             where id = $1 returning name",
        )
        .bind(node_id)
        .bind(until)
        .fetch_optional(&o.db)
        .await?;
        if let (Some(name), Some(old)) = (name, old)
            && until > old + chrono::Duration::minutes(10)
        {
            let html = format!(
                "⏸ <b>{}</b>: Claude usage is out for longer, until {}.",
                esc(&name),
                esc(&local_time(o, until).await)
            );
            tell_owner(o, html, Notify::Silent);
        }
    }
    if let Some(task) = task.filter(|t| !crate::model::is_builtin_task(&t.id)) {
        resume_later(o, task, until).await?;
    }
    Ok(())
}

/// Lifts a hold before its time, when Claude says the node may work again.
async fn lift(o: &Arc<Orchestrator>, node_id: &str) -> Result<()> {
    let name: Option<String> = sqlx::query_scalar(
        "update nodes set claude_limited_until = null
         where id = $1 and claude_limited_until > now() returning name",
    )
    .bind(node_id)
    .fetch_optional(&o.db)
    .await?;
    if let Some(name) = name {
        o.audit(
            "orchestrator",
            "claude.limit_reset",
            json!({"nodes": [name]}),
        )
        .await;
        tell_owner(
            o,
            format!("▶️ <b>{}</b>: Claude usage is back.", esc(&name)),
            Notify::Silent,
        );
        crate::tasks::dispatch_queued(o).await;
    }
    Ok(())
}

/// What a task cut off by the limit is told when it is woken.
const RESUME_NOTE: &str = "Your last turn was cut off by the Claude usage limit on your machine, \
     which has reset now. Carry on from where you stopped: check what you had already done first, \
     since nothing after the cut-off happened.";

/// Wakes `task` a little after `until`, through an ordinary wakeup armed by the orchestrator.
/// One per task: a limit that cuts off several turns of one task in a row is still one cut-off.
/// Not for a task that is over or stopped by now (read afresh: the row the caller holds may
/// predate a stop).
async fn resume_later(o: &Orchestrator, task: &TaskRow, until: DateTime<Utc>) -> Result<()> {
    let Some(task) = o.task(&task.id).await? else {
        return Ok(());
    };
    if crate::tasks::is_finished(&task) {
        return Ok(());
    }
    let at = until + chrono::Duration::seconds(RESUME_AFTER_RESET_SECS);
    let moved = sqlx::query(
        "update wakeups set fires_at = greatest(fires_at, $2),
                            expires_at = greatest(expires_at, $2 + interval '1 day')
         where task_id = $1 and created_by = $3 and state = 'armed'",
    )
    .bind(&task.id)
    .bind(at)
    .bind(crate::wakeups::BY_ORCHESTRATOR)
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
/// `total_cost_usd` up across a session's turns, and carries it over when a session is resumed in
/// a new process (verified with 2.1.331: 0.0067, then 0.0113 after a second turn, then 0.0118 on
/// the first turn after `--resume`). So a run keeps the largest total it has seen, which is its
/// session's total so far, and a task's cost counts each session once ([`cost_of_tasks`]).
/// `usage` is the turn's own tokens, so those are added. A line without a cost changes no cost.
/// The cost is at API prices, which on a subscription is a measure of use rather than a bill.
pub async fn record_turn(o: &Orchestrator, run_id: &str, line: &Value) -> Result<()> {
    let u = &line["usage"];
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    sqlx::query(
        "update runs set
            cost_usd = case when $2::float8 is null then cost_usd
                            else greatest(coalesce(cost_usd, 0), $2) end,
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

/// What one run cost on its own, as SQL over a `runs` row `r`: its session's total so far, less
/// what the session had cost before this run began (a resumed session carries its total over).
pub const RUN_COST_SQL: &str = "(r.cost_usd - coalesce((select max(p.cost_usd) from runs p
      where p.session_id = r.session_id and p.started_at < r.started_at), 0))";

/// What a task's runs cost all together, for its page and its board card: `None` when no run
/// has reported a turn yet.
pub fn cost_json(row: Option<&(String, f64, i64, i64)>) -> Value {
    match row {
        Some((_, usd, tokens, turns)) => json!({"usd": usd, "tokens": tokens, "turns": turns}),
        None => Value::Null,
    }
}

/// Per task of `ids`: its cost (each Claude session counted once, at its largest total, since a
/// resumed session carries its total into the next run), tokens (all four kinds, per turn, so
/// added up) and turns.
pub async fn cost_of_tasks(
    o: &Orchestrator,
    ids: &[String],
) -> Result<Vec<(String, f64, i64, i64)>> {
    Ok(sqlx::query_as(
        "select task_id, coalesce(sum(usd), 0)::float8, coalesce(sum(tokens), 0)::bigint,
                coalesce(sum(turns), 0)::bigint
         from (select task_id, max(cost_usd) as usd,
                      sum(coalesce(input_tokens, 0) + coalesce(output_tokens, 0)
                          + coalesce(cache_read_tokens, 0) + coalesce(cache_write_tokens, 0)) as tokens,
                      sum(turns) as turns
               from runs where task_id = any($1) and turns is not null
               group by task_id, coalesce(session_id, run_id)) per_session
         group by task_id",
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
    tell_owner(
        o,
        format!(
            "▶️ <b>{}</b>: Claude usage is back.",
            esc(&names.join(", "))
        ),
        Notify::Silent,
    );
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
    fn a_message_may_carry_its_reset() {
        let soon = (Utc::now() + chrono::Duration::hours(2)).timestamp();
        let at = reset_in_message(&format!("Claude AI usage limit reached|{soon}")).unwrap();
        assert_eq!(at.timestamp(), soon);
        assert!(reset_in_message("You've hit your limit · resets 3pm").is_none());
        assert!(reset_in_message("Claude AI usage limit reached|1000").is_none());
    }

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
        // An API key's spend limit is the owner's setting, not the subscription's window.
        assert!(!is_limit_message(
            "You have reached your specified workspace API usage limits"
        ));
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
        // The event's own reset is the 5-hour one, but the weekly window is out too: the node is
        // out until both have reset.
        let both = json!({"status": "rejected", "resetsAt": soon, "unifiedWindows": {
            "five_hour": {"status": "rejected", "resetsAt": soon},
            "seven_day": {"status": "rejected", "resetsAt": later},
        }});
        assert_eq!(reset_of(&both).unwrap().timestamp(), later);
        assert!(any_rejected(&both));
        assert!(!any_rejected(
            &json!({"unifiedWindows": {"five_hour": {"status": "allowed"}}})
        ));
        // A reset in the past is no reset: the caller guesses instead of holding nothing.
        assert!(reset_of(&json!({"resetsAt": 1_000_000})).is_none());
        assert!(reset_of(&json!({})).is_none());
    }
}
