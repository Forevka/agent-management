//! Client-side flood control.
//!
//! Telegram documents its limits only loosely: about one message per second per chat, and about
//! twenty per minute per bot in one group. Exceeding them earns a `429` whose `retry_after` can
//! be much longer than the burst that caused it, so it pays to stay under them up front and
//! treat `429` as the exception. Edits count against the same limits as new messages.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::bot::{Bot, SendMessage};
use crate::error::TgError;
use crate::types::{ForumTopic, InlineKeyboardMarkup, Message, ParseMode};

/// One token bucket: up to `capacity` calls at once, then one more per `refill_every`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub capacity: u32,
    pub refill_every: Duration,
}

impl Bucket {
    /// About one message per second in any one chat.
    pub const PER_CHAT: Bucket = Bucket {
        capacity: 1,
        refill_every: Duration::from_secs(1),
    };

    /// About twenty messages per minute per bot in one group. A bucket of capacity `c` refilled
    /// every `t` admits at most `c + 60s / t` calls in any minute, so 4 + 16 keeps every
    /// sliding minute at twenty while still letting a short burst through without delay.
    pub const PER_GROUP: Bucket = Bucket {
        capacity: 4,
        refill_every: Duration::from_millis(3750),
    };
}

/// An async rate limiter made of one or more token buckets that must all admit a call.
///
/// Clones share state. Waiters are served in arrival order, which keeps messages to one chat in
/// the order they were sent.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    state: Arc<tokio::sync::Mutex<State>>,
}

#[derive(Debug)]
struct State {
    buckets: Vec<BucketState>,
    /// Set from a `429`: nobody sharing this limiter calls before then.
    blocked_until: Option<Instant>,
}

/// A token bucket kept as a GCRA "theoretical arrival time" rather than a fractional token
/// count, so every deadline is an exact `Instant` and paused-clock tests are deterministic.
#[derive(Debug)]
struct BucketState {
    interval: Duration,
    /// How far ahead of now the arrival time may run: room for `capacity - 1` extra calls.
    burst: Duration,
    /// When the bucket would be full again if nothing else were taken. `None` = full.
    tat: Option<Instant>,
}

impl BucketState {
    fn new(bucket: Bucket) -> Self {
        let interval = bucket.refill_every;
        let burst = interval.saturating_mul(bucket.capacity.saturating_sub(1));
        Self {
            interval,
            burst,
            tat: None,
        }
    }

    /// The earliest instant this bucket admits another call.
    fn ready_at(&self, now: Instant) -> Instant {
        match self.tat {
            // `checked_sub` only fails for instants before the clock's origin, which are
            // long past, so the call is admitted immediately.
            Some(tat) => tat
                .checked_sub(self.burst)
                .map_or(now, |ready| ready.max(now)),
            None => now,
        }
    }

    fn take(&mut self, now: Instant) {
        let base = self.tat.map_or(now, |tat| tat.max(now));
        self.tat = Some(base + self.interval);
    }
}

impl State {
    /// Waits until every bucket admits a call, then takes one token from each.
    async fn take(&mut self) {
        loop {
            let now = Instant::now();
            let ready = self
                .buckets
                .iter()
                .map(|bucket| bucket.ready_at(now))
                .chain(self.blocked_until)
                .fold(now, Instant::max);
            if ready <= now {
                for bucket in &mut self.buckets {
                    bucket.take(now);
                }
                return;
            }
            tokio::time::sleep_until(ready).await;
        }
    }

    fn block_for(&mut self, wait: Duration) {
        let until = Instant::now() + wait;
        self.blocked_until = Some(
            self.blocked_until
                .map_or(until, |current| current.max(until)),
        );
    }
}

impl RateLimiter {
    /// A limiter that admits a call only when every bucket has a token.
    pub fn new(buckets: &[Bucket]) -> Self {
        let state = State {
            buckets: buckets.iter().copied().map(BucketState::new).collect(),
            blocked_until: None,
        };
        Self {
            state: Arc::new(tokio::sync::Mutex::new(state)),
        }
    }

    /// The limits for one bot posting into one group: [`Bucket::PER_CHAT`] and
    /// [`Bucket::PER_GROUP`].
    pub fn telegram_group() -> Self {
        Self::new(&[Bucket::PER_CHAT, Bucket::PER_GROUP])
    }

    /// Waits for a slot and takes it.
    pub async fn acquire(&self) {
        self.state.lock().await.take().await;
    }

    /// Takes a slot if one is free right now, without waiting.
    pub fn try_acquire(&self) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return false;
        };
        let now = Instant::now();
        let free = state.blocked_until.is_none_or(|until| until <= now)
            && state
                .buckets
                .iter()
                .all(|bucket| bucket.ready_at(now) <= now);
        if free {
            for bucket in &mut state.buckets {
                bucket.take(now);
            }
        }
        free
    }

    /// Holds every caller of this limiter back for `wait`, as Telegram asks with `retry_after`.
    pub async fn block_for(&self, wait: Duration) {
        self.state.lock().await.block_for(wait);
    }
}

