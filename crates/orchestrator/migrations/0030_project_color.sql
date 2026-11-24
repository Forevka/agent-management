-- A project's colour (see `admin::PROJECT_COLORS`): what marks its tasks, tickets and cards in the
-- web UI. Null means the default, picked from the slug. Nullable, so the previous release runs.
alter table projects add column color text;
