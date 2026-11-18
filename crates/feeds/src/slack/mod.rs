//! Slack, two ways.
//!
//! - [`SlackSocket`] (in `socket.rs`) receives every message in every conversation the owner is
//!   in, pushed through Socket Mode by the owner's own Slack app. It needs an app-level token as
//!   well as the user token.
//! - [`SlackSource`] (this file) polls a fixed list of channels through the Web API with nothing
//!   but the user's token. It is what runs when no app token is configured.
//!
//! For the poller the request budget is the scarce resource (since 2025 Slack throttles
//! `conversations.history` and `conversations.replies` to one request a minute for apps that are
//! neither internal nor in the Marketplace). The design follows from that:
//!
//! - A quiet channel costs exactly one `conversations.history` request per poll: everything after
//!   the channel's cursor, which is empty when nothing happened.
//! - Replies in threads do not show up in channel history, so a thread's activity is noticed on
//!   its root message (`reply_count`, `latest_reply`). Every
//!   [thread sweep interval](SlackSource::with_thread_sweep_interval) that same single request
//!   reaches back over the last day instead of only past the cursor, which shows the current
//!   `latest_reply` of every recent root. Only threads whose `latest_reply` moved cost a
//!   `conversations.replies` call, and only a bounded number per poll.
//! - Names (users, channels, the workspace URL for permalinks) are looked up once and cached.
//!
//! Cursor safety: a channel's cursor only moves past a contiguous run of messages that this poll
//! returns. History pages arrive newest first, so a rate limit in the middle of paginating would
//! leave a gap below the pages already fetched; those pages are held in memory and the next poll
//! resumes the pagination, and only a complete run is returned.

mod api;
mod directory;
mod mrkdwn;
mod pace;
mod socket;
mod state;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Capabilities, ConversationKind, FeedError, FeedMessage, Replier, Source};
use api::{Api, AuthTest, ConversationInfo, Page, Posted, RawMessage, UserInfo, is_fatal};
use state::{ChannelState, Cursor, ThreadState, ts_from_unix, ts_gt, ts_max, ts_unix};

pub use socket::SlackSocket;

/// The public Slack Web API.
pub const DEFAULT_BASE_URL: &str = "https://slack.com/api";

const KIND: &str = "slack";
/// Slack's documented maximum page size for history and replies.
const MAX_PAGE_SIZE: u32 = 200;
/// More cached user names than this and the cache starts over, so memory stays bounded in a
/// workspace with many thousands of members.
const USER_CACHE_CAP: usize = 2000;
/// `users.info` is a cheaper tier than history, but a burst of strangers should still not turn
/// one poll into hundreds of requests; names not looked up now are looked up next time.
const MAX_USER_LOOKUPS_PER_POLL: usize = 50;

/// Subtypes worth mirroring. Everything else (joins, leaves, topic changes, bot installs, pins,
/// deletions...) is channel housekeeping, skipped but still counted as seen.
const KEPT_SUBTYPES: &[&str] = &[
    "thread_broadcast",
    "file_share",
    "me_message",
    "bot_message",
    "message_changed",
];

/// A [`Source`] for the Slack channels the user is a member of.
#[derive(Debug)]
pub struct SlackSource {
    api: Api,
    channels: Vec<String>,
    cursor: Cursor,

    page_size: u32,
    initial_backfill: u32,
    thread_window: Duration,
    thread_sweep_interval: Duration,
    max_threads: usize,
    max_thread_fetches_per_poll: usize,
    max_direct_checks_per_sweep: usize,
    min_interval: Duration,
    max_interval: Duration,

    /// The idle back-off; see [`suggested_interval`](Self::suggested_interval).
    interval: Duration,
    rate_limited_until: Option<Instant>,
    /// Which channel goes first this poll. Rotating it keeps a rate limit that always strikes
    /// after the same number of requests from starving the channels at the end of the list.
    rotation: usize,
    /// Partly paginated history, per channel, waiting to be completed by the next poll.
    pending_history: HashMap<String, PendingHistory>,
    /// User id to display name; `None` when Slack does not know the user.
    users: HashMap<String, Option<String>>,
    /// Channel id to name; `None` when the lookup failed for good.
    channel_names: HashMap<String, Option<String>>,
    /// Channel id to its kind, learned from the same `conversations.info` call as the name.
    channel_kinds: HashMap<String, ConversationKind>,
    /// Whether the owner is in each channel, and when that was checked.
    members: HashMap<String, (bool, Instant)>,
    auth: Option<AuthInfo>,
    last_errors: Vec<(String, FeedError)>,
}

