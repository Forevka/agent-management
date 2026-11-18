use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::error::TgError;
use crate::types::{ForumTopic, InlineKeyboardMarkup, Message, ParseMode, Update, User};

const DEFAULT_BASE_URL: &str = "https://api.telegram.org";

/// Ordinary calls answer within a second or two; this only bounds a stuck connection.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Uploads move real bytes, so they get longer than ordinary calls.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Added on top of the long-poll timeout. Telegram holds `getUpdates` open for the whole poll,
/// so an HTTP timeout equal to it would cut off empty polls just before Telegram answers them.
const LONG_POLL_GRACE: Duration = Duration::from_secs(15);

/// A Telegram bot: its token, the API base URL and an HTTP client.
///
/// Cloning is cheap and clones share one connection pool. Every bot created with [`Bot::new`]
/// also shares the same pool, which matters with one bot per agent all talking to the same host.
#[derive(Clone)]
pub struct Bot {
    inner: Arc<Inner>,
}

struct Inner {
    token: String,
    base_url: String,
    client: reqwest::Client,
}

impl fmt::Debug for Bot {
    // Hand-written so the token never ends up in a log line through `{:?}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bot")
            .field("base_url", &self.inner.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Bot {
    /// A bot talking to `https://api.telegram.org`.
    ///
    /// Fails only if the HTTP client cannot be built (no usable TLS roots, for example).
    pub fn new(token: impl Into<String>) -> Result<Self, TgError> {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// A bot talking to another Bot API server: a mock in tests, or a self-hosted
    /// `telegram-bot-api` server.
    pub fn with_base_url(
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, TgError> {
        Ok(Self::with_client(token, base_url, shared_client()?))
    }

    /// A bot using a caller-provided HTTP client (proxy settings, custom roots). The client
    /// must not set a global request timeout shorter than the long-poll timeout.
    pub fn with_client(
        token: impl Into<String>,
        base_url: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Self {
            inner: Arc::new(Inner {
                token: token.into(),
                base_url,
                client,
            }),
        }
    }

    /// The numeric bot id, which Telegram puts before the colon of every token. Lets callers
    /// recognise their own messages in updates without a `getMe` round trip.
    pub fn id(&self) -> Option<i64> {
        self.inner
            .token
            .split_once(':')
            .and_then(|(id, _)| id.parse().ok())
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.inner.base_url, self.inner.token, method)
    }

    /// Calls any Bot API method with JSON parameters. The typed methods below cover what the
    /// fleet uses; this is the escape hatch for the rest.
    pub async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &(impl Serialize + ?Sized),
    ) -> Result<R, TgError> {
        self.call_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    async fn call_with_timeout<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &(impl Serialize + ?Sized),
        timeout: Duration,
    ) -> Result<R, TgError> {
        tracing::trace!(method, "telegram call");
        let response = self
            .inner
            .client
            .post(self.url(method))
            .timeout(timeout)
            .json(params)
            .send()
            .await
            .map_err(TgError::http)?;
        decode_response(method, response).await
    }

    /// Like [`Bot::call`], but an edit that changed nothing counts as success.
    async fn call_unit_allow_unmodified(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<(), TgError> {
        match self.call::<Value>(method, params).await {
            Ok(_) => Ok(()),
            Err(err) if err.is_not_modified() => {
                tracing::trace!(method, "telegram reported no change; treating as success");
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// `getMe`: the bot's own user.
    pub async fn get_me(&self) -> Result<User, TgError> {
        self.call("getMe", &json!({})).await
    }

    /// `getUpdates`: long-polls for up to `timeout_secs`. Pass the last seen `update_id + 1` as
    /// `offset` to acknowledge everything before it. `allowed_updates` is sent as given; an
    /// empty slice asks for every update kind except the chat-member ones.
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u32,
        allowed_updates: &[&str],
    ) -> Result<Vec<Update>, TgError> {
        let mut params = json!({
            "timeout": timeout_secs,
            "allowed_updates": allowed_updates,
        });
        if let Some(offset) = offset {
            params["offset"] = json!(offset);
        }
        let http_timeout = Duration::from_secs(u64::from(timeout_secs)) + LONG_POLL_GRACE;
        self.call_with_timeout("getUpdates", &params, http_timeout)
            .await
    }

    /// `sendMessage`.
    pub async fn send_message(&self, message: &SendMessage) -> Result<Message, TgError> {
        self.call("sendMessage", &message.to_params()).await
    }

    /// `editMessageText`. Editing to identical text and markup succeeds without a change.
    /// A `None` markup removes the message's keyboard, as Telegram does when it is omitted.
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<(), TgError> {
        let mut params = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(mode) = parse_mode {
            params["parse_mode"] = json!(mode);
        }
        if let Some(markup) = reply_markup {
            params["reply_markup"] = json!(markup);
        }
        self.call_unit_allow_unmodified("editMessageText", &params)
            .await
    }

    /// `editMessageReplyMarkup`: replaces the keyboard, or removes it with `None`.
    pub async fn edit_message_reply_markup(
        &self,
        chat_id: i64,
        message_id: i64,
        reply_markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<(), TgError> {
        let mut params = json!({ "chat_id": chat_id, "message_id": message_id });
        if let Some(markup) = reply_markup {
            params["reply_markup"] = json!(markup);
        }
        self.call_unit_allow_unmodified("editMessageReplyMarkup", &params)
            .await
    }

    /// `createForumTopic`. `name` must be 1 to 128 characters. The bot needs the
    /// `can_manage_topics` admin right.
    pub async fn create_forum_topic(
        &self,
        chat_id: i64,
        name: &str,
    ) -> Result<ForumTopic, TgError> {
        self.call(
            "createForumTopic",
            &json!({ "chat_id": chat_id, "name": name }),
        )
        .await
    }

    /// `editForumTopic`: renames a topic. Renaming to the current name succeeds.
    pub async fn edit_forum_topic(
        &self,
        chat_id: i64,
        thread_id: i64,
        name: &str,
    ) -> Result<(), TgError> {
        let params = json!({ "chat_id": chat_id, "message_thread_id": thread_id, "name": name });
        self.call_unit_allow_unmodified("editForumTopic", &params)
            .await
    }

    /// `closeForumTopic`. Closing a closed topic succeeds.
    pub async fn close_forum_topic(&self, chat_id: i64, thread_id: i64) -> Result<(), TgError> {
        let params = json!({ "chat_id": chat_id, "message_thread_id": thread_id });
        self.call_unit_allow_unmodified("closeForumTopic", &params)
            .await
    }

    /// `reopenForumTopic`. Reopening an open topic succeeds.
    pub async fn reopen_forum_topic(&self, chat_id: i64, thread_id: i64) -> Result<(), TgError> {
        let params = json!({ "chat_id": chat_id, "message_thread_id": thread_id });
        self.call_unit_allow_unmodified("reopenForumTopic", &params)
            .await
    }

    /// `answerCallbackQuery`. `text`, if given, shows as a toast on the presser's client.
    pub async fn answer_callback_query(&self, id: &str, text: Option<&str>) -> Result<(), TgError> {
        let mut params = json!({ "callback_query_id": id });
        if let Some(text) = text {
            params["text"] = json!(text);
        }
        self.call::<Value>("answerCallbackQuery", &params)
            .await
            .map(drop)
    }

    /// `setMyName`. Telegram rate-limits profile changes hard (a `429` here can ask for
    /// minutes), so call this only when the name actually changes.
    pub async fn set_my_name(&self, name: &str) -> Result<(), TgError> {
        self.call::<Value>("setMyName", &json!({ "name": name }))
            .await
            .map(drop)
    }

    /// `setMyDescription`: the text shown in an empty private chat with the bot.
    pub async fn set_my_description(&self, description: &str) -> Result<(), TgError> {
        self.call::<Value>("setMyDescription", &json!({ "description": description }))
            .await
            .map(drop)
    }

    /// `setMyShortDescription`: the text on the bot's profile page.
    pub async fn set_my_short_description(&self, short_description: &str) -> Result<(), TgError> {
        let params = json!({ "short_description": short_description });
        self.call::<Value>("setMyShortDescription", &params)
            .await
            .map(drop)
    }

    /// `sendDocument`, uploading `bytes` as a file named `file_name`.
    pub async fn send_document(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        file_name: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> Result<Message, TgError> {
        let document = reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_owned());
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", document);
        if let Some(thread_id) = thread_id {
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(caption) = caption {
            form = form.text("caption", caption.to_owned());
        }
        tracing::trace!(method = "sendDocument", "telegram call");
        let response = self
            .inner
            .client
            .post(self.url("sendDocument"))
            .timeout(UPLOAD_TIMEOUT)
            .multipart(form)
            .send()
            .await
            .map_err(TgError::http)?;
        decode_response("sendDocument", response).await
    }

    /// `setMessageReaction`: sets the bot's reaction to a message, replacing any it had.
    /// `emoji` must be one of the emoji Telegram allows as reactions (`ReactionTypeEmoji`), and
    /// the chat must allow it.
    pub async fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: &str,
    ) -> Result<(), TgError> {
        let params = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{ "type": "emoji", "emoji": emoji }],
        });
        self.call::<Value>("setMessageReaction", &params)
            .await
            .map(drop)
    }

    /// `sendChatAction`, e.g. `"typing"`: Telegram shows it for about five seconds, or until the
    /// bot's next message, so a longer wait needs it sent again.
    pub async fn send_chat_action(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        action: &str,
    ) -> Result<(), TgError> {
        let mut params = json!({ "chat_id": chat_id, "action": action });
        if let Some(thread_id) = thread_id {
            params["message_thread_id"] = json!(thread_id);
        }
        self.call::<Value>("sendChatAction", &params)
            .await
            .map(drop)
    }

    /// `deleteMessage`.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<(), TgError> {
        let params = json!({ "chat_id": chat_id, "message_id": message_id });
        self.call::<Value>("deleteMessage", &params).await.map(drop)
    }
}

/// The client every [`Bot::new`] shares. Building one per bot would give each agent bot its
/// own connection pool and TLS sessions to the very same host.
fn shared_client() -> Result<reqwest::Client, TgError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    // reqwest is built without a bundled crypto provider, so one must be the process default
    // before a client exists. An error means the application installed its own, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(TgError::http)?;
    // Two threads may race to build; whichever lands first is kept and the other dropped.
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// The `{ok, result, description, error_code, parameters}` wrapper around every reply.
#[derive(serde::Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    parameters: Option<ResponseParameters>,
}

#[derive(serde::Deserialize)]
struct ResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

async fn decode_response<R: DeserializeOwned>(
    method: &str,
    response: reqwest::Response,
) -> Result<R, TgError> {
    let status = response.status();
    let body = response.bytes().await.map_err(TgError::http)?;
    let envelope: Envelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        // A proxy or load balancer in front of Telegram can answer with HTML. Report the HTTP
        // status as the error rather than a JSON error that hides it.
        Err(_) if !status.is_success() => {
            return Err(TgError::Api {
                code: i64::from(status.as_u16()),
                description: format!("{method}: HTTP {status} with a non-JSON body"),
            });
        }
        Err(err) => return Err(TgError::Decode(format!("{method}: {err}"))),
    };
    if !envelope.ok {
        if let Some(seconds) = envelope.parameters.and_then(|p| p.retry_after) {
            return Err(TgError::RetryAfter(seconds));
        }
        return Err(TgError::Api {
            code: envelope
                .error_code
                .unwrap_or_else(|| i64::from(status.as_u16())),
            description: envelope.description.unwrap_or_default(),
        });
    }
    let result = envelope
        .result
        .ok_or_else(|| TgError::Decode(format!("{method}: ok response without a result")))?;
    serde_json::from_value(result).map_err(|err| TgError::Decode(format!("{method}: {err}")))
}

