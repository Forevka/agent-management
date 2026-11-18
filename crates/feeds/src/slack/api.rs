//! A thin, typed client for the handful of Slack Web API methods the feed uses.
//!
//! Every method is called with the user's token in the `Authorization` header (never in the URL,
//! where proxies and logs would see it). Responses are parsed leniently: Slack adds fields freely
//! and omits empty ones, so every field is optional and a malformed message is skipped rather
//! than failing the whole batch.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, RETRY_AFTER};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ConversationKind, FeedError};

/// What to wait when Slack rate limits without saying for how long.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Caps a single request, so one hung connection cannot stall the orchestrator's poll loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A Slack token (the user's `xoxp-`, or the app-level `xapp-`). Its `Debug` is redacted so that it cannot leak through a log line or panic
/// message that prints the source.
#[derive(Clone)]
pub struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct Api {
    /// `Err` holds why the HTTP client could not be built (a broken TLS installation). Every call
    /// then fails with that reason instead of the constructor panicking.
    client: Result<reqwest::Client, String>,
    base: String,
    token: Token,
}

impl Api {
    pub fn new(token: String, base: String) -> Self {
        // reqwest is built without a bundled crypto provider; ring is the one the rest of the
        // process uses. Installing fails harmlessly if a provider is already set.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot build http client: {e}"));
        Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token: Token(token),
        }
    }

    pub fn set_base(&mut self, base: &str) {
        self.base = base.trim_end_matches('/').to_owned();
    }

    fn client(&self) -> Result<&reqwest::Client, FeedError> {
        self.client.as_ref().map_err(|e| FeedError::Http(e.clone()))
    }

    /// Calls a read method with query parameters.
    pub async fn get<T: DeserializeOwned>(
        &self,
        method: &str,
        query: &[(&str, &str)],
    ) -> Result<T, FeedError> {
        tracing::trace!(method, "slack api call");
        let req = self
            .client()?
            .get(format!("{}/{method}", self.base))
            .header(AUTHORIZATION, format!("Bearer {}", self.token.0))
            .query(query);
        self.finish(method, req).await
    }

    /// Calls a write method with a JSON body.
    pub async fn post_json<T: DeserializeOwned>(
        &self,
        method: &str,
        body: &Value,
    ) -> Result<T, FeedError> {
        tracing::trace!(method, "slack api call");
        let req = self
            .client()?
            .post(format!("{}/{method}", self.base))
            .header(AUTHORIZATION, format!("Bearer {}", self.token.0))
            .json(body);
        self.finish(method, req).await
    }

    async fn finish<T: DeserializeOwned>(
        &self,
        method: &str,
        req: reqwest::RequestBuilder,
    ) -> Result<T, FeedError> {
        // `without_url` keeps channel ids and cursors out of error strings; the token is in a
        // header and never part of the error.
        let resp = req
            .send()
            .await
            .map_err(|e| FeedError::Http(e.without_url().to_string()))?;
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let wait = retry_after(resp.headers());
            tracing::debug!(method, ?wait, "slack rate limited (429)");
            return Err(FeedError::RateLimited(wait));
        }
        let headers = resp.headers().clone();
        if !status.is_success() {
            return Err(FeedError::Http(format!("{method}: HTTP {status}")));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| FeedError::Http(e.without_url().to_string()))?;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|e| FeedError::Decode(format!("{method}: {e}")))?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            if error == "ratelimited" || error == "rate_limited" {
                let wait = retry_after(&headers);
                tracing::debug!(method, ?wait, "slack rate limited (ok:false)");
                return Err(FeedError::RateLimited(wait));
            }
            return Err(FeedError::Api(error.to_owned()));
        }
        serde_json::from_value(value).map_err(|e| FeedError::Decode(format!("{method}: {e}")))
    }
}

fn retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

