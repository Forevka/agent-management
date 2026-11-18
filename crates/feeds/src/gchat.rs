//! Google Chat, polled through the Chat REST API as the owner (user authentication).
//!
//! The owner provides an "authorized user" OAuth credential (client id and secret, and a
//! refresh token) with the scopes `chat.spaces.readonly`, `chat.messages.readonly` and
//! `chat.messages.create`. There is no push: this is a polling [`Source`], designed around the
//! API as documented (developers.google.com/workspace/chat/api/reference/rest):
//!
//! - `spaces.list` returns only the spaces the caller is a member of (group chats and direct
//!   messages appear once they have a message), up to 1000 a page, each with `lastActiveTime`,
//!   "the timestamp of the last message in the space". One request per poll lists them all for
//!   a typical owner, and membership needs no separate check.
//! - Only spaces whose `lastActiveTime` is newer than the space's cursor get a
//!   `spaces.messages.list` with `filter=createTime > "<cursor>"`, which lists oldest first by
//!   default. A quiet poll therefore costs one request.
//! - Quotas: 3000 space reads and 3000 message reads a minute per project, and 15 reads a
//!   second per space. Requests are spaced 100 ms apart, well inside both.
//!
//! Owner identity, for `from_owner` and `mentions_owner`: a Chat user is `users/{id}`, where
//! the id is the person's Google account id, which is what Google's token-info endpoint calls
//! `sub`. It is read once from there. Should that not answer, the owner is recognised as the one
//! sender found in two different direct-message spaces: every direct message is between the
//! owner and one other person, so nobody else can appear in two of them. It can also be set.
//!
//! Sender names: the API fills a sender's `displayName` under user authentication when the
//! sender is a member of the space or has prior affinity with the owner, which covers the
//! owner's own spaces; otherwise only `users/{id}` is known.
//!
//! What is not covered: edits and deletions of messages older than a space's cursor (the
//! `createTime` filter cannot see them), and per-conversation notification settings (no scope
//! exposes them), so `conversation_attention` is always `None`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::slack::pace::Gate;
use crate::{
    Author, Capabilities, Conversation, ConversationKind, Directory, FeedError, FeedMessage,
    Replier, Source,
};

/// The Chat API root.
pub const DEFAULT_BASE_URL: &str = "https://chat.googleapis.com";
/// Google's token-info endpoint, used once to learn the owner's account id.
pub const DEFAULT_TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

const KIND: &str = "gchat";
/// A token is renewed this long before it expires, so a request never goes out with one that
/// lapses on the way.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// When Google rate limits without saying for how long.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Token-endpoint errors that no retry fixes: the owner has to authorise again.
const FATAL_AUTH: &[&str] = &["invalid_grant", "invalid_client", "unauthorized_client"];

/// The owner's "authorized user" OAuth credential.
#[derive(Clone, Deserialize)]
pub struct GoogleCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    DEFAULT_TOKEN_URI.to_owned()
}

impl GoogleCredentials {
    /// A credential for Google's standard token endpoint.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            token_uri: default_token_uri(),
        }
    }

    /// Reads the JSON Google's tools write for an authorized user (`"type": "authorized_user"`,
    /// `client_id`, `client_secret`, `refresh_token`, optionally `token_uri`).
    pub fn from_json(json: &str) -> Result<Self, FeedError> {
        serde_json::from_str(json)
            .map_err(|e| FeedError::Decode(format!("google credentials: {e}")))
    }
}

impl fmt::Debug for GoogleCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("token_uri", &self.token_uri)
            .finish()
    }
}

