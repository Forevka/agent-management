-- The secretary: a cheap session that triages every message from every conversation the owner
-- is in (any source), keeps the owner's todo list and escalates to the brain. Nothing here names
-- a source: Slack is only the first one.

-- Conversations the owner is in, whatever their source: a name and a kind for the secretary,
-- summaries and the web page.
create table feed_conversations (
    source          text not null,
    conversation_id text not null,
    name            text not null default '',
    kind            text not null default 'channel'
        check (kind in ('direct', 'group', 'private', 'channel', 'email_thread', 'other')),
    primary key (source, conversation_id)
);

-- The generic message fields every source can fill (beyond the original Slack-shaped ones), and
-- the secretary's progress through them: 'pending' until a batch with the message reached its
-- session, then 'sent'. Null: not for the secretary (a triage channel the brain already sees,
-- the owner's own message, one deleted at its source).
alter table feed_messages add column author_id text;
alter table feed_messages add column subject text;
alter table feed_messages add column mentions_owner boolean not null default false;
alter table feed_messages add column secretary text check (secretary in ('pending', 'sent'));
-- An edit of a message already triaged keeps the text it was triaged on and adds the new one.
alter table feed_messages add column edited boolean not null default false;
alter table feed_messages add column edited_text text;
create index feed_messages_secretary on feed_messages(id) where secretary = 'pending';

-- The owner's todo list, kept by the secretary (and the owner).
create table todos (
    id          bigserial primary key,
    title       text not null,
    detail      text not null default '',
    -- Opaque message references (`<source>:<conversation>:<message>`) the todo came from.
    refs        text[] not null default '{}',
    -- tasked: the owner asked the brain for a task from it; nobody changes it after that.
    status      text not null default 'open'
        check (status in ('open', 'done', 'dismissed', 'tasked')),
    created_by  text not null check (created_by in ('secretary', 'owner', 'orchestrator')),
    -- Who last changed its status, so the owner sees what the secretary did (and can undo it).
    updated_by  text check (updated_by in ('secretary', 'owner')),
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    due         timestamptz,
    -- Its message (with buttons) in the Inbox topic.
    tg_msg      bigint
);
create index todos_open on todos(status) where status = 'open';

-- Triage preferences the owner taught the secretary. They only make triage quieter or louder;
-- every change is posted to the Inbox topic with an Undo (or Restore) button.
create table secretary_rules (
    id         bigserial primary key,
    -- One line, no control characters (checked by the orchestrator).
    text       text not null,
    source     text not null check (source in ('owner', 'orchestrator', 'secretary')),
    -- What asked for it (the owner's words the secretary cites).
    cited      text,
    active     boolean not null default true,
    created_at timestamptz not null default now(),
    tg_msg     bigint
);

-- What the secretary decided about messages, for the web page and the escalation cap.
create table secretary_triage (
    id       bigserial primary key,
    at       timestamptz not null default now(),
    refs     text[] not null,
    decision text not null check (decision in ('ignore', 'note', 'todo', 'escalate')),
    note     text not null default ''
);
create index secretary_triage_at on secretary_triage(at);

-- Batches handed to the secretary: the hourly turn cap and the daily counters.
create table secretary_batches (
    id       bigserial primary key,
    at       timestamptz not null default now(),
    messages integer not null,
    urgent   boolean not null
);
create index secretary_batches_at on secretary_batches(at);
