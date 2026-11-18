//! Showing the owner that an agent is working on what they wrote: a 👀 reaction on their
//! message at once, then "typing…" in the topic until the agent has answered.
//!
//! "Answered" is counted, not guessed. Every user message handed to a run (its first prompt,
//! then each message sent into it) makes Claude take one turn ending in one `result` line:
//! a message that arrives mid-turn is queued and gets its own turn and `result`, verified
//! against the real CLI (2.1.268, stream-json both ways). So "typing…" stops when the run has
//! a `result` for every message it was given, not at the first `result`, which may belong to
//! an earlier message. It also stops when the agent replies to the owner, waits on the owner
//! (a plan, a question, a permission), or its run ends.
//!
//! All of it is best effort: it never delays delivering anything, and a failure is logged.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::chat::{Chat, From};

/// How often "typing…" is sent again: Telegram shows it for about five seconds.
pub const TYPING_EVERY: Duration = Duration::from_millis(4500);
/// A turn that runs longer than this stops showing "typing…" anyway.
pub const TYPING_CAP: Duration = Duration::from_secs(600);
/// The reaction that says "received"; one Telegram allows as a reaction.
pub const RECEIVED: &str = "👀";

pub struct Working {
    chat: Arc<dyn Chat>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_generation: u64,
    /// Task -> the generation of the "typing…" loop shown for it. A newer message takes over
    /// (its loop has a newer generation, and the older loop ends).
    typing: HashMap<String, u64>,
    /// Task -> its current run and the user messages handed to that run without a `result`
    /// yet.
    in_flight: HashMap<String, (String, u32)>,
}

impl Working {
    pub fn new(chat: Arc<dyn Chat>) -> Arc<Self> {
        Arc::new(Self {
            chat,
            state: Default::default(),
        })
    }

    /// Reacts to the owner's `message` (if any) and shows "typing…" in `topic`, sent as `from`,
    /// until [`Self::stop`] or [`TYPING_CAP`]. The reaction always goes out; "typing…" only
    /// while the task is still being worked on.
    pub fn show(
        self: &Arc<Self>,
        message: Option<i64>,
        from: From,
        topic: Option<i64>,
        task_id: &str,
    ) {
        let generation = {
            let mut st = self.state.lock().unwrap();
            st.next_generation += 1;
            let g = st.next_generation;
            st.typing.insert(task_id.to_owned(), g);
            g
        };
        let (this, task_id) = (self.clone(), task_id.to_owned());
        tokio::spawn(async move {
            if let Some(m) = message
                && let Err(e) = this.chat.react(m, RECEIVED).await
            {
                tracing::debug!("reacting to the owner's message failed: {e:#}");
            }
            let deadline = tokio::time::Instant::now() + TYPING_CAP;
            // Checked before every tick, the first included: a message that was only queued, or
            // a turn that ended while reacting, shows no "typing…" at all.
            while this.current(&task_id, generation) && tokio::time::Instant::now() < deadline {
                if let Err(e) = this.chat.typing(from, topic).await {
                    tracing::debug!("showing typing failed: {e:#}");
                }
                tokio::time::sleep(TYPING_EVERY).await;
            }
            let mut st = this.state.lock().unwrap();
            if st.typing.get(&task_id) == Some(&generation) {
                st.typing.remove(&task_id);
            }
        });
    }

    fn current(&self, task_id: &str, generation: u64) -> bool {
        self.state.lock().unwrap().typing.get(task_id) == Some(&generation)
    }

    /// Stops "typing…" for the task (its loop ends at its next tick).
    pub fn stop(&self, task_id: &str) {
        self.state.lock().unwrap().typing.remove(task_id);
    }

    /// A user message was handed to the task's run `run_id` (a newer run starts the count over).
    pub fn message_sent(&self, task_id: &str, run_id: &str) {
        let mut st = self.state.lock().unwrap();
        let entry = st
            .in_flight
            .entry(task_id.to_owned())
            .or_insert_with(|| (run_id.to_owned(), 0));
        if entry.0 != run_id {
            *entry = (run_id.to_owned(), 0);
        }
        entry.1 += 1;
    }

    /// The run `run_id` finished a turn. Once it has answered every message it was given,
    /// "typing…" stops.
    pub fn turn_ended(&self, task_id: &str, run_id: &str) {
        let mut st = self.state.lock().unwrap();
        let done = match st.in_flight.get_mut(task_id) {
            Some((run, n)) if run == run_id => {
                *n = n.saturating_sub(1);
                *n == 0
            }
            // A run we did not count (sent before this process started): nothing to wait for.
            _ => true,
        };
        if done {
            st.in_flight.remove(task_id);
            st.typing.remove(task_id);
        }
    }

