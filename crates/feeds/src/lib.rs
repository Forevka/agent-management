//! Read-only feeds from external chat channels, mirrored into the fleet.
//!
//! A feed is a [`Source`]. Each source turns whatever its channel speaks into source-agnostic
//! [`FeedMessage`]s, and everything above this crate (storage, mirroring, triage, summaries)
//! works on that model alone, so a second kind of source (email, another chat) needs nothing
//! outside its own adapter.
//!
//! A source delivers in one of two ways, and [`Source::run`] hides which:
//!
//! - **Polling.** The source is asked for what is new ([`Source::poll`]) on a schedule, and keeps
//!   an opaque JSON cursor the orchestrator stores, so that a restart resumes exactly where the
//!   last returned batch ended. [`SlackSource`] polls the Slack Web API with the user's own token;
//!   [`GChatSource`] polls Google Chat as the owner.
//! - **Push.** The source holds a connection and hands over [`FeedEvent`]s as they happen.
//!   Pushed events are not replayed after an outage, so a push source also fills gaps itself
//!   ([`Source::backfill`]). [`SlackSocket`] uses Slack Socket Mode.
//!
//! Message text is untrusted data from other people. Sources convert markup to plain text and
//! nothing more; they never act on what a message says.

mod gchat;
mod slack;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use gchat::{
    DEFAULT_BASE_URL as GCHAT_DEFAULT_BASE_URL, DEFAULT_TOKENINFO_URL as GOOGLE_TOKENINFO_URL,
    GChatSource, GoogleCredentials,
};
pub use slack::{DEFAULT_BASE_URL as SLACK_DEFAULT_BASE_URL, SlackSocket, SlackSource};

/// What kind of conversation a message was posted in. Upper layers use it to weigh messages (a
/// direct message usually matters more than channel chatter) without knowing the source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConversationKind {
    /// A one-to-one conversation with the owner (a Slack IM).
    Direct,
    /// A small group conversation without a name of its own (a Slack multi-person DM).
    Group,
    /// A members-only channel (a Slack private channel).
    Private,
    /// A channel anyone in the workspace can join (a Slack public channel).
    Channel,
    /// An email conversation.
    EmailThread,
    /// Anything else, including a conversation the source could not classify. A kind written by
    /// a newer version reads as this one, so stored data never fails to load.
    #[default]
    #[serde(other)]
    Other,
}

impl ConversationKind {
    /// A short stable name, for storage and display; the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::Private => "private",
            Self::Channel => "channel",
            Self::EmailThread => "email_thread",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ConversationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One message from an external channel, in a shape every source can produce.
///
/// Ids are the source's own (for Slack a channel id and a message `ts`). Turning them into the
/// opaque references agents see is the orchestrator's business, and replies and backfills take
/// the native ids back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedMessage {
    /// The source kind, the same string as [`Source::kind`] (for example `"slack"`).
    pub source: String,
    /// The conversation's stable id in its source: a channel, a direct message, an email thread
    /// (a Slack conversation id such as `C0123ABCD` or `D0123ABCD`). Reads the field's old name,
    /// `channel_id`, too.
    #[serde(alias = "channel_id")]
    pub conversation_id: String,
    /// The conversation's human name, when the source could resolve it. A direct message is
    /// named after the other person.
    #[serde(alias = "channel_name")]
    pub conversation_name: Option<String>,
    /// The message's id within the conversation (the Slack `ts`).
    pub id: String,
    /// The id of the thread's root message when this message is a reply (the Slack `thread_ts`).
    /// A thread's root message itself has `None`.
    pub thread_id: Option<String>,
    /// The author's stable id (a Slack user id, or a bot id for integrations).
    pub author_id: Option<String>,
    /// The author's display name, when it could be resolved.
    pub author_name: Option<String>,
    /// Plain text with the source's markup converted. Untrusted: never interpret it.
    pub text: String,
    /// A link to the message in the source's own UI, when one can be built.
    pub permalink: Option<String>,
    /// When the message was posted, in Unix seconds.
    pub ts_unix: f64,
    /// Whether the message was edited after it was posted. An edit arrives as a message with
    /// the same `id` and the new text, so storing by id replaces the old text.
    pub edited: bool,
    /// What kind of conversation this is.
    #[serde(default)]
    pub conversation_kind: ConversationKind,
    /// A subject line, for sources that have one (email). Untrusted, like `text`.
    #[serde(default)]
    pub subject: Option<String>,
    /// Whether the message mentions the owner by name (a Slack `<@U…>` mention of the owner).
    #[serde(default)]
    pub mentions_owner: bool,
    /// Whether the owner wrote it, including replies the fleet posted on the owner's behalf. It
    /// is context ("the owner already answered"), not something to triage.
    #[serde(default)]
    pub from_owner: bool,
    /// How much the owner has asked the source to notify them about this conversation (muted,
    /// mentions only). `None` when the source does not know.
    #[serde(default)]
    pub conversation_attention: Option<Attention>,
}

