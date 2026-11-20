//! Telegram: the [`Chat`] implementation and the update loops that turn the owner's messages
//! and button presses into orchestrator actions.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use fleet_telegram::{
    Bot, InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton, Message, ParseMode,
    ReplyKeyboardMarkup, SendMessage, Throttled, Update,
};
use tokio::sync::RwLock;

use crate::{
    chat::{Chat, Deleted, From, Keyboard, Notify, clip, esc},
    core::Orchestrator,
    human::{self, Answer},
    model::{BRAIN_TASK, TaskRow},
    tasks,
};

pub struct TelegramChat {
    chat_id: i64,
    /// The owner's own user id, for the one thing that cannot happen in the forum group: the
    /// managed-bot button, which Telegram allows in a private chat only.
    owner_id: i64,
    /// Bot API base URL when not api.telegram.org (a self-hosted Bot API server).
    api_url: Option<String>,
    orchestrator: Throttled,
    bots: RwLock<HashMap<i64, Throttled>>,
}

impl TelegramChat {
    pub fn new(orchestrator: Bot, chat_id: i64, owner_id: i64, api_url: Option<String>) -> Self {
        Self {
            chat_id,
            owner_id,
            api_url,
            orchestrator: Throttled::new(orchestrator),
            bots: RwLock::new(HashMap::new()),
        }
    }

    fn bot(&self, token: &str) -> Result<Bot> {
        Ok(match &self.api_url {
            Some(url) => Bot::with_base_url(token, url.clone())?,
            None => Bot::new(token)?,
        })
    }

    async fn sender(&self, from: From) -> Throttled {
        match from {
            From::Orchestrator => self.orchestrator.clone(),
            // An agent whose bot is unusable still gets its message out, as the orchestrator.
            From::Bot(id) => self
                .bots
                .read()
                .await
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.orchestrator.clone()),
        }
    }

    /// Bots whose button presses must be polled (agent bots; the orchestrator polls itself).
    pub async fn agent_bots(&self) -> Vec<(i64, Bot)> {
        self.bots
            .read()
            .await
            .iter()
            .map(|(id, t)| (*id, t.bot().clone()))
            .collect()
    }
}

fn markup(kb: &Keyboard) -> Option<InlineKeyboardMarkup> {
    if kb.is_empty() {
        return None;
    }
    let mut m = InlineKeyboardMarkup::new();
    for row in kb {
        let buttons = row
            .iter()
            .map(|b| match (&b.data, &b.url) {
                (Some(d), _) => InlineKeyboardButton::callback(b.text.clone(), d.clone()),
                (None, Some(u)) => InlineKeyboardButton::url(b.text.clone(), u.clone()),
                (None, None) => InlineKeyboardButton::callback(b.text.clone(), "noop"),
            })
            .collect();
        m = m.row(buttons);
    }
    Some(m)
}

fn limit(html: &str) -> String {
    fleet_telegram::html::truncate_html(html, fleet_telegram::MAX_MESSAGE_LEN)
}

