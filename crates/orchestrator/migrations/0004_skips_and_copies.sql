-- A milestone the work did not need is skipped rather than left open, optionally with why.
-- Completing a task skips whatever is still open, so a finished checklist never shows an open
-- step.
alter table milestones add column skipped boolean not null default false;
alter table milestones add column skip_reason text;

-- Every further message that shows a human item (the pointer in its task's topic, next to the
-- main card in "needs you"), so answering it anywhere updates all of them.
alter table human_items add column tg_msgs bigint[] not null default '{}';
