//! Slack Socket Mode: every message in every conversation the owner is in, pushed as it happens.
//!
//! The owner's own Slack app subscribes to message events *on behalf of the user*
//! (`message.channels`, `message.groups`, `message.im`, `message.mpim`), so Slack sends whatever
//! the owner can see, with no channel list to maintain. Socket Mode delivers them over a
//! WebSocket the app opens itself, so the orchestrator needs no public URL. Two tokens:
//!
//! - the app-level token (`xapp-…`, created with the `connections:write` scope) only opens
//!   connections (`apps.connections.open`, a POST with the token in the `Authorization` header);
//! - the user token (`xoxp-…`) does everything else: `auth.test` for the owner's id, history for
//!   gaps, names, and approved replies.
//!
//! The protocol, as documented (docs.slack.dev, "Using Socket Mode"):
//!
//! - After connecting Slack sends `{"type":"hello", "num_connections", …}`.
//! - Each event arrives in an envelope `{"envelope_id", "type":"events_api", "payload",
//!   "accepts_response_payload", …}` and must be acknowledged with `{"envelope_id": …}`. The
//!   Events API allows three seconds before it counts a delivery as failed and retries (up to
//!   three times), so the reader acknowledges before doing anything else, never waits for the
//!   consumer, and drops retries that still happen by `event_id`.
//! - `{"type":"disconnect","reason":…}` announces the end of a connection: `warning` about ten
//!   seconds ahead, `refresh_requested` (connections are recycled every few hours), or
//!   `link_disabled` when Socket Mode was switched off in the app settings. For the first two
//!   the replacement connection is opened while the old one is still read, and the old one is
//!   dropped once the new one said hello, so a routine refresh misses nothing and costs no gap
//!   check.
//! - An app may hold up to ten connections, and Slack spreads events across all of them. A
//!   second process using the same app token therefore silently takes a share of the events;
//!   `num_connections` above one is logged as a warning.
//!
//! Gaps. Socket Mode does not replay what was sent while no connection was open, so a new
//! connection that did not take over from a live one checks for missed messages:
//!
//! - in every direct and group message the owner is a member of (from a fresh
//!   `users.conversations`), after any outage however short: these are what the owner must not
//!   miss, and whether Slack redelivers events sent while no connection was open is not
//!   documented for Socket Mode, so nothing here relies on it;
//! - in every other conversation active within a window before the connection was lost; the
//!   window grows with the length of the outage (a hundred times it, from an hour up to the
//!   backfill window). A message in a channel quiet for longer than that, sent during a short
//!   outage, is therefore missed, unless Slack redelivers it;
//! - after an outage longer than a minute, in every conversation the owner is a member of.
//!
//! Direct and group messages are checked first. A check is `conversations.history` from the
//! last message seen (reaching back further to see thread roots that got replies), plus
//! `conversations.replies` for those threads. These are Tier 3 methods (50+ requests a minute
//! for an internal app); every request of the source goes through one shared gate per tier. A
//! conversation's gap stays in the cursor, with how far the check got, until the check is
//! complete, so a cap, a rate limit or a restart in the middle loses nothing; gaps left over
//! are retried a few minutes later. Messages recovered this way may arrive in chunks out of
//! order; consumers store them by id.
//!
//! The progress the cursor records only covers what the consumer has received: the time the
//! connection was last known to be up only advances while no received event is still waiting
//! for the consumer, and an event that does not fit the (bounded) queue to the consumer opens
//! a gap for its conversation instead of being lost.
//!
//! Handling of message subtypes:
//!
//! - Plain messages, thread replies, `thread_broadcast`, `file_share`, `me_message` and
//!   `bot_message` are delivered.
//! - `message_changed` is delivered as the new version (same id, `edited` set) when the text
//!   changed. Slack also sends it for unfurls and reply-count updates with the text unchanged;
//!   those are dropped.
//! - `message_deleted` is delivered as [`FeedEvent::Deleted`].
//! - Everything else (joins, topic changes, …) is housekeeping and is dropped.
//! - The owner's own messages are delivered with `from_owner` set. That includes replies the
//!   fleet posted for the owner: they are posted with the user token, so they come back as the
//!   owner's messages. No bot of ours ever posts, so there is no bot echo to filter.
//! - Edits and deletions made while disconnected are not recovered: history shows the current
//!   text of messages in the gap, but not changes to older ones.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Display;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{Sink, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_websockets::{ClientBuilder, Connector, MaybeTlsStream, Message, WebSocketStream};

use super::api::{
    Api, AuthTest, ConnectionsOpen, Page, RawMessage, is_fatal, kind_from_channel_type,
};
use super::directory::SlackDirectory;
use super::pace::{self, Gates};
use super::state::{Cursor as PollCursor, ts_before, ts_cmp, ts_from_unix, ts_gt, ts_max, ts_unix};
use super::{
    DEFAULT_BASE_URL, KIND, Shape, effective, keep, mrkdwn, post_message, reply_thread,
    shape_message, sort_by_ts, unix_now,
};
use crate::{
    Backfiller, Capabilities, ConversationKind, Directory, FeedError, FeedEvent, FeedMessage,
    FeedSink, Replier, Source, SourceStatus,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How many delivered message ids and envelope event ids are remembered for de-duplication.
const RECENT_CAP: usize = 20_000;
/// A conversation not heard from for this long is dropped from the cursor, so it stays small.
const MARK_HORIZON: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// Mentioned users whose names one message may look up; the rest keep their ids.
const MAX_MENTION_LOOKUPS: usize = 10;
/// Retries of one gap-check request that keeps getting rate limited before the conversation is
/// left for later.
const MAX_RATE_LIMIT_RETRIES: usize = 3;
/// Every this many events dropped for a full queue, a warning is logged.
const OVERFLOW_WARN: usize = 1000;
/// Above this many threads waiting in one conversation's gap, a warning is logged.
const PENDING_ROOTS_WARN: usize = 1000;
/// Checks of one conversation per round of gap filling, so a gap that needs several pages
/// finishes within the round but a broken one cannot loop.
const MAX_CHECKS_PER_ROUND: usize = 3;
/// How long a replacement connection may take to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// A push [`Source`] for every conversation the owner is in, through Slack Socket Mode.
///
/// Configure it with the `with_*` methods before [`restore_cursor`](Source::restore_cursor),
/// then call [`run`](Source::run). Use [`directory`](Source::directory),
/// [`replier`](Source::replier) and [`backfiller`](Source::backfiller) while it runs.
#[derive(Debug)]
pub struct SlackSocket {
    shared: Arc<Shared>,
    cfg: Config,
}

#[derive(Debug, Clone)]
struct Config {
    backfill_window: Duration,
    min_check_window: Duration,
    max_backfill_conversations: usize,
    max_backfill_pages: usize,
    max_backfill_threads: usize,
    page_size: u32,
    thread_lookback: Duration,
    seed_after: Duration,
    refill_after: Duration,
    request_gap: Duration,
    reconnect_min: Duration,
    reconnect_max: Duration,
    ping_every: Duration,
    idle_timeout: Duration,
    connect_timeout: Duration,
    send_timeout: Duration,
    checkpoint_every: Duration,
    directory_refresh: Duration,
    event_queue: usize,
    /// Plain `ws://` URLs are accepted only from a test server, never from Slack.
    allow_plain_ws: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backfill_window: Duration::from_secs(7 * 24 * 60 * 60),
            min_check_window: Duration::from_secs(60 * 60),
            max_backfill_conversations: 500,
            max_backfill_pages: 5,
            max_backfill_threads: 20,
            page_size: 200,
            thread_lookback: Duration::from_secs(24 * 60 * 60),
            seed_after: Duration::from_secs(60),
            refill_after: Duration::from_secs(5 * 60),
            // Tier 3 allows 50+ requests a minute per method; 1.3 s apart stays under 50.
            request_gap: Duration::from_millis(1300),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            ping_every: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(30),
            // Without it a half-open TCP connection could block the reader for minutes.
            send_timeout: Duration::from_secs(10),
            checkpoint_every: Duration::from_secs(10),
            directory_refresh: Duration::from_secs(6 * 60 * 60),
            event_queue: 50_000,
            allow_plain_ws: false,
        }
    }
}

impl Config {
    /// How far before an outage of `outage` seconds conversations count as active.
    fn check_window(&self, outage: f64) -> Duration {
        let scaled = Duration::from_secs_f64((outage.max(0.0) * 100.0).min(1e9));
        scaled
            .max(self.min_check_window)
            .min(self.backfill_window.max(self.min_check_window))
    }
}

#[derive(Debug)]
struct Shared {
    user: Api,
    app: Api,
    gates: Arc<Gates>,
    dir: Arc<SlackDirectory>,
    auth: Mutex<Option<Auth>>,
    state: Mutex<PushState>,
    /// Wakes the connection when a background task hit an error that ends the run.
    fatal_notify: Notify,
    /// Events read from the socket whose delivery to the consumer has not finished.
    queued: AtomicUsize,
    /// Events dropped because the queue was full (each opened a gap instead).
    overflowed: AtomicUsize,
    /// What the processor did with events, for the once-a-minute summary.
    stats: Mutex<Stats>,
}

/// Counters for the processor's minute summary. They hold reasons and error codes, never
/// message text.
#[derive(Debug, Default)]
struct Stats {
    delivered: usize,
    drops: BTreeMap<&'static str, usize>,
    lookup_errors: BTreeMap<String, usize>,
    /// Lookup failures already logged as a warning.
    warned_lookups: HashSet<String>,
}

/// What may be logged about an event: its kind, never its content.
#[derive(Debug, Default)]
struct EventMeta {
    event_type: Option<String>,
    subtype: Option<String>,
    channel_type: Option<String>,
}

impl EventMeta {
    fn of(ev: &Value) -> Self {
        let field = |k: &str| ev.get(k).and_then(Value::as_str).map(str::to_owned);
        Self {
            event_type: field("type"),
            subtype: field("subtype"),
            channel_type: field("channel_type"),
        }
    }
}

/// Frames read on one connection, by envelope type, for the reader's minute summary.
#[derive(Debug, Default)]
struct FrameCounts {
    hello: usize,
    events_api: usize,
    disconnect: usize,
    other: usize,
}