/// How long a membership answer is trusted by the poller: a "no" briefly, so joining the channel
/// takes effect within minutes; a "yes" for hours.
const MEMBER_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const NOT_MEMBER_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Default, Clone)]
struct AuthInfo {
    /// The workspace URL (`https://team.slack.com/`), for building permalinks without a
    /// `chat.getPermalink` request per message.
    url: Option<String>,
    user_id: Option<String>,
}

#[derive(Debug)]
struct PendingHistory {
    /// The channel cursor the pagination started from; resuming is only valid while it holds.
    cursor_at_start: String,
    oldest: String,
    next_cursor: String,
    messages: Vec<RawMessage>,
}

/// A message on its way out, before names are resolved.
struct Collected {
    channel: String,
    thread_id: Option<String>,
    msg: RawMessage,
}

impl SlackSource {
    /// A source for `channels` (channel ids such as `C0123ABCD`), authenticated with the user's
    /// token (`xoxp-…`). The token is only ever sent in the `Authorization` header and is redacted
    /// from `Debug` output.
    pub fn new(token: impl Into<String>, channels: Vec<String>) -> Self {
        let mut seen = HashSet::new();
        let channels = channels
            .into_iter()
            .filter(|c| seen.insert(c.clone()))
            .collect();
        Self {
            api: Api::new(token.into(), DEFAULT_BASE_URL.to_owned()),
            channels,
            cursor: Cursor::new(),
            page_size: MAX_PAGE_SIZE,
            initial_backfill: 20,
            thread_window: Duration::from_secs(24 * 60 * 60),
            thread_sweep_interval: Duration::from_secs(60),
            max_threads: 200,
            max_thread_fetches_per_poll: 10,
            max_direct_checks_per_sweep: 2,
            min_interval: Duration::from_secs(15),
            max_interval: Duration::from_secs(120),
            interval: Duration::from_secs(15),
            rate_limited_until: None,
            rotation: 0,
            pending_history: HashMap::new(),
            users: HashMap::new(),
            channel_names: HashMap::new(),
            channel_kinds: HashMap::new(),
            members: HashMap::new(),
            auth: None,
            last_errors: Vec::new(),
        }
    }

