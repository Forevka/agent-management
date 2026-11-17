//! Read-only feeds from external chat channels, mirrored into the fleet.
//!
//! A feed is a [`Source`] the orchestrator polls on its own schedule. Each source turns whatever
//! its channel speaks into source-agnostic [`FeedMessage`]s, and exposes an opaque JSON cursor the
//! orchestrator stores in its database, so that a restart resumes exactly where the last returned
//! batch ended: no duplicates, no gaps.
//!
//! The only source so far is [`SlackSource`]. It works with nothing but the user's own token,
//! because the user cannot install a Slack app: there is no Events API and no Socket Mode, so it
//! polls the Web API, sees only the channels the user is a member of, and posts approved replies
//! as the user.
//!
//! Message text is untrusted data from other people. Sources convert markup to plain text and
//! nothing more; they never act on what a message says.

mod slack;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use slack::{DEFAULT_BASE_URL as SLACK_DEFAULT_BASE_URL, SlackSource};

/// One message from an external channel, in a shape every source can produce.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedMessage {
    /// The source kind, the same string as [`Source::kind`] (for example `"slack"`).
    pub source: String,
    /// The channel's stable id in its source (a Slack channel id such as `C0123ABCD`).
    pub channel_id: String,
    /// The channel's human name, when the source could resolve it.
    pub channel_name: Option<String>,
    /// The message's id within the channel (the Slack `ts`).
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
    /// Whether the message was edited after it was posted.
    pub edited: bool,
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
}

/// A pollable external channel (Slack today; email or Discord later).
///
/// The orchestrator owns the schedule: it calls [`poll`](Source::poll), persists
/// [`cursor`](Source::cursor) together with the returned messages, and hands the cursor back with
/// [`restore_cursor`](Source::restore_cursor) after a restart.
#[async_trait]
pub trait Source: Send {
    /// A short, stable name for the source kind, used as [`FeedMessage::source`].
    fn kind(&self) -> &'static str;

    /// Fetches messages that are new since the last successful poll, oldest first.
    ///
    /// The cursor advances only past messages this call returns. A source that is rate limited
    /// part way returns what it has (and the cursor covers exactly that); when it has nothing to
    /// return it reports [`FeedError::RateLimited`].
    async fn poll(&mut self) -> Result<Vec<FeedMessage>, FeedError>;

    /// The source's progress, to persist alongside the messages [`poll`](Source::poll) returned.
    fn cursor(&self) -> serde_json::Value;

    /// Resumes from a cursor previously returned by [`cursor`](Source::cursor). An unreadable
    /// cursor is ignored (and logged), so the source starts fresh rather than failing.
    fn restore_cursor(&mut self, v: serde_json::Value);

    /// Posts `text` to a channel, in a thread when `thread_id` is given, and returns the new
    /// message's id. The caller is responsible for having a human approve the text first.
    async fn post_reply(
        &self,
        channel_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError>;
}