/// Runs `call` once the limiter admits it, and again after the requested wait each time it
/// fails with [`TgError::RetryAfter`], up to `max_attempts` calls in all.
///
/// The limiter stays held for the whole sequence, retries included, so a message that hit flood
/// control keeps its place ahead of messages queued behind it. Transport errors are not retried:
/// a send that timed out may still have been delivered, and a retry would post it twice.
pub async fn call_with_retry<T, F, Fut>(
    limiter: &RateLimiter,
    max_attempts: u32,
    mut call: F,
) -> Result<T, TgError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, TgError>>,
{
    let mut state = limiter.state.lock().await;
    let mut attempt = 1;
    loop {
        state.take().await;
        match call().await {
            Err(TgError::RetryAfter(seconds)) => {
                // Block the limiter even when giving up, so later callers honour the wait too.
                state.block_for(Duration::from_secs(seconds));
                if attempt >= max_attempts {
                    return Err(TgError::RetryAfter(seconds));
                }
                tracing::warn!(
                    seconds,
                    attempt,
                    "telegram flood control; waiting before retry"
                );
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// A [`Bot`] whose sends and edits go through a per-chat [`RateLimiter`] and are retried on
/// `429`. Use one per bot: Telegram's limits are per bot, so bots must not share limiters.
#[derive(Debug, Clone)]
pub struct Throttled {
    bot: Bot,
    template: Vec<Bucket>,
    limiters: Arc<Mutex<HashMap<i64, RateLimiter>>>,
    max_attempts: u32,
}

impl Throttled {
    /// Default attempts per call, the first included.
    pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

    /// Wraps `bot` with [`RateLimiter::telegram_group`] limits for every chat.
    pub fn new(bot: Bot) -> Self {
        Self::with_buckets(bot, &[Bucket::PER_CHAT, Bucket::PER_GROUP])
    }

    /// Wraps `bot`, giving every chat a limiter built from `buckets`.
    pub fn with_buckets(bot: Bot, buckets: &[Bucket]) -> Self {
        Self {
            bot,
            template: buckets.to_vec(),
            limiters: Arc::new(Mutex::new(HashMap::new())),
            max_attempts: Self::DEFAULT_MAX_ATTEMPTS,
        }
    }

    /// Changes how many attempts a call gets (at least one).
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    /// The underlying bot, for calls that need no throttling (polling, callback answers).
    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    /// The limiter for `chat_id`, created on first use.
    pub fn limiter(&self, chat_id: i64) -> RateLimiter {
        // A poisoned map still holds valid limiters; a panic elsewhere must not stop sends.
        let mut limiters = self
            .limiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        limiters
            .entry(chat_id)
            .or_insert_with(|| RateLimiter::new(&self.template))
            .clone()
    }

    /// Runs any call against `chat_id`'s limiter, with the retry policy.
    pub async fn run<T, F, Fut>(&self, chat_id: i64, call: F) -> Result<T, TgError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, TgError>>,
    {
        call_with_retry(&self.limiter(chat_id), self.max_attempts, call).await
    }

    /// Throttled [`Bot::send_message`].
    pub async fn send_message(&self, message: &SendMessage) -> Result<Message, TgError> {
        self.run(message.chat_id, || self.bot.send_message(message))
            .await
    }

    /// Throttled [`Bot::edit_message_text`].
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
        reply_markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<(), TgError> {
        self.run(chat_id, || {
            self.bot
                .edit_message_text(chat_id, message_id, text, parse_mode, reply_markup)
        })
        .await
    }

    /// Throttled [`Bot::edit_message_reply_markup`].
    pub async fn edit_message_reply_markup(
        &self,
        chat_id: i64,
        message_id: i64,
        reply_markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<(), TgError> {
        self.run(chat_id, || {
            self.bot
                .edit_message_reply_markup(chat_id, message_id, reply_markup)
        })
        .await
    }

    /// Throttled [`Bot::send_document`].
    pub async fn send_document(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        file_name: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> Result<Message, TgError> {
        // Each attempt consumes its own copy because the multipart body takes ownership.
        self.run(chat_id, || {
            self.bot
                .send_document(chat_id, thread_id, file_name, bytes.clone(), caption)
        })
        .await
    }

    /// Throttled [`Bot::create_forum_topic`]. Creating a topic posts a service message into the
    /// group, so it counts against the same limit.
    pub async fn create_forum_topic(
        &self,
        chat_id: i64,
        name: &str,
    ) -> Result<ForumTopic, TgError> {
        self.run(chat_id, || self.bot.create_forum_topic(chat_id, name))
            .await
    }

    /// Throttled [`Bot::edit_forum_topic`], which also posts a service message.
    pub async fn edit_forum_topic(
        &self,
        chat_id: i64,
        thread_id: i64,
        name: &str,
    ) -> Result<(), TgError> {
        self.run(chat_id, || {
            self.bot.edit_forum_topic(chat_id, thread_id, name)
        })
        .await
    }

    /// Throttled [`Bot::delete_message`]. Deletes are not known to count against the send
    /// limit, but going through the limiter keeps them ordered after pending sends.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<(), TgError> {
        self.run(chat_id, || self.bot.delete_message(chat_id, message_id))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(start: Instant) -> f64 {
        start.elapsed().as_secs_f64()
    }

    #[tokio::test(start_paused = true)]
    async fn per_chat_bucket_spaces_calls_one_second_apart() {
        let limiter = RateLimiter::new(&[Bucket::PER_CHAT]);
        let start = Instant::now();
        let mut at = Vec::new();
        for _ in 0..4 {
            limiter.acquire().await;
            at.push(secs(start));
        }
        assert_eq!(at, vec![0.0, 1.0, 2.0, 3.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn burst_then_refill_rate() {
        let limiter = RateLimiter::new(&[Bucket {
            capacity: 3,
            refill_every: Duration::from_secs(20),
        }]);
        let start = Instant::now();
        let mut at = Vec::new();
        for _ in 0..5 {
            limiter.acquire().await;
            at.push(secs(start));
        }
        assert_eq!(at, vec![0.0, 0.0, 0.0, 20.0, 40.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_refills_while_idle() {
        let limiter = RateLimiter::new(&[Bucket {
            capacity: 2,
            refill_every: Duration::from_secs(10),
        }]);
        limiter.acquire().await;
        limiter.acquire().await;
        assert!(!limiter.try_acquire());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(limiter.try_acquire());
        assert!(!limiter.try_acquire());
        // A long idle spell refills only up to capacity, never beyond.
        tokio::time::advance(Duration::from_secs(600)).await;
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert!(!limiter.try_acquire());
    }

    #[tokio::test(start_paused = true)]
    async fn group_limits_never_exceed_twenty_per_minute() {
        let limiter = RateLimiter::telegram_group();
        let start = Instant::now();
        let mut at = Vec::new();
        for _ in 0..60 {
            limiter.acquire().await;
            at.push(start.elapsed());
        }
        // Consecutive calls are never closer than the per-chat second.
        assert!(at.windows(2).all(|w| w[1] - w[0] >= Duration::from_secs(1)));
        // No sliding sixty-second window holds more than twenty calls.
        for (i, first) in at.iter().enumerate() {
            let in_window = at[i..]
                .iter()
                .take_while(|t| **t - *first < Duration::from_secs(60))
                .count();
            assert!(
                in_window <= 20,
                "{in_window} calls within a minute starting at {first:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn block_for_delays_every_holder() {
        let limiter = RateLimiter::new(&[Bucket::PER_CHAT]);
        let start = Instant::now();
        limiter.block_for(Duration::from_secs(7)).await;
        let other = limiter.clone();
        other.acquire().await;
        assert_eq!(secs(start), 7.0);
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_are_served_in_arrival_order() {
        let limiter = RateLimiter::new(&[Bucket::PER_CHAT]);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for i in 0..5 {
            let limiter = limiter.clone();
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                limiter.acquire().await;
                order.lock().unwrap_or_else(|p| p.into_inner()).push(i);
            }));
            // Let each task reach the queue before the next one is spawned.
            tokio::task::yield_now().await;
        }
        for task in tasks {
            task.await.expect("task panicked");
        }
        assert_eq!(
            *order.lock().unwrap_or_else(|p| p.into_inner()),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_sleeps_then_retries() {
        let limiter = RateLimiter::new(&[Bucket::PER_CHAT]);
        let start = Instant::now();
        let mut calls = 0;
        let result = call_with_retry(&limiter, 3, || {
            calls += 1;
            let attempt = calls;
            async move {
                if attempt == 1 {
                    Err(TgError::RetryAfter(5))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(result.ok(), Some(2));
        assert_eq!(secs(start), 5.0);
        // The wait applies to the next caller as well, through the shared bucket.
        limiter.acquire().await;
        assert_eq!(secs(start), 6.0);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_gives_up_after_max_attempts() {
        let limiter = RateLimiter::new(&[Bucket::PER_CHAT]);
        let mut calls = 0;
        let result: Result<(), _> = call_with_retry(&limiter, 3, || {
            calls += 1;
            async { Err(TgError::RetryAfter(2)) }
        })
        .await;
        assert!(matches!(result, Err(TgError::RetryAfter(2))));
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn other_errors_are_not_retried() {
        let limiter = RateLimiter::new(&[Bucket::PER_CHAT]);
        let mut calls = 0;
        let result: Result<(), _> = call_with_retry(&limiter, 3, || {
            calls += 1;
            async {
                Err(TgError::Api {
                    code: 400,
                    description: "Bad Request: chat not found".into(),
                })
            }
        })
        .await;
        assert!(matches!(result, Err(TgError::Api { code: 400, .. })));
        assert_eq!(calls, 1);
    }
}