/// An access token. Its `Debug` is redacted.
#[derive(Clone)]
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Everything behind these locks stays consistent after a panic elsewhere.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The HTTP side: token refresh and Chat API calls, shared by the source, its directory and
/// its replier.
#[derive(Debug)]
struct Client {
    http: Result<reqwest::Client, String>,
    base: String,
    tokeninfo_url: String,
    creds: GoogleCredentials,
    /// The current access token and when it expires. A tokio mutex, held across the refresh,
    /// so concurrent callers share one refresh.
    token: tokio::sync::Mutex<Option<(Secret, Instant)>>,
    gate: Gate,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl Client {
    fn http(&self) -> Result<&reqwest::Client, FeedError> {
        self.http.as_ref().map_err(|e| FeedError::Http(e.clone()))
    }

    /// A valid access token, refreshed when it is missing or about to expire.
    async fn access_token(&self) -> Result<Secret, FeedError> {
        let mut slot = self.token.lock().await;
        if let Some((t, expires)) = &*slot
            && *expires > Instant::now() + TOKEN_MARGIN
        {
            return Ok(t.clone());
        }
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", self.creds.client_id.as_str()),
            ("client_secret", self.creds.client_secret.as_str()),
            ("refresh_token", self.creds.refresh_token.as_str()),
        ];
        let resp = self
            .http()?
            .post(&self.creds.token_uri)
            .form(&form)
            .send()
            .await
            .map_err(|e| FeedError::Http(e.without_url().to_string()))?;
        let status = resp.status();
        let retry = retry_after(resp.headers());
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(FeedError::RateLimited(retry));
        }
        if !status.is_success() {
            // OAuth errors are `{"error": "invalid_grant", "error_description": …}`.
            return Err(match body.get("error").and_then(Value::as_str) {
                Some(code) if status.is_client_error() => FeedError::Api(code.to_owned()),
                _ => FeedError::Http(format!("token refresh: HTTP {status}")),
            });
        }
        let token: TokenResponse = serde_json::from_value(body)
            .map_err(|e| FeedError::Decode(format!("token refresh: {e}")))?;
        let lifetime = Duration::from_secs(token.expires_in.unwrap_or(3600));
        let secret = Secret(token.access_token);
        *slot = Some((secret.clone(), Instant::now() + lifetime));
        Ok(secret)
    }

    async fn forget_token(&self) {
        *self.token.lock().await = None;
    }

    /// One Chat API call, paced. A 401 drops the token and tries once more with a fresh one.
    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<T, FeedError> {
        for attempt in 0..2 {
            self.gate.wait().await;
            let token = self.access_token().await?;
            let mut req = self
                .http()?
                .request(method.clone(), format!("{}/v1/{path}", self.base))
                .timeout(REQUEST_TIMEOUT)
                .bearer_auth(&token.0)
                .query(query);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| FeedError::Http(e.without_url().to_string()))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.forget_token().await;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                let wait = retry_after(resp.headers());
                self.gate.block(wait);
                return Err(FeedError::RateLimited(wait));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| FeedError::Http(e.without_url().to_string()))?;
            if !status.is_success() {
                // Google API errors: `{"error": {"code", "message", "status": "PERMISSION_DENIED"}}`.
                let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let code = v
                    .pointer("/error/status")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                return Err(match code {
                    Some(c) if status.is_client_error() => FeedError::Api(c),
                    _ => FeedError::Http(format!("chat api: HTTP {status}")),
                });
            }
            let v: Value = if bytes.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&bytes)
                    .map_err(|e| FeedError::Decode(format!("chat api: {e}")))?
            };
            return serde_json::from_value(v)
                .map_err(|e| FeedError::Decode(format!("chat api: {e}")));
        }
        Err(FeedError::Api("UNAUTHENTICATED".into()))
    }

    /// Every space the owner is a member of.
    async fn list_spaces(&self) -> Result<Vec<RawSpace>, FeedError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        // 50 pages of 1000 is far beyond any real owner; it only stops a runaway loop.
        for _ in 0..50 {
            let mut query = vec![("pageSize", "1000")];
            if let Some(t) = &token {
                query.push(("pageToken", t.as_str()));
            }
            let page: SpacePage = self
                .call(reqwest::Method::GET, "spaces", &query, None)
                .await?;
            out.extend(page.spaces);
            token = page.next_page_token.filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// The owner's account id from Google's token-info endpoint (`sub`), as `users/{id}`. The
    /// token goes in the body, never the URL.
    async fn owner_from_tokeninfo(&self) -> Result<Option<String>, FeedError> {
        let token = self.access_token().await?;
        let resp = self
            .http()?
            .post(&self.tokeninfo_url)
            .timeout(REQUEST_TIMEOUT)
            .form(&[("access_token", token.0.as_str())])
            .send()
            .await
            .map_err(|e| FeedError::Http(e.without_url().to_string()))?;
        if !resp.status().is_success() {
            return Err(FeedError::Http(format!(
                "tokeninfo: HTTP {}",
                resp.status()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| FeedError::Decode(format!("tokeninfo: {e}")))?;
        Ok(v.get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| format!("users/{s}")))
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SpacePage {
    spaces: Vec<RawSpace>,
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawSpace {
    name: String,
    space_type: Option<String>,
    display_name: Option<String>,
    last_active_time: Option<String>,
    space_uri: Option<String>,
    space_threading_state: Option<String>,
}

impl RawSpace {
    fn kind(&self) -> ConversationKind {
        match self.space_type.as_deref() {
            Some("DIRECT_MESSAGE") => ConversationKind::Direct,
            Some("GROUP_CHAT") => ConversationKind::Group,
            Some("SPACE") => ConversationKind::Channel,
            _ => ConversationKind::Other,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MessagePage {
    messages: Vec<Value>,
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawMessage {
    name: String,
    sender: Option<RawUser>,
    create_time: Option<String>,
    last_update_time: Option<String>,
    delete_time: Option<String>,
    text: Option<String>,
    annotations: Vec<RawAnnotation>,
    thread: Option<RawThread>,
    attachment: Vec<RawAttachment>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawUser {
    name: String,
    display_name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawAnnotation {
    #[serde(rename = "type")]
    kind: Option<String>,
    user_mention: Option<RawUserMention>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUserMention {
    user: Option<RawUser>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawThread {
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawAttachment {
    content_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Posted {
    name: String,
}

/// What the directory knows about one space.
#[derive(Debug, Clone)]
struct SpaceEntry {
    kind: ConversationKind,
    display_name: Option<String>,
    uri: Option<String>,
    threaded: bool,
}

/// Names for spaces and people, and the owner's identity, shared with the source.
#[derive(Debug)]
struct GDirectory {
    client: Arc<Client>,
    state: Mutex<DirState>,
}

#[derive(Debug, Default)]
struct DirState {
    spaces: HashMap<String, SpaceEntry>,
    listed: bool,
    users: HashMap<String, String>,
    owner: Option<String>,
    /// Human senders seen per direct-message space: to name the space after the other person,
    /// and to recognise the owner.
    dm_people: HashMap<String, HashSet<String>>,
}

impl GDirectory {
    fn state(&self) -> MutexGuard<'_, DirState> {
        lock(&self.state)
    }

    fn owner(&self) -> Option<String> {
        self.state().owner.clone()
    }

    fn update_spaces(&self, spaces: &[RawSpace]) {
        let mut st = self.state();
        st.spaces = spaces
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    SpaceEntry {
                        kind: s.kind(),
                        display_name: s.display_name.clone().filter(|n| !n.is_empty()),
                        uri: s.space_uri.clone().filter(|u| !u.is_empty()),
                        // Unknown threading counts as threaded: a thread id is harmless where
                        // replies cannot use it, and needed where they can.
                        threaded: s.space_threading_state.as_deref() != Some("UNTHREADED_MESSAGES")
                            && s.space_threading_state.as_deref() != Some("GROUPED_MESSAGES"),
                    },
                )
            })
            .collect();
        st.listed = true;
    }

    /// Learns names from a message's sender, and the owner from direct messages: the one
    /// human sender found in two different direct-message spaces.
    fn learn(&self, space: &str, kind: ConversationKind, sender: &RawUser) {
        let mut st = self.state();
        if let Some(n) = sender.display_name.as_ref().filter(|n| !n.is_empty()) {
            st.users.insert(sender.name.clone(), n.clone());
        }
        if kind != ConversationKind::Direct || sender.kind.as_deref() == Some("BOT") {
            return;
        }
        st.dm_people
            .entry(space.to_owned())
            .or_default()
            .insert(sender.name.clone());
        if st.owner.is_none() {
            let spaces = st
                .dm_people
                .values()
                .filter(|people| people.contains(&sender.name))
                .count();
            if spaces >= 2 {
                tracing::info!("google chat: recognised the owner from their direct messages");
                st.owner = Some(sender.name.clone());
            }
        }
    }

    fn conversation_of(&self, id: &str) -> Conversation {
        let st = self.state();
        let entry = st.spaces.get(id);
        let kind = entry.map(|e| e.kind).unwrap_or_default();
        let name = match kind {
            // Named after the other person, once the owner is known and they have written.
            ConversationKind::Direct => Some(
                st.owner
                    .as_ref()
                    .and_then(|owner| {
                        st.dm_people
                            .get(id)?
                            .iter()
                            .filter(|u| *u != owner)
                            .find_map(|u| st.users.get(u).cloned())
                    })
                    .unwrap_or_else(|| "Direct message".to_owned()),
            ),
            _ => entry.and_then(|e| e.display_name.clone()),
        };
        Conversation {
            id: id.to_owned(),
            name,
            kind,
            attention: None,
        }
    }
}

#[async_trait]
impl Directory for GDirectory {
    async fn conversation(&self, id: &str) -> Result<Conversation, FeedError> {
        Ok(self.conversation_of(id))
    }

    async fn author(&self, id: &str) -> Result<Author, FeedError> {
        Ok(Author {
            id: id.to_owned(),
            name: self.state().users.get(id).cloned(),
        })
    }

    async fn conversations(&self) -> Result<Vec<Conversation>, FeedError> {
        if !self.state().listed {
            let spaces = self.client.list_spaces().await?;
            self.update_spaces(&spaces);
        }
        let ids: Vec<String> = self.state().spaces.keys().cloned().collect();
        Ok(ids.iter().map(|id| self.conversation_of(id)).collect())
    }
}

/// Posts replies as the owner.
#[derive(Debug)]
struct GReplier {
    client: Arc<Client>,
    dir: Arc<GDirectory>,
}

#[async_trait]
impl Replier for GReplier {
    async fn post_reply(
        &self,
        conversation_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError> {
        post(&self.client, &self.dir, conversation_id, thread_id, text).await
    }

    /// Google documents no link to a single message; the space's own link is the closest.
    async fn permalink(&self, conversation_id: &str, _message_id: &str) -> Option<String> {
        self.dir
            .state()
            .spaces
            .get(conversation_id)
            .and_then(|e| e.uri.clone())
    }
}

/// `spaces.messages.create` as the owner. A thread is only named where Google supports
/// replying into one (named spaces); elsewhere the message simply goes to the conversation.
///
/// `thread` is a thread name (`spaces/…/threads/…`, what messages carry as `thread_id`), or a
/// message name (`spaces/…/messages/…`) to reply in that message's thread, which is looked up.
async fn post(
    client: &Client,
    dir: &GDirectory,
    space: &str,
    thread: Option<&str>,
    text: &str,
) -> Result<String, FeedError> {
    let threaded = dir
        .state()
        .spaces
        .get(space)
        .is_none_or(|e| e.kind == ConversationKind::Channel && e.threaded);
    let mut thread_name = thread.filter(|_| threaded).map(str::to_owned);
    if let Some(t) = thread_name.clone()
        && t.contains("/messages/")
    {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Found {
            thread: Option<RawThread>,
        }
        // A failed lookup only costs the thread: the reply then starts its own.
        thread_name = client
            .call::<Found>(reqwest::Method::GET, &t, &[], None)
            .await
            .ok()
            .and_then(|m| m.thread)
            .and_then(|th| th.name);
    }
    let mut body = json!({ "text": text });
    let mut query = Vec::new();
    if let Some(t) = thread_name {
        body["thread"] = json!({ "name": t });
        query.push(("messageReplyOption", "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"));
    }
    let posted: Posted = client
        .call(
            reqwest::Method::POST,
            &format!("{space}/messages"),
            &query,
            Some(&body),
        )
        .await?;
    if posted.name.is_empty() {
        return Err(FeedError::Decode("messages.create: no name".into()));
    }
    Ok(posted.name)
}

/// What the source persists: where watching began, and per space the newest message seen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct GCursor {
    /// Always [`GCursor::MODE`].
    mode: String,
    /// When watching began (RFC 3339). Spaces without their own cursor start here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    /// Space name to the `createTime` of its newest message seen.
    #[serde(default)]
    spaces: BTreeMap<String, String>,
}

impl GCursor {
    const MODE: &'static str = "gchat/1";

    fn new() -> Self {
        Self {
            mode: Self::MODE.to_owned(),
            ..Self::default()
        }
    }
}

/// A [`Source`] for the owner's Google Chat spaces, direct messages and group chats.
#[derive(Debug)]
pub struct GChatSource {
    creds: GoogleCredentials,
    base: String,
    tokeninfo_url: String,
    request_gap: Duration,
    client: Arc<Client>,
    dir: Arc<GDirectory>,
    cursor: GCursor,
    poll_interval: Duration,
    page_size: u32,
    max_pages_per_space: usize,
    owner_checked: bool,
    rate_limited_until: Option<Instant>,
    last_errors: Vec<(String, FeedError)>,
}

fn build_client(
    creds: &GoogleCredentials,
    base: &str,
    tokeninfo_url: &str,
    gap: Duration,
) -> Client {
    // reqwest is built without a bundled crypto provider; ring is the one the rest of the
    // process uses. Installing fails harmlessly if a provider is already set.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("cannot build http client: {e}"));
    Client {
        http,
        base: base.trim_end_matches('/').to_owned(),
        tokeninfo_url: tokeninfo_url.to_owned(),
        creds: creds.clone(),
        token: tokio::sync::Mutex::new(None),
        gate: Gate::new(gap),
    }
}

impl GChatSource {
    /// A source authenticated with the owner's credential. Tokens are only ever sent in
    /// `Authorization` headers or request bodies, and are redacted from `Debug` output.
    pub fn new(creds: GoogleCredentials) -> Self {
        let gap = Duration::from_millis(100);
        let client = Arc::new(build_client(
            &creds,
            DEFAULT_BASE_URL,
            DEFAULT_TOKENINFO_URL,
            gap,
        ));
        Self {
            dir: Arc::new(GDirectory {
                client: client.clone(),
                state: Mutex::new(DirState::default()),
            }),
            client,
            creds,
            base: DEFAULT_BASE_URL.to_owned(),
            tokeninfo_url: DEFAULT_TOKENINFO_URL.to_owned(),
            request_gap: gap,
            cursor: GCursor::new(),
            poll_interval: Duration::from_secs(30),
            page_size: 100,
            max_pages_per_space: 10,
            owner_checked: false,
            rate_limited_until: None,
            last_errors: Vec::new(),
        }
    }

    /// Rebuilds the client and directory after a setting changed, keeping what the directory
    /// knows. Only meant for configuration, before the source is used.
    fn rebuild(&mut self) {
        let state = std::mem::take(&mut *lock(&self.dir.state));
        self.client = Arc::new(build_client(
            &self.creds,
            &self.base,
            &self.tokeninfo_url,
            self.request_gap,
        ));
        self.dir = Arc::new(GDirectory {
            client: self.client.clone(),
            state: Mutex::new(state),
        });
    }

    /// Points the source at other endpoints (mock servers in tests): the Chat API root and the
    /// token-info endpoint. The token endpoint comes from the credential.
    pub fn with_endpoints(mut self, chat_base: &str, tokeninfo_url: &str) -> Self {
        self.base = chat_base.to_owned();
        self.tokeninfo_url = tokeninfo_url.to_owned();
        self.rebuild();
        self
    }

    /// The spacing of API requests (default 100 ms, well inside the per-project quotas).
    pub fn with_request_gap(mut self, gap: Duration) -> Self {
        self.request_gap = gap;
        self.rebuild();
        self
    }
    /// How often [`run`](Source::run) polls (default 30 s).
    pub fn with_poll_interval(mut self, every: Duration) -> Self {
        self.poll_interval = every;
        self
    }

    /// Messages per `spaces.messages.list` page (default 100, at most 1000) and pages per space
    /// per poll (default 10); a busier space continues on the next poll.
    pub fn with_paging(mut self, page_size: u32, max_pages_per_space: usize) -> Self {
        self.page_size = page_size.clamp(1, 1000);
        self.max_pages_per_space = max_pages_per_space.max(1);
        self
    }

    /// Sets the owner's Chat user (`users/{id}`) instead of discovering it.
    pub fn with_owner(self, user: impl Into<String>) -> Self {
        lock(&self.dir.state).owner = Some(user.into());
        Self {
            owner_checked: true,
            ..self
        }
    }

    /// The owner's Chat user, once known.
    pub fn owner(&self) -> Option<String> {
        self.dir.owner()
    }

    /// Per-space problems from the last poll that did not stop it, as `(space, error)`.
    pub fn last_errors(&self) -> &[(String, FeedError)] {
        &self.last_errors
    }

    /// Learns the owner once from token-info; on failure the direct-message rule takes over.
    async fn ensure_owner(&mut self) -> Result<(), FeedError> {
        if self.owner_checked {
            return Ok(());
        }
        match self.client.owner_from_tokeninfo().await {
            Ok(Some(owner)) => {
                lock(&self.dir.state).owner.get_or_insert(owner);
                self.owner_checked = true;
            }
            Ok(None) => {
                tracing::warn!(
                    "google chat: token-info gave no account id; the owner is recognised from direct messages instead"
                );
                self.owner_checked = true;
            }
            Err(e) if self.is_fatal(&e) => return Err(e),
            Err(e) => {
                // Retried on the next poll; nothing waits on it.
                tracing::debug!(error = %e, "google chat: token-info failed");
            }
        }
        Ok(())
    }

    /// The sender of a direct-message space's single most recent message, or `None` if it has
    /// none (or the fetch fails, logged and treated the same as none: the space is simply left
    /// for in-band learning).
    async fn latest_sender(&self, space: &str) -> Option<RawUser> {
        let page: MessagePage = match self
            .client
            .call(
                reqwest::Method::GET,
                &format!("{space}/messages"),
                &[("pageSize", "1"), ("orderBy", "createTime desc")],
                None,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(space, error = %e, "google chat: could not peek a direct message space while learning the owner");
                return None;
            }
        };
        page.messages
            .into_iter()
            .find_map(|v| serde_json::from_value::<RawMessage>(v).ok())
            .and_then(|m| m.sender)
    }

    /// Learns the owner from direct messages before the cursor starts, so every message
    /// delivered from then on is tagged correctly from the first one: once `cursor.start` is
    /// set, `to_feed_message` reads whatever `self.dir.owner()` says *at that moment*, and
    /// there is no way to tell the owner from a direct message's other person except comparing
    /// senders across spaces, which otherwise would not happen until two direct-message spaces
    /// had each produced a live message after startup — mislabelling the owner's own messages
    /// as a third party's in the meantime. One extra, paced request per direct-message space
    /// (bounded: the owner cannot be in more direct-message spaces than exist), stopping as soon
    /// as the owner is known. Does nothing once the owner is already known (from token-info, a
    /// prior run's cursor restore, or `with_owner`).
    async fn bootstrap_owner_from_dms(&self, spaces: &[RawSpace]) {
        if self.dir.owner().is_some() {
            return;
        }
        for space in spaces
            .iter()
            .filter(|s| s.kind() == ConversationKind::Direct)
        {
            if self.dir.owner().is_some() {
                return;
            }
            if let Some(sender) = self.latest_sender(&space.name).await {
                self.dir
                    .learn(&space.name, ConversationKind::Direct, &sender);
            }
        }
    }

    /// Fetches one space's messages after `since`, oldest first, up to the page budget.
    async fn space_messages(
        &self,
        space: &str,
        since: &str,
    ) -> Result<Vec<RawMessage>, (Vec<RawMessage>, FeedError)> {
        let filter = format!("createTime > \"{since}\"");
        let size = self.page_size.to_string();
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..self.max_pages_per_space {
            let mut query = vec![
                // Oldest first is the documented default order.
                ("filter", filter.as_str()),
                ("pageSize", size.as_str()),
            ];
            if let Some(t) = &token {
                query.push(("pageToken", t.as_str()));
            }
            let page: MessagePage = match self
                .client
                .call(
                    reqwest::Method::GET,
                    &format!("{space}/messages"),
                    &query,
                    None,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => return Err((out, e)),
            };
            out.extend(page.messages.into_iter().filter_map(|v| {
                serde_json::from_value::<RawMessage>(v)
                    .map_err(|e| tracing::warn!(error = %e, "skipping a google chat message that did not parse"))
                    .ok()
            }));
            token = page.next_page_token.filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    fn to_feed_message(&self, space: &str, m: &RawMessage) -> Option<FeedMessage> {
        let create = m.create_time.as_deref()?;
        let (secs, nanos) = parse_rfc3339(create)?;
        let (entry, owner) = {
            let st = self.dir.state();
            (st.spaces.get(space).cloned(), st.owner.clone())
        };
        let conv = self.dir.conversation_of(space);
        let sender = m.sender.clone().unwrap_or_default();
        let from_owner = owner.as_deref().is_some_and(|o| o == sender.name);
        let mentions_owner = !from_owner
            && owner.as_deref().is_some_and(|o| {
                m.annotations.iter().any(|a| {
                    a.kind.as_deref() == Some("USER_MENTION")
                        && a.user_mention
                            .as_ref()
                            .and_then(|u| u.user.as_ref())
                            .is_some_and(|u| u.name == o)
                })
            });
        let edited = m
            .last_update_time
            .as_deref()
            .and_then(parse_rfc3339)
            .is_some_and(|u| u > (secs, nanos));
        let mut text = m.text.clone().unwrap_or_default();
        let files: Vec<&str> = m
            .attachment
            .iter()
            .filter_map(|a| a.content_name.as_deref())
            .collect();
        if !files.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[files: {}]", files.join(", ")));
        }
        let threaded = entry.as_ref().is_none_or(|e| e.threaded);
        Some(FeedMessage {
            source: KIND.to_owned(),
            conversation_id: space.to_owned(),
            conversation_name: conv.name,
            id: m.name.clone(),
            thread_id: m
                .thread
                .as_ref()
                .and_then(|t| t.name.clone())
                .filter(|_| threaded),
            author_id: Some(sender.name.clone()).filter(|n| !n.is_empty()),
            author_name: sender.display_name.clone().filter(|n| !n.is_empty()),
            text,
            permalink: entry.and_then(|e| e.uri),
            ts_unix: secs as f64 + f64::from(nanos) / 1e9,
            edited,
            conversation_kind: conv.kind,
            subject: None,
            mentions_owner,
            from_owner,
            conversation_attention: None,
        })
    }
}

#[async_trait]
impl Source for GChatSource {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn poll(&mut self) -> Result<Vec<FeedMessage>, FeedError> {
        if let Some(until) = self.rate_limited_until {
            let left = until.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                return Err(FeedError::RateLimited(left));
            }
            self.rate_limited_until = None;
        }
        self.last_errors.clear();
        self.ensure_owner().await?;
        let spaces = match self.client.list_spaces().await {
            Ok(s) => s,
            Err(FeedError::RateLimited(d)) => {
                self.rate_limited_until = Some(Instant::now() + d);
                return Err(FeedError::RateLimited(d));
            }
            Err(e) => return Err(e),
        };
        self.dir.update_spaces(&spaces);
        let Some(start) = self.cursor.start.clone() else {
            // The first poll only marks where watching begins: no message is delivered from
            // this call. But the owner's identity has to be resolved before that line is drawn
            // (see `bootstrap_owner_from_dms`), or messages right after it would be mislabelled.
            self.bootstrap_owner_from_dms(&spaces).await;
            self.cursor.start = Some(now_rfc3339());
            return Ok(Vec::new());
        };

        let mut staged = self.cursor.spaces.clone();
        let mut active: Vec<(&RawSpace, String)> = spaces
            .iter()
            .filter_map(|s| {
                let since = staged
                    .get(&s.name)
                    .cloned()
                    .unwrap_or_else(|| start.clone());
                let last = s.last_active_time.as_deref().and_then(parse_rfc3339)?;
                (last > parse_rfc3339(&since)?).then_some((s, since))
            })
            .collect();
        active.sort_by(|a, b| a.0.last_active_time.cmp(&b.0.last_active_time));

        let mut out: Vec<FeedMessage> = Vec::new();
        let mut stop: Option<FeedError> = None;
        for (space, since) in active {
            let (msgs, err) = match self.space_messages(&space.name, &since).await {
                Ok(m) => (m, None),
                Err((m, e)) => (m, Some(e)),
            };
            let kind = space.kind();
            for m in &msgs {
                // Messages come oldest first, so the cursor can follow each one.
                if let Some(t) = &m.create_time
                    && parse_rfc3339(t).is_some()
                {
                    staged.insert(space.name.clone(), t.clone());
                }
                if m.delete_time.is_some() {
                    continue;
                }
                if let Some(sender) = &m.sender {
                    self.dir.learn(&space.name, kind, sender);
                }
                if let Some(fm) = self.to_feed_message(&space.name, m) {
                    out.push(fm);
                }
            }
            match err {
                None => {}
                Some(e) if self.is_fatal(&e) => return Err(e),
                Some(e @ (FeedError::RateLimited(_) | FeedError::Http(_))) => {
                    stop = Some(e);
                    break;
                }
                Some(e) => {
                    // A space the owner lost access to, say; the others go on.
                    tracing::warn!(error = %e, "google chat: a space could not be read");
                    self.last_errors.push((space.name.clone(), e));
                }
            }
        }
        self.cursor.spaces = staged;
        out.sort_by(|a, b| a.ts_unix.total_cmp(&b.ts_unix));
        match stop {
            Some(FeedError::RateLimited(d)) => {
                self.rate_limited_until = Some(Instant::now() + d);
                if out.is_empty() {
                    return Err(FeedError::RateLimited(d));
                }
            }
            Some(e) if out.is_empty() => return Err(e),
            Some(e) => tracing::warn!(error = %e, "google chat poll cut short"),
            None => {}
        }
        Ok(out)
    }

    fn cursor(&self) -> Value {
        serde_json::to_value(&self.cursor).unwrap_or(Value::Null)
    }

    fn restore_cursor(&mut self, v: Value) {
        if v.is_null() {
            self.cursor = GCursor::new();
            return;
        }
        match serde_json::from_value::<GCursor>(v) {
            Ok(c) if c.mode == GCursor::MODE => self.cursor = c,
            _ => tracing::warn!("ignoring an unreadable google chat cursor"),
        }
    }

    async fn post_reply(
        &self,
        conversation_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError> {
        post(&self.client, &self.dir, conversation_id, thread_id, text).await
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            poll: true,
            can_reply: true,
            can_reply_in_thread: true,
            directory: true,
            ..Capabilities::default()
        }
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
            .max(self.rate_limited_until.map_or(Duration::ZERO, |u| {
                u.saturating_duration_since(Instant::now())
            }))
    }

    /// A refused refresh token or client means the owner must authorise again.
    fn is_fatal(&self, err: &FeedError) -> bool {
        matches!(err, FeedError::Api(code) if FATAL_AUTH.contains(&code.as_str()))
            || matches!(err, FeedError::Unsupported(_) | FeedError::Internal(_))
    }

    fn directory(&self) -> Option<Arc<dyn Directory>> {
        Some(self.dir.clone())
    }

    fn replier(&self) -> Option<Arc<dyn Replier>> {
        Some(Arc::new(GReplier {
            client: self.client.clone(),
            dir: self.dir.clone(),
        }))
    }
}

/// Seconds and nanoseconds since the Unix epoch of an RFC 3339 timestamp, such as Google's
/// `2024-05-01T12:30:45.123456Z`; `None` for anything else.
fn parse_rfc3339(s: &str) -> Option<(i64, u32)> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut i = 19;
    let mut nanos: u32 = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &s[start..i];
        if digits.is_empty() {
            return None;
        }
        let padded: String = digits
            .chars()
            .chain(std::iter::repeat('0'))
            .take(9)
            .collect();
        nanos = padded.parse().ok()?;
    }
    let offset = match s.get(i..)? {
        "Z" | "z" => 0,
        tz if tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-')) => {
            let h: i64 = tz.get(1..3)?.parse().ok()?;
            let m: i64 = tz.get(4..6)?.parse().ok()?;
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            sign * (h * 3600 + m * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some((
        days * 86_400 + hour * 3600 + minute * 60 + second - offset,
        nanos,
    ))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number since 1970-01-01 (the inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// An RFC 3339 timestamp, UTC with microseconds, as Google writes them.
fn format_rfc3339(secs: i64, nanos: u32) -> String {
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        nanos / 1000
    )
}

fn now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(now.as_secs() as i64, now.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_and_format() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some((0, 0)));
        assert_eq!(
            parse_rfc3339("2024-05-01T12:30:45.123456Z"),
            Some((1_714_566_645, 123_456_000))
        );
        assert_eq!(
            parse_rfc3339("2024-05-01T14:30:45.5+02:00"),
            Some((1_714_566_645, 500_000_000))
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(
            format_rfc3339(1_714_566_645, 123_456_789),
            "2024-05-01T12:30:45.123456Z"
        );
        let t = now_rfc3339();
        assert!(parse_rfc3339(&t).is_some(), "{t}");
    }

    #[test]
    fn credentials_read_and_redact() {
        let c = GoogleCredentials::from_json(
            r#"{"type":"authorized_user","client_id":"id","client_secret":"s3cret","refresh_token":"r3fresh"}"#,
        )
        .unwrap();
        assert_eq!(c.token_uri, DEFAULT_TOKEN_URI);
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("s3cret") && !shown.contains("r3fresh"),
            "{shown}"
        );
    }
}
