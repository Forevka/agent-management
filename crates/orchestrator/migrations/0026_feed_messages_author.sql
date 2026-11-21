-- Messages by author: how the people records find a contact's messages (when they were last in
-- touch, whether an account is the owner's). Add-only; the previous release ignores it.
create index feed_messages_author on feed_messages (source, author_id, ts);