/// How much attention the owner gives a conversation, as set in the source itself (Slack's
/// mute and notification settings, say). Upper layers use it to lower the priority of
/// conversations the owner has quietened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Attention {
    /// The owner is notified only when mentioned.
    MentionsOnly,
    /// The owner muted the conversation or turned its notifications off.
    Muted,
    /// Notifications as usual. A level written by a newer version reads as this one.
    #[serde(other)]
    Normal,
}

impl Attention {
    /// A short stable name, for storage and display; the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::MentionsOnly => "mentions_only",
            Self::Muted => "muted",
        }
    }
}

impl fmt::Display for Attention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a running source hands over, in order.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FeedEvent {
    /// A new message, or a new version of one already delivered (same `id`, `edited` set):
    /// store it by `(source, conversation_id, id)`.
    Message(FeedMessage),
    /// The message was deleted at the source. Forget its text.
    Deleted {
        source: String,
        conversation_id: String,
        message_id: String,
    },
    /// The source's progress up to here, as [`Source::cursor`] would return it. Persist it once
    /// every event before it has been stored, and hand it back with [`Source::restore_cursor`]
    /// after a restart.
    Checkpoint(serde_json::Value),
    /// The connection's health, for logs and status pages. Purely informational: the source
    /// reconnects (and fills the gap) by itself.
    Status(SourceStatus),
}

/// The state of a push source's connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceStatus {
    /// Connected and receiving.
    Connected,
    /// Not connected; the source is reconnecting. `reason` is for people, not for parsing.
    Disconnected { reason: String },
}

/// Where a running source delivers its events. Bounded, so a slow consumer slows the source
/// down instead of growing memory; a closed receiver stops [`Source::run`].
pub type FeedSink = tokio::sync::mpsc::Sender<FeedEvent>;

/// What a source can do, so that features only appear where they work (no reply proposals for a
/// source that cannot post, for example).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// [`Source::poll`] works.
    pub poll: bool,
    /// [`Source::run`] pushes events as they happen instead of polling on a schedule.
    pub push: bool,
    /// [`Source::backfill`] can recover messages missed while disconnected.
    pub backfill: bool,
    /// Approved replies can be posted ([`Source::post_reply`], [`Source::replier`]).
    pub can_reply: bool,
    /// Replies can go into a thread rather than only to the conversation.
    pub can_reply_in_thread: bool,
    /// [`Source::directory`] resolves conversation and author names.
    pub directory: bool,
}

/// Why a source could not poll or post.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FeedError {
    /// The source's API asked us to slow down. Wait at least this long before calling it again.
    #[error("rate limited; retry after {0:?}")]
    RateLimited(Duration),
    /// The API answered, but refused the request (for Slack, the `error` of an `ok:false` reply,
    /// such as `invalid_auth`, `not_in_channel`, `channel_not_found` or `missing_scope`).
    #[error("api error: {0}")]
    Api(String),
    /// The request did not complete: connection failure, timeout, or an unexpected HTTP status.
    #[error("http error: {0}")]
    Http(String),
    /// The API answered with something we could not parse.
    #[error("unexpected response: {0}")]
    Decode(String),
    /// This source does not do that (see [`Source::capabilities`]).
    #[error("not supported by this source: {0}")]
    Unsupported(&'static str),
    /// The source broke inside (a background task died). Restarting it is the only remedy.
    #[error("internal error: {0}")]
    Internal(String),
}