/// Errors that mean the token itself is unusable, so polling other channels is pointless.
pub fn is_fatal(err: &FeedError) -> bool {
    matches!(
        err,
        FeedError::Api(e) if matches!(
            e.as_str(),
            "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" | "token_expired"
        )
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Page {
    /// Kept raw so that one odd message cannot fail the whole page; see [`RawMessage::parse`].
    pub messages: Vec<Value>,
    pub has_more: bool,
    pub response_metadata: Option<ResponseMetadata>,
}

impl Page {
    pub fn next_cursor(&self) -> Option<String> {
        self.response_metadata
            .as_ref()
            .and_then(|m| m.next_cursor.as_deref())
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ResponseMetadata {
    pub next_cursor: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawMessage {
    pub ts: Option<String>,
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub username: Option<String>,
    pub bot_profile: Option<BotProfile>,
    pub subtype: Option<String>,
    pub text: Option<String>,
    pub reply_count: Option<u64>,
    pub latest_reply: Option<String>,
    pub edited: Option<Value>,
    pub files: Vec<RawFile>,
    /// The new version of the message, for a `message_changed` event.
    pub message: Option<Box<RawMessage>>,
    // The fields below only appear on pushed events, never in history.
    /// The conversation, and its kind (`channel`, `group`, `im` or `mpim`).
    pub channel: Option<String>,
    pub channel_type: Option<String>,
    /// The version before an edit (`message_changed`) or deletion (`message_deleted`).
    pub previous_message: Option<Box<RawMessage>>,
    /// The deleted message's ts, for a `message_deleted` event.
    pub deleted_ts: Option<String>,
}

impl RawMessage {
    /// Parses one message; `None` when it is not a message object at all.
    pub fn parse(v: Value) -> Option<Self> {
        match serde_json::from_value(v) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(error = %e, "skipping a slack message that did not parse");
                None
            }
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct BotProfile {
    pub name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawFile {
    pub name: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserInfo {
    pub user: Option<User>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct User {
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub profile: Option<Profile>,
}

impl User {
    /// The name Slack itself shows: the display name, else the real name, else the handle.
    pub fn best_name(&self) -> Option<String> {
        let profile = self.profile.as_ref();
        [
            profile.and_then(|p| p.display_name.as_deref()),
            profile.and_then(|p| p.real_name.as_deref()),
            self.real_name.as_deref(),
            self.name.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|n| !n.is_empty())
        .map(str::to_owned)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub display_name: Option<String>,
    pub real_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConversationInfo {
    pub channel: Option<Conversation>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Conversation {
    pub id: Option<String>,
    pub name: Option<String>,
    pub is_channel: bool,
    pub is_group: bool,
    pub is_im: bool,
    pub is_mpim: bool,
    pub is_private: bool,
    /// For a direct message, the other person.
    pub user: Option<String>,
    /// Whether the token's user is in the conversation (`conversations.info`; never set by
    /// `users.conversations`, which lists only conversations the user is in).
    pub is_member: Option<bool>,
}

impl Conversation {
    pub fn kind(&self) -> ConversationKind {
        if self.is_im {
            ConversationKind::Direct
        } else if self.is_mpim {
            ConversationKind::Group
        } else if self.is_private || self.is_group {
            ConversationKind::Private
        } else if self.is_channel {
            ConversationKind::Channel
        } else {
            ConversationKind::Other
        }
    }
}

/// The kind of a conversation from the `channel_type` of a pushed message event.
pub fn kind_from_channel_type(t: &str) -> ConversationKind {
    match t {
        "im" => ConversationKind::Direct,
        "mpim" => ConversationKind::Group,
        // Slack's historical name for private channels.
        "group" => ConversationKind::Private,
        "channel" => ConversationKind::Channel,
        _ => ConversationKind::Other,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConversationList {
    /// Kept raw, like history pages, so that one odd entry cannot fail the whole page.
    pub channels: Vec<Value>,
    pub response_metadata: Option<ResponseMetadata>,
}

impl ConversationList {
    pub fn next_cursor(&self) -> Option<String> {
        self.response_metadata
            .as_ref()
            .and_then(|m| m.next_cursor.as_deref())
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AuthTest {
    pub url: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConnectionsOpen {
    pub url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Posted {
    pub ts: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_debug_is_redacted() {
        let api = Api::new("xoxp-secret-value".into(), "http://x".into());
        let shown = format!("{api:?}");
        assert!(!shown.contains("secret"), "{shown}");
    }
}
