//! What the orchestrator needs from a chat platform. Telegram implements it in [`crate::tg`];
//! tests use a recording fake.

use anyhow::Result;
use async_trait::async_trait;

/// Which identity a message is sent as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum From {
    /// The orchestrator's own bot.
    Orchestrator,
    /// An agent's bot (`bots.id`).
    Bot(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub text: String,
    /// Callback data (at most 64 bytes) or a URL; exactly one is set.
    pub data: Option<String>,
    pub url: Option<String>,
}

impl Button {
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: Some(data.into()),
            url: None,
        }
    }
    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: None,
            url: Some(url.into()),
        }
    }
}

pub type Keyboard = Vec<Vec<Button>>;

/// Whether a message makes the owner's phone sound. The fleet posts a lot; only what needs the
/// owner, or tells them how work ended, may interrupt them (see `docs/plan.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// Needs the owner (plans, questions, permissions, proposals, `ask`), a task's outcome
    /// (done, failed, stopped), or the brain answering the owner.
    Sound,
    /// Everything else: briefs, checklists, progress, queue notes, pointers, FYI items, feeds.
    Silent,
}

/// How deleting a topic went. Anything other than these two is an ordinary error worth retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// The topic is gone: this call removed it, or it was not there any more.
    Gone,
    /// The bot may not delete it (it needs the `can_delete_messages` admin right). Only the
    /// owner can change that, so a caller on a timer should stop asking rather than retry.
    NotAllowed,
}

/// One forum group. Topic ids are Telegram `message_thread_id`s; `None` is the General topic.
#[async_trait]
pub trait Chat: Send + Sync + 'static {
    async fn create_topic(&self, name: &str) -> Result<i64>;
    async fn rename_topic(&self, topic: i64, name: &str) -> Result<()>;
    async fn close_topic(&self, topic: i64) -> Result<()>;
    async fn reopen_topic(&self, topic: i64) -> Result<()>;
    /// Deletes a topic and everything written in it. Used only for a finished task's own topic
    /// (see `Orchestrator::prune_topics`), never for a standing one.
    async fn delete_topic(&self, topic: i64) -> Result<Deleted>;
    /// Sends HTML text; returns the message id.
    async fn send(
        &self,
        from: From,
        topic: Option<i64>,
        html: &str,
        keyboard: Keyboard,
        reply_to: Option<i64>,
        notify: Notify,
    ) -> Result<i64>;
    /// Reacts to a message (as the orchestrator), e.g. to show it was received. `emoji` must be
    /// one Telegram allows as a reaction.
    async fn react(&self, message_id: i64, emoji: &str) -> Result<()>;
    /// Shows "typing…" in a topic, as `from`, for a few seconds. Skipped (not an error) when
    /// that bot has no room under its rate limit right now: it is only a hint.
    async fn typing(&self, from: From, topic: Option<i64>) -> Result<()>;
    async fn edit(&self, from: From, message_id: i64, html: &str, keyboard: Keyboard)
    -> Result<()>;
    async fn send_file(
        &self,
        from: From,
        topic: Option<i64>,
        name: &str,
        bytes: Vec<u8>,
        caption: &str,
    ) -> Result<i64>;
    /// Checks a new agent bot token and returns (bot user id, username).
    async fn inspect_bot(&self, token: &str) -> Result<(i64, String)>;
    /// Makes a bot usable for sending (after it was added to the pool or loaded at startup).
    async fn add_bot(&self, bot_id: i64, token: &str) -> Result<()>;
    /// Renames a pool bot to the agent it now represents.
    async fn set_bot_profile(&self, bot_id: i64, name: &str, description: &str) -> Result<()>;
    /// Forgets a bot removed from the pool: nothing is sent as it or polled for it any more.
    async fn remove_bot(&self, bot_id: i64);
}

/// Escapes text for HTML, in Telegram messages and on the web UI. Quotes are escaped too, so the
/// result is also safe inside a quoted attribute (Telegram accepts these entities).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Shortens text to `max` characters (not bytes), marking the cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
pub mod fake {
    //! A [`Chat`] that records everything, for tests.
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Op {
        CreateTopic(String, i64),
        Rename(i64, String),
        Close(i64),
        Reopen(i64),
        Delete(i64),
        Send {
            id: i64,
            from: From,
            topic: Option<i64>,
            html: String,
            keyboard: Keyboard,
            notify: Notify,
        },
        Edit {
            from: From,
            id: i64,
            html: String,
            keyboard: Keyboard,
        },
        React(i64, String),
        Typing(From, Option<i64>),
        File {
            topic: Option<i64>,
            name: String,
        },
        BotProfile(i64, String),
        RemoveBot(i64),
    }