#[async_trait]
impl Chat for TelegramChat {
    async fn create_topic(&self, name: &str) -> Result<i64> {
        Ok(self
            .orchestrator
            .create_forum_topic(self.chat_id, &clip(name, 120))
            .await?
            .message_thread_id)
    }
    async fn rename_topic(&self, topic: i64, name: &str) -> Result<()> {
        Ok(self
            .orchestrator
            .edit_forum_topic(self.chat_id, topic, &clip(name, 120))
            .await?)
    }
    async fn close_topic(&self, topic: i64) -> Result<()> {
        Ok(self
            .orchestrator
            .bot()
            .close_forum_topic(self.chat_id, topic)
            .await?)
    }
    async fn reopen_topic(&self, topic: i64) -> Result<()> {
        Ok(self
            .orchestrator
            .bot()
            .reopen_forum_topic(self.chat_id, topic)
            .await?)
    }
    async fn delete_topic(&self, topic: i64) -> Result<Deleted> {
        match self
            .orchestrator
            .bot()
            .delete_forum_topic(self.chat_id, topic)
            .await
        {
            // A topic that is already gone is the state we wanted.
            Ok(()) => Ok(Deleted::Gone),
            Err(e) if e.is_thread_gone() => Ok(Deleted::Gone),
            Err(e) if e.is_missing_rights() => Ok(Deleted::NotAllowed),
            Err(e) => Err(e.into()),
        }
    }
    async fn send(
        &self,
        from: From,
        topic: Option<i64>,
        html: &str,
        keyboard: Keyboard,
        reply_to: Option<i64>,
        notify: Notify,
    ) -> Result<i64> {
        let mut msg = SendMessage::new(self.chat_id, limit(html)).html();
        if let Some(t) = topic {
            msg = msg.thread(t);
        }
        if notify == Notify::Silent {
            msg = msg.silent();
        }
        if let Some(m) = markup(&keyboard) {
            msg = msg.keyboard(m);
        }
        if let Some(r) = reply_to {
            msg = msg.reply_to(r);
        }
        Ok(self.sender(from).await.send_message(&msg).await?.message_id)
    }
    // Through the orchestrator bot's rate limiter like any call to the group: it waits for room
    // and retries on 429s. The caller runs it in the background and only logs a failure.
    async fn react(&self, message_id: i64, emoji: &str) -> Result<()> {
        let bot = self.orchestrator.bot().clone();
        Ok(self
            .orchestrator
            .run(self.chat_id, || {
                bot.set_message_reaction(self.chat_id, message_id, emoji)
            })
            .await?)
    }
    // A hint renewed every few seconds: it only takes room under the sending bot's rate limit
    // that is free right now (real messages come first), and backs off when Telegram asks to.
    async fn typing(&self, from: From, topic: Option<i64>) -> Result<()> {
        let sender = self.sender(from).await;
        let limiter = sender.limiter(self.chat_id);
        if !limiter.try_acquire() {
            return Ok(());
        }
        match sender
            .bot()
            .send_chat_action(self.chat_id, topic, "typing")
            .await
        {
            Err(fleet_telegram::TgError::RetryAfter(secs)) => {
                limiter.block_for(Duration::from_secs(secs)).await;
                Ok(())
            }
            other => Ok(other?),
        }
    }
    async fn edit(
        &self,
        from: From,
        message_id: i64,
        html: &str,
        keyboard: Keyboard,
    ) -> Result<()> {
        let m = markup(&keyboard);
        Ok(self
            .sender(from)
            .await
            .edit_message_text(
                self.chat_id,
                message_id,
                &limit(html),
                Some(ParseMode::Html),
                m.as_ref(),
            )
            .await?)
    }
    async fn send_file(
        &self,
        from: From,
        topic: Option<i64>,
        name: &str,
        bytes: Vec<u8>,
        caption: &str,
    ) -> Result<i64> {
        let caption =
            fleet_telegram::html::truncate_for_message(caption, fleet_telegram::MAX_CAPTION_LEN);
        Ok(self
            .sender(from)
            .await
            .send_document(self.chat_id, topic, name, bytes, Some(&caption))
            .await?
            .message_id)
    }
    async fn inspect_bot(&self, token: &str) -> Result<(i64, String)> {
        let me = self
            .bot(token)?
            .get_me()
            .await
            .context("Telegram rejected this bot token")?;
        Ok((me.id, me.username.unwrap_or_default()))
    }
    async fn add_bot(&self, bot_id: i64, token: &str) -> Result<()> {
        self.bots
            .write()
            .await
            .insert(bot_id, Throttled::new(self.bot(token)?));
        Ok(())
    }
    async fn set_bot_profile(&self, bot_id: i64, name: &str, description: &str) -> Result<()> {
        let bot = self
            .bots
            .read()
            .await
            .get(&bot_id)
            .map(|t| t.bot().clone())
            .context("bot not loaded")?;
        bot.set_my_name(name).await?;
        bot.set_my_description(description).await?;
        bot.set_my_short_description(&clip(description, 120))
            .await?;
        Ok(())
    }
    async fn request_bot(&self, name: &str, username: &str) -> Result<()> {
        // A reply keyboard in the owner's private chat: `request_managed_bot` is refused in
        // groups, so this is the one message the fleet does not send into its forum.
        let text = format!(
            "The fleet needs one more agent bot, so {} can work while the others are busy.\n\n\
             Tap below and Telegram will create it — name and username are filled in, and I get \
             the token myself. Nothing to copy.",
            esc(name)
        );
        let msg = SendMessage::new(self.owner_id, text).html().reply_keyboard(
            ReplyKeyboardMarkup::one_button(KeyboardButton::request_managed_bot(
                "🤖 Create the bot",
                1,
                name,
                username,
            )),
        );
        self.orchestrator.send_message(&msg).await?;
        Ok(())
    }
    async fn remove_bot(&self, bot_id: i64) {
        self.bots.write().await.remove(&bot_id);
    }
}

