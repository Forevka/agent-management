-- Attachments, search and the nightly archive (see `attachments`, `search`, `archive`).

-- What a task produced or is best read with: a titled link (a claude.ai artifact, a doc) or a
-- file (an asset, a migration for devops). Unlike `task_links`, which say what a task is *about*
-- in somebody else's tracker, these are its deliverables, and they need a title to be found.
create table task_attachments (
    id         bigserial primary key,
    task_id    text not null references tasks(id) on delete cascade,
    -- `url` or `file`.
    kind       text not null check (kind in ('url', 'file')),
    title      text not null,
    -- The link, for `url`.
    url        text,
    -- For `file`: its name as uploaded, its media type (a guess from the name; only ever a hint
    -- for the download, never trusted to render), its size and the SHA-256 of its bytes.
    file_name  text,
    mime       text,
    size       bigint,
    sha256     text,
    -- A file is uploaded in chunks and only becomes visible once the last one arrived and the
    -- whole was checked (size, hash, no secret inside). A link is complete at once.
    complete   boolean not null default false,
    added_by   text not null,
    created_at timestamptz not null default now()
);

create index task_attachments_task on task_attachments (task_id) where complete;
-- A link is attached to a task once; attaching it again only updates its title. Enforced here
-- rather than by a lookup first, so two attaches at the same moment cannot both insert.
create unique index task_attachments_url on task_attachments (task_id, url) where kind = 'url';

-- A file's bytes, in pieces of at most 1 MiB, so no message on the wire and no single row ever
-- has to hold a whole large file. In Postgres rather than on a disk of the orchestrator's so the
-- existing backup covers them and there is no second place for a deliverable to be lost from.
create table task_attachment_chunks (
    attachment_id bigint not null references task_attachments(id) on delete cascade,
    idx           integer not null,
    data          bytea not null,
    primary key (attachment_id, idx)
);

-- The agent's own closing words, kept with the task so a search can find a task by what it
-- turned out to be, not only by what it was asked to be.
alter table tasks add column summary text;

-- Task states are now: queued | planning | running | waiting | done | stopped | failed |
-- archived. `archived` is a `done` task the nightly archive moved out of the board's way.
create index tasks_status on tasks (status);
