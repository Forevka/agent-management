-- Hosts that joined the fleet. `id` is the node's iroh endpoint id (its public key).
create table nodes (
    id            text primary key,
    name          text not null,
    os            text not null default '',
    app_version   text not null default '',
    online        boolean not null default false,
    last_seen     timestamptz,
    -- How the node starts Claude Code (pushed to it on every connect).
    claude_command jsonb not null default '["claude"]',
    auth_mode     text not null default 'default',      -- default | oauth_token | api_key
    auth_secret   bytea,                                -- encrypted token or key
    max_agents    integer not null default 4,
    -- Last NodeInfo: [{project, path}] and running agent ids.
    projects      jsonb not null default '[]',
    running       jsonb not null default '[]'
);

-- Single-use invite secrets for `fleet join`.
create table invites (
    secret     text primary key,
    created_at timestamptz not null default now(),
    used_by    text
);

create table projects (
    slug            text primary key,
    name            text not null,
    description     text not null default '',
    next_ticket     integer not null default 1,
    needs_you_topic bigint,
    fyi_topic       bigint,
    created_at      timestamptz not null default now()
);

-- Free-form secrets manager: folders of key/value pairs, values encrypted at rest.
create table secret_folders (
    id     bigserial primary key,
    name   text not null unique,
    preset text not null default 'custom'
);
create table secret_items (
    folder_id bigint not null references secret_folders(id) on delete cascade,
    key       text not null,
    value     bytea not null,
    primary key (folder_id, key)
);
create table project_secrets (
    project   text not null references projects(slug) on delete cascade,
    folder_id bigint not null references secret_folders(id) on delete cascade,
    primary key (project, folder_id)
);

-- Pool of agent bot tokens registered by the owner.
create table bots (
    id          bigserial primary key,
    token       bytea not null,
    tg_user_id  bigint not null unique,
    username    text not null,
    agent_id    text unique
);

create table agents (
    id             text primary key,
    name           text not null unique,
    role           text not null,
    project        text references projects(slug) on delete set null,
    bot_id         bigint references bots(id) on delete set null,
    preferred_node text,
    created_at     timestamptz not null default now(),
    last_active    timestamptz
);

create table tasks (
    id               text primary key,              -- {project}-{ticket}
    project          text not null references projects(slug),
    title            text not null,
    brief            text not null,
    agent_id         text references agents(id),
    node_id          text,
    -- queued | planning | running | waiting | done | stopped | failed
    status           text not null default 'queued',
    topic_id         bigint,
    checklist_msg_id bigint,
    session_id       text,
    running          boolean not null default false,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now()
);
create index tasks_agent on tasks(agent_id);

create table milestones (
    task_id text not null references tasks(id) on delete cascade,
    idx     integer not null,
    title   text not null,
    done    boolean not null default false,
    primary key (task_id, idx)
);

-- Everything waiting on the human: questions, permission prompts, plans, proposals.
create table human_items (
    id          bigserial primary key,
    project     text,
    task_id     text,
    agent_id    text,
    kind        text not null,          -- question | permission | plan | ask | task_proposal | slack_reply
    blocking    boolean not null default true,
    payload     jsonb not null,
    state       text not null default 'open',   -- open | answered | cancelled
    answer      jsonb,
    tg_chat     bigint,
    tg_msg      bigint,
    -- For permission prompts: the node and tool call to answer.
    node_id     text,
    call_id     text,
    created_at  timestamptz not null default now(),
    answered_at timestamptz,
    reminded_at timestamptz
);
create index human_open on human_items(state) where state = 'open';

create table magic_links (
    token      text primary key,
    target     text not null,       -- path to open after login
    expires_at timestamptz not null,
    used       boolean not null default false
);
create table web_sessions (
    token      text primary key,
    expires_at timestamptz not null
);

-- Raw agent output (transcripts); pruned after the retention period.
create table task_events (
    id      bigserial primary key,
    task_id text not null,
    at      timestamptz not null default now(),
    kind    text not null,
    data    text not null
);
create index task_events_task on task_events(task_id, id);

-- Agent-to-agent and human-to-agent messages routed by the orchestrator.
create table messages (
    id         bigserial primary key,
    task_id    text,
    from_agent text,
    to_agent   text not null,
    text       text not null,
    created_at timestamptz not null default now()
);

create table feed_channels (
    source     text not null,
    channel_id text not null,
    name       text not null default '',
    project    text references projects(slug) on delete set null,
    topic_id   bigint,
    -- Every new message in a triage channel is shown to the orchestrator brain, which may
    -- propose tasks or replies. Other channels are only mirrored and searchable.
    triage     boolean not null default false,
    primary key (source, channel_id)
);
create table feed_messages (
    id          bigserial primary key,
    source      text not null,
    channel_id  text not null,
    msg_id      text not null,
    thread_id   text,
    author      text,
    text        text not null,
    ts          double precision not null,
    permalink   text,
    created_at  timestamptz not null default now(),
    search      tsvector generated always as (to_tsvector('simple', text)) stored,
    unique (source, channel_id, msg_id)
);
create index feed_messages_search on feed_messages using gin(search);
create index feed_messages_channel on feed_messages(source, channel_id, ts);

create table audit_log (
    id     bigserial primary key,
    at     timestamptz not null default now(),
    actor  text not null,
    action text not null,
    detail jsonb not null default '{}'
);

-- Small settings and cursors (Telegram update offsets, feed cursors).
create table kv (
    key   text primary key,
    value jsonb not null
);