impl FrameCounts {
    fn any(&self) -> bool {
        self.hello + self.events_api + self.disconnect + self.other > 0
    }
}

#[derive(Debug, Clone, Default)]
struct Auth {
    /// The workspace URL, for permalinks.
    url: Option<String>,
    /// The owner's Slack user id, for `mentions_owner` and `from_owner`.
    user_id: Option<String>,
}

#[derive(Debug)]
struct PushState {
    cursor: PushCursor,
    /// `(conversation, ts)` of messages handed to the consumer, so a gap check that overlaps
    /// live events does not deliver them twice. Entries are added only after a successful
    /// send: a message dropped half way must still be delivered later.
    delivered: Recent<(String, String)>,
    /// Event ids already handled; Slack retries an event it thinks was not acknowledged.
    events: Recent<String>,
    /// The cursor changed since the last checkpoint.
    dirty: bool,
    /// Bumped on every new connection that needs a gap check. A check started before it may
    /// not cover the newest outage, so its result is not allowed to close a gap.
    generation: u64,
    /// A round of gap checks is wanted.
    fill_requested: bool,
    /// When gaps left over from the last round are tried again.
    next_fill_at: Option<Instant>,
    /// A maintenance task is running; there is never more than one.
    maintenance_active: bool,
    /// An error from a background task that ends the run.
    fatal: Option<FeedError>,
}

impl Default for PushState {
    fn default() -> Self {
        Self {
            cursor: PushCursor::new(),
            delivered: Recent::new(RECENT_CAP),
            events: Recent::new(RECENT_CAP),
            dirty: false,
            generation: 0,
            fill_requested: false,
            next_fill_at: None,
            maintenance_active: false,
            fatal: None,
        }
    }
}

/// A bounded set that forgets its oldest entries.
#[derive(Debug)]
struct Recent<T> {
    set: HashSet<T>,
    order: VecDeque<T>,
    cap: usize,
}

impl<T: Eq + Hash + Clone> Recent<T> {
    fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn contains(&self, t: &T) -> bool {
        self.set.contains(t)
    }

    /// Adds `t`; `false` when it was already there.
    fn insert(&mut self, t: T) -> bool {
        if !self.set.insert(t.clone()) {
            return false;
        }
        self.order.push_back(t);
        if self.order.len() > self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

/// What a [`SlackSocket`] persists: per conversation, the newest message seen and any gap still
/// to be checked, and when the connection was last known to be up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PushCursor {
    /// Always [`PushCursor::MODE`], so a polling cursor is never mistaken for this one.
    mode: String,
    /// Unix seconds up to which everything received has reached the consumer while connected.
    /// The gap after a restart or outage is measured from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_alive: Option<f64>,
    /// Member conversations still have to get a gap from here (after an outage, until the
    /// membership list could be read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seed_from: Option<String>,
    /// The seed covers only direct and group messages (after a short outage).
    #[serde(default, skip_serializing_if = "is_false")]
    seed_dms_only: bool,
    #[serde(default)]
    conversations: BTreeMap<String, Mark>,
}

impl PushCursor {
    const MODE: &'static str = "slack_socket/1";

