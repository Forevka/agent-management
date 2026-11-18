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

/// One forum group. Topic ids are Telegram `message_thread_id`s; `None` is the General topic.
#[async_trait]
pub trait Chat: Send + Sync + 'static {
    async fn create_topic(&self, name: &str) -> Result<i64>;
    async fn rename_topic(&self, topic: i64, name: &str) -> Result<()>;
    async fn close_topic(&self, topic: i64) -> Result<()>;
    async fn reopen_topic(&self, topic: i64) -> Result<()>;
    /// Sends HTML text; returns the message id.
    async fn send(
        &self,
        from: From,
        topic: Option<i64>,
        html: &str,
        keyboard: Keyboard,
        reply_to: Option<i64>,
    ) -> Result<i64>;
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
        Send {
            id: i64,
            from: From,
            topic: Option<i64>,
            html: String,
            keyboard: Keyboard,
        },
        Edit {
            from: From,
            id: i64,
            html: String,
        },
        File {
            topic: Option<i64>,
            name: String,
        },
        BotProfile(i64, String),
        RemoveBot(i64),
    }

    #[derive(Default)]
    pub struct FakeChat {
        pub ops: Mutex<Vec<Op>>,
        next: Mutex<i64>,
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
        async fn send(
            &self,
            from: From,
            topic: Option<i64>,
            html: &str,
            keyboard: Keyboard,
            _r: Option<i64>,
        ) -> Result<i64> {
            let id = self.id();
            self.ops.lock().unwrap().push(Op::Send {
                id,
                from,
                topic,
                html: html.into(),
                keyboard,
            });
            Ok(id)
        }
        async fn edit(&self, from: From, id: i64, html: &str, _k: Keyboard) -> Result<()> {
            self.ops.lock().unwrap().push(Op::Edit {
                from,
                id,
                html: html.into(),
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
