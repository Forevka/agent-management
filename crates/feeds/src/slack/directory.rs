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
//! The cache sits behind a `std::sync::Mutex` that is never held across an `.await`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::api::{Api, ConversationInfo, ConversationList, UserInfo};
use super::pace::{self, Gates};
use crate::{Author, Conversation, ConversationKind, Directory, FeedError};

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
    cache: Mutex<Cache>,
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
}

impl SlackDirectory {
    pub fn new(api: Api, gates: Arc<Gates>, refresh_every: Duration) -> Self {
        Self {
            api,
            gates,
            refresh_every,
            cache: Mutex::new(Cache::default()),
        }
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
        }
    }

    fn remember(&self, conv: Conversation) {
        let mut cache = self.cache();
        if cache.conversations.len() >= CACHE_CAP {
            cache.conversations.clear();
        }
        cache.conversations.insert(conv.id.clone(), conv);
    }

    /// Lists the owner's conversations with `users.conversations` and caches them. A failure
    /// starts or lengthens the back-off [`refresh_due`](Self::refresh_due) honours.
    pub async fn refresh(&self) -> Result<Vec<Conversation>, FeedError> {
        let res = self.list().await;
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
        res
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
        self.cache().membership =
            Some((Instant::now(), out.iter().map(|c| c.id.clone()).collect()));
        Ok(out)
    }
}

#[async_trait]
impl Directory for SlackDirectory {
    async fn conversation(&self, id: &str) -> Result<Conversation, FeedError> {
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
            },
            Err(e) => return Err(e),
        };
        self.remember(conv.clone());
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
        let cache = self.cache();
        let ids = cache
            .membership
            .as_ref()
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default();
        Ok(ids
            .iter()
            .map(|id| {
                cache
                    .conversations
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| Conversation {
                        id: id.clone(),
                        ..Conversation::default()
                    })
            })
            .collect())
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
    fn group_dm_names_are_readable() {
        assert_eq!(pretty_mpdm("mpdm-alice--bob--carol-1"), "alice, bob, carol");
        assert_eq!(pretty_mpdm("general"), "general");
    }
}
