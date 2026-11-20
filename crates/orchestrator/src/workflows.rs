//! Workflows: a task whose topic is kept open so it can be triggered again, on a schedule or
//! whenever the owner writes into it.
//!
//! A workflow is not a new kind of session or a new approval mechanism: it is an ordinary task
//! that went through the usual plan-mode approval, except that finishing it with
//! `workflow_finish` (`tools.rs`) instead of `complete` leaves its topic open and adds this
//! module's control card (schedule presets, Pause/Resume, Delete). Writing into that topic
//! already resumes the task through the existing follow-up machinery (`tasks::deliver_as`); the
//! only new mechanism here is the periodic tick ([`run_due`]), checked from the orchestrator's
//! housekeeping loop.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::{
    chat::{Button, From, Keyboard, Notify},
    core::Orchestrator,
    model::{TaskRow, WorkflowRow},
    plan_gate::{self, Gate},
    tasks,
};

/// The schedules offered on the control card: `(code stored in the database, button label)`.
/// Deliberately a short fixed list, not a cron syntax (see `docs/plan.md`).
pub const SCHEDULES: [(&str, &str); 5] = [
    ("every:15m", "Every 15 min"),
    ("every:30m", "Every 30 min"),
    ("every:1h", "Every hour"),
    ("every:6h", "Every 6 hours"),
    ("daily:09:00", "Daily at 09:00"),
];

/// Whether `schedule` is one `workflow_finish`/a card button may set.
pub fn validate_schedule(schedule: &str) -> Result<()> {
    ensure!(
        SCHEDULES.iter().any(|(code, _)| *code == schedule),
        "{schedule:?} is not a schedule this fleet offers; omit it for on-demand only"
    );
    Ok(())
}

/// How a schedule and enabled/disabled reads on the card and in a button's confirmation.
fn describe(schedule: Option<&str>, enabled: bool) -> String {
    match (schedule, enabled) {
        (None, _) => "On-demand only: write into this topic to run it.".into(),
        (Some(_), false) => "Paused: it will not trigger on its own until resumed.".into(),
        (Some(s), true) => {
            let label = SCHEDULES
                .iter()
                .find(|(code, _)| *code == s)
                .map_or(s, |(_, l)| l);
            format!("Runs {label} (lower case: also whenever you write into this topic).")
        }
    }
}

fn card_html(schedule: Option<&str>, enabled: bool, gate: Gate) -> String {
    format!(
        "⚙️ <b>Workflow settings</b>\n{}\n{}",
        crate::chat::esc(&describe(schedule, enabled)),
        crate::chat::esc(plan_gate::describe(gate))
    )
}

fn card_keyboard(task_id: &str, enabled: bool, gate: Gate) -> Keyboard {
    let row = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(label, action)| Button::callback(*label, format!("wf|{task_id}|{action}")))
            .collect()
    };
    vec![
        row(&[
            ("15 min", "p15"),
            ("30 min", "p30"),
            ("Hourly", "p1h"),
            ("6 hourly", "p6h"),
        ]),
        row(&[("Daily 09:00", "pd9"), ("On-demand only", "ondemand")]),
        row(&[
            (
                if enabled {
                    "⏸ Pause"
                } else {
                    "▶️ Resume"
                },
                if enabled { "pause" } else { "resume" },
            ),
            ("🗑 Delete", "del"),
        ]),
        // Who approves its plans (`plan_gate`): the owner's tap here is the only way to change it.
        row(&[match gate {
            Gate::Auto => ("🔒 Ask me for plans", "gate_owner"),
            Gate::Owner => ("🔓 Approve plans automatically", "gate_auto"),
        }]),
    ]
}

