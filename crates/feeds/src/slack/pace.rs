//! Request pacing for the Slack Web API, shared by everything a [`SlackSocket`] does with the
//! user token (gap checks, on-demand backfills, name lookups, membership refreshes).
//!
//! Slack rate limits per method tier and per workspace, so two callers pacing themselves
//! separately would together exceed the tier. One [`Gate`] per tier spaces requests for all of
//! them, and a rate limit reported by Slack blocks the whole gate until it is over.
//!
//! [`SlackSocket`]: super::SlackSocket

use std::sync::Mutex;
use std::time::Duration;

use serde::de::DeserializeOwned;
// tokio's clock, so that paused-time tests see the waits.
use tokio::time::Instant;

use super::api::Api;
use crate::FeedError;

/// Spaces requests of one rate tier.
#[derive(Debug)]
pub struct Gate {
    gap: Duration,
    /// When the next request may go. A tokio mutex, held while waiting, so waiters queue up in
    /// order instead of all waking at once.
    next: tokio::sync::Mutex<Option<Instant>>,
    /// Until when Slack asked us to stop. Never held across an `.await`.
    blocked_until: Mutex<Option<Instant>>,
}

impl Gate {
    pub fn new(gap: Duration) -> Self {
        Self {
            gap,
            next: tokio::sync::Mutex::new(None),
            blocked_until: Mutex::new(None),
        }
    }

    fn blocked(&self) -> Option<Instant> {
        *self.blocked_until.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits until a request may go, and reserves the slot.
    pub async fn wait(&self) {
        let mut next = self.next.lock().await;
        loop {
            let due = [*next, self.blocked()].into_iter().flatten().max();
            match due {
                Some(d) if d > Instant::now() => tokio::time::sleep_until(d).await,
                // A block may have been set while sleeping; look again.
                _ => break,
            }
        }
        *next = Some(Instant::now() + self.gap);
    }

    /// Stops every request of this tier for `d` (Slack's `Retry-After`).
    pub fn block(&self, d: Duration) {
        let until = Instant::now() + d;
        let mut b = self.blocked_until.lock().unwrap_or_else(|e| e.into_inner());
        if b.is_none_or(|old| old < until) {
            *b = Some(until);
        }
    }
}

/// The gates of the tiers the socket uses.
#[derive(Debug)]
pub struct Gates {
    /// `conversations.history`, `conversations.replies`, `conversations.info`,
    /// `users.conversations`: 50+ requests a minute.
    pub tier3: Gate,
    /// `users.info`: 100+ requests a minute.
    pub tier4: Gate,
}

impl Gates {
    /// `tier3_gap` apart for Tier 3, half that for Tier 4.
    pub fn new(tier3_gap: Duration) -> Self {
        Self {
            tier3: Gate::new(tier3_gap),
            tier4: Gate::new(tier3_gap / 2),
        }
    }
}

/// A read call through `gate`. A rate limit blocks the gate and is retried up to `retries`
/// times; after that it is returned.
pub async fn get<T: DeserializeOwned>(
    api: &Api,
    gate: &Gate,
    retries: usize,
    method: &str,
    query: &[(&str, &str)],
) -> Result<T, FeedError> {
    let mut tries = 0;
    loop {
        gate.wait().await;
        match api.get::<T>(method, query).await {
            Err(FeedError::RateLimited(d)) => {
                gate.block(d);
                if tries >= retries {
                    return Err(FeedError::RateLimited(d));
                }
                tries += 1;
                tracing::debug!(method, ?d, "slack rate limited; waiting");
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn requests_are_spaced_and_blocks_hold_everyone() {
        let g = Gate::new(Duration::from_secs(2));
        let t0 = Instant::now();
        g.wait().await;
        g.wait().await;
        assert!(t0.elapsed() >= Duration::from_secs(2));
        g.block(Duration::from_secs(30));
        let t1 = Instant::now();
        g.wait().await;
        assert!(t1.elapsed() >= Duration::from_secs(30));
    }
}