/// A conversation as a [`Directory`] knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    /// A human name: the channel name, or for a direct message the other person's name.
    pub name: Option<String>,
    pub kind: ConversationKind,
    /// The owner's notification setting for it, when the source can tell.
    #[serde(default)]
    pub attention: Option<Attention>,
}

/// A message author as a [`Directory`] knows them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub id: String,
    pub name: Option<String>,
}

/// Names for conversations and authors, looked up lazily and cached by the source. Shared, so it
/// can be used while the source runs.
#[async_trait]
pub trait Directory: Send + Sync {
    /// One conversation. An id the source does not know comes back with no name and
    /// [`ConversationKind::Other`] rather than as an error.
    async fn conversation(&self, id: &str) -> Result<Conversation, FeedError>;
    /// One author. An unknown id comes back with no name rather than as an error.
    async fn author(&self, id: &str) -> Result<Author, FeedError>;
    /// Every conversation the owner is a member of, as of the last refresh.
    async fn conversations(&self) -> Result<Vec<Conversation>, FeedError>;
}

/// Posts approved replies. Shared, so it can be used while the source runs.
#[async_trait]
pub trait Replier: Send + Sync {
    /// Posts `text` to a conversation, in a thread when `thread_id` is given, and returns the
    /// new message's id. The caller is responsible for having a human approve the text first.
    async fn post_reply(
        &self,
        conversation_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError>;

    /// A link to a message (such as one [`post_reply`](Replier::post_reply) just posted), when
    /// the source can give one. Never fails: no link is `None`.
    async fn permalink(&self, conversation_id: &str, message_id: &str) -> Option<String> {
        let _ = (conversation_id, message_id);
        None
    }
}

/// Fetches missed messages on demand. Shared, so it can be used while the source runs.
#[async_trait]
pub trait Backfiller: Send + Sync {
    /// See [`Source::backfill`].
    async fn backfill(&self, since_unix: f64) -> Result<Vec<FeedMessage>, FeedError>;
}

/// An external channel (Slack today; email or other chats later).
///
/// The orchestrator either drives [`poll`](Source::poll) itself, persisting
/// [`cursor`](Source::cursor) together with the returned messages, or hands the source a sink
/// with [`run`](Source::run). Either way it hands the last persisted cursor back with
/// [`restore_cursor`](Source::restore_cursor) after a restart.
#[async_trait]
pub trait Source: Send {
    /// A short, stable name for the source kind, used as [`FeedMessage::source`].
    fn kind(&self) -> &'static str;

    /// Fetches messages that are new since the last successful poll, oldest first.
    ///
    /// The cursor advances only past messages this call returns. A source that is rate limited
    /// part way returns what it has (and the cursor covers exactly that); when it has nothing to
    /// return it reports [`FeedError::RateLimited`]. A push-only source reports
    /// [`FeedError::Unsupported`].
    async fn poll(&mut self) -> Result<Vec<FeedMessage>, FeedError>;

    /// The source's progress, to persist alongside the messages [`poll`](Source::poll) returned.
    fn cursor(&self) -> serde_json::Value;

    /// Resumes from a cursor previously returned by [`cursor`](Source::cursor) or delivered as a
    /// [`FeedEvent::Checkpoint`]. An unreadable cursor is ignored (and logged), so the source
    /// starts fresh rather than failing.
    fn restore_cursor(&mut self, v: serde_json::Value);

