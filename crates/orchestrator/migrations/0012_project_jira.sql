-- Each project's own Jira (see jira.rs). One credential is normally shared by every project:
-- it lives in a secrets-manager folder (the `jira` preset's JIRA_URL, JIRA_EMAIL and
-- JIRA_API_TOKEN, so the site travels with the credential), named here only to override the
-- fleet's default folder. What really differs per project is its key in that Jira (BIOINS, DAP)
-- and, when "my sprint" must be precise, its board. The credential itself never leaves the
-- orchestrator: the brain reaches Jira through tools only.
alter table projects add column jira_folder text;
alter table projects add column jira_key text;
alter table projects add column jira_board integer;