    /// Points the source at another Web API root (a mock server in tests). The default is
    /// [`DEFAULT_BASE_URL`].
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        self.api.set_base(base_url.as_ref());
        self
    }

    /// The `limit` asked of history and replies (1 to 200, default 200). Lower it for tokens that
    /// Slack caps at fewer messages per request.
    pub fn with_page_size(mut self, n: u32) -> Self {
        self.page_size = n.clamp(1, MAX_PAGE_SIZE);
        self
    }

    /// How many existing messages the first poll of a channel returns (default 20, at most 200),
    /// as context. Zero starts silently from the newest message. The backfill is one request:
    /// the first poll never pages through a channel's whole history.
    pub fn with_initial_backfill(mut self, n: u32) -> Self {
        self.initial_backfill = n.min(MAX_PAGE_SIZE);
        self
    }

    /// How long a thread stays tracked after its last activity, and how far back a sweep looks
    /// for thread roots (default 24 hours).
    pub fn with_thread_window(mut self, window: Duration) -> Self {
        self.thread_window = window;
        self
    }

    /// How often a channel's poll reaches back over the thread window to notice new replies
    /// (default 60 s). A sweep is still a single history request, just a larger answer.
    pub fn with_thread_sweep_interval(mut self, every: Duration) -> Self {
        self.thread_sweep_interval = every;
        self
    }

    /// How many threads per channel are tracked at most (default 200, the most recently active).
    pub fn with_max_tracked_threads(mut self, n: usize) -> Self {
        self.max_threads = n.max(1);
        self
    }

    /// How many `conversations.replies` calls one poll may make across all channels (default
    /// 10). Threads beyond it stay pending and are fetched by later polls.
    pub fn with_max_thread_fetches_per_poll(mut self, n: usize) -> Self {
        self.max_thread_fetches_per_poll = n;
        self
    }

    /// The range [`suggested_interval`](Self::suggested_interval) moves in (default 15 s to
    /// 120 s).
    pub fn with_poll_interval(mut self, min: Duration, max: Duration) -> Self {
        self.min_interval = min;
        self.max_interval = max.max(min);
        self.interval = min;
        self
    }

    /// The configured channel ids.
    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    /// How long to wait before the next poll: short right after messages arrived, doubling up
    /// to the maximum while the channels stay quiet, and never shorter than a pending rate
    /// limit.
    pub fn suggested_interval(&self) -> Duration {
        self.interval
            .max(self.rate_limited_for().unwrap_or_default())
    }

    /// How much longer Slack has asked us to wait, if it has.
    pub fn rate_limited_for(&self) -> Option<Duration> {
        let left = self
            .rate_limited_until?
            .saturating_duration_since(Instant::now());
        (!left.is_zero()).then_some(left)
    }

    /// Per-channel problems from the last poll that did not stop it (for example
    /// `not_in_channel` for one channel while the others worked), as `(channel id, error)`.
    pub fn last_errors(&self) -> &[(String, FeedError)] {
        &self.last_errors
    }

    /// The token owner's user id, known after the first poll. Messages the user posted
    /// (including replies sent through [`Source::post_reply`]) carry it as `author_id`.
    pub fn self_user_id(&self) -> Option<&str> {
        self.auth.as_ref()?.user_id.as_deref()
    }

    fn note_rate_limit(&mut self, wait: Duration) {
        let until = Instant::now() + wait;
        if self.rate_limited_until.is_none_or(|u| u < until) {
            self.rate_limited_until = Some(until);
        }
    }

    async fn ensure_auth(&mut self) -> Result<(), FeedError> {
        if self.auth.is_some() {
            return Ok(());
        }
        match self.api.get::<AuthTest>("auth.test", &[]).await {
            Ok(a) => {
                let url = a
                    .url
                    .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
                self.auth = Some(AuthInfo {
                    url,
                    user_id: a.user_id,
                });
                Ok(())
            }
            Err(e) if is_fatal(&e) => Err(e),
            Err(FeedError::RateLimited(d)) => Err(FeedError::RateLimited(d)),
            Err(FeedError::Http(e)) => {
                // Possibly transient: poll on without permalinks and try again next time.
                tracing::debug!(error = %e, "slack auth.test failed; retrying next poll");
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "slack auth.test failed; permalinks unavailable");
                self.auth = Some(AuthInfo::default());
                Ok(())
            }
        }
    }

    /// Whether the owner is in `channel`, from `conversations.info` (cached). Its name and kind
    /// are kept from the same answer.
    async fn check_membership(&mut self, channel: &str) -> Result<bool, FeedError> {
        if let Some((member, at)) = self.members.get(channel) {
            let ttl = if *member { MEMBER_TTL } else { NOT_MEMBER_TTL };
            if at.elapsed() < ttl {
                return Ok(*member);
            }
        }
        let info: ConversationInfo = self
            .api
            .get("conversations.info", &[("channel", channel)])
            .await?;
        let conv = info.channel.unwrap_or_default();
        let kind = conv.kind();
        let member = match kind {
            ConversationKind::Direct | ConversationKind::Group => true,
            // Slack states `is_member` for the token's user. Should it ever not, the owner
            // mapped this channel on purpose, which is taken as consent.
            _ => conv.is_member.unwrap_or(true),
        };
        self.channel_kinds.insert(channel.to_owned(), kind);
        self.channel_names.insert(channel.to_owned(), conv.name);
        self.members
            .insert(channel.to_owned(), (member, Instant::now()));
        Ok(member)
    }

    /// Fetches what is new in one channel's history into `out`, and notices thread activity.
    async fn poll_history(
        &mut self,
        channel: &str,
        st: &mut ChannelState,
        now: f64,
        out: &mut Vec<Collected>,
    ) -> Result<(), FeedError> {
        let Some(cursor) = st.last_ts.clone() else {
            return self.first_poll(channel, st, now, out).await;
        };

        let sweep = now - st.last_sweep >= self.thread_sweep_interval.as_secs_f64();
        let mut oldest = if sweep {
            let window_start = ts_from_unix(now - self.thread_window.as_secs_f64());
            if ts_gt(&cursor, &window_start) {
                window_start
            } else {
                cursor.clone()
            }
        } else {
            cursor.clone()
        };

        let mut collected = Vec::new();
        let mut next: Option<String> = None;
        if let Some(p) = self.pending_history.remove(channel)
            && p.cursor_at_start == cursor
        {
            oldest = p.oldest;
            collected = p.messages;
            next = Some(p.next_cursor);
        }
        let limit = self.page_size.to_string();
        let mut complete = true;
        loop {
            let mut query = vec![
                ("channel", channel),
                ("oldest", oldest.as_str()),
                ("inclusive", "false"),
                ("limit", limit.as_str()),
            ];
            if let Some(c) = &next {
                query.push(("cursor", c.as_str()));
            }
            let page: Page = match self.api.get("conversations.history", &query).await {
                Ok(p) => p,
                Err(e) => {
                    // Keep the pages already fetched, unless Slack says the pagination cursor
                    // itself is no longer good, in which case the next poll starts over.
                    let resumable = !matches!(&e, FeedError::Api(code) if code == "invalid_cursor");
                    if let (Some(next_cursor), true) = (next, resumable) {
                        self.pending_history.insert(
                            channel.to_owned(),
                            PendingHistory {
                                cursor_at_start: cursor,
                                oldest,
                                next_cursor,
                                messages: collected,
                            },
                        );
                    }
                    return Err(e);
                }
            };
            let next_cursor = page.next_cursor();
            let has_more = page.has_more;
            let msgs: Vec<RawMessage> = page
                .messages
                .into_iter()
                .filter_map(RawMessage::parse)
                .collect();
            let reached_cursor = msgs
                .iter()
                .any(|m| m.ts.as_deref().is_some_and(|ts| !ts_gt(ts, &cursor)));
            collected.extend(msgs);
            match next_cursor {
                Some(c) if has_more => {
                    // Everything new is in hand once a page reaches back to the cursor; the rest
                    // of a sweep only refreshes older thread roots, which is best effort.
                    if reached_cursor {
                        complete = false;
                        break;
                    }
                    next = Some(c);
                }
                None if has_more => {
                    tracing::warn!(channel, "slack history has_more without a cursor; stopping");
                    complete = false;
                    break;
                }
                _ => break,
            }
        }

        sort_by_ts(&mut collected);
        let mut last = cursor.clone();
        for m in collected.iter() {
            note_thread_activity(st, m);
        }
        for m in collected.iter() {
            let Some(ts) = m.ts.as_deref() else { continue };
            if !ts_gt(ts, &cursor) {
                continue;
            }
            last = ts_max(&last, ts);
            if keep(m) {
                out.push(Collected {
                    channel: channel.to_owned(),
                    thread_id: reply_thread(m),
                    msg: m.clone(),
                });
            }
        }
        st.last_ts = Some(last);

        if sweep {
            st.last_sweep = now;
            // Roots older than what this sweep saw cannot report new replies through it; check
            // a few of the tracked ones directly, least recently checked first.
            let covered_from = if complete {
                Some(oldest)
            } else {
                collected.iter().filter_map(|m| m.ts.clone()).next()
            };
            let active_after = now - self.thread_window.as_secs_f64();
            let mut uncovered: Vec<(&String, &mut ThreadState)> = st
                .threads
                .iter_mut()
                .filter(|(root, t)| {
                    !t.pending
                        && t.last_activity >= active_after
                        && covered_from
                            .as_deref()
                            .is_none_or(|from| !ts_gt(root, from))
                })
                .collect();
            uncovered.sort_by(|a, b| a.1.last_checked.total_cmp(&b.1.last_checked));
            for (_, t) in uncovered.into_iter().take(self.max_direct_checks_per_sweep) {
                t.pending = true;
            }
        }
        Ok(())
    }

    /// The first look at a channel: one request for the newest messages, as context, and the
    /// point from which thread replies count as new.
    async fn first_poll(
        &mut self,
        channel: &str,
        st: &mut ChannelState,
        now: f64,
        out: &mut Vec<Collected>,
    ) -> Result<(), FeedError> {
        let limit = self.initial_backfill.max(1).to_string();
        let page: Page = self
            .api
            .get(
                "conversations.history",
                &[("channel", channel), ("limit", limit.as_str())],
            )
            .await?;
        let mut msgs: Vec<RawMessage> = page
            .messages
            .into_iter()
            .filter_map(RawMessage::parse)
            .collect();
        sort_by_ts(&mut msgs);
        // An empty channel starts from zero: anything posted later is new. Using the local
        // clock instead would risk skipping messages whenever it runs ahead of Slack's.
        let newest = msgs.iter().filter_map(|m| m.ts.clone()).next_back();
        let start = newest.unwrap_or_else(|| "0.000000".to_owned());
        st.last_ts = Some(start.clone());
        st.since = Some(start);
        st.last_sweep = now;
        for m in &msgs {
            note_thread_activity(st, m);
        }
        if self.initial_backfill > 0 {
            for m in msgs.into_iter().filter(keep) {
                out.push(Collected {
                    channel: channel.to_owned(),
                    thread_id: reply_thread(&m),
                    msg: m,
                });
            }
        }
        Ok(())
    }

    /// Fetches a thread's replies after its `last_reply` into `out`. Replies come oldest first,
    /// so whatever was fetched before a failure is a contiguous run and `last_reply` may move
    /// past it.
    async fn fetch_replies(
        &self,
        channel: &str,
        root: &str,
        t: &mut ThreadState,
        now: f64,
        out: &mut Vec<Collected>,
    ) -> Result<(), FeedError> {
        t.last_checked = now;
        let oldest = t.last_reply.clone();
        let limit = self.page_size.to_string();
        let mut next: Option<String> = None;
        loop {
            let mut query = vec![
                ("channel", channel),
                ("ts", root),
                ("oldest", oldest.as_str()),
                ("inclusive", "false"),
                ("limit", limit.as_str()),
            ];
            if let Some(c) = &next {
                query.push(("cursor", c.as_str()));
            }
            let page: Page = self.api.get("conversations.replies", &query).await?;
            let next_cursor = page.next_cursor();
            let has_more = page.has_more;
            let mut msgs: Vec<RawMessage> = page
                .messages
                .into_iter()
                .filter_map(RawMessage::parse)
                .collect();
            sort_by_ts(&mut msgs);
            for m in msgs {
                let Some(ts) = m.ts.clone() else { continue };
                // The root always comes back with its replies; it is not a reply.
                if ts == root || !ts_gt(&ts, &oldest) || !ts_gt(&ts, &t.last_reply) {
                    continue;
                }
                t.last_reply = ts.clone();
                if let Some(u) = ts_unix(&ts) {
                    t.last_activity = t.last_activity.max(u);
                }
                // A reply also sent to the channel is returned by channel history; returning it
                // here as well would duplicate it.
                if m.subtype.as_deref() == Some("thread_broadcast") || !keep(&m) {
                    continue;
                }
                out.push(Collected {
                    channel: channel.to_owned(),
                    thread_id: Some(root.to_owned()),
                    msg: m,
                });
            }
            match next_cursor {
                Some(c) if has_more => next = Some(c),
                _ => break,
            }
        }
        // Every reply up to now has been seen. If the root advertised a newer one that did not
        // come back (it was deleted), move past it too, or the thread would be fetched forever.
        if let Some(hint) = &t.latest_hint {
            t.last_reply = ts_max(&t.last_reply, hint);
        }
        t.pending = false;
        Ok(())
    }

    /// Looks up names for the authors and mentions in `out`, within the per-poll budget. A
    /// failed lookup only costs the name, never the message.
    async fn resolve_names(&mut self, out: &[Collected]) {
        let mut wanted: Vec<String> = Vec::new();
        let mut channels: Vec<String> = Vec::new();
        for c in out {
            let m = effective(&c.msg);
            wanted.extend(m.user.iter().cloned());
            if let Some(text) = &m.text {
                wanted.extend(mrkdwn::mentioned_users(text));
            }
            if !self.channel_names.contains_key(&c.channel) && !channels.contains(&c.channel) {
                channels.push(c.channel.clone());
            }
        }
        let mut seen = HashSet::new();
        wanted.retain(|u| !self.users.contains_key(u) && seen.insert(u.clone()));

        for user in wanted.into_iter().take(MAX_USER_LOOKUPS_PER_POLL) {
            if self.users.len() >= USER_CACHE_CAP {
                self.users.clear();
            }
            match self
                .api
                .get::<UserInfo>("users.info", &[("user", user.as_str())])
                .await
            {
                Ok(info) => {
                    let name = info.user.and_then(|u| u.best_name());
                    self.users.insert(user, name);
                }
                Err(FeedError::Api(code)) if code == "user_not_found" => {
                    self.users.insert(user, None);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "slack users.info failed; names resolved later");
                    break;
                }
            }
        }
        for channel in channels {
            match self
                .api
                .get::<ConversationInfo>("conversations.info", &[("channel", channel.as_str())])
                .await
            {
                Ok(info) => {
                    let conv = info.channel.unwrap_or_default();
                    self.channel_kinds.insert(channel.clone(), conv.kind());
                    self.channel_names.insert(channel, conv.name);
                }
                Err(e @ (FeedError::RateLimited(_) | FeedError::Http(_))) => {
                    tracing::debug!(error = %e, "slack conversations.info failed; retrying later");
                    break;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "slack conversations.info failed");
                    self.channel_names.insert(channel, None);
                }
            }
        }
    }

    fn to_feed_message(&self, c: Collected) -> Option<FeedMessage> {
        let auth = self.auth.as_ref();
        shape_message(
            &c.msg,
            Shape {
                channel: &c.channel,
                channel_name: self.channel_names.get(&c.channel).cloned().flatten(),
                kind: self
                    .channel_kinds
                    .get(&c.channel)
                    .copied()
                    .unwrap_or_default(),
                thread_id: c.thread_id,
                workspace_url: auth.and_then(|a| a.url.as_deref()),
                owner: auth.and_then(|a| a.user_id.as_deref()),
            },
            |id| self.users.get(id).cloned().flatten(),
            |id| self.channel_names.get(id).cloned().flatten(),
        )
    }
}

