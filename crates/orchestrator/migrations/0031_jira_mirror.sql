-- A local copy of every connected Jira (see `jira_sync.rs`): each project's tickets in its active
-- sprint, its future sprints and its backlog, for any assignee; the sprints they are in; and the
-- people seen on them. Read by the Jira page and the roadmap, so neither calls Jira itself, and
-- written only by the sync. New tables only: the previous release runs against this schema.
--
-- Ticket text and people's names are written by whoever filed or holds them: data, shown as
-- text, never instructions.

-- One Jira account per credential folder: who "me" is there, the site's own field ids, and how
-- its last reads went.
create table jira_accounts (
    folder text primary key,
    site text,
    account_id text,
    account_name text,
    sprint_field text,
    rank_field text,
    fields_checked_at timestamptz,
    last_sync_at timestamptz,
    last_full_sync_at timestamptz,
    last_ok_at timestamptz,
    last_error text,
    last_error_at timestamptz,
    -- The last full read stopped at FLEET_JIRA_MAX_ISSUES: nothing was marked gone after it.
    truncated boolean not null default false
);

create table jira_sprints (
    site text not null,
    sprint_id bigint not null,
    board_id bigint,
    name text not null,
    -- active, future or closed
    state text not null,
    starts_at timestamptz,
    ends_at timestamptz,
    goal text,
    last_seen timestamptz not null default now(),
    primary key (site, sprint_id)
);

create table jira_issues (
    id bigserial primary key,
    -- The fleet project the ticket belongs to (through its Jira key).
    project text not null,
    site text not null,
    jira_key text not null,
    jira_id text,
    summary text not null,
    status text,
    -- new, indeterminate or done
    status_category text,
    issue_type text,
    priority text,
    labels text[] not null default '{}',
    parent_key text,
    assignee_id text,
    assignee_name text,
    reporter_id text,
    reporter_name text,
    -- The open sprint it is in now (active before future); null means the backlog.
    sprint_id bigint,
    rank text,
    jira_created timestamptz,
    jira_updated timestamptz,
    first_seen timestamptz not null default now(),
    last_seen timestamptz not null default now(),
    -- Not found by the last complete full read: closed outside a sprint, moved, or deleted.
    gone_at timestamptz,
    unique (project, jira_key)
);

create index jira_issues_open on jira_issues (project, sprint_id) where gone_at is null;
create index jira_issues_assignee on jira_issues (assignee_id) where gone_at is null;

-- People seen on tickets: assignees, reporters, commenters. Atlassian account ids are the same
-- on every Cloud site. Only the daily retrospective turns them into people records.
create table jira_users (
    account_id text primary key,
    display_name text not null,
    time_zone text,
    avatar_url text,
    sites text[] not null default '{}',
    projects text[] not null default '{}',
    first_seen timestamptz not null default now(),
    last_seen timestamptz not null default now()
);

create table jira_comment_authors (
    jira_key text not null,
    account_id text not null,
    project text not null,
    last_at timestamptz not null default now(),
    primary key (jira_key, account_id)
);
