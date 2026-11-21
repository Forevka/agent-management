-- The owner's own accounts on each source, as the owner said on the web: their messages are the
-- owner's (`feed_messages.from_owner`) whatever the source could tell at ingest, and they are
-- never one of the people. A source is free text (slack, gchat, or one no feed reads yet, such as
-- gitlab or jira). A new table the previous release never reads or writes.
create table owner_accounts (
    source      text not null,
    native_id   text not null,
    -- How the owner appears there, for the owner's own reading (@handle, display name).
    label       text,
    added_at    timestamptz not null default now(),
    -- Set when the owner said the account is *not* theirs ("Not mine"): it then counts as
    -- someone else's even where a source flags its messages as the owner's, so the answer
    -- sticks. Null: the account is the owner's.
    disowned_at timestamptz,
    primary key (source, native_id)
);