/// Where a message was seen, for [`shape_message`].
struct Shape<'a> {
    channel: &'a str,
    channel_name: Option<String>,
    kind: ConversationKind,
    thread_id: Option<String>,
    /// The workspace URL from `auth.test`, for permalinks.
    workspace_url: Option<&'a str>,
    /// The owner's Slack user id, for `mentions_owner` and `from_owner`.
    owner: Option<&'a str>,
}

/// Turns a raw Slack message (or `message_changed` event) into a [`FeedMessage`]. Shared by the
/// poller and the Socket Mode client, so both shape messages identically. `user_name` and
/// `channel_name` look up cached names and never make requests.
fn shape_message(
    raw: &RawMessage,
    s: Shape<'_>,
    user_name: impl Fn(&str) -> Option<String>,
    channel_name: impl Fn(&str) -> Option<String>,
) -> Option<FeedMessage> {
    let changed = raw.subtype.as_deref() == Some("message_changed");
    let m = effective(raw);
    let id = m.ts.clone().or_else(|| raw.ts.clone())?;
    let ts_unix = ts_unix(&id)?;
    let author_id = m.user.clone().or_else(|| m.bot_id.clone());
    let author_name = match &m.user {
        Some(u) => user_name(u),
        None => m
            .username
            .clone()
            .or_else(|| m.bot_profile.as_ref().and_then(|b| b.name.clone())),
    };
    let raw_text = m.text.as_deref().unwrap_or("");
    let mut text = mrkdwn::to_plain(raw_text, &user_name, &channel_name);
    let files: Vec<&str> = m
        .files
        .iter()
        .filter_map(|f| f.name.as_deref().or(f.title.as_deref()))
        .collect();
    if !files.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[files: {}]", files.join(", ")));
    }
    let permalink = s.workspace_url.map(|base| {
        let base = base.trim_end_matches('/');
        let mut link = format!("{base}/archives/{}/p{}", s.channel, id.replace('.', ""));
        if let Some(thread) = &s.thread_id {
            link.push_str(&format!("?thread_ts={thread}&cid={}", s.channel));
        }
        link
    });
    let from_owner = s.owner.is_some() && m.user.as_deref() == s.owner;
    let mentions_owner = !from_owner && s.owner.is_some_and(|o| mrkdwn::mentions_user(raw_text, o));
    Some(FeedMessage {
        source: KIND.to_owned(),
        conversation_name: s.channel_name,
        conversation_id: s.channel.to_owned(),
        id,
        thread_id: s.thread_id,
        author_id,
        author_name,
        text,
        permalink,
        ts_unix,
        edited: changed || m.edited.is_some(),
        conversation_kind: s.kind,
        subject: None,
        mentions_owner,
        from_owner,
        conversation_attention: None,
    })
}

