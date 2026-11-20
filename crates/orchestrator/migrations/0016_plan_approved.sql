-- Whether the owner has approved the plan of the task's current planning round. A run starts
-- in plan mode with it false, and approving the plan sets it. A task whose run was lost while
-- it planned (a node restart, an idle session closed) is resumed in plan mode again rather
-- than in auto: the owner's approval is never skipped by an interruption.
alter table tasks add column plan_approved boolean not null default true;

-- Tasks still planning (a follow-up round included), and tasks that are not over and never had
-- a plan approved, whatever their status says now (`queued` and `stopped` overwrite `planning`).
update tasks t set plan_approved = false
 where t.id not in ('brain', 'secretary', 'retro')
   and (t.status = 'planning'
        or (t.status <> 'done'
            and not exists (select 1 from human_items h
                             where h.task_id = t.id and h.kind = 'plan' and h.answer->>'approved' = 'true')));