    pub struct FakeChat {
        pub ops: Mutex<Vec<Op>>,
        next: Mutex<i64>,
        /// Whether `delete_topic` is allowed, so a test can play a bot without the right.
        pub may_delete_topics: Mutex<bool>,
    }

    impl Default for FakeChat {
        fn default() -> Self {
            Self {
                ops: Mutex::default(),
                next: Mutex::default(),
                may_delete_topics: Mutex::new(true),
            }
        }
    }

    impl FakeChat {
        fn id(&self) -> i64 {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            100 + *n
        }
        pub fn sent(&self) -> Vec<(i64, Option<i64>, String, Keyboard)> {
            self.ops
                .lock()
                .unwrap()
                .iter()
                .filter_map(|o| match o {
                    Op::Send {
                        id,
                        topic,
                        html,
                        keyboard,
                        ..
                    } => Some((*id, *topic, html.clone(), keyboard.clone())),
                    _ => None,
                })
                .collect()
        }

        /// Whether the sent message whose text contains `needle` (the first one) sounded.
        pub fn notify_of(&self, needle: &str) -> Option<Notify> {
            self.ops.lock().unwrap().iter().find_map(|o| match o {
                Op::Send { html, notify, .. } if html.contains(needle) => Some(*notify),
                _ => None,
            })
        }
    }

    #[async_trait]
    impl Chat for FakeChat {
        async fn create_topic(&self, name: &str) -> Result<i64> {
            let id = self.id();
            self.ops
                .lock()
                .unwrap()
                .push(Op::CreateTopic(name.into(), id));
            Ok(id)
        }
        async fn rename_topic(&self, topic: i64, name: &str) -> Result<()> {
            self.ops
                .lock()
                .unwrap()
                .push(Op::Rename(topic, name.into()));
            Ok(())
        }
        async fn close_topic(&self, topic: i64) -> Result<()> {
            self.ops.lock().unwrap().push(Op::Close(topic));
            Ok(())
        }
        async fn reopen_topic(&self, topic: i64) -> Result<()> {
            self.ops.lock().unwrap().push(Op::Reopen(topic));
            Ok(())
        }
        async fn delete_topic(&self, topic: i64) -> Result<Deleted> {
            self.ops.lock().unwrap().push(Op::Delete(topic));
            // A test that wants the refusal sets `may_delete_topics` to false.
            Ok(if *self.may_delete_topics.lock().unwrap() {
                Deleted::Gone
            } else {
                Deleted::NotAllowed
            })
        }
        async fn send(
            &self,
            from: From,
            topic: Option<i64>,
            html: &str,
            keyboard: Keyboard,
            _r: Option<i64>,
            notify: Notify,
        ) -> Result<i64> {
            let id = self.id();
            self.ops.lock().unwrap().push(Op::Send {
                id,
                from,
                topic,
                html: html.into(),
                keyboard,
                notify,
            });
            Ok(id)
        }
        async fn react(&self, message_id: i64, emoji: &str) -> Result<()> {
            self.ops
                .lock()
                .unwrap()
                .push(Op::React(message_id, emoji.into()));
            Ok(())
        }
        async fn typing(&self, from: From, topic: Option<i64>) -> Result<()> {
            self.ops.lock().unwrap().push(Op::Typing(from, topic));
            Ok(())
        }
        async fn edit(&self, from: From, id: i64, html: &str, keyboard: Keyboard) -> Result<()> {
            self.ops.lock().unwrap().push(Op::Edit {
                from,
                id,
                html: html.into(),
                keyboard,
            });
            Ok(())
        }
        async fn send_file(
            &self,
            _f: From,
            topic: Option<i64>,
            name: &str,
            _b: Vec<u8>,
            _c: &str,
        ) -> Result<i64> {
            self.ops.lock().unwrap().push(Op::File {
                topic,
                name: name.into(),
            });
            Ok(self.id())
        }
        async fn inspect_bot(&self, token: &str) -> Result<(i64, String)> {
            let n: i64 = token.len() as i64 * 1000 + token.bytes().map(i64::from).sum::<i64>();
            Ok((n, format!("bot{n}")))
        }
        async fn add_bot(&self, _bot_id: i64, _token: &str) -> Result<()> {
            Ok(())
        }
        async fn set_bot_profile(&self, bot_id: i64, name: &str, _d: &str) -> Result<()> {
            self.ops
                .lock()
                .unwrap()
                .push(Op::BotProfile(bot_id, name.into()));
            Ok(())
        }
        async fn remove_bot(&self, bot_id: i64) {
            self.ops.lock().unwrap().push(Op::RemoveBot(bot_id));
        }
    }
}