/// Turns `task` into a workflow: records its trigger settings and posts the control card into
/// its (still open) topic. Called once, right after `workflow_finish` marks the task done.
pub async fn start(o: &Arc<Orchestrator>, task: &TaskRow, schedule: Option<&str>) -> Result<()> {
    if let Some(s) = schedule {
        validate_schedule(s)?;
    }
    // A new workflow's plans wait for the owner (the column's default) until they tap 🔓 on the
    // card: the agent calls `workflow_finish` itself, so setting one up must not open the gate.
    // Set up again (a second `workflow_finish`), it keeps the plan gate the owner chose: only
    // the owner changes that, never the agent finishing.
    let gate: String = sqlx::query_scalar(
        "insert into workflows (task_id, schedule, enabled) values ($1, $2, true)
         on conflict (task_id) do update set schedule = excluded.schedule, enabled = true
         returning plan_gate",
    )
    .bind(&task.id)
    .bind(schedule)
    .fetch_one(&o.db)
    .await?;
    let gate = Gate::parse(&gate)?;
    let Some(topic) = task.topic_id else {
        return Ok(());
    };
    let msg = o
        .chat
        .send(
            From::Orchestrator,
            Some(topic),
            &card_html(schedule, true, gate),
            card_keyboard(&task.id, true, gate),
            None,
            Notify::Silent,
        )
        .await?;
    sqlx::query("update workflows set tg_msg = $2 where task_id = $1")
        .bind(&task.id)
        .bind(msg)
        .execute(&o.db)
        .await?;
    Ok(())
}

