-- How much attention the owner gives a conversation in its source (Slack lets them mute a
-- channel or ask for mentions only), when the source can tell. Null: unknown. The secretary
-- treats muted conversations as low priority, and they never hurry a batch.
alter table feed_conversations add column attention text
    check (attention in ('normal', 'mentions_only', 'muted'));
