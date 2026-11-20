-- A short fleet-wide handle for a task, so the owner can name one out loud.
--
-- A task's id is long and half of it is often a Jira key (`bioins-BIOINS-5068`), which makes it
-- both awkward to say and easy to confuse with the ticket it refers to. The key is the fleet's
-- own: one counter for the whole fleet, rendered `F-41` (see `model::task_key`). The prefix is
-- checked at startup against every project's Jira key, so it can never name both.
create sequence task_key_seq;

-- Defaulted at the column, not set by the one caller that creates tasks today, so a task made
-- by some later path cannot quietly end up without a handle. Explicit null still means none.
alter table tasks add column key_no bigint default nextval('task_key_seq');

-- The fleet's own standing sessions are not work and get no key.
with ordered as (
    select id, row_number() over (order by created_at, id) as n
    from tasks
    where id not in ('brain', 'secretary', 'retro')
)
update tasks t set key_no = ordered.n from ordered where t.id = ordered.id;

select setval('task_key_seq', coalesce((select max(key_no) from tasks), 0) + 1, false);

-- Two tasks must never answer to the same key; a task without one simply has no key.
create unique index tasks_key_no on tasks (key_no);
