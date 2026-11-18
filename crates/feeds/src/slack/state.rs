//! The persisted progress of a [`SlackSource`](super::SlackSource), and Slack timestamp helpers.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Compares two Slack timestamps (`"1700000000.000100"`).
///
/// Slack ids are decimal strings with a fixed six-digit fraction, but comparing them as strings
/// breaks the moment the integer part changes length, and as `f64` they are only just precise
/// enough, so they are compared as integer pairs. A malformed timestamp sorts first, which makes
/// it look old and therefore never moves a cursor.
pub fn ts_cmp(a: &str, b: &str) -> Ordering {
    ts_key(a).cmp(&ts_key(b))
}

/// Whether `a` is strictly newer than `b`.
pub fn ts_gt(a: &str, b: &str) -> bool {
    ts_cmp(a, b) == Ordering::Greater
}

/// The later of two timestamps, as an owned string.
pub fn ts_max(a: &str, b: &str) -> String {
    if ts_gt(b, a) {
        b.to_owned()
    } else {
        a.to_owned()
    }
}

/// Parses a timestamp into `(seconds, microseconds)`; `None` when it is not a Slack timestamp.
pub fn ts_key(ts: &str) -> Option<(u64, u64)> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, ""));
    if secs.is_empty() || frac.len() > 6 {
        return None;
    }
    let secs: u64 = secs.parse().ok()?;
    let micros = if frac.is_empty() {
        0
    } else {
        // Right-pad so "1.5" and "1.500000" compare equal.
        let padded = format!("{frac:0<6}");
        padded.parse().ok()?
    };
    Some((secs, micros))
}

/// A timestamp as Unix seconds, for display and ordering of the output only.
pub fn ts_unix(ts: &str) -> Option<f64> {
    let (secs, micros) = ts_key(ts)?;
    Some(secs as f64 + micros as f64 / 1_000_000.0)
}

/// The timestamp one microsecond before `ts`, so that an exclusive `oldest` includes `ts`
/// itself. A malformed timestamp is returned unchanged.
pub fn ts_before(ts: &str) -> String {
    match ts_key(ts) {
        Some((0, 0)) | None => ts.to_owned(),
        Some((secs, 0)) => format!("{}.999999", secs - 1),
        Some((secs, micros)) => format!("{secs}.{:06}", micros - 1),
    }
}

/// Formats Unix seconds as a Slack timestamp, for the `oldest` of a time window.
pub fn ts_from_unix(secs: f64) -> String {
    let secs = secs.max(0.0);
    format!("{:.6}", secs)
}

/// Everything a [`SlackSource`](super::SlackSource) needs to resume without duplicates or gaps.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Cursor {
    /// Format version, so that a later layout can migrate or ignore an old one.
    #[serde(default = "Cursor::version")]
    pub v: u32,
    #[serde(default)]
    pub channels: BTreeMap<String, ChannelState>,
}

impl Cursor {
    pub const VERSION: u32 = 1;

    fn version() -> u32 {
        Self::VERSION
    }

    pub fn new() -> Self {
        Self {
            v: Self::VERSION,
            channels: BTreeMap::new(),
        }
    }
}

/// Progress in one channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChannelState {
    /// The newest top-level message ts already returned (or skipped as noise). Every channel
    /// message at or before it has been dealt with; `None` until the first poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<String>,
    /// Where watching began. Thread replies at or before it predate the feed and are not
    /// replayed when an old thread is discovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// The newest reply ts of any thread dropped from `threads`. A thread rediscovered after it
    /// was dropped resumes from here, so its old replies are not returned a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_floor: Option<String>,
    /// Unix seconds of the last thread sweep (see `SlackSource::with_thread_sweep_interval`).
    #[serde(default)]
    pub last_sweep: f64,
    /// Recently active threads, keyed by their root ts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub threads: BTreeMap<String, ThreadState>,
}

