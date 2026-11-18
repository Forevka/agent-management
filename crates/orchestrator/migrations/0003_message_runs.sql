-- A run being started takes the task's waiting messages (and the new one) by tagging them with
-- its run id; they are deleted once the node reports the run started, and untagged again if it
-- never does (the node failed to start it, or was revoked or lost first), so the next run gets
-- them instead of their being lost.
alter table messages add column run_id text;
create index messages_run on messages(run_id) where run_id is not null;
