//! Names for Slack conversations and people, looked up lazily and cached.
//!
//! Every lookup is a Web API call with the user token (`conversations.info` and
//! `users.conversations` are Tier 3, `users.info` Tier 4), so names are fetched once and kept.
//! The membership list from `users.conversations` is refreshed at most every
//! [`refresh_every`](SlackDirectory::new), since it is only needed for the web UI and summaries.
//!
//! Lookups go through the socket's shared rate gates, so they count against the same budget as
//! gap checks. A failed membership refresh is retried with an exponential back-off.
//!
//! The owner's notification settings ([`Attention`]) are read with the membership list and then
//! on their own, shorter interval (a mute should count within minutes), from two places:
//!
//! - `users.conversations` with `exclude_muted=true`, a documented argument: the conversations
//!   it leaves out are the muted ones. It needs no scope beyond the membership list's.
//! - `users.prefs.get`, which Slack's own clients use but which is **not documented** (no
//!   documented scope, response shape or token types; it may refuse a user token or change
//!   without notice). When it answers, its `muted_channels` and per-channel notification levels
//!   in `all_notifications_prefs` add "mentions only". When it fails, that is logged once and
//!   only the documented mute state is used.
//!
//! Either failing leaves the setting unknown (`None`); delivery never depends on it.
//!
//! The cache sits behind a `std::sync::Mutex` that is never held across an `.await`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::api::{Api, ConversationInfo, ConversationList, UserInfo};
use super::pace::{self, Gates};
use crate::{Attention, Author, Conversation, ConversationKind, Directory, FeedError};

/// More cached names than this and that cache starts over, so memory stays bounded in a
/// workspace with many thousands of members.
const CACHE_CAP: usize = 5000;
/// `users.conversations` pages fetched per refresh at most (200 conversations a page).
const MAX_LIST_PAGES: usize = 50;
/// Direct-message partners whose names one refresh looks up at most; the rest are looked up
/// when a message from them arrives.
const MAX_NAME_LOOKUPS_PER_REFRESH: usize = 50;
/// The first retry of a failed membership refresh; it doubles per failure, up to the refresh
/// interval.
const REFRESH_RETRY: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct SlackDirectory {
    api: Api,
    gates: Arc<Gates>,
    refresh_every: Duration,
    /// How often the notification settings are re-read; shorter than the membership refresh,
    /// since a mute is expected to count within minutes.
    attention_every: Duration,
    cache: Mutex<Cache>,
    /// Held while the membership list is read, so callers that ask at the same time (the
    /// maintenance task and a `conversations()` caller at startup, say) share one read instead
    /// of each paging through `users.conversations`. A tokio mutex: it is held across `.await`.
    refreshing: tokio::sync::Mutex<()>,
}

#[derive(Debug, Default)]
struct Cache {
    /// User id to display name; `None` when Slack does not know the user.
    users: HashMap<String, Option<String>>,
    conversations: HashMap<String, Conversation>,
    /// The ids `users.conversations` returned, and when.
    membership: Option<(Instant, Vec<String>)>,
    /// Consecutive failed refreshes, and when the next one may be tried.
    refresh_failures: u32,
    retry_at: Option<Instant>,
    /// Membership decided for one conversation (from `conversations.info` or a join or leave
    /// event), and when.
    decided: HashMap<String, (bool, Instant)>,
    /// The owner's notification settings.
    attention: AttentionCache,
}

#[derive(Debug, Default)]
struct AttentionCache {
    /// When the settings were last read (successfully or not).
    read_at: Option<Instant>,
    /// Muted conversations, from `exclude_muted`; `None` until that has worked once.
    muted: Option<HashSet<String>>,
    /// What `users.prefs.get` said, when it answered.
    prefs: Option<Prefs>,
    /// Whether each source's failure was logged already.
    warned_muted: bool,
    warned_prefs: bool,
}

/// The parts of Slack's (undocumented) user preferences that say how loud a conversation is.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Prefs {
    pub muted: HashSet<String>,
    /// Conversations with an explicit notification level.
    pub levels: HashMap<String, Attention>,
    /// The level every conversation without its own follows, when known.
    pub global: Option<Attention>,
}