/// Every workflow, newest first: for the brain and the secretary to find one by id when the
/// owner names it (e.g. so a todo can link to it, or a request can `send-to-task` it instead of
/// creating a new one).
pub async fn list(o: &Orchestrator) -> Result<String> {
    let rows: Vec<(String, String, Option<String>, bool, String)> = sqlx::query_as(
        "select w.task_id, t.title, w.schedule, w.enabled, w.plan_gate
         from workflows w join tasks t on t.id = w.task_id
         order by w.created_at desc",
    )
    .fetch_all(&o.db)
    .await?;
    let out: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(task_id, title, schedule, enabled, plan_gate)| {
            json!({
                "task": task_id,
                "title": title,
                "schedule": schedule,
                "enabled": enabled,
                // Read-only here: the owner sets it on the workflow's card.
                "plan_gate": plan_gate,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&out)?)
}

/// Whether a button's callback data is a workflow's (`wf|<task id>|<action>`).
pub fn is_button(data: &str) -> bool {
    data.starts_with("wf|")
}

/// The owner tapped a workflow's control card. Returns the short confirmation shown to them.
pub async fn button(o: &Arc<Orchestrator>, data: &str) -> Result<String> {
    let mut parts = data.splitn(3, '|');
    let (_, task_id, action) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    ensure!(!task_id.is_empty(), "bad button");
    let row: Option<WorkflowRow> = sqlx::query_as("select * from workflows where task_id = $1")
        .bind(task_id)
        .fetch_optional(&o.db)
        .await?;
    let Some(row) = row else {
        return Ok("This workflow no longer exists.".into());
    };
    if action == "del" {
        sqlx::query("delete from workflows where task_id = $1")
            .bind(task_id)
            .execute(&o.db)
            .await?;
        o.audit("owner", "workflow.delete", json!({"task": task_id}))
            .await;
        if let Some(msg) = row.tg_msg {
            let _ = o
                .chat
                .edit(
                    From::Orchestrator,
                    msg,
                    "🗑 <b>Workflow deleted.</b> The task and its history stay; it will not \
                     trigger on its own again.",
                    vec![],
                )
                .await;
        }
        return Ok("Workflow deleted.".into());
    }
    if let Some(gate) = match action {
        "gate_owner" => Some(Gate::Owner),
        "gate_auto" => Some(Gate::Auto),
        _ => None,
    } {
        sqlx::query("update workflows set plan_gate = $2 where task_id = $1")
            .bind(task_id)
            .bind(gate.as_str())
            .execute(&o.db)
            .await?;
        o.audit(
            "owner",
            "workflow.plan_gate",
            json!({"task": task_id, "gate": gate.as_str()}),
        )
        .await;
        if let Some(msg) = row.tg_msg {
            let _ = o
                .chat
                .edit(
                    From::Orchestrator,
                    msg,
                    &card_html(row.schedule.as_deref(), row.enabled, gate),
                    card_keyboard(task_id, row.enabled, gate),
                )
                .await;
        }
        return Ok(plan_gate::describe(gate).into());
    }
    let (schedule, enabled): (Option<String>, bool) = match action {
        "p15" => (Some("every:15m".into()), true),
        "p30" => (Some("every:30m".into()), true),
        "p1h" => (Some("every:1h".into()), true),
        "p6h" => (Some("every:6h".into()), true),
        "pd9" => (Some("daily:09:00".into()), true),
        "ondemand" => (None, row.enabled),
        "pause" => (row.schedule.clone(), false),
        "resume" => {
            ensure!(
                row.schedule.is_some(),
                "set a schedule first, or just write into the topic"
            );
            (row.schedule.clone(), true)
        }
        _ => bail!("unknown button"),
    };
    let gate = Gate::parse(&row.plan_gate)?;
    sqlx::query("update workflows set schedule = $2, enabled = $3 where task_id = $1")
        .bind(task_id)
        .bind(&schedule)
        .bind(enabled)
        .execute(&o.db)
        .await?;
    o.audit(
        "owner",
        "workflow.update",
        json!({"task": task_id, "schedule": schedule, "enabled": enabled}),
    )
    .await;
    if let Some(msg) = row.tg_msg {
        let _ = o
            .chat
            .edit(
                From::Orchestrator,
                msg,
                &card_html(schedule.as_deref(), enabled, gate),
                card_keyboard(task_id, enabled, gate),
            )
            .await;
    }
    Ok(describe(schedule.as_deref(), enabled))
}

/// Whether `schedule` is due, given when it last ran. `every:<N>m`/`every:<N>h` compare elapsed
/// time; `daily:HH:MM` compares the owner's local time and fires only once a calendar day (in
/// `cfg.secretary.timezone`, the same zone the daily digest uses); `weekly:<dow>:HH:MM` is that
/// again, on one weekday. Shared with `habits`, whose `do` habits use the same vocabulary.
pub(crate) async fn due(
    o: &Orchestrator,
    schedule: &str,
    last_run_at: Option<DateTime<Utc>>,
) -> Result<bool> {
    if let Some(rest) = schedule.strip_prefix("every:") {
        return Ok(interval_due(parse_every_minutes(rest)?, last_run_at));
    }
    if let Some(hhmm) = schedule.strip_prefix("daily:") {
        let (target_h, target_m) = parse_hhmm(hhmm)?;
        let (hour, minute, ran_today): (i32, i32, bool) = sqlx::query_as(
            "select extract(hour from now() at time zone $1)::int,
                    extract(minute from now() at time zone $1)::int,
                    ($2::timestamptz is not null
                     and to_char($2 at time zone $1, 'YYYY-MM-DD')
                       = to_char(now() at time zone $1, 'YYYY-MM-DD'))",
        )
        .bind(&o.cfg.secretary.timezone)
        .bind(last_run_at)
        .fetch_one(&o.db)
        .await?;
        return Ok(!ran_today && (hour, minute) >= (target_h, target_m));
    }
    if let Some(rest) = schedule.strip_prefix("weekly:") {
        let (dow, hhmm) = rest
            .split_once(':')
            .context("expected weekly:<dow>:HH:MM")?;
        let target_dow = parse_dow(dow)?;
        let (target_h, target_m) = parse_hhmm(hhmm)?;
        let (today, hour, minute, ran_today): (i32, i32, i32, bool) = sqlx::query_as(
            "select extract(dow from now() at time zone $1)::int,
                    extract(hour from now() at time zone $1)::int,
                    extract(minute from now() at time zone $1)::int,
                    ($2::timestamptz is not null
                     and to_char($2 at time zone $1, 'YYYY-MM-DD')
                       = to_char(now() at time zone $1, 'YYYY-MM-DD'))",
        )
        .bind(&o.cfg.secretary.timezone)
        .bind(last_run_at)
        .fetch_one(&o.db)
        .await?;
        return Ok(!ran_today && today == target_dow && (hour, minute) >= (target_h, target_m));
    }
    bail!("unknown schedule {schedule:?}")
}

/// A weekday as Postgres' `extract(dow)` numbers it: Sunday is 0.
pub(crate) fn parse_dow(s: &str) -> Result<i32> {
    const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    let s = s.trim().to_ascii_lowercase();
    DAYS.iter()
        .position(|d| s.starts_with(d))
        .map(|i| i as i32)
        .with_context(|| format!("{s:?} is not a weekday (mon, tue, …)"))
}

/// Whether an `every:<N>m`/`every:<N>h` schedule is due: never run yet, or at least `minutes`
/// since the last run.
fn interval_due(minutes: i64, last_run_at: Option<DateTime<Utc>>) -> bool {
    match last_run_at {
        Some(t) => Utc::now() - t >= chrono::Duration::minutes(minutes),
        None => true,
    }
}

pub(crate) fn parse_every_minutes(rest: &str) -> Result<i64> {
    if let Some(n) = rest.strip_suffix('m') {
        return n.parse().context("bad interval");
    }
    if let Some(n) = rest.strip_suffix('h') {
        return Ok(n.parse::<i64>().context("bad interval")? * 60);
    }
    bail!("bad interval {rest:?}")
}

pub(crate) fn parse_hhmm(s: &str) -> Result<(i32, i32)> {
    let (h, m) = s.split_once(':').context("expected HH:MM")?;
    let (h, m): (i32, i32) = (
        h.parse().context("bad hour")?,
        m.parse().context("bad minute")?,
    );
    ensure!(
        (0..24).contains(&h) && (0..60).contains(&m),
        "{s:?} is not a time of day"
    );
    Ok((h, m))
}

/// Delivers a tick to every enabled workflow whose schedule is due, so it checks what is needed
/// and acts — the same resume machinery a follow-up message already uses. Called from the
/// orchestrator's housekeeping loop.
pub async fn run_due(o: &Arc<Orchestrator>) -> Result<()> {
    let rows: Vec<WorkflowRow> =
        sqlx::query_as("select * from workflows where enabled and schedule is not null")
            .fetch_all(&o.db)
            .await?;
    for row in rows {
        let Some(schedule) = row.schedule.as_deref() else {
            continue;
        };
        match due(o, schedule, row.last_run_at).await {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                tracing::warn!("checking workflow {}'s schedule failed: {e:#}", row.task_id);
                continue;
            }
        }
        let Some(task) = o.task(&row.task_id).await? else {
            continue;
        };
        if let Err(e) = tasks::deliver_as(
            o,
            &task,
            "[Workflow tick] Its schedule is due. Check what is needed (e.g. with \
             `fleet agent feed read`/`search` since your last run) and act, same as any other \
             time you are asked to."
                .into(),
            tasks::Origin::Other,
        )
        .await
        {
            tracing::warn!("ticking workflow {} failed: {e:#}", row.task_id);
            continue;
        }
        sqlx::query("update workflows set last_run_at = now() where task_id = $1")
            .bind(&row.task_id)
            .execute(&o.db)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_validation() {
        assert!(validate_schedule("every:15m").is_ok());
        assert!(validate_schedule("daily:09:00").is_ok());
        assert!(validate_schedule("every:5m").is_err());
        assert!(validate_schedule("weekly").is_err());
    }

    #[test]
    fn card_says_who_approves_plans_and_offers_the_other_way() {
        let actions = |gate| -> Vec<String> {
            card_keyboard("dap-1", true, gate)
                .into_iter()
                .flatten()
                .filter_map(|b| b.data)
                .collect()
        };
        assert!(card_html(None, true, Gate::Auto).contains("Plans: approved automatically"));
        assert!(actions(Gate::Auto).contains(&"wf|dap-1|gate_owner".to_owned()));
        assert!(!actions(Gate::Auto).contains(&"wf|dap-1|gate_auto".to_owned()));
        assert!(card_html(None, true, Gate::Owner).contains("Plans: wait for your approval"));
        assert!(actions(Gate::Owner).contains(&"wf|dap-1|gate_auto".to_owned()));
    }

    #[test]
    fn button_prefix() {
        assert!(is_button("wf|dap-1|pause"));
        assert!(!is_button("x|1|y"));
        assert!(!is_button("t|1|d"));
    }

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_every_minutes("15m").unwrap(), 15);
        assert_eq!(parse_every_minutes("6h").unwrap(), 360);
        assert!(parse_every_minutes("soon").is_err());
        assert_eq!(parse_hhmm("09:30").unwrap(), (9, 30));
        assert!(parse_hhmm("9").is_err());
    }

    #[test]
    fn interval_due_compares_elapsed_time() {
        assert!(interval_due(15, None), "never run: due at once");
        assert!(!interval_due(15, Some(Utc::now())), "just ran: not due");
        assert!(
            interval_due(15, Some(Utc::now() - chrono::Duration::minutes(20))),
            "20 min since a 15 min schedule: due"
        );
        assert!(
            !interval_due(15, Some(Utc::now() - chrono::Duration::minutes(5))),
            "5 min since a 15 min schedule: not due"
        );
    }
}
