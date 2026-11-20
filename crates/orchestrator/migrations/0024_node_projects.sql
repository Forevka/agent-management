-- Projects assigned to a node, cloned there on demand. A clean machine joined as a node needs
-- no project checked out by hand: the owner assigns projects to it in the web UI, and when a
-- task for one is placed there the node clones it (with the machine's own git login) and maps
-- it. Nothing assigned means nothing is ever cloned. All additions, nullable or defaulted, so
-- the previous release runs against this schema unchanged.

-- Where the project's code lives. Never with credentials in it (checked on entry).
alter table projects add column repo_url text;

-- Where a node puts its clones (`<root>/<slug>`); null for the node's default,
-- `~/fleet-projects`.
alter table nodes add column projects_root text;

create table node_projects (
    node_id     text not null references nodes(id) on delete cascade,
    project     text not null references projects(slug) on delete cascade,
    assigned_at timestamptz not null default now(),
    -- The node's last report for this assignment: cloning | cloned | failed (null: none yet).
    state       text,
    -- Where it was cloned to, or why it failed.
    detail      text,
    primary key (node_id, project)
);