/// Long-polls the orchestrator bot forever.
pub async fn run_updates(o: Arc<Orchestrator>, bot: Bot, chat_id: i64) {
    let key = "tg_offset";
    let mut offset: Option<i64> =
        sqlx::query_scalar::<_, serde_json::Value>("select value from kv where key = $1")
            .bind(key)
            .fetch_optional(&o.db)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.as_i64());
    // `managed_bot` has to be named: Telegram sends every kind only for an *empty*
    // allowed_updates, and this list is not empty. An older Bot API server — which
    // `TELEGRAM_API_URL` is there to allow — rejects a value it does not know, and that would
    // fail every poll rather than just this feature, so the first rejection drops it for good
    // and the fleet carries on without the one-tap bot flow.
    const WITH_BOTS: &[&str] = &["message", "callback_query", "managed_bot"];
    const PLAIN: &[&str] = &["message", "callback_query"];
    let mut kinds = WITH_BOTS;
    loop {
        match bot.get_updates(offset, 50, kinds).await {
            Ok(updates) => {
                for u in updates {
                    offset = Some(u.update_id + 1);
                    handle_update(&o, &bot, chat_id, u).await;
                }
                if let Some(off) = offset {
                    let _ = sqlx::query("insert into kv (key, value) values ($1, $2) on conflict (key) do update set value = $2")
                        .bind(key)
                        .bind(serde_json::json!(off))
                        .execute(&o.db)
                        .await;
                }
            }
            Err(e) => {
                // Only a 400 means the request itself was wrong; a 409 (another poller) or a
                // network error says nothing about whether the server knows `managed_bot`.
                if kinds == WITH_BOTS
                    && matches!(&e, fleet_telegram::TgError::Api { code: 400, .. })
                {
                    tracing::warn!(
                        "this Bot API server does not accept the `managed_bot` update ({e}); \
                         polling without it, so a new agent bot has to be added by hand"
                    );
                    kinds = PLAIN;
                    continue;
                }
                tracing::warn!("telegram getUpdates failed: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Polls agent bots for button presses on the messages they sent. Bots added later are picked
/// up on the next round.
pub async fn run_agent_bot_callbacks(o: Arc<Orchestrator>, chat: Arc<TelegramChat>, chat_id: i64) {
    let mut running: HashMap<i64, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        let bots = chat.agent_bots().await;
        // Bots removed from the pool stop being polled with their (deleted) token.
        running.retain(|id, h| {
            let keep = bots.iter().any(|(b, _)| b == id);
            if !keep {
                h.abort();
            }
            keep
        });
        for (id, bot) in bots {
            if running.get(&id).is_some_and(|h| !h.is_finished()) {
                continue;
            }
            let o = o.clone();
            running.insert(
                id,
                tokio::spawn(async move {
                    let mut offset = None;
                    loop {
                        match bot.get_updates(offset, 50, &["callback_query"]).await {
                            Ok(updates) => {
                                for u in updates {
                                    offset = Some(u.update_id + 1);
                                    // Only button presses. allowed_updates does not drop updates
                                    // queued before it took effect, and agent bots that are group
                                    // admins receive every group message: handling those here
                                    // would run the owner's commands once per agent bot.
                                    if u.callback_query.is_some() {
                                        handle_update(&o, &bot, chat_id, u).await;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::debug!("agent bot {id} getUpdates failed: {e}");
                                tokio::time::sleep(Duration::from_secs(10)).await;
                            }
                        }
                    }
                }),
            );
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn handle_update(o: &Arc<Orchestrator>, bot: &Bot, chat_id: i64, u: Update) {
    if let Some(mb) = u.managed_bot {
        // Only the owner's own bots: the update names who created it, and nobody else may add to
        // the fleet's pool.
        if mb.user.id != o.cfg.owner_id {
            tracing::warn!("ignoring a managed bot created by {}", mb.user.id);
            return;
        }
        // The token goes straight from Telegram into the vault; it is never logged and never
        // shown, not even to the owner who caused it to exist.
        let taken = match bot.get_managed_bot_token(mb.bot.id).await {
            Ok(token) => crate::admin::adopt_managed_bot(o, mb.bot.id, &token).await,
            Err(e) => Err(anyhow::anyhow!("Telegram would not give me its token: {e}")),
        };
        let reply = match taken {
            Ok(said) => said,
            Err(e) => {
                // The owner has really made a bot, and it is now in their account counting
                // against Telegram's cap on how many they may own. Releasing today's ask lets
                // the fleet offer again rather than leave them with a bot it never took, and
                // the message says the one thing they can do with it in the meantime.
                crate::tasks::release_bot_ask(o).await;
                tracing::warn!("adopting managed bot {} failed: {e:#}", mb.bot.id);
                format!(
                    "@{} was created but I could not take it into the pool: {e:#}\n\nIt is yours \
                     either way. Either wait for me to offer again, or get its token from \
                     @BotFather and paste it on the Agents page.",
                    mb.bot.username.as_deref().unwrap_or("the new bot")
                )
            }
        };
        let _ = o
            .chat
            .send(
                From::Orchestrator,
                None,
                &esc(&reply),
                vec![],
                None,
                Notify::Silent,
            )
            .await;
        return;
    }
    if let Some(cb) = u.callback_query {
        if cb.from.id != o.cfg.owner_id {
            let _ = bot
                .answer_callback_query(&cb.id, Some("Only the fleet owner can answer."))
                .await;
            return;
        }
        // Todo and rule buttons in the Inbox are the secretary's; a workflow's control card is
        // its own; the rest answer human items.
        if let Some(data) = cb
            .data
            .as_deref()
            .filter(|d| crate::secretary::is_button(d))
        {
            let reply = crate::secretary::button(o, data)
                .await
                .unwrap_or_else(|e| format!("Failed: {e:#}"));
            let _ = bot
                .answer_callback_query(&cb.id, Some(&clip(&reply, 190)))
                .await;
            return;
        }
        if let Some(data) = cb
            .data
            .as_deref()
            .filter(|d| crate::workflows::is_button(d))
        {
            let reply = crate::workflows::button(o, data)
                .await
                .unwrap_or_else(|e| format!("Failed: {e:#}"));
            let _ = bot
                .answer_callback_query(&cb.id, Some(&clip(&reply, 190)))
                .await;
            return;
        }
        // A retrospective's proposal: make it a task, or dismiss it.
        if let Some(data) = cb.data.as_deref().filter(|d| crate::retro::is_button(d)) {
            let reply = crate::retro::button(o, data)
                .await
                .unwrap_or_else(|e| format!("Failed: {e:#}"));
            let _ = bot
                .answer_callback_query(&cb.id, Some(&clip(&reply, 190)))
                .await;
            return;
        }
        // Taking back something a habit did, or paused it, must always work.
        if let Some(data) = cb.data.as_deref().filter(|d| crate::habits::is_button(d)) {
            let reply = crate::habits::button(o, data)
                .await
                .unwrap_or_else(|e| format!("Failed: {e:#}"));
            let _ = bot
                .answer_callback_query(&cb.id, Some(&clip(&reply, 190)))
                .await;
            return;
        }
        let reply = match cb.data.as_deref().map(callback) {
            Some(f) => match f {
                Ok((id, ans)) => human::answer(o, id, ans)
                    .await
                    .unwrap_or_else(|e| format!("Failed: {e:#}")),
                Err(e) => format!("{e:#}"),
            },
            None => "Nothing to do.".into(),
        };
        let _ = bot
            .answer_callback_query(&cb.id, Some(&clip(&reply, 190)))
            .await;
        return;
    }
    let Some(msg) = u.message else { return };
    if msg.chat.id != chat_id || msg.from.as_ref().map(|f| f.id) != Some(o.cfg.owner_id) {
        return;
    }
    if let Err(e) = owner_message(o, &msg).await {
        tracing::warn!("handling owner message failed: {e:#}");
        let _ = o
            .chat
            .send(
                From::Orchestrator,
                msg.topic_id(),
                &format!("⚠️ {}", esc(&format!("{e:#}"))),
                vec![],
                Some(msg.message_id),
                Notify::Sound,
            )
            .await;
    }
}

/// `kind|item|choice` from our own buttons.
fn callback(data: &str) -> Result<(i64, Answer)> {
    let mut parts = data.splitn(3, '|');
    let (kind, id, choice) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let id: i64 = id.parse().context("bad button")?;
    let ans = match (kind, choice) {
        ("h" | "q", n) => Answer::Choice(n.parse().context("bad option")?),
        ("p", _) => Answer::Approve,
        ("a", "y") => Answer::Allow,
        ("a", _) => Answer::Deny,
        ("x", "y") => Answer::Accept,
        // A reply draft is edited by replying to its card; the button says how.
        ("e", _) => anyhow::bail!(
            "Reply to the draft with the new text: it replaces the draft as you write it."
        ),
        ("x", _) => Answer::Dismiss,
        _ => anyhow::bail!("unknown button"),
    };
    Ok((id, ans))
}

pub(crate) async fn owner_message(o: &Arc<Orchestrator>, msg: &Message) -> Result<()> {
    let Some(text) = msg
        .text_or_caption()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(());
    };
    let topic = msg.topic_id();
    if text.starts_with('/') {
        return command(o, msg, topic, text).await;
    }
    // A reply to a question answers it, wherever it is.
    if let Some(reply) = &msg.reply_to_message
        && let Some(item) = human::by_message(o, reply.message_id).await?
    {
        let ans = match item.kind.as_str() {
            "plan" => Answer::Revise(text.to_owned()),
            _ => Answer::Text(text.to_owned()),
        };
        let res = human::answer(o, item.id, ans).await?;
        o.chat
            .send(
                From::Orchestrator,
                topic,
                &esc(&res),
                vec![],
                Some(msg.message_id),
                // The owner just acted; the confirmation needs no sound.
                Notify::Silent,
            )
            .await?;
        return Ok(());
    }
    // The owner sees at once that the message arrived (a reaction) and that work is going on
    // ("typing…" until the agent's turn ends). Fire and forget: delivery never waits for it.
    // In a task topic "typing…" comes from the agent who is working, not the orchestrator.
    let (task_id, delivered) = match topic {
        None => {
            o.show_working(msg.message_id, From::Orchestrator, None, BRAIN_TASK);
            // Kept as what the owner asked for, apart from the transcript's own lines: it is
            // what `habits` reflects on to notice a request becoming a routine.
            o.record_owner_request(text).await;
            (
                BRAIN_TASK.to_owned(),
                tasks::to_brain_as(
                    o,
                    format!("[Message from the owner] {text}"),
                    tasks::Origin::Owner(text.to_owned()),
                )
                .await,
            )
        }
        // The Inbox topic is the secretary's.
        Some(t) if crate::topics::inbox_if_any(o).await? == Some(t) => {
            return crate::secretary::owner_message(
                o,
                msg.message_id,
                text,
                msg.reply_to_message.as_ref().map(|r| r.message_id),
            )
            .await;
        }
        Some(t) => {
            let task: Option<TaskRow> = sqlx::query_as("select * from tasks where topic_id = $1")
                .bind(t)
                .fetch_optional(&o.db)
                .await?;
            match task {
                Some(task) => {
                    let from = o.task_sender(&task).await;
                    o.show_working(msg.message_id, from, Some(t), &task.id);
                    let r = tasks::deliver_as(
                        o,
                        &task,
                        format!("[Message from the human] {text}"),
                        tasks::Origin::Owner(text.to_owned()),
                    )
                    .await;
                    (task.id, r)
                }
                // Human and feed topics only take replies to specific items.
                None => return Ok(()),
            }
        }
    };
    // Kept for later (no node can take it now) or not delivered: nobody is working on it. Nor
    // while the agent waits on the owner (a plan, a question, a permission, a blocking ask): the
    // message waits in its session until the owner answers, so only the 👀 stays.
    let blocked = matches!(delivered, Ok(tasks::Delivered::ToRun))
        && sqlx::query_scalar::<_, bool>(
            "select exists(select 1 from human_items where task_id = $1 and state = 'open' and blocking)",
        )
        .bind(&task_id)
        .fetch_one(&o.db)
        .await
        .unwrap_or(false);
    if blocked || !matches!(delivered, Ok(tasks::Delivered::ToRun)) {
        o.stop_working(&task_id);
    }
    delivered.map(drop)
}

async fn command(
    o: &Arc<Orchestrator>,
    msg: &Message,
    topic: Option<i64>,
    text: &str,
) -> Result<()> {
    let (cmd, arg) = text
        .split_once(char::is_whitespace)
        .map(|(c, a)| (c, a.trim()))
        .unwrap_or((text, ""));
    let cmd = cmd.split('@').next().unwrap_or(cmd);
    let html = match cmd {
        "/status" => status(o).await?,
        "/queue" => {
            let items = human::open_items(o).await?;
            if items.is_empty() {
                "Nothing is waiting on you.".into()
            } else {
                let mut s = String::from("<b>Waiting on you</b>\n");
                for i in items.iter().take(30) {
                    s.push_str(&format!(
                        "{} Q-{} · {} · {}\n",
                        if i.blocking { "🔴" } else { "🟡" },
                        i.id,
                        esc(&i.kind),
                        esc(i.task_id.as_deref().or(i.project.as_deref()).unwrap_or("-"))
                    ));
                }
                s
            }
        }
        // Wakeups are scheduled behaviour that would otherwise be invisible.
        "/wakeups" => {
            let rows: Vec<crate::model::WakeupRow> = sqlx::query_as(
                "select * from wakeups where state in ('armed', 'drafting') order by fires_at nulls last, id limit 30",
            )
            .fetch_all(&o.db)
            .await?;
            if rows.is_empty() {
                "Nothing is waiting to be woken.".into()
            } else {
                let mut s = String::from("<b>Armed wakeups</b>\n");
                for w in &rows {
                    s.push_str(&format!(
                        "W-{} · {} · {}{} · {}\n",
                        w.id,
                        esc(&w.task_id),
                        match w.fires_at {
                            Some(f) => f.format("%Y-%m-%d %H:%M UTC").to_string(),
                            None => "when it holds".into(),
                        },
                        w.every
                            .as_deref()
                            .map(|e| format!(" (every {})", esc(e)))
                            .unwrap_or_default(),
                        esc(&clip(w.wish.as_deref().unwrap_or(&w.note), 120))
                    ));
                }
                s
            }
        }
        // What acts without asking, and the way to stop it.
        "/habits" => {
            let rows: Vec<crate::model::HabitRow> =
                sqlx::query_as("select * from habits order by id")
                    .fetch_all(&o.db)
                    .await?;
            if rows.is_empty() {
                "No habits yet. I offer one when I see you deciding the same thing over and \
                 over, and you decide whether to take it."
                    .into()
            } else {
                let mut s = String::from("<b>Habits</b>\n");
                for h in &rows {
                    s.push_str(&format!(
                        "{} H-{} · {}\n",
                        if h.active { "🔁" } else { "⏸" },
                        h.id,
                        esc(&crate::habits::describe(h))
                    ));
                }
                s.push_str("<i>Each one's card in the Rules topic has Pause and Delete.</i>");
                s
            }
        }
        "/tasks" => {
            let rows: Vec<(String, String, String, bool)> =
                sqlx::query_as("select id, title, status, running from tasks where id not in ('brain', 'secretary', 'retro') and status not in ('done') order by updated_at desc limit 30")
                    .fetch_all(&o.db)
                    .await?;
            if rows.is_empty() {
                "No open tasks.".into()
            } else {
                // A task in progress whose agent finished its turn keeps the status `running`
                // with no session behind it: saying "running" there is not true (see the web
                // UI's StatusBadge, which reads the same way).
                rows.iter()
                    .map(|(id, t, s, live)| {
                        let shown = match (s.as_str(), live) {
                            ("running" | "planning", false) => "idle",
                            (other, _) => other,
                        };
                        format!(
                            "<b>{}</b> [{}]{} {}",
                            esc(id),
                            esc(shown),
                            if *live { " ▶" } else { "" },
                            esc(t)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "/stop" => {
            let id = if arg.is_empty() {
                let t: Option<String> = match topic {
                    Some(t) => sqlx::query_scalar("select id from tasks where topic_id = $1").bind(t).fetch_optional(&o.db).await?,
                    None => None,
                };
                t.context("use /stop <task-id> or send it in the task's topic")?
            } else {
                arg.to_owned()
            };
            let task = o.task(&id).await?.with_context(|| format!("no task {id}"))?;
            tasks::stop(o, &task, false).await?;
            format!("Stopping <b>{}</b>.", esc(&id))
        }
        // `/web` opens the dashboard; `/web /sessions?project=dap` opens exactly that page, so a
        // link can point at whatever is being discussed.
        "/web" => {
            let page = arg.trim();
            let page = if page.is_empty() { "/" } else { page };
            let url = crate::web::magic_link(o, page).await?;
            format!(
                "<a href=\"{}\">Open <code>{}</code></a> (link valid for one login).",
                esc(&url),
                esc(page)
            )
        }
        "/invite" => {
            let ticket = crate::web::new_invite(o).await?;
            format!("Run on the new host:\n<code>fleet join {}</code>\nSingle use.", esc(&ticket))
        }
        "/brain_reset" => {
            tasks::reset_builtin(o, crate::model::BRAIN_TASK).await?;
            "The orchestrator brain will start a fresh session on your next message.".into()
        }
        "/secretary_reset" => {
            tasks::reset_builtin(o, crate::model::SECRETARY_TASK).await?;
            "The secretary will start a fresh session with its next batch or your next message in the Inbox (its todos and rules stay).".into()
        }
        "/retro_reset" => {
            tasks::reset_builtin(o, crate::model::RETRO_TASK).await?;
            "The retrospective will start a fresh session at its next run (its proposals stay)."
                .into()
        }
        // What the fleet concluded about its own days, and what came of it. `/retro now [days]`
        // asks for a review right away, over as many days as you say (the scheduled one covers
        // the last day).
        "/retro" if arg.split_whitespace().next() == Some("now") => {
            let days = arg
                .split_whitespace()
                .nth(1)
                .and_then(|d| d.parse::<i64>().ok())
                .unwrap_or(1);
            crate::retro::run_now(o, days).await?
        }
        "/retro" => {
            let rows: Vec<(i64, String, String, String, Option<String>)> = sqlx::query_as(
                "select id, area, title, state, task_id from retro_proposals
                  order by (state = 'open') desc, id desc limit 20",
            )
            .fetch_all(&o.db)
            .await?;
            let last: Option<(String, Option<String>, i32)> = sqlx::query_as(
                "select day, verdict, proposals from retro_runs order by day desc limit 1",
            )
            .fetch_optional(&o.db)
            .await?;
            let mut s = String::new();
            match last {
                Some((day, verdict, n)) => s.push_str(&format!(
                    "<b>Last retrospective · {}</b> ({n} proposal(s))\n{}\n\n",
                    esc(&day),
                    esc(verdict.as_deref().unwrap_or("still thinking…"))
                )),
                None => s.push_str(
                    "No retrospective has run yet. It runs daily once FLEET_RETRO_HOUR is set.\n\n",
                ),
            }
            if rows.is_empty() {
                s.push_str("No proposals.");
            } else {
                s.push_str("<b>Proposals</b>\n");
                for (id, area, title, state, task) in &rows {
                    s.push_str(&format!(
                        "{} P-{id} · {} · {}{}\n",
                        match state.as_str() {
                            "open" => "💡",
                            "tasked" => "🛠",
                            _ => "✖",
                        },
                        esc(area),
                        esc(&clip(title, 90)),
                        task.as_deref()
                            .map(|t| format!(" → {}", esc(t)))
                            .unwrap_or_default()
                    ));
                }
                s.push_str("<i>The cards in 📈 Retro have Make task and Dismiss.</i>");
            }
            s
        }
        "/help" | "/start" => "Write in General to talk to the orchestrator, or in 📋 Inbox to talk to the secretary. Reply in a task topic to talk to its agent; reply to a question to answer it.\n/status /queue /tasks /habits /retro /wakeups /stop [task] /web /invite /brain_reset /secretary_reset /retro_reset".into(),
        _ => format!("Unknown command {}. Try /help.", esc(cmd)),
    };
    o.chat
        .send(
            From::Orchestrator,
            topic,
            &html,
            vec![],
            Some(msg.message_id),
            // An answer to a command the owner just typed.
            Notify::Silent,
        )
        .await?;
    Ok(())
}

async fn status(o: &Orchestrator) -> Result<String> {
    let nodes: Vec<crate::model::NodeRow> = sqlx::query_as("select * from nodes order by name")
        .fetch_all(&o.db)
        .await?;
    let mut s = String::from("<b>Nodes</b>\n");
    for n in &nodes {
        let disk = match (
            crate::disk::DiskState::parse(&n.disk_state),
            crate::disk::least_free(n),
        ) {
            (crate::disk::DiskState::Ok, _) | (_, None) => String::new(),
            (state, Some(free)) => format!(
                " · {} {} free",
                if state == crate::disk::DiskState::Full {
                    "🛑"
                } else {
                    "💾"
                },
                crate::disk::bytes(free)
            ),
        };
        s.push_str(&format!(
            "{} {} · {}/{} agents · {} projects{}\n",
            if n.online { "🟢" } else { "⚪" },
            esc(&n.name),
            n.running.0.len(),
            n.max_agents,
            n.projects.0.len(),
            disk
        ));
    }
    if nodes.is_empty() {
        s.push_str("none yet (/invite)\n");
    }
    let (active, waiting): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where status in ('planning','running','waiting')), count(*) filter (where status = 'queued') from tasks where id not in ('brain', 'secretary')",
    )
    .fetch_one(&o.db)
    .await?;
    let open: i64 = sqlx::query_scalar("select count(*) from human_items where state = 'open'")
        .fetch_one(&o.db)
        .await?;
    let free_bots: i64 = sqlx::query_scalar("select count(*) from bots where agent_id is null")
        .fetch_one(&o.db)
        .await?;
    s.push_str(&format!("\n<b>Tasks</b>: {active} active, {waiting} queued\n<b>Waiting on you</b>: {open}\n<b>Free bots</b>: {free_bots}"));
    if free_bots < 2 {
        s.push_str(" ⚠️ add bots on the web Agents page");
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_parsing() {
        assert!(matches!(
            callback("h|12|3").unwrap(),
            (12, Answer::Choice(3))
        ));
        assert!(matches!(callback("p|5|ok").unwrap(), (5, Answer::Approve)));
        assert!(matches!(callback("a|5|y").unwrap(), (5, Answer::Allow)));
        assert!(matches!(callback("a|5|n").unwrap(), (5, Answer::Deny)));
        assert!(matches!(callback("x|5|y").unwrap(), (5, Answer::Accept)));
        assert!(matches!(callback("x|5|n").unwrap(), (5, Answer::Dismiss)));
        assert!(callback("zz|5|n").is_err());
        assert!(callback("h|x|1").is_err());
    }
}
