-- The people network: the owner's colleagues the fleet comes into contact with, so the brain and
-- the secretary know who they are writing to. The retrospective writes these records from what
-- it read (untrusted material, so the records are untrusted data too); the owner edits them.
-- New tables only: the previous release never reads or writes them.
create table people (
    id               bigserial primary key,
    name             text not null,
    position         text,
    -- What they are involved in on the project.
    brief            text,
    -- Free-text project names at the owner's company, not necessarily fleet projects.
    projects         text[] not null default '{}',
    language         text check (language in ('en', 'uk')),
    observations     text[] not null default '{}',
    -- Whatever fits no field; recurring tags are what the owner later turns into fields.
    tags             text[] not null default '{}',
    last_interaction timestamptz,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    updated_by       text not null default 'retro' check (updated_by in ('retro', 'owner'))
);
-- One record per name, whatever its case: the retrospective updates by name, never duplicates.
create unique index people_name on people (lower(name));

-- How to reach a person, and how a feed message's author is recognised as them.
create table person_contacts (
    id        bigserial primary key,
    person_id bigint not null references people(id) on delete cascade,
    -- slack, gchat, email, telegram or other (checked in code, so a new source needs no migration).
    source    text not null,
    -- The source's stable user id (Slack's U0123...), when known: what feed authors match on.
    native_id text,
    -- What a person reads: @name, a display name, an address.
    handle    text not null,
    unique (source, native_id)
);
create index person_contacts_person on person_contacts (person_id);