    fn new() -> Self {
        Self {
            mode: Self::MODE.to_owned(),
            last_alive: None,
            seed_from: None,
            seed_dms_only: false,
            conversations: BTreeMap::new(),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Mark {
    /// The newest message ts seen in the conversation (top level or reply); empty for a
    /// conversation only known from the membership list.
    last_ts: String,
    /// Messages after this ts may have been missed and are still to be checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gap_from: Option<String>,
    /// History above this ts has been checked already; the next check continues below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gap_until: Option<String>,
    /// History of the whole gap has been checked; only `pending_roots` remain.
    #[serde(default, skip_serializing_if = "is_false")]
    history_done: bool,
    /// Threads with replies in the gap that are still to be fetched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_roots: Vec<PendingRoot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<ConversationKind>,
    /// Added from the membership list for one gap check; dropped again when it finds nothing.
    #[serde(default, skip_serializing_if = "is_false")]
    seeded: bool,
}

/// A thread whose replies in a gap are still to be fetched, after `after` when part of them
/// already was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PendingRoot {
    root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
}

/// One gap check to run.
#[derive(Debug, Clone)]
struct GapJob {
    channel: String,
    from: String,
    until: Option<String>,
    history_done: bool,
    roots: Vec<PendingRoot>,
    kind: Option<ConversationKind>,
}

/// What one gap check found, and how far it got.
#[derive(Debug, Default)]
struct GapResult {
    msgs: Vec<FeedMessage>,
    history_done: bool,
    history_until: Option<String>,
    pending_roots: Vec<PendingRoot>,
}

impl GapResult {
    fn complete(&self) -> bool {
        self.history_done && self.pending_roots.is_empty()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Everything behind these locks stays consistent after a panic elsewhere.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The order gaps are checked in: the conversations most likely addressed to the owner first.
fn kind_rank(k: Option<ConversationKind>) -> u8 {
    match k {
        Some(ConversationKind::Direct) => 0,
        Some(ConversationKind::Group) => 1,
        Some(ConversationKind::Private) => 2,
        Some(ConversationKind::Channel) => 3,
        _ => 4,
    }
}

/// Lowers (or sets) a mark's gap start to `from`. A gap whose start moves restarts at the top.
fn widen_gap(m: &mut Mark, from: &str) {
    match &m.gap_from {
        Some(g) if !ts_gt(g, from) => {}
        _ => {
            m.gap_from = Some(from.to_owned());
            m.gap_until = None;
            m.history_done = false;
        }
    }
}

impl Shared {
    fn new(user: Api, app: Api, cfg: &Config, state: PushState) -> Self {
        let gates = Arc::new(Gates::new(cfg.request_gap));
        Self {
            dir: Arc::new(SlackDirectory::new(
                user.clone(),
                gates.clone(),
                cfg.directory_refresh,
            )),
            gates,
            user,
            app,
            auth: Mutex::new(None),
            state: Mutex::new(state),
            fatal_notify: Notify::new(),
            queued: AtomicUsize::new(0),
            overflowed: AtomicUsize::new(0),
            stats: Mutex::new(Stats::default()),
        }
    }

    fn auth(&self) -> Auth {
        lock(&self.auth).clone().unwrap_or_default()
    }

    /// Records that a message up to `ts` was delivered (or deliberately skipped).
    fn note(&self, channel: &str, ts: &str, kind: Option<ConversationKind>) {
        let mut st = lock(&self.state);
        let mark = st
            .cursor
            .conversations
            .entry(channel.to_owned())
            .or_default();
        mark.last_ts = ts_max(&mark.last_ts, ts);
        if let Some(k) = kind.filter(|k| *k != ConversationKind::Other) {
            mark.kind = Some(k);
        }
        st.dirty = true;
    }

    fn is_delivered(&self, channel: &str, ts: &str) -> bool {
        lock(&self.state)
            .delivered
            .contains(&(channel.to_owned(), ts.to_owned()))
    }

    fn mark_delivered(&self, channel: &str, ts: &str) {
        lock(&self.state)
            .delivered
            .insert((channel.to_owned(), ts.to_owned()));
    }

    /// The cursor, when it changed since the last call. Only the event processor calls this,
    /// so checkpoints reach the consumer in the order they were taken and never go backwards.
    fn checkpoint(&self) -> Option<Value> {
        let mut st = lock(&self.state);
        if !st.dirty {
            return None;
        }
        st.dirty = false;
        serde_json::to_value(&st.cursor).ok()
    }

    /// Records that, at `now`, the connection is up and everything received so far has reached
    /// the consumer. While events still wait in the queue the time stays where it was, so a
    /// restart checks the conversations those events belonged to.
    fn alive(&self, now: f64) {
        if self.queued.load(Ordering::SeqCst) > 0 {
            return;
        }
        let mut st = lock(&self.state);
        st.cursor.last_alive = Some(now);
        st.dirty = true;
    }

    fn generation(&self) -> u64 {
        lock(&self.state).generation
    }

    /// A new connection said hello: opens gaps (unless it took over from a live connection),
    /// asks for a round of gap checks, and says whether a maintenance task has to be started.
    fn on_hello(&self, now: f64, cfg: &Config, took_over: bool) -> bool {
        if took_over {
            // The old connection was read until this one said hello: nothing was missed.
            return false;
        }
        let mut st = lock(&self.state);
        let last_alive = st.cursor.last_alive;
        // Activity is measured from when the connection was lost, not from now, so a long
        // outage does not push conversations out of the window.
        let (reference, window) = match last_alive {
            Some(lost) => (lost.min(now), cfg.check_window(now - lost)),
            None => (now, cfg.backfill_window),
        };
        open_gaps(&mut st, reference, window);
        // Direct and group messages are checked after every outage, however short: the
        // secretary must not miss them, and Slack's redelivery of events sent while no
        // connection was open is not documented for Socket Mode. Every other conversation is
        // added only after a long outage.
        if let Some(lost) = last_alive {
            let dms_only = now - lost < cfg.seed_after.as_secs_f64();
            merge_seed(&mut st.cursor, ts_from_unix(lost), dms_only);
        }
        st.generation += 1;
        st.fill_requested = true;
        st.next_fill_at = None;
        st.dirty = true;
        claim_maintenance(&mut st)
    }

    /// Asks for a maintenance task without a round of gap checks (a membership refresh).
    fn request_maintenance(&self) -> bool {
        claim_maintenance(&mut lock(&self.state))
    }

    /// Asks for a round of gap checks when the retry of leftover gaps is due.
    fn request_due_fill(&self) -> bool {
        let mut st = lock(&self.state);
        if st.next_fill_at.is_none_or(|at| at > Instant::now()) {
            return false;
        }
        st.next_fill_at = None;
        st.fill_requested = true;
        claim_maintenance(&mut st)
    }

    /// Schedules another round for gaps that are still open.
    fn schedule_refill(&self, after: Duration) {
        let mut st = lock(&self.state);
        let at = Instant::now() + after;
        if st.next_fill_at.is_none_or(|old| old > at) {
            st.next_fill_at = Some(at);
        }
    }

    fn take_fill(&self) -> bool {
        std::mem::take(&mut lock(&self.state).fill_requested)
    }

    /// The pending seed: where it starts, and whether it covers only direct and group messages.
    fn take_seed(&self) -> Option<(String, bool)> {
        let mut st = lock(&self.state);
        let seed = st.cursor.seed_from.take()?;
        let dms_only = std::mem::take(&mut st.cursor.seed_dms_only);
        st.dirty = true;
        Some((seed, dms_only))
    }

    /// Puts a seed back after the membership list could not be read.
    fn restore_seed(&self, from: String, dms_only: bool) {
        let mut st = lock(&self.state);
        merge_seed(&mut st.cursor, from, dms_only);
        st.dirty = true;
    }

    /// Whether the maintenance task should go round again; when not, it is marked finished in
    /// the same step, so a request arriving right now starts a new task instead of being lost.
    fn maintenance_continue(&self) -> bool {
        let mut st = lock(&self.state);
        if st.fill_requested {
            return true;
        }
        st.maintenance_active = false;
        false
    }

    fn set_fatal(&self, e: FeedError) {
        lock(&self.state).fatal = Some(e);
        self.fatal_notify.notify_one();
    }

    fn take_fatal(&self) -> Option<FeedError> {
        lock(&self.state).fatal.take()
    }

    /// An event that did not fit the queue: its conversation gets a gap from just before it,
    /// and a round of gap checks is asked for. Returns whether a maintenance task has to be
    /// started.
    fn overflow(&self, event: &Value) -> bool {
        let n = self.overflowed.fetch_add(1, Ordering::Relaxed) + 1;
        if n % OVERFLOW_WARN == 1 {
            tracing::warn!(
                dropped = n,
                "slack events arrive faster than they are consumed; the excess is recovered from history"
            );
        }
        let (Some(channel), Some(ts)) = (
            event.get("channel").and_then(Value::as_str),
            event.get("ts").and_then(Value::as_str),
        ) else {
            return false;
        };
        let mut st = lock(&self.state);
        let mark = st
            .cursor
            .conversations
            .entry(channel.to_owned())
            .or_default();
        widen_gap(mark, &ts_before(ts));
        st.fill_requested = true;
        st.dirty = true;
        claim_maintenance(&mut st)
    }

    /// Opens a gap in every member conversation that has none (only direct and group messages
    /// with `dms_only`), from `from` (or from its last message, if older).
    fn seed(&self, members: Vec<crate::Conversation>, from: &str, dms_only: bool) {
        let mut st = lock(&self.state);
        for c in members {
            if dms_only && !matches!(c.kind, ConversationKind::Direct | ConversationKind::Group) {
                continue;
            }
            let kind = Some(c.kind).filter(|k| *k != ConversationKind::Other);
            let mark = st.cursor.conversations.entry(c.id).or_insert_with(|| Mark {
                seeded: true,
                ..Mark::default()
            });
            if mark.kind.is_none() {
                mark.kind = kind;
            }
            if mark.gap_from.is_none() {
                let start = if mark.last_ts.is_empty() || ts_gt(&mark.last_ts, from) {
                    from.to_owned()
                } else {
                    mark.last_ts.clone()
                };
                widen_gap(mark, &start);
            }
        }
        st.dirty = true;
    }

    /// The open gaps: direct and group messages first, then conversations with known activity
    /// before seeded ones, most recent first.
    fn pending_gaps(&self) -> Vec<GapJob> {
        let st = lock(&self.state);
        let mut gaps: Vec<(&String, &Mark)> = st
            .cursor
            .conversations
            .iter()
            .filter(|(_, m)| m.gap_from.is_some())
            .collect();
        gaps.sort_by(|a, b| {
            (kind_rank(a.1.kind), a.1.seeded)
                .cmp(&(kind_rank(b.1.kind), b.1.seeded))
                .then_with(|| ts_cmp(&b.1.last_ts, &a.1.last_ts))
        });
        gaps.into_iter()
            .map(|(c, m)| GapJob {
                channel: c.clone(),
                from: m.gap_from.clone().unwrap_or_default(),
                until: m.gap_until.clone(),
                history_done: m.history_done,
                roots: m.pending_roots.clone(),
                kind: m.kind,
            })
            .collect()
    }

    /// Records the outcome of a gap check: closes the gap when the check was complete, or
    /// remembers how far it got. A check started before the newest connection may have missed
    /// that outage, so its result changes nothing.
    fn finish_gap(&self, job: &GapJob, generation: u64, result: &GapResult, found: bool) {
        let mut st = lock(&self.state);
        if st.generation != generation {
            return;
        }
        let Some(m) = st.cursor.conversations.get_mut(&job.channel) else {
            return;
        };
        if m.gap_from.as_deref() != Some(job.from.as_str()) {
            return;
        }
        if result.complete() {
            // A seeded conversation that turned out quiet is let go again, unless a live
            // message arrived meanwhile and made it a real one.
            if m.seeded && !found && m.last_ts.is_empty() {
                st.cursor.conversations.remove(&job.channel);
            } else {
                m.gap_from = None;
                m.gap_until = None;
                m.history_done = false;
                m.pending_roots.clear();
                m.seeded = false;
            }
        } else {
            m.gap_until = result.history_until.clone();
            m.history_done = result.history_done;
            m.pending_roots = result.pending_roots.clone();
        }
        st.dirty = true;
    }

    fn drop_mark(&self, channel: &str) {
        let mut st = lock(&self.state);
        st.cursor.conversations.remove(channel);
        st.dirty = true;
    }

    /// Learns the owner's id and the workspace URL with one `auth.test`, unless already known.
    async fn auth_once(&self) -> Result<(), FeedError> {
        if lock(&self.auth).is_some() {
            return Ok(());
        }
        let a = self.user.get::<AuthTest>("auth.test", &[]).await?;
        let url = a
            .url
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
        *lock(&self.auth) = Some(Auth {
            url,
            user_id: a.user_id,
        });
        Ok(())
    }

    /// [`auth_once`](Self::auth_once), retried until it works; `Err` only when the user token
    /// is unusable. Without the owner's id `mentions_owner` would be wrong, so nothing is
    /// delivered before it is known.
    async fn ensure_auth(&self, cfg: &Config, sink: &FeedSink) -> Result<(), FeedError> {
        let mut wait = cfg.reconnect_min;
        while lock(&self.auth).is_none() {
            match self.auth_once().await {
                Ok(()) => {}
                Err(e) if is_fatal(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "slack auth.test failed; retrying");
                    if sink.is_closed() {
                        return Ok(());
                    }
                    let d = match e {
                        FeedError::RateLimited(d) => d,
                        _ => wait,
                    };
                    tokio::time::sleep(d).await;
                    wait = (wait * 2).min(cfg.reconnect_max);
                }
            }
        }
        Ok(())
    }

    /// Turns a raw message into a [`FeedMessage`], looking up the names it needs first. A failed
    /// lookup only costs the name.
    async fn shape(
        &self,
        channel: &str,
        hint: Option<ConversationKind>,
        raw: &RawMessage,
    ) -> Option<FeedMessage> {
        let conv = match self.dir.conversation(channel).await {
            Ok(c) => Some(c),
            Err(e) => {
                self.lookup_failed("conversations.info", &e);
                None
            }
        };
        let kind = hint
            .filter(|k| *k != ConversationKind::Other)
            .or(conv.as_ref().map(|c| c.kind))
            .unwrap_or_default();
        let m = effective(raw);
        if let Some(u) = &m.user
            && let Err(e) = self.dir.user_name(u).await
        {
            self.lookup_failed("users.info", &e);
        }
        if let Some(text) = &m.text {
            for u in mrkdwn::mentioned_users(text)
                .into_iter()
                .take(MAX_MENTION_LOOKUPS)
            {
                if let Err(e) = self.dir.user_name(&u).await {
                    self.lookup_failed("users.info", &e);
                }
            }
        }
        let auth = self.auth();
        shape_message(
            raw,
            Shape {
                channel,
                channel_name: conv.and_then(|c| c.name),
                kind,
                thread_id: reply_thread(raw),
                workspace_url: auth.url.as_deref(),
                owner: auth.user_id.as_deref(),
            },
            |id| self.dir.cached_user(id),
            |id| self.dir.cached_conversation_name(id),
        )
    }

    /// Handles one pushed event: the events to deliver, and the progress to record once they
    /// are delivered.
    async fn handle_event(&self, ev: Value) -> Handled {
        let mut out = Handled::default();
        let meta = EventMeta::of(&ev);
        if meta.event_type.as_deref() != Some("message") {
            self.dropped("not a message event", &meta);
            return out;
        }
        let Some(raw) = RawMessage::parse(ev) else {
            self.dropped("unparsable message", &meta);
            return out;
        };
        let Some(channel) = raw.channel.clone() else {
            self.dropped("no channel", &meta);
            return out;
        };
        let hint = raw.channel_type.as_deref().map(kind_from_channel_type);
        if let Some(ts) = &raw.ts {
            out.mark = Some((channel.clone(), ts.clone(), hint));
        }
        match raw.subtype.as_deref() {
            Some("message_deleted") => {
                let deleted = raw
                    .deleted_ts
                    .clone()
                    .or_else(|| raw.previous_message.as_ref().and_then(|p| p.ts.clone()));
                match deleted {
                    Some(id) => out.events.push(FeedEvent::Deleted {
                        source: KIND.to_owned(),
                        conversation_id: channel,
                        message_id: id,
                    }),
                    None => self.dropped("deletion without a message id", &meta),
                }
            }
            Some("message_changed") => {
                let Some(new) = raw.message.as_deref() else {
                    self.dropped("edit without a new version", &meta);
                    return out;
                };
                let unchanged = raw
                    .previous_message
                    .as_deref()
                    .is_some_and(|old| old.text == new.text && old.files.len() == new.files.len());
                // An unfurl or a new reply count, not an edit.
                if unchanged {
                    self.dropped("edit without a text change", &meta);
                    return out;
                }
                match self.shape(&channel, hint, &raw).await {
                    Some(m) => out.events.push(FeedEvent::Message(m)),
                    None => self.shape_failed(&meta),
                }
            }
            _ if keep(&raw) => {
                let Some(ts) = raw.ts.as_deref() else {
                    self.dropped("no ts", &meta);
                    return out;
                };
                if self.is_delivered(&channel, ts) {
                    self.dropped("already delivered", &meta);
                    return out;
                }
                match self.shape(&channel, hint, &raw).await {
                    Some(m) => out.events.push(FeedEvent::Message(m)),
                    None => self.shape_failed(&meta),
                }
            }
            _ => self.dropped("housekeeping subtype", &meta),
        }
        out
    }

    /// Records an event that is deliberately not delivered. Only its type, subtype and
    /// conversation type are logged, never its text.
    fn dropped(&self, reason: &'static str, meta: &EventMeta) {
        tracing::debug!(
            reason,
            event_type = ?meta.event_type,
            subtype = ?meta.subtype,
            channel_type = ?meta.channel_type,
            "slack event not delivered"
        );
        *lock(&self.stats).drops.entry(reason).or_default() += 1;
    }

    /// A message event that could not be turned into a feed message (no usable ts).
    fn shape_failed(&self, meta: &EventMeta) {
        tracing::warn!(
            event_type = ?meta.event_type,
            subtype = ?meta.subtype,
            channel_type = ?meta.channel_type,
            "slack message event dropped: it has no usable ts"
        );
        *lock(&self.stats)
            .drops
            .entry("could not be shaped")
            .or_default() += 1;
    }

    /// A name lookup failed; the message is still delivered, without the name. Each distinct
    /// failure is logged as a warning once (a missing scope would otherwise repeat for every
    /// message), and all are counted in the minute summary.
    fn lookup_failed(&self, method: &'static str, e: &FeedError) {
        let key = format!("{method}: {e}");
        let mut stats = lock(&self.stats);
        *stats.lookup_errors.entry(key.clone()).or_default() += 1;
        if stats.warned_lookups.insert(key) {
            tracing::warn!(method, error = %e, "slack name lookup failed; messages go on without the name");
        } else {
            tracing::debug!(method, error = %e, "slack name lookup failed");
        }
    }

    /// Logs, once a minute, what the processor did with the events it got.
    fn log_minute_stats(&self) {
        let mut stats = lock(&self.stats);
        let drops = std::mem::take(&mut stats.drops);
        let lookups = std::mem::take(&mut stats.lookup_errors);
        let delivered = std::mem::take(&mut stats.delivered);
        if delivered == 0 && drops.is_empty() && lookups.is_empty() {
            return;
        }
        let dropped: Vec<String> = drops.iter().map(|(r, n)| format!("{r}: {n}")).collect();
        let failed: Vec<String> = lookups.iter().map(|(k, n)| format!("{k} ({n})")).collect();
        tracing::info!(
            delivered,
            dropped = %dropped.join(", "),
            lookup_errors = %failed.join(", "),
            "slack events processed in the last minute"
        );
    }

    /// One request of a gap check, through the shared Tier 3 gate.
    async fn paced_get<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        query: &[(&str, &str)],
    ) -> Result<T, FeedError> {
        pace::get(
            &self.user,
            &self.gates.tier3,
            MAX_RATE_LIMIT_RETRIES,
            method,
            query,
        )
        .await
    }

    /// One step of a gap check: history from the top of what is left of the gap, and the
    /// replies of threads active in it, each within its page budget. Oldest first within the
    /// step. With `dedupe`, messages delivered already are left out.
    async fn check_gap(
        &self,
        cfg: &Config,
        job: &GapJob,
        dedupe: bool,
    ) -> Result<GapResult, FeedError> {
        let channel = job.channel.as_str();
        let from = job.from.as_str();
        let pages = cfg.max_backfill_pages.max(1);
        let limit = cfg.page_size.clamp(1, 999).to_string();
        let mut result = GapResult {
            history_done: job.history_done,
            history_until: job.until.clone(),
            ..GapResult::default()
        };

        let mut history: Vec<RawMessage> = Vec::new();
        if !job.history_done {
            let from_unix = ts_unix(from).unwrap_or(0.0);
            // Reaching back past `from` shows thread roots that are older than the gap but got
            // replies during it, on the page that reaches `from`.
            let oldest = ts_from_unix(from_unix - cfg.thread_lookback.as_secs_f64());
            let mut next: Option<String> = None;
            let mut reached = false;
            let mut exhausted = false;
            for _ in 0..pages {
                let mut query = vec![
                    ("channel", channel),
                    ("oldest", oldest.as_str()),
                    ("inclusive", "false"),
                    ("limit", limit.as_str()),
                ];
                if let Some(until) = &job.until {
                    query.push(("latest", until.as_str()));
                }
                if let Some(c) = &next {
                    query.push(("cursor", c.as_str()));
                }
                let page: Page = self.paced_get("conversations.history", &query).await?;
                next = page.next_cursor();
                let has_more = page.has_more;
                let msgs: Vec<RawMessage> = page
                    .messages
                    .into_iter()
                    .filter_map(RawMessage::parse)
                    .collect();
                reached |= msgs
                    .iter()
                    .any(|m| m.ts.as_deref().is_some_and(|ts| !ts_gt(ts, from)));
                history.extend(msgs);
                if !has_more || next.is_none() {
                    exhausted = true;
                    break;
                }
                // Pages come newest first: once one reaches `from`, all of the gap is in hand.
                if reached {
                    break;
                }
            }
            sort_by_ts(&mut history);
            let lowest = history.iter().filter_map(|m| m.ts.clone()).next();
            if reached || exhausted || lowest.is_none() {
                result.history_done = true;
                result.history_until = None;
            } else {
                // The rest of the gap lies below the oldest message fetched.
                result.history_done = false;
                result.history_until = lowest;
                tracing::debug!(channel, "slack gap check continues in another step");
            }
        }

        let mut candidates: Vec<RawMessage> = Vec::new();
        let mut roots: Vec<PendingRoot> = job.roots.clone();
        let mut found_roots: Vec<(String, String)> = Vec::new();
        for m in &history {
            let Some(ts) = m.ts.as_deref() else { continue };
            if m.thread_ts.as_deref() == Some(ts)
                && m.reply_count.unwrap_or(0) > 0
                && let Some(latest) = m.latest_reply.as_deref()
                && ts_gt(latest, from)
            {
                found_roots.push((ts.to_owned(), latest.to_owned()));
            }
            if ts_gt(ts, from) && keep(m) {
                candidates.push(m.clone());
            }
        }
        found_roots.sort_by(|a, b| ts_cmp(&b.1, &a.1));
        for (root, _) in found_roots {
            if !roots.iter().any(|r| r.root == root) {
                roots.push(PendingRoot { root, after: None });
            }
        }
        // Threads beyond this step's budget are kept for the next step, all of them: the gap
        // stays open until every one was fetched.
        let later = roots.split_off(roots.len().min(cfg.max_backfill_threads));
        result.pending_roots = later;
        for pr in roots {
            let after = pr.after.clone().unwrap_or_else(|| from.to_owned());
            let mut last = after.clone();
            let mut next: Option<String> = None;
            let mut finished = false;
            for _ in 0..pages {
                let mut query = vec![
                    ("channel", channel),
                    ("ts", pr.root.as_str()),
                    ("oldest", after.as_str()),
                    ("inclusive", "false"),
                    ("limit", limit.as_str()),
                ];
                if let Some(c) = &next {
                    query.push(("cursor", c.as_str()));
                }
                let page: Page = match self.paced_get("conversations.replies", &query).await {
                    Ok(p) => p,
                    // The root was deleted meanwhile; its replies went with it.
                    Err(FeedError::Api(code)) if code == "thread_not_found" => {
                        finished = true;
                        break;
                    }
                    Err(e) => return Err(e),
                };
                next = page.next_cursor();
                let has_more = page.has_more;
                for r in page.messages.into_iter().filter_map(RawMessage::parse) {
                    let Some(ts) = r.ts.clone() else { continue };
                    // The root always comes back with its replies, and a reply also sent to
                    // the channel is already in history.
                    if ts == pr.root || !ts_gt(&ts, &after) {
                        continue;
                    }
                    last = ts_max(&last, &ts);
                    if r.subtype.as_deref() == Some("thread_broadcast") || !keep(&r) {
                        continue;
                    }
                    candidates.push(r);
                }
                if !has_more || next.is_none() {
                    finished = true;
                    break;
                }
            }
            if !finished {
                // Replies come oldest first, so the rest of this thread starts after `last`.
                result.pending_roots.push(PendingRoot {
                    root: pr.root,
                    after: Some(last),
                });
            }
        }
        if result.pending_roots.len() > PENDING_ROOTS_WARN {
            tracing::warn!(
                channel,
                threads = result.pending_roots.len(),
                "slack gap check: many active threads still to fetch"
            );
        }
        sort_by_ts(&mut candidates);

        for m in candidates {
            let ts = effective(&m).ts.clone().or_else(|| m.ts.clone());
            let Some(ts) = ts else { continue };
            if dedupe && self.is_delivered(channel, &ts) {
                continue;
            }
            if let Some(msg) = self.shape(channel, job.kind, &m).await {
                result.msgs.push(msg);
            }
        }
        Ok(result)
    }
}

/// Opens a gap in every conversation active within `window` before `reference`, from its
/// newest message seen, and forgets conversations quiet for longer than the horizon. A gap that
/// was open already starts over at the top: the new outage lies above what was checked.
fn open_gaps(st: &mut PushState, reference: f64, window: Duration) {
    let active_after = reference - window.as_secs_f64();
    let forget_before = reference - window.max(MARK_HORIZON).as_secs_f64();
    st.cursor
        .conversations
        .retain(|_, m| m.gap_from.is_some() || ts_unix(&m.last_ts).unwrap_or(0.0) >= forget_before);
    for m in st.cursor.conversations.values_mut() {
        if m.gap_from.is_some() {
            m.gap_until = None;
            m.history_done = false;
        }
        if m.last_ts.is_empty() || ts_unix(&m.last_ts).unwrap_or(0.0) < active_after {
            continue;
        }
        let from = m.last_ts.clone();
        widen_gap(m, &from);
    }
    st.dirty = true;
}

/// Adds a seed to the cursor: the older start wins, and it covers everything unless both cover
/// only direct and group messages.
fn merge_seed(cursor: &mut PushCursor, from: String, dms_only: bool) {
    match cursor.seed_from.take() {
        Some(old) => {
            cursor.seed_dms_only = cursor.seed_dms_only && dms_only;
            cursor.seed_from = Some(if ts_gt(&from, &old) { old } else { from });
        }
        None => {
            cursor.seed_dms_only = dms_only;
            cursor.seed_from = Some(from);
        }
    }
}

/// Marks a maintenance task as running; `true` when the caller has to start one.
fn claim_maintenance(st: &mut PushState) -> bool {
    if st.maintenance_active {
        return false;
    }
    st.maintenance_active = true;
    true
}

#[derive(Debug, Default)]
struct Handled {
    events: Vec<FeedEvent>,
    mark: Option<(String, String, Option<ConversationKind>)>,
}

/// Aborts a background task when its owner goes away.
struct Task<T = ()>(JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Clears `maintenance_active` when a maintenance task is aborted or panics, so the next
/// connection can start a new one. A task that ends normally disarms it, having cleared the flag
/// itself in the same step as deciding to stop.
struct ActiveGuard {
    shared: Arc<Shared>,
    armed: bool,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.shared.state).maintenance_active = false;
        }
    }
}

/// A replacement connection that has said hello.
type Ready = (Box<Ws>, Value);

/// How one connection ended.
enum Ended {
    /// Slack asked us to move to a new connection, and opening it early failed.
    Refresh,
    /// Socket Mode was switched off in the app settings.
    LinkDisabled,
    /// The connection broke or went silent.
    Lost(String),
    /// A replacement connection is up. `took_over` when the old one was read until the new
    /// one said hello, so nothing can have been missed.
    Replaced { next: Ready, took_over: bool },
    /// Nobody is listening any more.
    SinkClosed,
    /// The run cannot go on.
    Fatal(FeedError),
}

/// Sends one frame, giving up after `timeout`.
async fn send_frame<S>(ws: &mut S, msg: Message, timeout: Duration) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: Display,
{
    match tokio::time::timeout(timeout, ws.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("sending to slack timed out".into()),
    }
}

/// Asks Slack for a connection URL.
async fn open_url(shared: &Shared, cfg: &Config) -> Result<String, FeedError> {
    let open: ConnectionsOpen = shared
        .app
        .post_json("apps.connections.open", &json!({}))
        .await?;
    open.url
        .filter(|u| u.starts_with("wss://") || (cfg.allow_plain_ws && u.starts_with("ws://")))
        .ok_or_else(|| FeedError::Decode("apps.connections.open: no usable url".into()))
}

/// Opens the WebSocket.
async fn connect(url: &str, connector: &Connector, cfg: &Config) -> Result<Ws, String> {
    let builder = ClientBuilder::new()
        .uri(url)
        .map_err(|e| format!("socket url: {e}"))?
        .connector(connector);
    match tokio::time::timeout(cfg.connect_timeout, builder.connect()).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("connect timed out".into()),
    }
}

