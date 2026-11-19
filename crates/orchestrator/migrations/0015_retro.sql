-- The daily retrospective (see retro.rs and docs/plan.md, "Retrospective"): once a day the
-- fleet reads its own day — tasks, the owner's decisions and requests, the agents' chatter, what
-- the secretary triaged, what went wrong — and proposes how to be better tomorrow. A proposal is
-- words until the owner taps "Make task", and then it is a task on the fleet's own code.
create table retro_runs (
    id        bigserial primary key,
    -- The owner's local calendar day, so one run a day is a unique row rather than a timer.
    day       text not null unique,
    at        timestamptz not null default now(),
    -- Its own closing line: what it looked at and what it concluded, posted even when it
    -- proposes nothing, so a quiet day reads as a quiet day and not as a failure.
    verdict   text,
    proposals integer not null default 0
);

create table retro_proposals (
    id         bigserial primary key,
    run_id     bigint references retro_runs(id) on delete set null,
    -- What it is about, for reading at a glance: harness | agents | flows | features | other.
    area       text not null default 'other',
    title      text not null,
    detail     text not null,
    state      text not null default 'open' check (state in ('open', 'tasked', 'dismissed')),
    -- The task the owner's tap created, if they did.
    task_id    text references tasks(id) on delete set null,
    -- Its card in the Retro topic, edited in place as it is taken up or dismissed.
    tg_msg     bigint,
    created_at timestamptz not null default now(),
    decided_at timestamptz
);
create index retro_open on retro_proposals(state) where state = 'open';
