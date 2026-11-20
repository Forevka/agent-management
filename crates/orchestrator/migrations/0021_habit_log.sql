-- What a habit decided each time it was looked at, so "did something go wrong?" has an answer the
-- owner can read rather than one they have to reconstruct. See docs/plan.md ("Habits").
--
-- When the orchestrator last evaluated the habit: a `do` habit every housekeeping tick, a `decide`
-- habit whenever a plan of its job arrived. `last_used_at` stays the schedule's anchor (it moves
-- before delivery, so it cannot say whether anything was actually handed over); the last time a
-- habit really acted is its newest `habit_uses` row.
alter table habits add column last_checked_at timestamptz;

-- Everything a habit did *not* do, and what the brain said it did with a standing request. Kept
-- apart from `habit_uses` because those rows are what the daily cap counts.
create table habit_log (
    id        bigserial primary key,
    habit_id  bigint not null references habits(id) on delete cascade,
    at        timestamptz not null default now(),
    -- 'skipped': it looked and stood aside (a stopper, the daily cap, a candidate the brain passed
    --            over); 'failed': it meant to act and could not; 'report': the brain's account of
    --            what it did with a standing request.
    kind      text not null check (kind in ('skipped', 'failed', 'report')),
    -- Who wrote it: the orchestrator's own matching, or the brain (its words, shown as text).
    source    text not null check (source in ('orchestrator', 'brain')),
    task_id   text,
    item_id   bigint,
    -- What was considered: a task id, or a candidate the brain names ("MR !412").
    candidate text,
    what      text not null
);
create index habit_log_recent on habit_log(habit_id, at desc);
