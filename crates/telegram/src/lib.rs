//! Thin, typed Telegram Bot API client for the agent fleet.
//!
//! The orchestrator drives one orchestrator bot plus one bot per agent, all posting into a
//! single forum supergroup where every agent gets its own topic. This crate covers exactly the
//! Bot API surface that needs: long polling, messages and edits with inline keyboards, forum
//! topics, bot profile names, and document uploads.
//!
//! - [`Bot`] is the raw client: one HTTP call per method, errors surfaced as [`TgError`].
//! - [`Throttled`] wraps a [`Bot`] with a per-chat [`RateLimiter`] and retries on `429`, and is
//!   what anything that posts into the group should use.
//! - [`html`] and [`callback`] hold the small encoding helpers Telegram's limits call for.

mod bot;
pub mod callback;
mod error;
pub mod html;
mod ratelimit;
mod types;

pub use bot::{Bot, SendMessage};
pub use error::TgError;
pub use ratelimit::{Bucket, RateLimiter, Throttled, call_with_retry};
pub use types::{
    CallbackQuery, Chat, ForumTopic, InlineKeyboardButton, InlineKeyboardMarkup, Message,
    ParseMode, Update, User,
};

/// Telegram's limit on a message's text, in UTF-16 code units.
pub const MAX_MESSAGE_LEN: usize = 4096;

/// Telegram's limit on a media caption, in UTF-16 code units.
pub const MAX_CAPTION_LEN: usize = 1024;