/// Opens a replacement connection and waits for its hello, while the old one keeps being
/// read.
async fn open_replacement(
    shared: Arc<Shared>,
    cfg: Config,
    connector: Arc<Connector>,
) -> Result<Ready, String> {
    let url = open_url(&shared, &cfg).await.map_err(|e| e.to_string())?;
    let mut ws = connect(&url, &connector, &cfg).await?;
    let first = tokio::time::timeout(HELLO_TIMEOUT, ws.next())
        .await
        .map_err(|_| "no hello from the replacement connection".to_owned())?;
    let msg = match first {
        Some(Ok(m)) => m,
        Some(Err(e)) => return Err(e.to_string()),
        None => return Err("replacement connection closed".into()),
    };
    let v: Value = msg
        .as_text()
        .and_then(|t| serde_json::from_str(t).ok())
        .ok_or_else(|| "replacement connection sent no hello".to_owned())?;
    if v.get("type").and_then(Value::as_str) != Some("hello") {
        return Err("replacement connection sent no hello".into());
    }
    Ok((Box::new(ws), v))
}

impl SlackSocket {
    /// A Socket Mode source. `user_token` is the owner's `xoxp-…` token, `app_token` the app's
    /// `xapp-…` token with `connections:write`. Both are only ever sent in `Authorization`
    /// headers and are redacted from `Debug` output.
    pub fn new(user_token: impl Into<String>, app_token: impl Into<String>) -> Self {
        let cfg = Config::default();
        let user = Api::new(user_token.into(), DEFAULT_BASE_URL.to_owned());
        let app = Api::new(app_token.into(), DEFAULT_BASE_URL.to_owned());
        Self {
            shared: Arc::new(Shared::new(user, app, &cfg, PushState::default())),
            cfg,
        }
    }