impl Prefs {
    /// Reads `users.prefs.get`'s `prefs` object. `all_notifications_prefs` arrives as a
    /// JSON-encoded string (or, defensively, an object); `muted_channels` as a comma-separated
    /// string (or a list). Anything unreadable is skipped.
    pub fn parse(prefs: &Value) -> Self {
        let mut out = Prefs::default();
        match prefs.get("muted_channels") {
            Some(Value::String(s)) => out.muted.extend(
                s.split(',')
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned),
            ),
            Some(Value::Array(a)) => {
                out.muted
                    .extend(a.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            _ => {}
        }
        let all = match prefs.get("all_notifications_prefs") {
            Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
            Some(v) => v.clone(),
            None => Value::Null,
        };
        let global = all.get("global");
        let global_of = |device: &str| {
            global
                .and_then(|g| g.get(format!("global_{device}")))
                .and_then(Value::as_str)
                .and_then(level)
        };
        let (gd, gm) = (global_of("desktop"), global_of("mobile"));
        out.global = loudest([gd, gm]);
        if let Some(channels) = all.get("channels").and_then(Value::as_object) {
            for (id, c) in channels {
                if c.get("muted").and_then(Value::as_bool) == Some(true) {
                    out.muted.insert(id.clone());
                }
                let device =
                    |d: &str, fallback: Option<Attention>| match c.get(d).and_then(Value::as_str) {
                        Some("default") | None => fallback,
                        Some(l) => level(l),
                    };
                if let Some(l) = loudest([device("desktop", gd), device("mobile", gm)]) {
                    out.levels.insert(id.clone(), l);
                }
            }
        }
        out
    }
}

/// One device's notification level.
fn level(l: &str) -> Option<Attention> {
    match l {
        "nothing" | "none" | "never" => Some(Attention::Muted),
        "mention" | "mentions" => Some(Attention::MentionsOnly),
        "everything" | "all" => Some(Attention::Normal),
        _ => None,
    }
}

/// The level that notifies most: a conversation the owner hears about on any device is not
/// muted.
fn loudest(levels: [Option<Attention>; 2]) -> Option<Attention> {
    let rank = |a: Attention| match a {
        Attention::Muted => 0,
        Attention::MentionsOnly => 1,
        _ => 2,
    };
    levels.into_iter().flatten().max_by_key(|a| rank(*a))
}

/// How long a "not a member" answer is trusted. Short, so a channel joined without a join
/// event starts delivering within minutes.
const NOT_MEMBER_TTL: Duration = Duration::from_secs(10 * 60);
/// A membership list at least this old is read again before a conversation missing from it is
/// declared not a member.
const LIST_RECHECK: Duration = Duration::from_secs(60);

impl SlackDirectory {
    pub fn new(
        api: Api,
        gates: Arc<Gates>,
        refresh_every: Duration,
        attention_every: Duration,
    ) -> Self {
        Self {
            api,
            gates,
            refresh_every,
            attention_every,
            cache: Mutex::new(Cache::default()),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// Whether the notification settings are due to be re-read. Only once a membership list
    /// exists: the first read comes with it.
    pub fn attention_due(&self) -> bool {
        let cache = self.cache();
        cache.membership.is_some()
            && cache
                .attention
                .read_at
                .is_none_or(|at| at.elapsed() >= self.attention_every)
    }

    /// Re-reads the notification settings for the conversations of the cached membership list,
    /// when due.
    pub async fn refresh_attention_if_due(&self) {
        if !self.attention_due() {
            return;
        }
        let ids = self
            .cache()
            .membership
            .as_ref()
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default();
        self.refresh_attention(&ids).await;
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        // A panic while holding the lock leaves only a half-updated cache behind, which is
        // still a valid cache.
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A cached user name, without a request.
    pub fn cached_user(&self, id: &str) -> Option<String> {
        self.cache().users.get(id).cloned().flatten()
    }

    /// A cached conversation name, without a request.
    pub fn cached_conversation_name(&self, id: &str) -> Option<String> {
        self.cache()
            .conversations
            .get(id)
            .and_then(|c| c.name.clone())
    }

    /// Whether the membership list is older than the refresh interval (or missing), and no
    /// back-off after a failed refresh is running.
    pub fn refresh_due(&self) -> bool {
        let cache = self.cache();
        if cache.retry_at.is_some_and(|at| at > Instant::now()) {
            return false;
        }
        cache
            .membership
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= self.refresh_every)
    }

    /// A user's display name, looked up on first use. `Ok(None)` for a user Slack does not know;
    /// a failed lookup is not cached, so it is tried again next time.
    pub async fn user_name(&self, id: &str) -> Result<Option<String>, FeedError> {
        if let Some(name) = self.cache().users.get(id) {
            return Ok(name.clone());
        }
        // No retries: a name is not worth holding up the message it belongs to.
        let name = match pace::get::<UserInfo>(
            &self.api,
            &self.gates.tier4,
            0,
            "users.info",
            &[("user", id)],
        )
        .await
        {
            Ok(info) => info.user.and_then(|u| u.best_name()),
            Err(FeedError::Api(code)) if code == "user_not_found" => None,
            Err(e) => return Err(e),
        };
        let mut cache = self.cache();
        if cache.users.len() >= CACHE_CAP {
            cache.users.clear();
        }
        cache.users.insert(id.to_owned(), name.clone());
        Ok(name)
    }

    /// Turns Slack's conversation object into ours: a direct message is named after the other
    /// person, and a group DM's generated `mpdm-alice--bob-1` becomes `alice, bob`.
    async fn to_conversation(&self, id: &str, c: super::api::Conversation) -> Conversation {
        let kind = c.kind();
        let name = match kind {
            ConversationKind::Direct => match &c.user {
                Some(user) => self
                    .user_name(user)
                    .await
                    .ok()
                    .flatten()
                    .or(Some(user.clone())),
                None => None,
            },
            ConversationKind::Group => c.name.as_deref().map(pretty_mpdm),
            _ => c.name,
        };
        Conversation {
            id: id.to_owned(),
            name,
            kind,
            // Filled in from the settings cache whenever a conversation is handed out.
            attention: None,
        }
    }

    fn remember(&self, conv: Conversation) {
        let mut cache = self.cache();
        if cache.conversations.len() >= CACHE_CAP {
            cache.conversations.clear();
        }
        cache.conversations.insert(conv.id.clone(), conv);
    }

    /// The owner's notification setting for a conversation, from the cache; `None` when it is
    /// not known.
    pub fn attention(&self, id: &str) -> Option<Attention> {
        let cache = self.cache();
        let a = &cache.attention;
        let prefs = a.prefs.as_ref();
        if a.muted.as_ref().is_some_and(|m| m.contains(id))
            || prefs.is_some_and(|p| p.muted.contains(id))
        {
            return Some(Attention::Muted);
        }
        if let Some(p) = prefs {
            if let Some(l) = p.levels.get(id) {
                return Some(*l);
            }
            if let Some(g) = p.global {
                return Some(g);
            }
        }
        // Not muted, and no finer setting known.
        a.muted.as_ref().map(|_| Attention::Normal)
    }

    /// Re-reads the notification settings if they are older than their own interval. Never
    /// fails: what cannot be read stays as it was, and each kind of failure is logged once.
    async fn refresh_attention(&self, members: &[String]) {
        if self
            .cache()
            .attention
            .read_at
            .is_some_and(|at| at.elapsed() < self.attention_every)
        {
            return;
        }
        match self.list_ids(true).await {
            Ok(unmuted) => {
                let unmuted: HashSet<String> = unmuted.into_iter().collect();
                let muted = members
                    .iter()
                    .filter(|id| !unmuted.contains(*id))
                    .cloned()
                    .collect();
                self.cache().attention.muted = Some(muted);
            }
            Err(e) => {
                let first = !std::mem::replace(&mut self.cache().attention.warned_muted, true);
                if first {
                    tracing::warn!(error = %e, "slack mute settings unavailable; conversations go without them");
                }
            }
        }
        // Undocumented: see the module documentation.
        match pace::get::<Value>(&self.api, &self.gates.tier3, 0, "users.prefs.get", &[]).await {
            Ok(v) => {
                let prefs = Prefs::parse(v.get("prefs").unwrap_or(&Value::Null));
                self.cache().attention.prefs = Some(prefs);
            }
            Err(e) => {
                let first = !std::mem::replace(&mut self.cache().attention.warned_prefs, true);
                if first {
                    tracing::warn!(
                        error = %e,
                        "slack notification levels unavailable (users.prefs.get is undocumented); only mutes are known"
                    );
                }
            }
        }
        // Stamped once both reads are done, so a concurrent caller never mistakes a read in
        // progress for one that found nothing.
        self.cache().attention.read_at = Some(Instant::now());
    }

    /// Applies a `pref_change` event, should Slack send one (documented for RTM only).
    pub fn apply_pref(&self, name: &str, value: &Value) {
        if !matches!(name, "muted_channels" | "all_notifications_prefs") {
            return;
        }
        let update = Prefs::parse(&serde_json::json!({ name: value }));
        let mut cache = self.cache();
        let prefs = cache.attention.prefs.get_or_insert_with(Prefs::default);
        if name == "muted_channels" {
            prefs.muted = update.muted;
            // The documented list is older than this; the event is the newer word.
            cache.attention.muted = None;
        } else {
            prefs.levels = update.levels;
            prefs.global = update.global;
            prefs.muted.extend(update.muted);
        }
    }

    /// Records a membership change the owner made (a join or leave event).
    pub fn set_member(&self, id: &str, member: bool) {
        let mut cache = self.cache();
        if cache.decided.len() >= CACHE_CAP {
            cache.decided.clear();
        }
        cache
            .decided
            .insert(id.to_owned(), (member, Instant::now()));
        if !member && let Some((_, ids)) = cache.membership.as_mut() {
            ids.retain(|c| c != id);
        }
    }

    /// What is known about the owner's membership without a request.
    fn known_member(&self, id: &str) -> Option<bool> {
        let cache = self.cache();
        if let Some((member, at)) = cache.decided.get(id) {
            let ttl = if *member {
                self.refresh_every
            } else {
                NOT_MEMBER_TTL
            };
            if at.elapsed() < ttl {
                return Some(*member);
            }
        }
        match &cache.membership {
            Some((at, ids)) if at.elapsed() < self.refresh_every && ids.iter().any(|c| c == id) => {
                Some(true)
            }
            _ => None,
        }
    }

    /// Whether the owner is a member of a conversation. Direct and group messages always are
    /// (`kind_hint` from the event's `channel_type` says so without a request). Otherwise the
    /// cached answer, else `conversations.info`'s `is_member`, else the membership list. An
    /// error means the answer is unknown, and the caller must not deliver.
    pub async fn is_member(
        &self,
        id: &str,
        kind_hint: Option<ConversationKind>,
    ) -> Result<bool, FeedError> {
        if matches!(
            kind_hint,
            Some(ConversationKind::Direct | ConversationKind::Group)
        ) {
            return Ok(true);
        }
        if let Some(member) = self.known_member(id) {
            return Ok(member);
        }
        let info = match pace::get::<ConversationInfo>(
            &self.api,
            &self.gates.tier3,
            1,
            "conversations.info",
            &[("channel", id)],
        )
        .await
        {
            Ok(info) => info.channel.unwrap_or_default(),
            // A conversation the token cannot see is not one the owner is in.
            Err(FeedError::Api(code)) if code == "channel_not_found" => {
                self.set_member(id, false);
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let kind = info.kind();
        let stated = info.is_member;
        let conv = self.to_conversation(id, info).await;
        self.remember(conv);
        let member = match (kind, stated) {
            (ConversationKind::Direct | ConversationKind::Group, _) => true,
            (_, Some(m)) => m,
            // Slack did not say: decide from a fresh membership list.
            (_, None) => {
                let fresh = self
                    .cache()
                    .membership
                    .as_ref()
                    .is_some_and(|(at, _)| at.elapsed() < LIST_RECHECK);
                let ids = if fresh {
                    self.cache()
                        .membership
                        .as_ref()
                        .map(|(_, ids)| ids.clone())
                        .unwrap_or_default()
                } else {
                    self.refresh().await?.into_iter().map(|c| c.id).collect()
                };
                ids.iter().any(|c| c == id)
            }
        };
        self.set_member(id, member);
        Ok(member)
    }

    /// Lists the owner's conversations with `users.conversations` and caches them. A failure
    /// starts or lengthens the back-off [`refresh_due`](Self::refresh_due) honours.
    pub async fn refresh(&self) -> Result<Vec<Conversation>, FeedError> {
        let asked = Instant::now();
        let _one_at_a_time = self.refreshing.lock().await;
        // Another caller finished a read while this one waited: that list is as new as the
        // one this call would fetch.
        let fresh = self
            .cache()
            .membership
            .as_ref()
            .and_then(|(at, ids)| (*at >= asked).then(|| ids.clone()));
        if let Some(ids) = fresh {
            self.refresh_attention(&ids).await;
            return Ok(self.cached_list(&ids));
        }
        let res = self.list().await;
        {
            let mut cache = self.cache();
            match &res {
                Ok(_) => {
                    cache.refresh_failures = 0;
                    cache.retry_at = None;
                }
                Err(_) => {
                    cache.refresh_failures = cache.refresh_failures.saturating_add(1);
                    let wait = REFRESH_RETRY
                        .saturating_mul(1 << (cache.refresh_failures - 1).min(16))
                        .min(self.refresh_every.max(REFRESH_RETRY));
                    cache.retry_at = Some(Instant::now() + wait);
                }
            }
        }
        let list = res?;
        let ids: Vec<String> = list.iter().map(|c| c.id.clone()).collect();
        self.refresh_attention(&ids).await;
        Ok(list
            .into_iter()
            .map(|mut c| {
                c.attention = self.attention(&c.id);
                c
            })
            .collect())
    }

    /// The ids of the owner's conversations, all or (with `exclude_muted`) only those not
    /// muted.
    async fn list_ids(&self, exclude_muted: bool) -> Result<Vec<String>, FeedError> {
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut query = vec![
                ("types", "public_channel,private_channel,mpim,im"),
                ("exclude_archived", "true"),
                ("limit", "200"),
            ];
            if exclude_muted {
                query.push(("exclude_muted", "true"));
            }
            if let Some(c) = &cursor {
                query.push(("cursor", c.as_str()));
            }
            let page: ConversationList = pace::get(
                &self.api,
                &self.gates.tier3,
                2,
                "users.conversations",
                &query,
            )
            .await?;
            cursor = page.next_cursor();
            ids.extend(
                page.channels
                    .iter()
                    .filter_map(|c| c.get("id").and_then(Value::as_str))
                    .map(str::to_owned),
            );
            if cursor.is_none() {
                break;
            }
        }
        Ok(ids)
    }

    async fn list(&self) -> Result<Vec<Conversation>, FeedError> {
        let mut raw: Vec<super::api::Conversation> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut query = vec![
                ("types", "public_channel,private_channel,mpim,im"),
                ("exclude_archived", "true"),
                ("limit", "200"),
            ];
            if let Some(c) = &cursor {
                query.push(("cursor", c.as_str()));
            }
            let page: ConversationList = pace::get(
                &self.api,
                &self.gates.tier3,
                2,
                "users.conversations",
                &query,
            )
            .await?;
            cursor = page.next_cursor();
            raw.extend(
                page.channels
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok()),
            );
            if cursor.is_none() {
                break;
            }
        }
        let mut lookups = 0;
        let mut out = Vec::with_capacity(raw.len());
        for c in raw {
            let Some(id) = c.id.clone() else { continue };
            // Name direct messages from the cache, and look up only a bounded number of
            // partners; the rest get their name when they next write.
            let unknown_partner = c.kind() == ConversationKind::Direct
                && c.user
                    .as_ref()
                    .is_some_and(|u| !self.cache().users.contains_key(u));
            if unknown_partner && lookups >= MAX_NAME_LOOKUPS_PER_REFRESH {
                // Listed under the partner's id for now, and not cached, so that the first
                // message from them still looks the name up.
                out.push(Conversation {
                    id,
                    name: c.user.clone(),
                    kind: ConversationKind::Direct,
                    attention: None,
                });
                continue;
            }
            if unknown_partner {
                lookups += 1;
            }
            let conv = self.to_conversation(&id, c).await;
            self.remember(conv.clone());
            out.push(conv);
        }
        let mut cache = self.cache();
        // The list is the newer word: a conversation on it is one the owner is in now.
        for c in &out {
            cache.decided.remove(&c.id);
        }
        cache.membership = Some((Instant::now(), out.iter().map(|c| c.id.clone()).collect()));
        drop(cache);
        Ok(out)
    }
}

#[async_trait]
impl Directory for SlackDirectory {
    async fn conversation(&self, id: &str) -> Result<Conversation, FeedError> {
        let mut conv = self.lookup_conversation(id).await?;
        conv.attention = self.attention(id);
        Ok(conv)
    }