/// Parameters for [`Bot::send_message`]. Start from [`SendMessage::new`] and chain the setters.
#[derive(Debug, Clone, PartialEq)]
pub struct SendMessage {
    pub chat_id: i64,
    /// The forum topic to post in; `None` posts in the General topic.
    pub message_thread_id: Option<i64>,
    pub text: String,
    pub parse_mode: Option<ParseMode>,
    pub reply_markup: Option<InlineKeyboardMarkup>,
    pub reply_to_message_id: Option<i64>,
    pub disable_notification: bool,
    /// Off by default: agent output is full of URLs, and a preview card for each one buries
    /// the text it belongs to.
    pub link_preview: bool,
}

impl SendMessage {
    /// A plain-text message to `chat_id`, without a link preview.
    pub fn new(chat_id: i64, text: impl Into<String>) -> Self {
        Self {
            chat_id,
            message_thread_id: None,
            text: text.into(),
            parse_mode: None,
            reply_markup: None,
            reply_to_message_id: None,
            disable_notification: false,
            link_preview: false,
        }
    }

    /// Posts in the given forum topic.
    pub fn thread(mut self, thread_id: i64) -> Self {
        self.message_thread_id = Some(thread_id);
        self
    }

    /// Parses `text` as HTML (escape user content with [`crate::html::escape`]).
    pub fn html(mut self) -> Self {
        self.parse_mode = Some(ParseMode::Html);
        self
    }

