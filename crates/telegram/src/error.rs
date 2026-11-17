/// Everything a Bot API call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum TgError {
    /// The request never produced a usable HTTP response (connect, TLS, timeout, body read).
    /// The URL is stripped before this is built, because it contains the bot token.
    #[error("telegram transport error: {0}")]
    Http(#[source] reqwest::Error),

    /// Telegram answered `ok: false`.
    #[error("telegram api error {code}: {description}")]
    Api { code: i64, description: String },

    /// Telegram answered `429` and asked the bot to wait this many seconds.
    #[error("telegram flood control: retry after {0}s")]
    RetryAfter(u64),

    /// Telegram answered, but not with what the method promises.
    #[error("telegram response decode error: {0}")]
    Decode(String),

    /// Callback data that cannot be encoded or decoded (see [`crate::callback`]).
    #[error("invalid callback data: {0}")]
    CallbackData(String),
}

impl TgError {
    /// True for the `400` Telegram returns when an edit changes nothing. Callers that re-render
    /// a message on every state change hit this routinely, and the message already shows what
    /// they wanted, so the edit methods treat it as success.
    pub fn is_not_modified(&self) -> bool {
        match self {
            TgError::Api {
                code: 400,
                description,
            } => {
                description.contains("message is not modified")
                    || description.contains("TOPIC_NOT_MODIFIED")
            }
            _ => false,
        }
    }

    pub(crate) fn http(err: reqwest::Error) -> Self {
        // reqwest puts the request URL in its Display output, and ours carries the token.
        TgError::Http(err.without_url())
    }
}