    /// Rebuilds the shared part with changed APIs or settings, keeping the progress.
    fn rebuild(&mut self, base: Option<&str>) {
        let mut user = self.shared.user.clone();
        let mut app = self.shared.app.clone();
        if let Some(base) = base {
            user.set_base(base);
            app.set_base(base);
        }
        let state = std::mem::take(&mut *lock(&self.shared.state));
        self.shared = Arc::new(Shared::new(user, app, &self.cfg, state));
    }

    /// Points both tokens at another Web API root (a mock server in tests). The default is
    /// [`DEFAULT_BASE_URL`]. The WebSocket URL always comes from `apps.connections.open`; a
    /// plain `ws://` one is accepted only with a base URL other than Slack's.
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        let base = base_url.as_ref();
        self.cfg.allow_plain_ws = base.trim_end_matches('/') != DEFAULT_BASE_URL;
        self.rebuild(Some(base));
        self
    }

    /// The longest window before an outage in which conversations get a gap check
    /// (default 7 days). The window used is a hundred times the outage, at least an hour, at
    /// most this.
    pub fn with_backfill_window(mut self, window: Duration) -> Self {
        self.cfg.backfill_window = window;
        self
    }

    /// How many gap-check steps one round may take at most (default 500). Gaps left over keep
    /// their place in the cursor and are tried again later.
    pub fn with_max_backfill_conversations(mut self, n: usize) -> Self {
        self.cfg.max_backfill_conversations = n;
        self
    }

    /// How many pages of history, and of each thread's replies, one gap-check step reads
    /// (default 5), and how many messages a page holds (default 200).
    pub fn with_backfill_pages(mut self, pages: usize, page_size: u32) -> Self {
        self.cfg.max_backfill_pages = pages.max(1);
        self.cfg.page_size = page_size.clamp(1, 999);
        self
    }

    /// How many threads one gap-check step fetches replies for (default 20); the rest wait
    /// for the next step.
    pub fn with_max_backfill_threads(mut self, n: usize) -> Self {
        self.cfg.max_backfill_threads = n;
        self
    }

    /// How far before a gap a check reaches back to notice thread roots with new replies
    /// (default 24 hours). It makes the history answer larger, not the number of requests.
    pub fn with_thread_lookback(mut self, lookback: Duration) -> Self {
        self.cfg.thread_lookback = lookback;
        self
    }

    /// An outage longer than this also checks every member conversation, not just recently
    /// active ones (default 60 s).
    pub fn with_seed_after(mut self, after: Duration) -> Self {
        self.cfg.seed_after = after;
        self
    }

    /// How long until gaps a round could not finish are tried again (default 5 minutes).
    pub fn with_refill_after(mut self, after: Duration) -> Self {
        self.cfg.refill_after = after;
        self
    }

    /// The spacing of Tier 3 requests (default 1.3 s, under Tier 3's 50 a minute); Tier 4
    /// requests are spaced half as much.
    pub fn with_backfill_request_gap(mut self, gap: Duration) -> Self {
        self.cfg.request_gap = gap;
        self.rebuild(None);
        self
    }

    /// The reconnect back-off range (default 1 s to 60 s, doubling per failed attempt).
    pub fn with_reconnect_backoff(mut self, min: Duration, max: Duration) -> Self {
        self.cfg.reconnect_min = min;
        self.cfg.reconnect_max = max.max(min);
        self
    }

    /// How often a ping is sent, and how long a connection may stay silent before it is
    /// considered dead and replaced (default 30 s and 90 s).
    pub fn with_keepalive(mut self, ping_every: Duration, idle_timeout: Duration) -> Self {
        self.cfg.ping_every = ping_every;
        self.cfg.idle_timeout = idle_timeout.max(ping_every);
        self
    }

    /// How many received events may wait for the consumer (default 50 000). Past that the
    /// reader keeps acknowledging, drops the event and opens a gap for its conversation.
    pub fn with_event_queue(mut self, n: usize) -> Self {
        self.cfg.event_queue = n.max(1);
        self
    }

    /// How often, at most, a [`FeedEvent::Checkpoint`] is sent when progress was made (default
    /// 10 s).
    pub fn with_checkpoint_interval(mut self, every: Duration) -> Self {
        self.cfg.checkpoint_every = every;
        self
    }

    /// How often the membership list (`users.conversations`) is refreshed (default 6 hours).
    pub fn with_directory_refresh(mut self, every: Duration) -> Self {
        self.cfg.directory_refresh = every;
        self.rebuild(None);
        self
    }

    /// The owner's Slack user id, once `auth.test` has answered.
    pub fn self_user_id(&self) -> Option<String> {
        self.shared.auth().user_id
    }

    /// Handles a hello. `took_over` when the old connection was read until now.
    fn hello(
        &self,
        v: &Value,
        sink: &FeedSink,
        maintenance: &mut Option<Task>,
        took_over: bool,
        announce: bool,
    ) {
        let connections = v.get("num_connections").and_then(Value::as_u64);
        tracing::info!(?connections, took_over, "slack socket connected");
        if connections.is_some_and(|n| n > if took_over { 2 } else { 1 }) {
            tracing::warn!(
                ?connections,
                "another connection uses this Slack app's token; Slack splits events between \
                 connections, so this one will miss whatever goes to the others"
            );
        }
        if announce {
            // Status is informational: never let a full sink hold up the reader.
            let _ = sink.try_send(FeedEvent::Status(SourceStatus::Connected));
        }
        if self.shared.on_hello(unix_now(), &self.cfg, took_over) {
            *maintenance = Some(self.spawn_maintenance(sink.clone()));
        }
    }

    /// Runs one connection until it ends. `first` is a hello already read from it, when it
    /// replaced another connection.
    #[allow(clippy::too_many_arguments)]
    async fn session(
        &self,
        ws: &mut Ws,
        first: Option<(Value, bool)>,
        connector: &Arc<Connector>,
        events: &mpsc::Sender<Value>,
        sink: &FeedSink,
        maintenance: &mut Option<Task>,
        processor: &mut Task,
        hello: &mut bool,
    ) -> Ended {
        if let Some((v, took_over)) = first {
            *hello = true;
            self.hello(&v, sink, maintenance, took_over, false);
        }
        let mut ping = tokio::time::interval(self.cfg.ping_every);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;
        let mut last_frame = Instant::now();
        let mut replacement: Option<Task<Result<Ready, String>>> = None;
        // What arrives, summarised once a minute, so that "connected but nothing comes" can be
        // told apart from "events come but are not delivered".
        let mut counts = FrameCounts::default();
        let mut first_event = true;
        let mut minute = tokio::time::interval(Duration::from_secs(60));
        minute.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        minute.tick().await;
        loop {
            tokio::select! {
                frame = ws.next() => {
                    let msg = match frame {
                        Some(Ok(m)) if !m.is_close() => m,
                        other => {
                            let why = match other {
                                Some(Err(e)) => e.to_string(),
                                Some(Ok(_)) => "closed by slack".to_owned(),
                                None => "connection closed".to_owned(),
                            };
                            // A replacement on its way is still worth having, but the gap
                            // between the two connections has to be checked.
                            if let Some(mut r) = replacement.take()
                                && let Ok(Ok(next)) = (&mut r.0).await
                            {
                                return Ended::Replaced { next, took_over: false };
                            }
                            return Ended::Lost(why);
                        }
                    };
                    last_frame = Instant::now();
                    let Some(text) = msg.as_text() else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(text) else {
                        tracing::warn!("slack socket sent a frame that is not json");
                        continue;
                    };
                    // Acknowledge first: Slack retries anything not acknowledged within three
                    // seconds, and everything below may take longer than that.
                    if let Some(id) = v.get("envelope_id").and_then(Value::as_str) {
                        let ack = json!({ "envelope_id": id }).to_string();
                        if let Err(why) =
                            send_frame(ws, Message::text(ack), self.cfg.send_timeout).await
                        {
                            return Ended::Lost(why);
                        }
                    }
                    match v.get("type").and_then(Value::as_str) {
                        Some("hello") => {
                            counts.hello += 1;
                            *hello = true;
                            self.hello(&v, sink, maintenance, false, true);
                        }
                        Some("disconnect") => {
                            counts.disconnect += 1;
                            let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
                            tracing::info!(reason, "slack socket disconnect requested");
                            if reason == "link_disabled" {
                                return Ended::LinkDisabled;
                            }
                            if replacement.is_none() {
                                replacement = Some(Task(tokio::spawn(open_replacement(
                                    self.shared.clone(),
                                    self.cfg.clone(),
                                    connector.clone(),
                                ))));
                            }
                        }
                        Some("events_api") => {
                            counts.events_api += 1;
                            let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                            let meta = EventMeta::of(payload.get("event").unwrap_or(&Value::Null));
                            if first_event {
                                first_event = false;
                                tracing::info!(
                                    event_type = ?meta.event_type,
                                    subtype = ?meta.subtype,
                                    channel_type = ?meta.channel_type,
                                    "slack socket: first event since connecting"
                                );
                            } else {
                                tracing::debug!(
                                    event_type = ?meta.event_type,
                                    subtype = ?meta.subtype,
                                    channel_type = ?meta.channel_type,
                                    "slack socket: event"
                                );
                            }
                            let key = payload
                                .get("event_id")
                                .and_then(Value::as_str)
                                .or_else(|| v.get("envelope_id").and_then(Value::as_str))
                                .map(str::to_owned);
                            let fresh =
                                key.is_none_or(|k| lock(&self.shared.state).events.insert(k));
                            if !fresh {
                                tracing::debug!("slack event delivered again; dropped");
                                continue;
                            }
                            let Some(event) = payload.get("event").cloned() else {
                                tracing::warn!("slack events_api envelope without an event; ignored");
                                continue;
                            };
                            // Count before sending, so the processor never sees fewer than it
                            // has to finish.
                            self.shared.queued.fetch_add(1, Ordering::SeqCst);
                            match events.try_send(event) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(event)) => {
                                    self.shared.queued.fetch_sub(1, Ordering::SeqCst);
                                    // The reader never waits for the consumer: the event is
                                    // dropped and recovered from history.
                                    if self.shared.overflow(&event) {
                                        *maintenance = Some(self.spawn_maintenance(sink.clone()));
                                    }
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    self.shared.queued.fetch_sub(1, Ordering::SeqCst);
                                    return processor_gone(sink);
                                }
                            }
                        }
                        // Slash commands and interactivity are acknowledged above and otherwise
                        // not ours to handle.
                        other => {
                            counts.other += 1;
                            tracing::debug!(frame_type = ?other, "slack socket: frame ignored");
                        }
                    }
                }
                _ = minute.tick() => {
                    if counts.any() {
                        tracing::info!(
                            events_api = counts.events_api,
                            hello = counts.hello,
                            disconnect = counts.disconnect,
                            other = counts.other,
                            "slack socket: {} events_api, {} other frames in the last minute",
                            counts.events_api,
                            counts.hello + counts.disconnect + counts.other,
                        );
                        counts = FrameCounts::default();
                    }
                }
                ready = async { (&mut replacement.as_mut().expect("guarded").0).await },
                    if replacement.is_some() =>
                {
                    match ready {
                        Ok(Ok(next)) => return Ended::Replaced { next, took_over: true },
                        Ok(Err(why)) => {
                            tracing::warn!(reason = %why, "slack replacement connection failed");
                            return Ended::Refresh;
                        }
                        Err(_) => return Ended::Refresh,
                    }
                }
                _ = ping.tick() => {
                    if sink.is_closed() {
                        return Ended::SinkClosed;
                    }
                    if last_frame.elapsed() >= self.cfg.idle_timeout {
                        return Ended::Lost("no traffic from slack".into());
                    }
                    if let Err(why) = send_frame(ws, Message::ping(""), self.cfg.send_timeout).await {
                        return Ended::Lost(why);
                    }
                    if *hello {
                        self.shared.alive(unix_now());
                    }
                    let wanted = self.shared.request_due_fill()
                        || (self.shared.dir.refresh_due() && self.shared.request_maintenance());
                    if wanted {
                        *maintenance = Some(self.spawn_maintenance(sink.clone()));
                    }
                }
                _ = self.shared.fatal_notify.notified() => {
                    if let Some(e) = self.shared.take_fatal() {
                        return Ended::Fatal(e);
                    }
                }
                _ = &mut processor.0 => return processor_gone(sink),
            }
        }
    }

    /// Gap checks and membership refreshes, in the background so that the connection keeps
    /// acknowledging events meanwhile. One task at a time; it goes round again while new
    /// requests come in.
    fn spawn_maintenance(&self, sink: FeedSink) -> Task {
        let shared = self.shared.clone();
        let cfg = self.cfg.clone();
        Task(tokio::spawn(async move {
            let mut guard = ActiveGuard {
                shared: shared.clone(),
                armed: true,
            };
            loop {
                if shared.take_fill() {
                    if let Some((from, dms_only)) = shared.take_seed() {
                        seed_gaps(&shared, &cfg, from, dms_only).await;
                    }
                    fill_gaps(&shared, &cfg, &sink).await;
                }
                if shared.dir.refresh_due() {
                    match shared.dir.refresh().await {
                        Ok(list) => {
                            tracing::debug!(count = list.len(), "slack membership refreshed")
                        }
                        Err(e) if is_fatal(&e) => shared.set_fatal(e),
                        Err(e) => tracing::warn!(error = %e, "slack membership refresh failed"),
                    }
                }
                if !shared.maintenance_continue() {
                    guard.armed = false;
                    break;
                }
            }
        }))
    }
}