#[async_trait]
impl Source for SlackSource {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn poll_interval(&self) -> Duration {
        self.suggested_interval()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            poll: true,
            can_reply: true,
            can_reply_in_thread: true,
            ..Capabilities::default()
        }
    }

    fn is_fatal(&self, err: &FeedError) -> bool {
        is_fatal(err) || matches!(err, FeedError::Unsupported(_) | FeedError::Internal(_))
    }

    async fn poll(&mut self) -> Result<Vec<FeedMessage>, FeedError> {
        if let Some(left) = self.rate_limited_for() {
            return Err(FeedError::RateLimited(left));
        }
        self.rate_limited_until = None;
        self.last_errors.clear();
        if let Err(e) = self.ensure_auth().await {
            if let FeedError::RateLimited(d) = e {
                self.note_rate_limit(d);
            }
            return Err(e);
        }

        let now = unix_now();
        // All progress is staged and committed at the end, so that a poll that fails outright
        // leaves the cursor exactly where it was.
        let mut staged = self.cursor.clone();
        let mut out: Vec<Collected> = Vec::new();
        let mut stop: Option<FeedError> = None;
        let mut any_ok = false;

        let n = self.channels.len();
        let first = if n == 0 { 0 } else { self.rotation % n };
        self.rotation = self.rotation.wrapping_add(1);
        let order: Vec<String> = (0..n)
            .map(|i| self.channels[(first + i) % n].clone())
            .collect();

        // Channels confirmed this poll to be ones the owner is in; only they are read.
        let mut allowed: HashSet<String> = HashSet::new();
        for channel in &order {
            // A mapped channel can be public and readable without the owner being in it, but
            // only conversations the owner is a member of may produce messages. When that
            // cannot be told, the channel is skipped this poll and its cursor does not move.
            match self.check_membership(channel).await {
                Ok(true) => {
                    allowed.insert(channel.clone());
                }
                Ok(false) => {
                    tracing::warn!(channel = %channel, "slack: the owner is not in this mapped channel; not polled");
                    self.last_errors
                        .push((channel.clone(), FeedError::Api("not_in_channel".into())));
                    continue;
                }
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e @ (FeedError::RateLimited(_) | FeedError::Http(_))) => {
                    stop = Some(e);
                    break;
                }
                Err(e) => {
                    tracing::warn!(channel = %channel, error = %e, "slack membership check failed");
                    self.last_errors.push((channel.clone(), e));
                    continue;
                }
            }
            let mut st = staged.channels.get(channel).cloned().unwrap_or_default();
            let result = self.poll_history(channel, &mut st, now, &mut out).await;
            // The channel state is consistent with `out` whether or not the call failed: it
            // only moved past what was pushed.
            staged.channels.insert(channel.clone(), st);
            match result {
                Ok(()) => any_ok = true,
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e @ (FeedError::RateLimited(_) | FeedError::Http(_))) => {
                    stop = Some(e);
                    break;
                }
                Err(e) => {
                    tracing::warn!(channel = %channel, error = %e, "slack channel poll failed");
                    self.last_errors.push((channel.clone(), e));
                }
            }
        }

        // Threads, most recently active first, within the per-poll budget.
        let window = self.thread_window.as_secs_f64();
        let mut due: Vec<(String, String, f64)> = Vec::new();
        for channel in order.iter().filter(|c| allowed.contains(*c)) {
            if let Some(st) = staged.channels.get_mut(channel) {
                st.prune_threads(now, window, self.max_threads);
                due.extend(
                    st.threads
                        .iter()
                        .filter(|(_, t)| t.pending)
                        .map(|(root, t)| (channel.clone(), root.clone(), t.last_activity)),
                );
            }
        }
        due.sort_by(|a, b| b.2.total_cmp(&a.2));
        if stop.is_none() {
            for (channel, root, _) in due.into_iter().take(self.max_thread_fetches_per_poll) {
                let Some(t) = staged
                    .channels
                    .get_mut(&channel)
                    .and_then(|s| s.threads.get_mut(&root))
                else {
                    continue;
                };
                match self.fetch_replies(&channel, &root, t, now, &mut out).await {
                    Ok(()) => any_ok = true,
                    Err(e) if is_fatal(&e) => return Err(e),
                    Err(e @ (FeedError::RateLimited(_) | FeedError::Http(_))) => {
                        stop = Some(e);
                        break;
                    }
                    Err(FeedError::Api(code)) if code == "thread_not_found" => {
                        // The root was deleted; nothing more will arrive in this thread.
                        if let Some(st) = staged.channels.get_mut(&channel) {
                            st.threads.remove(&root);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(channel = %channel, error = %e, "slack replies poll failed");
                        // Retrying every poll would not fix it; the next sweep may re-flag it.
                        t.pending = false;
                        self.last_errors.push((channel.clone(), e));
                    }
                }
            }
        }

        if out.is_empty()
            && stop.is_none()
            && !any_ok
            && let Some((_, e)) = self.last_errors.first()
        {
            // Every channel failed: most likely a configuration problem the caller should see.
            return Err(e.clone());
        }

        self.resolve_names(&out).await;
        let mut messages: Vec<FeedMessage> = out
            .into_iter()
            .filter_map(|c| self.to_feed_message(c))
            .collect();
        messages.sort_by(|a, b| a.ts_unix.total_cmp(&b.ts_unix));
        self.cursor = staged;

        self.interval = if messages.is_empty() {
            (self.interval * 2).clamp(self.min_interval, self.max_interval)
        } else {
            self.min_interval
        };
        match stop {
            Some(FeedError::RateLimited(d)) => {
                self.note_rate_limit(d);
                if messages.is_empty() {
                    return Err(FeedError::RateLimited(d));
                }
                tracing::debug!(
                    returned = messages.len(),
                    "slack poll cut short by a rate limit"
                );
            }
            Some(e) => {
                if messages.is_empty() {
                    return Err(e);
                }
                tracing::warn!(error = %e, "slack poll cut short");
            }
            None => {}
        }
        Ok(messages)
    }

    fn cursor(&self) -> Value {
        serde_json::to_value(&self.cursor).unwrap_or(Value::Null)
    }

    fn restore_cursor(&mut self, v: Value) {
        if v.is_null() {
            self.cursor = Cursor::new();
        } else {
            match serde_json::from_value::<Cursor>(v) {
                Ok(c) if c.v == Cursor::VERSION => self.cursor = c,
                Ok(c) => {
                    tracing::warn!(version = c.v, "ignoring a slack cursor of another version");
                }
                Err(e) => tracing::warn!(error = %e, "ignoring an unreadable slack cursor"),
            }
        }
        // Held pages belong to the cursor they started from; drop them with it.
        self.pending_history.clear();
    }

    async fn post_reply(
        &self,
        channel_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError> {
        post_message(&self.api, channel_id, thread_id, text).await
    }

    fn replier(&self) -> Option<Arc<dyn Replier>> {
        Some(Arc::new(SlackReplier(self.api.clone())))
    }
}

