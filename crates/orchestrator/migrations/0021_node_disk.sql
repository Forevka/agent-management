-- Free disk space each node last reported (`Body::DiskReport`), and what the
-- orchestrator made of it. A full disk on a node corrupts session transcripts and fails every
-- run started there, so the owner is warned before that and new runs are held on such a node.
alter table nodes
    -- The last report: {claude_dir, volumes: [{volume, free, total, claude_dir, projects}], biggest}.
    add column disk       jsonb,
    add column disk_at    timestamptz,
    -- ok | low (the owner was warned) | full (new task runs are held here).
    add column disk_state text not null default 'ok',
    -- The state of each volume in the last report, {volume: state}: a volume leaves a state
    -- only with a margin above its threshold, so its own previous state has to be known.
    add column disk_levels jsonb not null default '{}';
