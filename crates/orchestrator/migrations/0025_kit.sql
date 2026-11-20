-- The kit: the owner's Claude Code skills, plugins and MCP servers, managed on the web UI and
-- installed on every node. Enabled MCP servers are also the allowlist of agent sessions. See
-- docs/plan.md ("The kit") and crates/orchestrator/src/kit.rs.
--
-- Two new tables and nothing else: the previous release never reads them.
create table kit_items (
    id         bigserial primary key,
    kind       text not null check (kind in ('skill', 'plugin', 'mcp')),
    -- A skill's directory name, `plugin@marketplace`, or an MCP server's name.
    name       text not null,
    enabled    boolean not null default true,
    -- A plugin: {plugin, marketplace, marketplace_source}. An MCP server: its config
    -- (command/args/env, or type/url/headers) with every secret value replaced by a
    -- `${secret:NAME}` placeholder; the values are in the vault (the `kit` secret folder),
    -- never here. A skill: {}.
    spec       jsonb not null default '{}',
    -- A skill's files, {relative path: text}. Text only: binary files are not taken into the kit.
    files      jsonb,
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