/// What ended the run when the event processor is gone: a closed sink is a normal stop, anything
/// else means the processor died.
fn processor_gone(sink: &FeedSink) -> Ended {
    if sink.is_closed() {
        Ended::SinkClosed
    } else {
        Ended::Fatal(FeedError::Internal(
            "the slack event processor stopped".into(),
        ))
    }
}

/// Opens gaps from `from` in every member conversation, after a long outage. The membership
/// list is read fresh: the cached one may be hours old and miss a conversation that started
/// during the outage. When it cannot be read the seed is kept and tried again later.
async fn seed_gaps(shared: &Shared, cfg: &Config, from: String, dms_only: bool) {
    match shared.dir.refresh().await {
        Ok(members) => shared.seed(members, &from, dms_only),
        Err(e) if is_fatal(&e) => shared.set_fatal(e),
        Err(e) => {
            tracing::warn!(error = %e, "slack membership unavailable; checking all conversations later");
            shared.restore_seed(from, dms_only);
            shared.schedule_refill(cfg.refill_after);
        }
    }
}

/// One round of gap checks: every open gap, each checked up to a few times while its check
/// makes progress, within the round's step budget. Gaps still open afterwards are tried again
/// after `refill_after`.
async fn fill_gaps(shared: &Shared, cfg: &Config, sink: &FeedSink) {
    let mut checks: HashMap<String, usize> = HashMap::new();
    let mut budget = cfg.max_backfill_conversations;
    let mut recovered = 0usize;
    while budget > 0 {
        let Some(job) = shared
            .pending_gaps()
            .into_iter()
            .find(|j| checks.get(&j.channel).copied().unwrap_or(0) < MAX_CHECKS_PER_ROUND)
        else {
            break;
        };
        budget -= 1;
        *checks.entry(job.channel.clone()).or_default() += 1;
        let generation = shared.generation();
        match shared.check_gap(cfg, &job, true).await {
            Ok(mut result) => {
                let found = !result.msgs.is_empty();
                recovered += result.msgs.len();
                for m in std::mem::take(&mut result.msgs) {
                    let id = m.id.clone();
                    if sink.send(FeedEvent::Message(m)).await.is_err() {
                        return;
                    }
                    shared.mark_delivered(&job.channel, &id);
                    shared.note(&job.channel, &id, job.kind);
                }
                shared.finish_gap(&job, generation, &result, found);
            }
            Err(e) if is_fatal(&e) => {
                shared.set_fatal(e);
                return;
            }
            Err(FeedError::Api(code))
                if matches!(code.as_str(), "channel_not_found" | "not_in_channel") =>
            {
                // The owner left or lost access; nothing more will come from there.
                shared.drop_mark(&job.channel);
            }
            Err(e) => {
                tracing::warn!(channel = %job.channel, error = %e, "slack gap check failed; retried later");
                checks.insert(job.channel.clone(), MAX_CHECKS_PER_ROUND);
            }
        }
    }
    if recovered > 0 {
        tracing::info!(recovered, "slack gap check recovered missed messages");
    }
    if !shared.pending_gaps().is_empty() {
        shared.schedule_refill(cfg.refill_after);
    }
}

