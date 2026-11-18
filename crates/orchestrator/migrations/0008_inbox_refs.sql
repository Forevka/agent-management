-- Which source messages a post in the Inbox topic is about (an escalation note, a digest that
-- cites messages, a reply draft), so the owner can reply to that post with an instruction and
-- the secretary gets the messages as context. Todos keep their refs on the todo itself.
create table inbox_refs (
    tg_msg     bigint primary key,
    refs       text[] not null,
    created_at timestamptz not null default now()
);
create index inbox_refs_created on inbox_refs(created_at);
