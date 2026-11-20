-- What a task is about in somebody else's tracker.
--
-- The fleet's own key (`F-41`) names a task; this says which outside things it belongs to — the
-- Jira ticket it came from, the merge request it opened, a pull request, a page. Kept as rows
-- rather than a column because one task often has several, and because the interesting one (the
-- MR an agent just opened) is discovered during the work, not known when the task is created.
create table task_links (
    id         bigserial primary key,
    task_id    text not null references tasks(id) on delete cascade,
    -- `jira`, `mr`, `pr` or `url`. Kept as text so a tracker nobody has thought of yet does not
    -- need a migration, and so an unknown kind degrades to a plain chip instead of breaking.
    kind       text not null,
    -- How a person says it: `BIOINS-5068`, `!940`, `#482`, or the URL itself for `url`.
    ref        text not null,
    -- Where it is, when the fleet can work that out (a project's Jira site, or a link given to
    -- it). Null means the ref is worth showing but not clickable.
    url        text,
    -- Who said so, for the same reason the audit log exists: `owner`, `orchestrator`, or the
    -- agent's id.
    added_by   text not null,
    created_at timestamptz not null default now(),
    -- One task does not carry the same ticket twice, however many times it is mentioned.
    unique (task_id, kind, ref)
);

create index task_links_task on task_links (task_id);

-- Backfill: a task created from a ticket has that ticket in its id, as `<project>-<TICKET>`
-- (see `tasks::create_task`). Only where the suffix actually looks like the project's Jira key,
-- so `bioins-2` (a plain counter) is not mistaken for a ticket.
insert into task_links (task_id, kind, ref, added_by)
select t.id, 'jira', substring(t.id from length(p.slug) + 2), 'orchestrator'
from tasks t
join projects p on p.slug = t.project
where p.jira_key is not null
  and t.id like p.slug || '-' || p.jira_key || '-%'
on conflict do nothing;
