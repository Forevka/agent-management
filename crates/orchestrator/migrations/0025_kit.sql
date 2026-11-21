-- The kit: the owner's Claude Code skills, plugins and MCP servers, managed on the web UI and
-- installed on every node. Enabled MCP servers are also the allowlist of agent sessions. See
-- docs/plan.md ("The kit") and crates/orchestrator/src/kit.rs.
--
-- Two new tables and one nullable column: the previous release never reads them.
create table kit_items (
    id         bigserial primary key,
    kind       text not null check (kind in ('skill', 'plugin', 'mcp')),
    -- A skill's directory name, `plugin@marketplace`, or an MCP server's name.
    name       text not null,
    enabled    boolean not null default true,
    -- A plugin: {plugin, marketplace, marketplace_source} (a source with no credential in it).
    -- An MCP server: only a summary that holds no secret, {type, command (its name, no path or
    -- arguments) or endpoint (scheme and host)}. A skill: {}.
    spec       jsonb not null default '{}',
    -- A skill's files, {relative path: text}. Text only: binary files are not taken into the kit.
    files      jsonb,
    -- An MCP server's whole config, sealed with the vault (FLEET_MASTER_KEY): a config is a secret
    -- as a whole, so none of it is kept in the clear. Null for skills and plugins.
    config     bytea,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (kind, name)
);

-- How each item fared on each node, as the node last reported it.
create table node_kit (
    node_id text not null references nodes(id) on delete cascade,
    item_id bigint not null references kit_items(id) on delete cascade,
    -- pending | installed | failed | conflict | removed
    state   text not null default 'pending',
    detail  text not null default '',
    at      timestamptz not null default now(),
    primary key (node_id, item_id)
);

-- What a node says about its own kit, when something is wrong: its claude CLI cannot hold agent
-- sessions to the kit's MCP servers (no --strict-mcp-config), or its record of what it installed
-- was unreadable. Null when all is well.
alter table nodes add column kit_note text;
