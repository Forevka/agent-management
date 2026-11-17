-- Each start of an agent process is a run. Events carry the run id, so late events from a run
-- that was replaced (an "exited" after a respawn) cannot change the task's state.
alter table tasks add column run_id text;
-- When the current run was sent to its node; a run is only presumed dead by a node report after
-- a grace period, because the report may predate the spawn.
alter table tasks add column dispatched_at timestamptz;
-- The node process last seen. A different value means the node restarted and its agents died.
alter table nodes add column incarnation text;
