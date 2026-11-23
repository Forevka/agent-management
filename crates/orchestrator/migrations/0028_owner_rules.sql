-- The owner's operating rules: how the fleet acts in their name (approve, merge, post, transition,
-- reply), one record each, so a habit or a brief can cite a rule instead of keeping its own copy
-- of it. See docs/plan.md ("Owner rules") and admin.rs. A rule is only ever written by an approved
-- `admin_change`, because its current text reaches every standing request of the habits that cite
-- it. Triage preferences stay in `secretary_rules`: those only make triage quieter or louder.
create table owner_rules (
    id            bigserial primary key,
    -- One line, no control characters (checked by the orchestrator).
    text          text not null,
    -- The owner's words it was written from, quoted on its card.
    because       text not null,
    -- The rule this one replaced, if any. A rule is replaced at most once: the chain is linear.
    supersedes    bigint unique references owner_rules(id),
    -- False once replaced or removed; the record stays, so the chain can be read back.
    active        boolean not null default true,
    created_at    timestamptz not null default now(),
    -- When it stopped holding (replaced or removed).
    ended_at      timestamptz,
    -- Its card in the Rules topic.
    tg_msg        bigint
);
create index owner_rules_live on owner_rules(id) where active;

-- The owner rules a habit follows. A habit whose brief states what to do on an outward action
-- must cite at least one; replacing or removing a cited rule pauses the habit.
alter table habits add column rules bigint[] not null default '{}';
