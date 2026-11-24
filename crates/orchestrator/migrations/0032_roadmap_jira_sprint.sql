-- The roadmap reads the local copy of Jira (`jira_sync`) and knows each ticket's Jira sprint: a
-- ticket of a sprint that has not started waits for it (`not_before`, its start), and one the
-- owner added by hand (`origin = 'owner'`) stays on the plan whoever holds it in Jira. Nullable
-- columns only, so the previous release runs against this schema.
alter table roadmap_items add column jira_sprint_id bigint;
alter table roadmap_items add column jira_sprint_name text;
alter table roadmap_items add column jira_sprint_state text;
alter table roadmap_items add column not_before timestamptz;
alter table roadmap_items add column origin text;