    /// Attaches an inline keyboard.
    pub fn keyboard(mut self, markup: InlineKeyboardMarkup) -> Self {
        self.reply_markup = Some(markup);
        self
    }

    /// Replies to a message in the same chat.
    pub fn reply_to(mut self, message_id: i64) -> Self {
        self.reply_to_message_id = Some(message_id);
        self
    }

    /// Delivers without a notification sound.
    pub fn silent(mut self) -> Self {
        self.disable_notification = true;
        self
    }

    /// Shows a preview for the first link in the text.
    pub fn with_link_preview(mut self) -> Self {
        self.link_preview = true;
        self
    }

    fn to_params(&self) -> Value {
        let mut params = json!({
            "chat_id": self.chat_id,
            "text": self.text,
            "link_preview_options": { "is_disabled": !self.link_preview },
        });
        if let Some(thread_id) = self.message_thread_id {
            params["message_thread_id"] = json!(thread_id);
        }
        if let Some(mode) = self.parse_mode {
            params["parse_mode"] = json!(mode);
        }
        if let Some(markup) = &self.reply_markup {
            params["reply_markup"] = json!(markup);
        }
        if let Some(message_id) = self.reply_to_message_id {
            // `reply_parameters` replaced `reply_to_message_id` in Bot API 7.0. Allowing a send
            // without the reply keeps a message from being lost because its target was deleted.
            params["reply_parameters"] = json!({
                "message_id": message_id,
                "allow_sending_without_reply": true,
            });
        }
        if self.disable_notification {
            params["disable_notification"] = json!(true);
        }
        params
    }
}
