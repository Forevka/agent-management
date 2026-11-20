-- Who approves an agent's plan: the owner (a plan card that waits for their tap) or the
-- orchestrator on their behalf (`auto`: approved at once and posted silently for the record).
-- See docs/plan.md ("Plan gate"). Only the owner changes either setting: the workflow card for a
-- workflow, the project page for a project; nothing the brain or an agent can call sets it.

-- A workflow was approved once, when it was set up; every later run of it re-plans (its task is
-- `done`), and asking the owner again each time is what this exists to stop. So the workflows
-- that already exist, all set up at the owner's request, become `auto`.
alter table workflows
    add column plan_gate text not null default 'auto' check (plan_gate in ('owner', 'auto'));

-- A new one starts at `owner`: an agent can turn its own task into a workflow with
-- `workflow_finish`, unasked, and that must not be what opens the gate. One tap on the new
-- workflow's card (🔓) makes it `auto`.
alter table workflows alter column plan_gate set default 'owner';

-- The default for the project's tasks that are not workflows. `owner`, as it always was.
alter table projects
    add column plan_gate text not null default 'owner' check (plan_gate in ('owner', 'auto'));
