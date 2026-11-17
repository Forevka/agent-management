//! The subset of Bot API objects the fleet reads and writes.
//!
//! Every type ignores fields it does not name, so new Bot API releases never break decoding.

use serde::{Deserialize, Serialize};

/// One incoming update from `getUpdates`. Only the kinds the fleet asks for are modelled; an
/// update of any other kind decodes with all three fields `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_query: Option<CallbackQuery>,
}

/// A message in a chat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    /// Set for messages in a forum topic, but also for replies in a non-forum supergroup,
    /// where it names the reply thread. Use [`Message::topic_id`] to get the forum topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_topic_message: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<User>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// In a forum topic, a message that is not an explicit reply still carries the topic's
    /// creation service message here, so a non-`None` value does not mean the user replied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message: Option<Box<Message>>,
    /// Unix time, in seconds.
    pub date: i64,
}

impl Message {
    /// The forum topic this message was posted in, or `None` for the General topic and for
    /// chats that are not forums. Checking `is_topic_message` first keeps reply threads in
    /// ordinary supergroups from being mistaken for topics.
    pub fn topic_id(&self) -> Option<i64> {
        match self.is_topic_message {
            Some(true) => self.message_thread_id,
            _ => None,
        }
    }

    /// The text of a text message, or the caption of a media message.
    pub fn text_or_caption(&self) -> Option<&str> {
        self.text.as_deref().or(self.caption.as_deref())
    }
}

/// A chat (private, group, supergroup or channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private`, `group`, `supergroup` or `channel`. Kept as a string so a new chat type does
    /// not make whole updates undecodable.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub is_forum: bool,
}

/// A Telegram user or bot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub is_bot: bool,
    pub first_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// A press on an inline keyboard button. Answer every one with
/// [`crate::Bot::answer_callback_query`], or the user's client shows a spinner until it times out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    /// The message the keyboard is attached to. Telegram omits it when the message is too old.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// A forum topic, as returned by `createForumTopic`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForumTopic {
    pub message_thread_id: i64,
    pub name: String,
}

/// How Telegram parses entities in a text. The fleet only formats with HTML, because escaping
/// three characters is far less error-prone than MarkdownV2's eighteen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParseMode {
    #[serde(rename = "HTML")]
    Html,
}

/// An inline keyboard attached to a message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl InlineKeyboardMarkup {
    /// An empty keyboard; add rows with [`InlineKeyboardMarkup::row`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A keyboard with a single row of buttons.
    pub fn single_row(buttons: Vec<InlineKeyboardButton>) -> Self {
        Self {
            inline_keyboard: vec![buttons],
        }
    }

    /// Appends a row of buttons.
    pub fn row(mut self, buttons: Vec<InlineKeyboardButton>) -> Self {
        self.inline_keyboard.push(buttons);
        self
    }

    /// True when the keyboard has no buttons at all.
    pub fn is_empty(&self) -> bool {
        self.inline_keyboard.iter().all(Vec::is_empty)
    }
}

/// One button of an inline keyboard. Exactly one of `callback_data` and `url` should be set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl InlineKeyboardButton {
    /// A button that sends `data` back as a [`CallbackQuery`]. Build `data` with
    /// [`crate::callback::encode`] so it stays within Telegram's 64-byte limit.
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(data.into()),
            url: None,
        }
    }

    /// A button that opens `url`.
    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: None,
            url: Some(url.into()),
        }
    }
}