impl ChannelState {
    /// The reply ts a newly discovered thread starts after: nothing before the thread itself,
    /// before watching began, or before a thread we already let go of.
    pub fn thread_baseline(&self, thread_ts: &str) -> String {
        let mut base = thread_ts.to_owned();
        for bound in [&self.since, &self.thread_floor].into_iter().flatten() {
            base = ts_max(&base, bound);
        }
        base
    }

    /// Drops threads idle for longer than `window_secs`, then keeps only the `max` most recently
    /// active, raising `thread_floor` past every reply of the threads it drops.
    ///
    /// A pending thread is not dropped for being idle: its replies were noticed but not fetched
    /// yet (say, after a long outage), and dropping it would raise the floor over them.
    pub fn prune_threads(&mut self, now: f64, window_secs: f64, max: usize) {
        let cutoff = now - window_secs;
        let idle = |t: &ThreadState| t.last_activity < cutoff && !t.pending;
        let mut dropped: Vec<String> = self
            .threads
            .iter()
            .filter(|(_, t)| idle(t))
            .map(|(k, _)| k.clone())
            .collect();
        if self.threads.len() - dropped.len() > max {
            let mut live: Vec<(&String, &ThreadState)> =
                self.threads.iter().filter(|(_, t)| !idle(t)).collect();
            live.sort_by(|a, b| b.1.last_activity.total_cmp(&a.1.last_activity));
            dropped.extend(live.into_iter().skip(max).map(|(k, _)| k.clone()));
        }
        for key in dropped {
            if let Some(t) = self.threads.remove(&key) {
                let floor = match &self.thread_floor {
                    Some(f) => ts_max(f, &t.last_reply),
                    None => t.last_reply.clone(),
                };
                self.thread_floor = Some(floor);
            }
        }
    }
}

/// Progress in one thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThreadState {
    /// The newest reply ts already returned (or deliberately skipped); the next
    /// `conversations.replies` call asks for replies after it.
    pub last_reply: String,
    /// The newest reply ts the root message advertised (`latest_reply`), when it was last seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_hint: Option<String>,
    /// Unix seconds of the newest known activity, which decides which threads stay tracked.
    #[serde(default)]
    pub last_activity: f64,
    /// Unix seconds of the last `conversations.replies` call for this thread.
    #[serde(default)]
    pub last_checked: f64,
    /// Replies are known (or suspected) to be waiting. Persisted, so that a rate limit or a
    /// restart between noticing activity and fetching it does not lose the replies.
    #[serde(default)]
    pub pending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_compare_numerically() {
        assert!(ts_gt("1700000000.000100", "1700000000.000099"));
        assert!(ts_gt("10000000000.000000", "9999999999.999999"));
        assert_eq!(ts_cmp("1.5", "1.500000"), Ordering::Equal);
        assert!(ts_gt("1.000001", "garbage"));
        assert_eq!(ts_key("1.1234567"), None);
        assert_eq!(ts_unix("1700000000.500000"), Some(1_700_000_000.5));
        assert_eq!(ts_from_unix(12.25), "12.250000");
        assert_eq!(ts_before("1700000000.000100"), "1700000000.000099");
        assert_eq!(ts_before("1700000000.000000"), "1699999999.999999");
        assert_eq!(ts_before("x"), "x");
    }

    #[test]
    fn pruning_raises_the_floor() {
        let mut ch = ChannelState::default();
        for (ts, act) in [("1.000000", 10.0), ("2.000000", 20.0), ("3.000000", 30.0)] {
            ch.threads.insert(
                ts.into(),
                ThreadState {
                    last_reply: format!("{act}.000000"),
                    last_activity: act,
                    ..Default::default()
                },
            );
        }
        ch.prune_threads(35.0, 100.0, 2);
        assert_eq!(ch.threads.len(), 2);
        assert!(!ch.threads.contains_key("1.000000"));
        assert_eq!(ch.thread_floor.as_deref(), Some("10.000000"));
        // Idle past the window.
        ch.prune_threads(125.0, 100.0, 2);
        assert_eq!(ch.threads.len(), 1);
        assert_eq!(ch.thread_floor.as_deref(), Some("20.000000"));
        assert_eq!(ch.thread_baseline("5.000000"), "20.000000");
    }
}