/// Turns pushed events into feed events, in order, off the connection's task so that
/// acknowledgements never wait for name lookups or a slow consumer. It is also the only sender
/// of checkpoints.
async fn process(
    shared: Arc<Shared>,
    mut events: mpsc::Receiver<Value>,
    sink: FeedSink,
    every: Duration,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut minute = tokio::time::interval(Duration::from_secs(60));
    minute.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    minute.tick().await;
    loop {
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else { return };
                let handled = shared.handle_event(ev).await;
                for e in handled.events {
                    let key = match &e {
                        FeedEvent::Message(m) => Some((m.conversation_id.clone(), m.id.clone())),
                        _ => None,
                    };
                    if sink.send(e).await.is_err() {
                        return;
                    }
                    if let Some((c, id)) = key {
                        shared.mark_delivered(&c, &id);
                        lock(&shared.stats).delivered += 1;
                    }
                }
                // Progress is recorded only once its messages are with the consumer, so a
                // checkpoint never covers a message that was not delivered.
                if let Some((channel, ts, kind)) = handled.mark {
                    shared.note(&channel, &ts, kind);
                }
                shared.queued.fetch_sub(1, Ordering::SeqCst);
            }
            _ = tick.tick() => {
                if let Some(c) = shared.checkpoint()
                    && sink.send(FeedEvent::Checkpoint(c)).await.is_err()
                {
                    return;
                }
            }
            _ = minute.tick() => shared.log_minute_stats(),
        }
    }
}

/// The TLS client for the WebSocket: ring, like the rest of the process, and the platform's
/// certificate verifier, like the Web API client. A failure here is a broken installation.
fn tls_connector() -> Result<Connector, FeedError> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| FeedError::Internal(format!("tls setup: {e}")))?
        .with_platform_verifier()
        .map_err(|e| FeedError::Internal(format!("tls setup: {e}")))?
        .with_no_client_auth();
    Ok(Connector::Rustls(tokio_rustls::TlsConnector::from(
        Arc::new(config),
    )))
}

/// Errors from `apps.connections.open` that retrying cannot fix.
fn open_is_fatal(e: &FeedError) -> bool {
    is_fatal(e)
        || matches!(e, FeedError::Api(code) if matches!(
            code.as_str(),
            "not_allowed_token_type" | "missing_scope" | "access_denied"
        ))
}

/// Spreads reconnects a little, so many clients do not retry in lockstep.
fn jitter(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    d + d.mul_f64(f64::from(nanos % 250) / 1000.0)
}

/// On-demand backfill for a [`SlackSocket`], usable while it runs. It shares the socket's rate
/// gates.
struct SocketBackfiller {
    shared: Arc<Shared>,
    cfg: Config,
}

#[async_trait]
impl Backfiller for SocketBackfiller {
    /// Checks every conversation with activity within the backfill window before `since_unix`,
    /// most recent first, for messages after `since_unix`.
    async fn backfill(&self, since_unix: f64) -> Result<Vec<FeedMessage>, FeedError> {
        // One attempt: the caller decides whether to retry a one-off backfill.
        self.shared.auth_once().await?;
        let from = ts_from_unix(since_unix);
        let active_after = since_unix - self.cfg.backfill_window.as_secs_f64();
        let mut convs: Vec<(String, Mark)> = lock(&self.shared.state)
            .cursor
            .conversations
            .iter()
            .filter(|(_, m)| ts_unix(&m.last_ts).unwrap_or(0.0) >= active_after)
            .map(|(c, m)| (c.clone(), m.clone()))
            .collect();
        convs.sort_by(|a, b| ts_cmp(&b.1.last_ts, &a.1.last_ts));
        let mut out = Vec::new();
        'conversations: for (channel, mark) in
            convs.into_iter().take(self.cfg.max_backfill_conversations)
        {
            let mut job = GapJob {
                channel: channel.clone(),
                from: from.clone(),
                until: None,
                history_done: false,
                roots: Vec::new(),
                kind: mark.kind,
            };
            for _ in 0..MAX_CHECKS_PER_ROUND * 3 {
                match self.shared.check_gap(&self.cfg, &job, false).await {
                    Ok(mut r) => {
                        out.append(&mut r.msgs);
                        if r.complete() {
                            continue 'conversations;
                        }
                        job.until = r.history_until;
                        job.history_done = r.history_done;
                        job.roots = r.pending_roots;
                    }
                    Err(e) if is_fatal(&e) => return Err(e),
                    Err(FeedError::RateLimited(d)) if out.is_empty() => {
                        return Err(FeedError::RateLimited(d));
                    }
                    Err(e) => {
                        tracing::warn!(channel = %channel, error = %e, "slack backfill failed for a conversation");
                        continue 'conversations;
                    }
                }
            }
            tracing::warn!(channel = %channel, "slack backfill stopped before the end of a large gap");
        }
        out.sort_by(|a, b| a.ts_unix.total_cmp(&b.ts_unix));
        out.dedup_by(|a, b| a.conversation_id == b.conversation_id && a.id == b.id);
        Ok(out)
    }
}

#[async_trait]
impl Source for SlackSocket {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Socket Mode pushes; use [`run`](Source::run).
    async fn poll(&mut self) -> Result<Vec<FeedMessage>, FeedError> {
        Err(FeedError::Unsupported(
            "poll (Socket Mode pushes events; use run)",
        ))
    }

    fn cursor(&self) -> Value {
        serde_json::to_value(&lock(&self.shared.state).cursor).unwrap_or(Value::Null)
    }

    /// Accepts this source's own cursor, or the polling [`SlackSource`](super::SlackSource)'s:
    /// switching from polling to Socket Mode then checks the polled channels for messages since
    /// the poller stopped.
    fn restore_cursor(&mut self, v: Value) {
        let mut st = lock(&self.shared.state);
        if v.is_null() {
            st.cursor = PushCursor::new();
            return;
        }
        if let Ok(c) = serde_json::from_value::<PushCursor>(v.clone())
            && c.mode == PushCursor::MODE
        {
            st.cursor = c;
            return;
        }
        match serde_json::from_value::<PollCursor>(v) {
            Ok(p) if p.v == PollCursor::VERSION && !p.channels.is_empty() => {
                let mut c = PushCursor::new();
                for (channel, s) in p.channels {
                    if let Some(last) = s.last_ts {
                        c.conversations.insert(
                            channel,
                            Mark {
                                last_ts: last,
                                ..Mark::default()
                            },
                        );
                    }
                }
                st.cursor = c;
            }
            _ => tracing::warn!("ignoring an unreadable slack socket cursor"),
        }
    }

