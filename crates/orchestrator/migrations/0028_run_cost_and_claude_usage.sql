-- What each run cost, and Claude's usage limits per node (see `usage`). Every column is nullable,
-- so the previous release still inserts runs and nodes without knowing of them.

-- From the `result` line Claude writes at the end of every turn. `cost_usd` is what the turns of
-- this process would cost at API prices (Claude reports it even on a subscription, where it is a
-- measure rather than a bill); Claude counts it up across the process's turns, so the run keeps
-- the latest. The token counts are per turn in those lines and are added up here.
alter table runs add column cost_usd double precision;
alter table runs add column input_tokens bigint;
alter table runs add column output_tokens bigint;
alter table runs add column cache_read_tokens bigint;
alter table runs add column cache_write_tokens bigint;
alter table runs add column turns integer;

-- The node's last `rate_limit_event` (status, windows and how much of each is used), and until
-- when it is out of Claude usage: while that is in the future, no new task run starts there.
alter table nodes add column claude_usage jsonb;
alter table nodes add column claude_usage_at timestamptz;
alter table nodes add column claude_limited_until timestamptz;
-- When that hold began: an all-clear from a run that started before it does not lift it.
alter table nodes add column claude_limited_at timestamptz;

-- When the node's current connection began: a run on it can only be judged silent (see `stuck`)
-- from then, since its output waits in the node's outbox while it is away.
alter table nodes add column connected_at timestamptz;