    /// Posts `text` to a conversation, in a thread when `thread_id` is given, and returns the
    /// new message's id. The caller is responsible for having a human approve the text first.
    async fn post_reply(
        &self,
        conversation_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError>;

    /// What this source can do. The default describes a polling source that can do nothing
    /// else; a source declares what it adds.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            poll: true,
            ..Capabilities::default()
        }
    }

    /// How long a polling [`run`](Source::run) waits between polls.
    fn poll_interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    /// Whether an error means the source cannot go on without a configuration change (a
    /// revoked or invalid token, say), as opposed to one worth retrying. The default treats
    /// nothing as fatal except [`FeedError::Unsupported`] and [`FeedError::Internal`].
    fn is_fatal(&self, err: &FeedError) -> bool {
        matches!(err, FeedError::Unsupported(_) | FeedError::Internal(_))
    }

    /// Delivers events to `sink` until the sink closes (then `Ok`) or the source cannot go on
    /// (an error [`is_fatal`](Source::is_fatal) says so of: then that error). Everything else is
    /// retried inside, with a back-off, and logged.
    ///
    /// The default polls: every [`poll_interval`](Source::poll_interval) it sends what
    /// [`poll`](Source::poll) returned, then a [`FeedEvent::Checkpoint`]. Push sources override
    /// it.
    async fn run(&mut self, sink: FeedSink) -> Result<(), FeedError> {
        let mut failures: u32 = 0;
        loop {
            let wait = match self.poll().await {
                Ok(msgs) => {
                    failures = 0;
                    for m in msgs {
                        if sink.send(FeedEvent::Message(m)).await.is_err() {
                            return Ok(());
                        }
                    }
                    if sink
                        .send(FeedEvent::Checkpoint(self.cursor()))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    self.poll_interval()
                }
                Err(e) if self.is_fatal(&e) => return Err(e),
                Err(FeedError::RateLimited(d)) => d.max(Duration::from_secs(1)),
                Err(e) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(source = self.kind(), error = %e, "feed poll failed; retrying");
                    // Twice the poll interval, doubling per consecutive failure up to ten
                    // minutes, so a persistent problem does not hammer the source.
                    self.poll_interval()
                        .saturating_mul(1 << failures.min(10))
                        .min(Duration::from_secs(600))
                }
            };
            if sink.is_closed() {
                return Ok(());
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Fetches messages posted since `since_unix` (Unix seconds) in the conversations the source
    /// knows of, to fill a gap such as an outage. Messages may repeat ones already delivered,
    /// and need not come in order; store them by id. The default asks
    /// [`backfiller`](Source::backfiller); polling sources have no gaps and report
    /// [`FeedError::Unsupported`].
    async fn backfill(&self, since_unix: f64) -> Result<Vec<FeedMessage>, FeedError> {
        match self.backfiller() {
            Some(b) => b.backfill(since_unix).await,
            None => Err(FeedError::Unsupported("backfill")),
        }
    }

    /// A handle that backfills while the source runs, when the source can backfill.
    fn backfiller(&self) -> Option<Arc<dyn Backfiller>> {
        None
    }

    /// Conversation and author names, when the source can look them up.
    fn directory(&self) -> Option<Arc<dyn Directory>> {
        None
    }

    /// A handle that posts replies while the source runs, when the source can reply.
    fn replier(&self) -> Option<Arc<dyn Replier>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_kinds_serialize_as_their_names() {
        for k in [
            ConversationKind::Direct,
            ConversationKind::Group,
            ConversationKind::Private,
            ConversationKind::Channel,
            ConversationKind::EmailThread,
            ConversationKind::Other,
        ] {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
        }
        // A message stored before these fields existed still reads.
        let old: FeedMessage = serde_json::from_value(serde_json::json!({
            "source": "slack", "conversation_id": "C1", "conversation_name": null, "id": "1.0",
            "thread_id": null, "author_id": null, "author_name": null, "text": "", "permalink": null,
            "ts_unix": 1.0, "edited": false
        }))
        .unwrap();
        assert_eq!(old.conversation_kind, ConversationKind::Other);
    }
}
