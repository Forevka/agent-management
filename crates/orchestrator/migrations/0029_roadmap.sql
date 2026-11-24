-- The roadmap (see `roadmap.rs`): the fleet's own two-week sprints, and the Jira tickets each one
-- works through. New tables only, so the previous release runs against this schema untouched.

create table sprints (
    id serial primary key,
    -- First day, and the day after the last (local dates in the fleet's time zone).
    starts_on date not null unique,
    ends_on date not null,
    created_at timestamptz not null default now()
);

create table roadmap_items (
    id bigserial primary key,
    sprint_id int not null references sprints (id) on delete cascade,
    project text not null,
    jira_key text not null,
    -- What Jira said at the last sync. Ticket text: shown as text, never acted on.
    title text not null,
    jira_status text,
    -- Jira's status category is `done`: nothing left to plan.
    jira_done boolean not null default false,
    issue_type text,
    priority text,
    url text,
    -- How much of one Claude usage window it takes, in percent (over 100 spans windows).
    usage int not null default 40,
    usage_note text,
    -- Who set the estimate (the brain, the owner); null while it is the guess by type.
    estimated_by text,
    -- When the brain was asked to estimate it. The owner's card waits for estimates, or for
    -- `ASK_WAIT_MINUTES` after this.
    asked_at timestamptz,
    -- The order it waits in: Jira's priority when it was first seen, then its key.
    position int not null default 0,
    -- Where the owner put it by hand (a day of the sprint and a window of that day).
    pinned_day int,
    pinned_session int,
    -- new: waits for the owner's approval; approved: planned, starts when its window opens;
    -- declined: the owner said not now; skipped: taken off the plan; started: a task works on it;
    -- left: it is no longer in the owner's Jira sprint.
    state text not null default 'new',
    approved_at timestamptz,
    task_id text,
    started_at timestamptz,
    -- The window (day * windows per day + window) it started in, where the board shows it.
    started_cell int,
    -- When the fleet moved it in Jira, so each move is made once.
    jira_progress_at timestamptz,
    jira_review_at timestamptz,
    -- Why the last start failed, while it keeps failing.
    start_error text,
    first_seen timestamptz not null default now(),
    last_seen timestamptz not null default now(),
    unique (sprint_id, jira_key)
);

create index roadmap_items_task on roadmap_items (task_id) where task_id is not null;
