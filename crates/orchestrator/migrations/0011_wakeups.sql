-- What an agent asked to be woken for, so it needs no session while it waits: the orchestrator
-- is the alarm clock (see wakeups.rs). A 'timer' fires at its time and the agent checks
-- whatever it was waiting for with its own tools; a 'script' fires when the condition the brain
-- wrote for the agent's plain-text wish returns true.
create table wakeups (
    id         bigserial primary key,
    task_id    text not null references tasks(id) on delete cascade,
    -- The agent's own words, handed back when it wakes, so the resumed session knows what it
    -- was waiting for and what to do about it.
    note       text not null,
    kind       text not null check (kind in ('timer', 'script')),
    -- timer: when to fire. script: not before this; re-checked every tick after that.
    fires_at   timestamptz,
    -- null: one-shot. Otherwise the interval ('30m') to re-arm with after firing.
    every      text,
    -- What the agent asked for in plain text, and the Rhai the brain wrote for it (null while
    -- the brain is still writing it).
    wish       text,
    script     text,
    state      text not null default 'armed'
        check (state in ('drafting', 'armed', 'fired', 'cancelled', 'failed')),
    fires      integer not null default 0,
    -- Consecutive evaluation errors; a script that keeps failing is disarmed.
    errors     integer not null default 0,
    -- The script's own scratch between ticks ("the sha I saw last time").
    memo       jsonb not null default '{}',
    created_by text not null,
    expires_at timestamptz not null,
    created_at timestamptz not null default now(),
    last_at    timestamptz
);
create index wakeups_due on wakeups(fires_at) where state in ('armed', 'drafting');
create index wakeups_task on wakeups(task_id);