/// Posts replies with the user token, independently of the source it came from, so that a
/// running source (which holds `&mut self`) does not block approved replies.
struct SlackReplier(Api);

#[async_trait]
impl Replier for SlackReplier {
    async fn post_reply(
        &self,
        conversation_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError> {
        post_message(&self.0, conversation_id, thread_id, text).await
    }
}

/// Posts as the user (`chat.postMessage` with the user token), so a reply reads as the owner's.
async fn post_message(
    api: &Api,
    channel_id: &str,
    thread_id: Option<&str>,
    text: &str,
) -> Result<String, FeedError> {
    let mut body = json!({ "channel": channel_id, "text": text });
    if let Some(thread) = thread_id {
        body["thread_ts"] = Value::String(thread.to_owned());
    }
    let posted: Posted = api.post_json("chat.postMessage", &body).await?;
    posted
        .ts
        .ok_or_else(|| FeedError::Decode("chat.postMessage: no ts".into()))
}

/// Starts or refreshes tracking for a thread whose root (or broadcast reply) shows activity
/// newer than what we have seen.
fn note_thread_activity(st: &mut ChannelState, m: &RawMessage) {
    let (Some(ts), Some(root)) = (m.ts.as_deref(), m.thread_ts.as_deref()) else {
        return;
    };
    let latest = if ts == root {
        if m.reply_count.unwrap_or(0) == 0 {
            return;
        }
        match m.latest_reply.as_deref() {
            Some(l) => l,
            None => return,
        }
    } else {
        // A reply that appears in channel history (a broadcast) shows the thread is alive.
        ts
    };
    let activity = ts_unix(latest).unwrap_or(0.0);
    match st.threads.get_mut(root) {
        Some(t) => {
            if ts_gt(latest, &t.last_reply) {
                t.pending = true;
            }
            t.latest_hint = Some(match &t.latest_hint {
                Some(h) => ts_max(h, latest),
                None => latest.to_owned(),
            });
            t.last_activity = t.last_activity.max(activity);
        }
        None => {
            let base = st.thread_baseline(root);
            if ts_gt(latest, &base) {
                st.threads.insert(
                    root.to_owned(),
                    ThreadState {
                        last_reply: base,
                        latest_hint: Some(latest.to_owned()),
                        last_activity: activity,
                        last_checked: 0.0,
                        pending: true,
                    },
                );
            }
        }
    }
}

/// Whether a message is conversation rather than channel housekeeping.
fn keep(m: &RawMessage) -> bool {
    match m.subtype.as_deref() {
        None => true,
        Some("message_changed") => m.message.is_some(),
        Some(s) => KEPT_SUBTYPES.contains(&s),
    }
}

/// The message to show: the new version for an edit event, the message itself otherwise.
fn effective(m: &RawMessage) -> &RawMessage {
    match (&m.subtype, &m.message) {
        (Some(s), Some(inner)) if s == "message_changed" => inner,
        _ => m,
    }
}

/// The thread a message replies to, if it is a reply rather than a root or a plain message.
fn reply_thread(m: &RawMessage) -> Option<String> {
    let m = effective(m);
    match (&m.ts, &m.thread_ts) {
        (Some(ts), Some(root)) if ts != root => Some(root.clone()),
        _ => None,
    }
}

fn sort_by_ts(msgs: &mut Vec<RawMessage>) {
    msgs.sort_by(|a, b| {
        state::ts_cmp(a.ts.as_deref().unwrap_or(""), b.ts.as_deref().unwrap_or(""))
    });
    // A message can land on two pages when the channel changes between requests.
    msgs.dedup_by(|a, b| a.ts.is_some() && a.ts == b.ts);
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
