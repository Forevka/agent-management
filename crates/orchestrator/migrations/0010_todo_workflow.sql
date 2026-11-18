-- A todo can point at a workflow (see workflows.rs): tapping its "Run workflow" button nudges
-- that task instead of asking the brain to create a new one.
alter table todos add column workflow_task text references tasks(id) on delete cascade;