    async fn author(&self, id: &str) -> Result<Author, FeedError> {
        Ok(Author {
            id: id.to_owned(),
            name: self.user_name(id).await?,
        })
    }

    async fn conversations(&self) -> Result<Vec<Conversation>, FeedError> {
        if self.refresh_due() {
            return self.refresh().await;
        }
        let ids = self
            .cache()
            .membership
            .as_ref()
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default();
        // A refresh elsewhere may have listed the conversations but not yet read the settings.
        self.refresh_attention(&ids).await;
        Ok(self.cached_list(&ids))
    }
}

impl SlackDirectory {
    /// The conversations of a membership list, from the cache, with their settings.
    fn cached_list(&self, ids: &[String]) -> Vec<Conversation> {
        ids.iter()
            .map(|id| {
                let mut c = self
                    .cache()
                    .conversations
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| Conversation {
                        id: id.clone(),
                        ..Conversation::default()
                    });
                c.attention = self.attention(id);
                c
            })
            .collect()
    }

    /// One conversation's name and kind, cached after the first `conversations.info`.
    async fn lookup_conversation(&self, id: &str) -> Result<Conversation, FeedError> {
        if let Some(c) = self.cache().conversations.get(id) {
            return Ok(c.clone());
        }
        let conv = match pace::get::<ConversationInfo>(
            &self.api,
            &self.gates.tier3,
            0,
            "conversations.info",
            &[("channel", id)],
        )
        .await
        {
            Ok(info) => {
                self.to_conversation(id, info.channel.unwrap_or_default())
                    .await
            }
            Err(FeedError::Api(code)) if code == "channel_not_found" => Conversation {
                id: id.to_owned(),
                name: None,
                kind: ConversationKind::Other,
                attention: None,
            },
            Err(e) => return Err(e),
        };
        self.remember(conv.clone());
        Ok(conv)
    }
}

/// `mpdm-alice--bob--carol-1` to `alice, bob, carol`. Anything else is returned unchanged.
fn pretty_mpdm(name: &str) -> String {
    let Some(inner) = name.strip_prefix("mpdm-") else {
        return name.to_owned();
    };
    let inner = match inner.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => inner,
    };
    inner.split("--").collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_give_mutes_and_levels() {
        // As Slack's clients receive it: the per-channel levels as a JSON string.
        let all = serde_json::json!({
            "global": {"global_desktop": "everything", "global_mobile": "mention"},
            "channels": {
                "CQUIET": {"muted": true, "desktop": "default", "mobile": "default"},
                "CMENTION": {"desktop": "mention", "mobile": "mention"},
                "COFF": {"desktop": "nothing", "mobile": "nothing"},
                "CPHONE": {"desktop": "nothing", "mobile": "everything"},
                "CDEFAULT": {"desktop": "default", "mobile": "default"},
                "CODD": {"desktop": "sometimes"}
            }
        });
        let prefs = serde_json::json!({
            "muted_channels": "CGITLAB, CQUIET,",
            "all_notifications_prefs": all.to_string()
        });
        let p = Prefs::parse(&prefs);
        let muted: HashSet<String> = ["CGITLAB", "CQUIET"].map(String::from).into();
        assert_eq!(p.muted, muted);
        assert_eq!(p.levels.get("CMENTION"), Some(&Attention::MentionsOnly));
        assert_eq!(p.levels.get("COFF"), Some(&Attention::Muted));
        assert_eq!(
            p.levels.get("CPHONE"),
            Some(&Attention::Normal),
            "heard on any device is not muted"
        );
        assert_eq!(
            p.levels.get("CDEFAULT"),
            Some(&Attention::Normal),
            "default follows the global level"
        );
        assert_eq!(p.levels.get("CODD"), Some(&Attention::MentionsOnly));
        assert_eq!(p.global, Some(Attention::Normal));
        // Partial and odd shapes are read as far as they go.
        let p = Prefs::parse(
            &serde_json::json!({"muted_channels": ["C1"], "all_notifications_prefs": "not json"}),
        );
        assert!(p.muted.contains("C1"));
        assert!(p.levels.is_empty() && p.global.is_none());
        assert_eq!(Prefs::parse(&Value::Null), Prefs::default());
    }

    #[test]
    fn attention_combines_mutes_levels_and_what_is_known() {
        let dir = SlackDirectory::new(
            Api::new("x".into(), "http://127.0.0.1:9".into()),
            Arc::new(Gates::new(Duration::ZERO)),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert_eq!(dir.attention("C1"), None, "nothing known yet");
        dir.cache().attention.muted = Some(["CMUTED".to_owned()].into());
        assert_eq!(dir.attention("CMUTED"), Some(Attention::Muted));
        assert_eq!(dir.attention("C1"), Some(Attention::Normal));
        dir.cache().attention.prefs = Some(Prefs {
            levels: [("C1".to_owned(), Attention::MentionsOnly)].into(),
            ..Prefs::default()
        });
        assert_eq!(dir.attention("C1"), Some(Attention::MentionsOnly));
        dir.apply_pref("muted_channels", &serde_json::json!("C1"));
        assert_eq!(
            dir.attention("C1"),
            Some(Attention::Muted),
            "a pref_change applies"
        );
    }

    #[test]
    fn group_dm_names_are_readable() {
        assert_eq!(pretty_mpdm("mpdm-alice--bob--carol-1"), "alice, bob, carol");
        assert_eq!(pretty_mpdm("general"), "general");
    }
}
