-- Every agent process start already had a `run_id`, but a run left only its transcript
-- behind: `tasks` keeps just the current one, so the fleet could show what is happening now
-- and never what happened before. These rows are that history — one per run, which is what
-- the owner means by "a session" — so the sessions of a task, or of a whole project, are an
-- indexed read instead of a guess from transcript lines.
create table runs (
    run_id     text primary key,
    task_id    text not null references tasks(id) on delete cascade,
    -- Denormalised from the task so a project's sessions are one indexed query, and so the
    -- history survives a task moving project.
    project    text not null,
    agent_id   text,
    node_id    text,
    -- Claude's own session id, which arrives after the process starts (and not at all if it
    -- dies first).
    session_id text,
    -- running | ended
    status     text not null default 'running',
    -- How it ended, in one field because a code and a loss are the same question: `exit 0`,
    -- `exit None` when the node reported no code, or `lost: its node restarted` for a run
    -- whose process died with its node and never sent an exit event.
    outcome    text,
    started_at timestamptz not null default now(),
    ended_at   timestamptz
);
create index runs_task on runs(task_id, started_at desc);
create index runs_project on runs(project, started_at desc);
