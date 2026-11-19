-- What the owner keeps doing that the fleet could do for them: a habit the orchestrator spotted
-- in their own decisions, proposed once, and honours until they pause it. See docs/plan.md
-- ("Habits") and habits.rs. A habit is only ever created by an approved `admin_change`, so
-- nothing here is decided without the owner.
create table habits (
    id           bigserial primary key,
    -- 'decide': answer an item of a known shape the way the owner always answers it.
    -- 'do':     make a standing request of theirs on a schedule, instead of waiting to be asked.
    kind         text not null check (kind in ('decide', 'do')),
    project      text references projects(slug) on delete cascade,
    -- decide: which items it covers and what it answers. Only plans for now: a plan's blast
    -- radius is one task. Widening this is a migration of its own, deliberately.
    item_kind    text check (item_kind in ('plan')),
    -- A `like` pattern over the task title's shape (lowercased, digits collapsed to '#'):
    -- 'review mr !#%' covers "Review MR !996" and "Review MR !1021".
    title_like   text,
    decision     text check (decision in ('approve')),
    -- do: 'every:<N>m' | 'every:<N>h' | 'daily:HH:MM' | 'weekly:<dow>:HH:MM'.
    schedule     text,
    -- The standing request, in the owner's own words.
    brief        text,
    -- Both: what it was learned from, quoted, so its card can say why it exists.
    why          text not null,
    active       boolean not null default true,
    paused_why   text,
    -- How many times it may act in a day, so a mistake cannot run away.
    cap          integer not null default 10,
    created_at   timestamptz not null default now(),
    last_used_at timestamptz,
    -- Its card in the Rules topic, edited in place as it is paused or resumed.
    tg_msg       bigint,
    -- A habit of each kind needs its own fields, and nothing else's.
    constraint habit_decides check (kind <> 'decide' or (project is not null and item_kind is not null
                                    and title_like is not null and decision is not null)),
    constraint habit_does check (kind <> 'do' or (schedule is not null and brief is not null))
);
create index habits_live on habits(kind) where active;

-- Every time a habit acted, so the owner can see what it has been doing and the daily cap can be
-- counted without a resettable counter. `at` is the column the retention sweep prunes by.
create table habit_uses (
    id       bigserial primary key,
    habit_id bigint not null references habits(id) on delete cascade,
    at       timestamptz not null default now(),
    task_id  text,
    item_id  bigint,
    -- What it did, in one line, for the card and the digest.
    what     text not null default ''
);
create index habit_uses_recent on habit_uses(habit_id, at desc);
