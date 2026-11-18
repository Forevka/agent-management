-- A workflow is a task whose topic is kept open for repeat triggers, plus its trigger
-- settings. See docs/plan.md ("Workflows").
create table workflows (
    task_id     text primary key references tasks(id) on delete cascade,
    -- null: on-demand only (the owner writes into the topic). Otherwise one of
    -- 'every:15m' | 'every:30m' | 'every:1h' | 'every:6h' | 'daily:HH:MM'.
    schedule    text,
    enabled     boolean not null default true,
    -- The control card's own message, edited in place as its settings change.
    tg_msg      bigint,
    last_run_at timestamptz,
    created_at  timestamptz not null default now()
);