    /// The task's run `run_id` is gone (exited, lost, failed to start): nothing more will be
    /// answered from it. A late report about an older run leaves a newer run's count alone.
    pub fn run_ended(&self, task_id: &str, run_id: &str) {
        let mut st = self.state.lock().unwrap();
        if st
            .in_flight
            .get(task_id)
            .is_some_and(|(run, _)| run != run_id)
        {
            return;
        }
        st.in_flight.remove(task_id);
        st.typing.remove(task_id);
    }

    pub fn is_showing(&self, task_id: &str) -> bool {
        self.state.lock().unwrap().typing.contains_key(task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::fake::{FakeChat, Op};

    fn typing_count(chat: &FakeChat) -> usize {
        chat.ops
            .lock()
            .unwrap()
            .iter()
            .filter(|o| matches!(o, Op::Typing(..)))
            .count()
    }

    /// Lets the spawned loop run up to its next sleep.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reacts_then_types_until_every_message_is_answered() {
        let chat = Arc::new(FakeChat::default());
        let w = Working::new(chat.clone());
        w.message_sent("t", "r1");
        w.show(Some(7), From::Orchestrator, Some(5), "t");
        settle().await;
        {
            let ops = chat.ops.lock().unwrap();
            assert_eq!(ops[0], Op::React(7, RECEIVED.into()));
            assert_eq!(ops[1], Op::Typing(From::Orchestrator, Some(5)));
        }
        // A second message while the first is being worked on: the first `result` is not the
        // end.
        w.message_sent("t", "r1");
        tokio::time::advance(TYPING_EVERY).await;
        settle().await;
        assert_eq!(typing_count(&chat), 2);
        w.turn_ended("t", "r1");
        assert!(w.is_showing("t"), "one message still unanswered");
        tokio::time::advance(TYPING_EVERY).await;
        settle().await;
        assert_eq!(typing_count(&chat), 3);
        w.turn_ended("t", "r1");
        assert!(!w.is_showing("t"), "all answered");
        tokio::time::advance(TYPING_EVERY * 3).await;
        settle().await;
        assert_eq!(typing_count(&chat), 3, "the loop ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_message_takes_over_and_stop_ends_it() {
        let chat = Arc::new(FakeChat::default());
        let w = Working::new(chat.clone());
        w.show(Some(1), From::Orchestrator, None, "t");
        settle().await;
        w.show(Some(2), From::Bot(9), None, "t");
        settle().await;
        // Two loops ran once each; at the next tick only the newer one goes on.
        tokio::time::advance(TYPING_EVERY).await;
        settle().await;
        let typings: Vec<From> = chat
            .ops
            .lock()
            .unwrap()
            .iter()
            .filter_map(|o| match o {
                Op::Typing(f, _) => Some(*f),
                _ => None,
            })
            .collect();
        assert_eq!(typings, [From::Orchestrator, From::Bot(9), From::Bot(9)]);
        w.stop("t");
        tokio::time::advance(TYPING_EVERY * 2).await;
        settle().await;
        assert_eq!(typing_count(&chat), 3);
        // A result for a run it never counted ends it too (nothing to wait for).
        w.show(None, From::Orchestrator, None, "u");
        w.turn_ended("u", "other-run");
        assert!(!w.is_showing("u"));
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_before_the_first_tick_only_reacts() {
        let chat = Arc::new(FakeChat::default());
        let w = Working::new(chat.clone());
        // Queued, say: stopped before the loop ever ran.
        w.show(Some(3), From::Orchestrator, None, "t");
        w.stop("t");
        settle().await;
        tokio::time::advance(TYPING_EVERY * 2).await;
        settle().await;
        assert_eq!(*chat.ops.lock().unwrap(), [Op::React(3, RECEIVED.into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_counted_run_ending_stops_typing() {
        let chat = Arc::new(FakeChat::default());
        let w = Working::new(chat.clone());
        w.message_sent("t", "r2");
        w.show(None, From::Orchestrator, None, "t");
        // A late exit of the run before: the message is with r2, still being answered.
        w.run_ended("t", "r1");
        assert!(w.is_showing("t"));
        w.run_ended("t", "r2");
        assert!(!w.is_showing("t"));
        // No count at all (nothing was sent this process): any run's end stops it.
        w.show(None, From::Orchestrator, None, "u");
        w.run_ended("u", "r9");
        assert!(!w.is_showing("u"));
    }

    #[tokio::test(start_paused = true)]
    async fn typing_stops_at_the_cap() {
        let chat = Arc::new(FakeChat::default());
        let w = Working::new(chat.clone());
        w.show(None, From::Orchestrator, None, "t");
        settle().await;
        for _ in 0..200 {
            tokio::time::advance(TYPING_EVERY).await;
            settle().await;
        }
        let n = typing_count(&chat);
        let expected = (TYPING_CAP.as_millis() / TYPING_EVERY.as_millis()) as usize;
        assert!(
            (expected..=expected + 1).contains(&n),
            "{n} typings, about {expected}"
        );
        assert!(!w.is_showing("t"));
    }
}
