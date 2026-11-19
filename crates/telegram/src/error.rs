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

    /// True when the bot is not an admin enough to do what it asked — most often deleting a
    /// topic without `can_delete_messages`. Nothing but the owner changing the bot's rights
    /// fixes it, so a caller retrying on a timer should stand down instead.
    pub fn is_missing_rights(&self) -> bool {
        match self {
            TgError::Api { code, description } if *code == 400 || *code == 403 => {
                let d = description.to_ascii_lowercase();
                d.contains("not enough rights")
                    || d.contains("chat_admin_required")
                    || d.contains("have no rights")
            }
            _ => false,
        }
    }

    /// True when the topic (or message thread) the request names is not there any more. For a
    /// delete that is the wanted end state, so callers treat it as success.
    pub fn is_thread_gone(&self) -> bool {
        match self {
            TgError::Api {
                code: 400,
                description,
            } => {
                let d = description.to_ascii_lowercase();
                d.contains("thread not found")
                    || d.contains("topic_deleted")
                    || d.contains("topic not found")
            }
            _ => false,
        }
    }

    pub(crate) fn http(err: reqwest::Error) -> Self {
        // reqwest puts the request URL in its Display output, and ours carries the token.
        TgError::Http(err.without_url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: i64, description: &str) -> TgError {
        TgError::Api {
            code,
            description: description.into(),
        }
    }

    /// The two answers a delete has to tell apart: "you may not" (the owner's to fix) from
    /// "it is not there" (which is what the caller wanted anyway).
    #[test]
    fn a_refusal_is_told_from_a_topic_that_is_already_gone() {
        for refusal in [
            api(400, "Bad Request: not enough rights to manage chat topics"),
            api(400, "Bad Request: CHAT_ADMIN_REQUIRED"),
            api(403, "Forbidden: bot have no rights to send a message"),
        ] {
            assert!(refusal.is_missing_rights(), "{refusal}");
            assert!(!refusal.is_thread_gone(), "{refusal}");
        }
        for gone in [
            api(400, "Bad Request: message thread not found"),
            api(400, "Bad Request: TOPIC_DELETED"),
        ] {
            assert!(gone.is_thread_gone(), "{gone}");
            assert!(!gone.is_missing_rights(), "{gone}");
        }
        // Anything else is an ordinary failure worth retrying.
        let other = api(400, "Bad Request: chat not found");
        assert!(!other.is_missing_rights() && !other.is_thread_gone());
        assert!(!TgError::RetryAfter(3).is_missing_rights());
    }
}