    async fn post_reply(
        &self,
        conversation_id: &str,
        thread_id: Option<&str>,
        text: &str,
    ) -> Result<String, FeedError> {
        post_message(&self.shared.user, conversation_id, thread_id, text).await
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            poll: false,
            push: true,
            backfill: true,
            can_reply: true,
            can_reply_in_thread: true,
            directory: true,
        }
    }

    fn is_fatal(&self, err: &FeedError) -> bool {
        open_is_fatal(err) || matches!(err, FeedError::Unsupported(_) | FeedError::Internal(_))
    }

    /// Returns only errors [`is_fatal`](Source::is_fatal) accepts; everything else is retried.
    async fn run(&mut self, sink: FeedSink) -> Result<(), FeedError> {
        // Counts and errors left over from an earlier run belong to that run.
        self.shared.queued.store(0, Ordering::SeqCst);
        self.shared.take_fatal();
        self.shared.ensure_auth(&self.cfg, &sink).await?;
        let connector = Arc::new(tls_connector()?);
        let (events_tx, events_rx) = mpsc::channel::<Value>(self.cfg.event_queue);
        let mut processor = Task(tokio::spawn(process(
            self.shared.clone(),
            events_rx,
            sink.clone(),
            self.cfg.checkpoint_every,
        )));
        let mut maintenance: Option<Task> = None;
        let mut backoff = self.cfg.reconnect_min;
        let mut ready: Option<(Ready, bool)> = None;
        loop {
            if sink.is_closed() {
                return Ok(());
            }
            if let Some(e) = self.shared.take_fatal() {
                return Err(e);
            }
            let (mut ws, first) = match ready.take() {
                Some(((ws, hello_v), took_over)) => (*ws, Some((hello_v, took_over))),
                None => {
                    let url = match open_url(&self.shared, &self.cfg).await {
                        Ok(u) => u,
                        Err(e) if open_is_fatal(&e) => return Err(e),
                        Err(e) => {
                            let wait = match e {
                                FeedError::RateLimited(d) => d,
                                _ => jitter(backoff),
                            };
                            tracing::warn!(error = %e, ?wait, "slack apps.connections.open failed");
                            tokio::time::sleep(wait).await;
                            backoff = (backoff * 2).min(self.cfg.reconnect_max);
                            continue;
                        }
                    };
                    match connect(&url, &connector, &self.cfg).await {
                        Ok(ws) => (ws, None),
                        Err(why) => {
                            tracing::warn!(reason = %why, "slack socket connect failed");
                            tokio::time::sleep(jitter(backoff)).await;
                            backoff = (backoff * 2).min(self.cfg.reconnect_max);
                            continue;
                        }
                    }
                }
            };
            let mut hello = false;
            let ended = self
                .session(
                    &mut ws,
                    first,
                    &connector,
                    &events_tx,
                    &sink,
                    &mut maintenance,
                    &mut processor,
                    &mut hello,
                )
                .await;
            // A clean close waits for the peer's answer, which a dying connection may never
            // send; the new connection must not wait for it.
            let _ = tokio::time::timeout(Duration::from_secs(2), ws.close()).await;
            if hello {
                // The gap the next connection checks starts here (if everything received has
                // reached the consumer).
                self.shared.alive(unix_now());
                backoff = self.cfg.reconnect_min;
                let reason = match &ended {
                    Ended::Refresh => Some("refresh requested by slack".to_owned()),
                    Ended::LinkDisabled => {
                        Some("socket mode disabled in the app settings".to_owned())
                    }
                    Ended::Lost(why) => Some(why.clone()),
                    Ended::Replaced { .. } | Ended::SinkClosed | Ended::Fatal(_) => None,
                };
                if let Some(reason) = reason {
                    let _ = sink.try_send(FeedEvent::Status(SourceStatus::Disconnected { reason }));
                }
            }
            match ended {
                Ended::SinkClosed => return Ok(()),
                Ended::Fatal(e) => return Err(e),
                Ended::Replaced { next, took_over } => ready = Some((next, took_over)),
                Ended::Refresh => {}
                Ended::LinkDisabled => {
                    tracing::warn!(
                        "slack Socket Mode is disabled in the app settings; retrying slowly"
                    );
                    tokio::time::sleep(self.cfg.reconnect_max).await;
                }
                Ended::Lost(why) => {
                    tracing::info!(reason = %why, "slack socket lost; reconnecting");
                    tokio::time::sleep(jitter(backoff)).await;
                    backoff = (backoff * 2).min(self.cfg.reconnect_max);
                }
            }
        }
    }

    fn backfiller(&self) -> Option<Arc<dyn Backfiller>> {
        Some(Arc::new(SocketBackfiller {
            shared: self.shared.clone(),
            cfg: self.cfg.clone(),
        }))
    }

    fn directory(&self) -> Option<Arc<dyn Directory>> {
        Some(self.shared.dir.clone())
    }

    fn replier(&self) -> Option<Arc<dyn Replier>> {
        Some(Arc::new(super::SlackReplier(self.shared.user.clone())))
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;

    fn shared() -> Shared {
        // Port 9 refuses connections at once, so name lookups fail fast.
        Shared::new(
            Api::new("x".into(), "http://127.0.0.1:9".into()),
            Api::new("y".into(), "http://127.0.0.1:9".into()),
            &Config::default(),
            PushState::default(),
        )
    }

    #[test]
    fn recent_forgets_the_oldest() {
        let mut r = Recent::new(2);
        assert!(r.insert(1));
        assert!(!r.insert(1));
        assert!(r.insert(2));
        assert!(r.insert(3));
        assert!(r.insert(1), "1 was forgotten when 3 came in");
    }

    #[test]
    fn gaps_open_for_conversations_active_before_the_outage() {
        let s = shared();
        let now = 1_700_000_000.0;
        let lost = now - 10.0 * 86_400.0;
        s.note("CNEW", &ts_from_unix(lost - 60.0), None);
        s.note("COLD", &ts_from_unix(lost - 30.0 * 86_400.0), None);
        s.note("CGONE", &ts_from_unix(lost - 400.0 * 86_400.0), None);
        s.alive(lost);
        let cfg = Config::default();
        assert!(
            s.on_hello(now, &cfg, false),
            "the first request starts a task"
        );
        assert!(!s.on_hello(now, &cfg, false), "one task at a time");
        let gaps = s.pending_gaps();
        assert_eq!(gaps.len(), 1, "measured from the outage, not from now");
        assert_eq!(gaps[0].channel, "CNEW");
        assert_eq!(gaps[0].from, ts_from_unix(lost - 60.0));
        assert_eq!(
            s.take_seed(),
            Some((ts_from_unix(lost), false)),
            "a long outage seeds every member conversation"
        );
        let st = lock(&s.state);
        assert!(st.cursor.conversations.contains_key("COLD"));
        assert!(!st.cursor.conversations.contains_key("CGONE"));
    }

    #[test]
    fn the_checked_window_grows_with_the_outage() {
        let cfg = Config::default();
        assert_eq!(cfg.check_window(5.0), Duration::from_secs(3600));
        assert_eq!(cfg.check_window(600.0), Duration::from_secs(60_000));
        assert_eq!(cfg.check_window(1e9), cfg.backfill_window);
        let s = shared();
        let now = 1_700_000_000.0;
        s.note("CHOUR", &ts_from_unix(now - 1800.0), None);
        s.note("CDAY", &ts_from_unix(now - 86_400.0), None);
        s.alive(now - 5.0);
        s.on_hello(now, &cfg, false);
        let gaps: Vec<String> = s.pending_gaps().into_iter().map(|g| g.channel).collect();
        assert_eq!(gaps, ["CHOUR"], "a short blip checks only the last hour");
        assert_eq!(
            s.take_seed(),
            Some((ts_from_unix(now - 5.0), true)),
            "but every direct and group message"
        );
        // Seeds merge: the older start wins, and a full seed is never narrowed.
        let mut c = PushCursor::new();
        merge_seed(&mut c, "200.000000".into(), false);
        merge_seed(&mut c, "100.000000".into(), true);
        assert_eq!(c.seed_from.as_deref(), Some("100.000000"));
        assert!(!c.seed_dms_only);
        // Taking over from a live connection checks nothing.
        let s = shared();
        s.note("CHOUR", &ts_from_unix(now - 60.0), None);
        assert!(!s.on_hello(now, &cfg, true));
        assert!(s.pending_gaps().is_empty());
    }

    #[test]
    fn liveness_waits_for_queued_events() {
        let s = shared();
        s.alive(100.0);
        s.queued.store(1, Ordering::SeqCst);
        s.alive(200.0);
        assert_eq!(lock(&s.state).cursor.last_alive, Some(100.0));
        s.queued.store(0, Ordering::SeqCst);
        s.alive(300.0);
        assert_eq!(lock(&s.state).cursor.last_alive, Some(300.0));
    }

    #[test]
    fn an_overflowing_event_opens_a_gap_just_before_it() {
        let s = shared();
        s.note("C1", "1700000000.000500", None);
        assert!(
            s.overflow(&json!({"type": "message", "channel": "C1", "ts": "1700000000.000100"}))
        );
        let gaps = s.pending_gaps();
        assert_eq!(gaps[0].from, "1700000000.000099");
        assert!(s.take_fill());
    }

    #[test]
    fn a_stale_check_does_not_close_a_gap() {
        let s = shared();
        let now = 1_700_000_000.0;
        s.note("C1", &ts_from_unix(now - 60.0), None);
        s.alive(now - 30.0);
        s.on_hello(now, &Config::default(), false);
        let job = s.pending_gaps().remove(0);
        let gen_before = s.generation();
        // A new connection while the check runs.
        s.on_hello(now + 1.0, &Config::default(), false);
        let done = GapResult {
            history_done: true,
            ..GapResult::default()
        };
        s.finish_gap(&job, gen_before, &done, false);
        assert_eq!(
            s.pending_gaps().len(),
            1,
            "the newer outage is still to check"
        );
        s.finish_gap(&job, s.generation(), &done, false);
        assert!(s.pending_gaps().is_empty());
    }

    #[test]
    fn a_seeded_mark_with_live_activity_is_kept() {
        let s = shared();
        s.seed(
            vec![crate::Conversation {
                id: "D1".into(),
                name: None,
                kind: ConversationKind::Direct,
            }],
            "1700000000.000000",
            false,
        );
        let job = s.pending_gaps().remove(0);
        s.note("D1", "1700000001.000000", None);
        let done = GapResult {
            history_done: true,
            ..GapResult::default()
        };
        s.finish_gap(&job, s.generation(), &done, false);
        assert!(lock(&s.state).cursor.conversations.contains_key("D1"));
    }

    #[test]
    fn gaps_are_ranked_by_kind_first() {
        let s = shared();
        s.note("C1", "1700000009.000000", Some(ConversationKind::Channel));
        s.seed(
            vec![
                crate::Conversation {
                    id: "D1".into(),
                    name: None,
                    kind: ConversationKind::Direct,
                },
                crate::Conversation {
                    id: "C1".into(),
                    name: None,
                    kind: ConversationKind::Channel,
                },
            ],
            "1700000000.000000",
            false,
        );
        let order: Vec<String> = s.pending_gaps().into_iter().map(|g| g.channel).collect();
        assert_eq!(order, ["D1", "C1"]);
    }

    #[test]
    fn maintenance_hand_off_never_loses_a_request() {
        let s = shared();
        assert!(s.request_maintenance());
        assert!(!s.request_maintenance());
        lock(&s.state).fill_requested = true;
        assert!(s.maintenance_continue(), "a pending request keeps it going");
        assert!(s.take_fill());
        assert!(!s.maintenance_continue());
        assert!(
            s.request_maintenance(),
            "after it stopped, a new one may start"
        );
    }

    /// A sink that never accepts anything, like a socket whose peer stopped reading.
    struct Stuck;

    impl Sink<Message> for Stuck {
        type Error = String;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), String>> {
            Poll::Pending
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), String> {
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), String>> {
            Poll::Pending
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), String>> {
            Poll::Pending
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_send_that_cannot_complete_times_out() {
        let res = send_frame(&mut Stuck, Message::text("x"), Duration::from_secs(10)).await;
        assert_eq!(res, Err("sending to slack timed out".to_owned()));
    }

    #[tokio::test]
    async fn a_failed_send_does_not_count_as_delivered() {
        let s = Arc::new(shared());
        let (tx, rx) = mpsc::channel(4);
        let (sink, sink_rx) = mpsc::channel(1);
        drop(sink_rx);
        s.queued.store(1, Ordering::SeqCst);
        tx.send(
            json!({"type": "message", "channel": "C1", "channel_type": "channel",
                       "user": "U1", "text": "hi", "ts": "1700000000.000100"}),
        )
        .await
        .unwrap();
        process(s.clone(), rx, sink, Duration::from_secs(60)).await;
        assert!(!s.is_delivered("C1", "1700000000.000100"));
        assert!(
            !lock(&s.state).cursor.conversations.contains_key("C1"),
            "no progress recorded for it either"
        );
    }

    #[test]
    fn a_dead_processor_is_an_internal_error_unless_the_sink_closed() {
        let (sink, rx) = mpsc::channel(1);
        assert!(matches!(
            processor_gone(&sink),
            Ended::Fatal(FeedError::Internal(_))
        ));
        drop(rx);
        assert!(matches!(processor_gone(&sink), Ended::SinkClosed));
    }
}
