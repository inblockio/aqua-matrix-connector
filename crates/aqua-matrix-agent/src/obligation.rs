//! Obligation state for the guaranteed-delivery contract.
//!
//! Every accepted peer message is an *obligation*: it is owed a delivered
//! answer or a delivered fallback, never silence and never two copies. The
//! state lives on the existing durable inbox journal ([`crate::WorkJournal`],
//! no second store); this module holds the types, the deterministic
//! transaction ids and the pure watchdog rules.
//!
//! ```text
//!   Open -> Answering -> Answered{event_id} -> (closed: proven, item removed)
//!                    \-> Fallback{event_id} -> (closed: proven, item removed)
//! ```
//!
//! "Closed" is the absence of the item, exactly as "delivered" always was, so a
//! journal that holds only closed work is empty and `open count == 0` is
//! `journal.len() == 0`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Source of "now" for deadlines. Injected so the watchdog is testable with a
/// fake clock and a caller (a poll loop) can share its own.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock you move by hand (tests, simulations).
#[derive(Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        Self(AtomicU64::new(start_ms))
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Deadlines and texts of the contract.
#[derive(Clone, Debug)]
pub struct ObligationPolicy {
    /// A turn running past this gets one liveness post; no turn running past it
    /// is re-engaged. Default 5 min.
    pub answer_deadline_ms: u64,
    /// Past this the fallback is posted whatever else is happening. Default 10 min.
    pub fallback_deadline_ms: u64,
    /// Re-engages before giving up on the turn (default 2).
    pub max_reengages: u32,
    /// How long a post may stay unproven before the sweep re-verifies it.
    pub verify_after_ms: u64,
    pub liveness_text: String,
    pub fallback_text: String,
    pub unsupported_text: String,
}

impl Default for ObligationPolicy {
    fn default() -> Self {
        Self {
            answer_deadline_ms: 5 * 60_000,
            fallback_deadline_ms: 10 * 60_000,
            max_reengages: 2,
            verify_after_ms: 60_000,
            liveness_text: "Still working on it, one moment.".into(),
            fallback_text: "Sorry, I could not answer this one, I have passed it to Tim.".into(),
            unsupported_text: "Sorry, I cannot read this kind of message yet. \
                               Please send it as text."
                .into(),
        }
    }
}

/// What a post is, which fixes its ordinal range (and so its transaction id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostKind {
    /// The answer; chunk `i` has ordinal `i`. Closes the obligation.
    Answer,
    /// The "still working" liveness post (ordinal 1000). Closes nothing.
    Liveness,
    /// The fallback; chunk `i` has ordinal `2000 + i`. Closes the obligation.
    Fallback,
}

pub(crate) const LIVENESS_ORDINAL: u32 = 1000;
pub(crate) const FALLBACK_BASE: u32 = 2000;
const CHUNK_LIMIT: u32 = 32;

impl PostKind {
    pub(crate) fn base(self) -> u32 {
        match self {
            PostKind::Answer => 0,
            PostKind::Liveness => LIVENESS_ORDINAL,
            PostKind::Fallback => FALLBACK_BASE,
        }
    }
    pub(crate) fn of_ordinal(ordinal: u32) -> Self {
        if ordinal >= FALLBACK_BASE {
            PostKind::Fallback
        } else if ordinal >= LIVENESS_ORDINAL {
            PostKind::Liveness
        } else {
            PostKind::Answer
        }
    }
    /// Whether proof of this post can close the obligation.
    pub(crate) fn closes(self) -> bool {
        !matches!(self, PostKind::Liveness)
    }
}

/// The ordinals an own-event echo is matched against.
pub(crate) fn candidate_ordinals() -> impl Iterator<Item = u32> {
    (0..CHUNK_LIMIT)
        .chain(std::iter::once(LIVENESS_ORDINAL))
        .chain(FALLBACK_BASE..FALLBACK_BASE + CHUNK_LIMIT)
}

pub(crate) const TXN_PREFIX: &str = "gd1.";

/// The Matrix transaction id of post `ordinal` of the obligation
/// `obligation_event_id`: a pure function of the pair, so it is identical across
/// retries, across a token rotation and across a restart. A retried send is the
/// same event (the homeserver dedupes on it).
pub fn txn_id(obligation_event_id: &str, ordinal: u32) -> String {
    let mut h = Sha256::new();
    h.update(obligation_event_id.as_bytes());
    h.update([0u8]);
    h.update(ordinal.to_be_bytes());
    format!("{TXN_PREFIX}{}", hex::encode(&h.finalize()[..16]))
}

/// The transaction id for a send keyed by the CALLER instead of an obligation
/// (`send_dm_reliable_keyed`): same function, key as the obligation, chunk
/// index as the ordinal.
pub fn keyed_txn_id(key: &str, chunk: u32) -> String {
    txn_id(key, chunk)
}

/// Where an obligation stands. `Closed` is not a phase: the item is removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Accepted, no turn running, nothing posted.
    Open,
    /// A turn is running.
    Answering,
    /// The answer is posted (last chunk's event id), proof pending.
    Answered { event_id: String },
    /// The fallback is posted, proof pending.
    Fallback { event_id: String },
}

/// One post of an obligation.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Post {
    /// The event id the homeserver gave (or the echo showed).
    pub event_id: String,
    /// Proven: fetched back from the room, or seen as the agent's own event.
    #[serde(default)]
    pub proven: bool,
}

/// Obligation state of one journal item. All fields default, so any subset
/// loads (see the compatibility note on `WorkItem::obligation`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Obligation {
    pub received_at_ms: u64,
    /// `received_at + answer deadline`.
    pub deadline_ms: u64,
    /// Turns started (the first run and every re-engage).
    #[serde(default)]
    pub attempts: u32,
    /// Re-engages the sweep has emitted.
    #[serde(default)]
    pub reengages: u32,
    #[serde(default)]
    pub liveness_sent: bool,
    /// A message no turn can answer (unknown msgtype): fallback at once.
    #[serde(default)]
    pub unsupported: bool,
    #[serde(default)]
    pub fallback_due: bool,
    #[serde(default)]
    pub operator_notified: bool,
    #[serde(default)]
    pub answer_chunks: u32,
    #[serde(default)]
    pub fallback_chunks: u32,
    /// ordinal -> post.
    #[serde(default)]
    pub posts: BTreeMap<u32, Post>,
    /// A turn is running since this time. Never persisted: after a restart no
    /// turn runs, which is what makes the next process re-engage.
    #[serde(skip)]
    pub turn_started_ms: Option<u64>,
}

/// What the watchdog asks its caller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SweepAction {
    /// Post the liveness text (once per obligation).
    Liveness { event_id: String, room_id: String },
    /// No turn runs past the answer deadline: run the turn again with the
    /// original message (`attempt` counts re-engages, 1-based), then call
    /// `begin_turn`.
    Reengage {
        event_id: String,
        room_id: String,
        attempt: u32,
    },
    /// Post the fallback (and DM the operator when `notify_operator`).
    Fallback {
        event_id: String,
        room_id: String,
        unsupported: bool,
        notify_operator: bool,
    },
    /// A post is recorded but unproven: fetch it, or wait for the echo.
    Verify { event_id: String, room_id: String },
}

impl Obligation {
    /// A fresh obligation received at `now_ms`.
    pub fn open(now_ms: u64, policy: &ObligationPolicy) -> Self {
        Self {
            received_at_ms: now_ms,
            deadline_ms: now_ms + policy.answer_deadline_ms,
            ..Self::default()
        }
    }

    pub fn phase(&self) -> Phase {
        let last = |kind: PostKind| {
            self.posts
                .iter()
                .rev()
                .find(|(ord, _)| PostKind::of_ordinal(**ord) == kind)
                .map(|(_, p)| p.event_id.clone())
        };
        if let Some(event_id) = last(PostKind::Fallback) {
            Phase::Fallback { event_id }
        } else if let Some(event_id) = last(PostKind::Answer) {
            Phase::Answered { event_id }
        } else if self.turn_started_ms.is_some() {
            Phase::Answering
        } else {
            Phase::Open
        }
    }

    fn all_proven(&self, kind: PostKind, chunks: u32) -> bool {
        chunks > 0
            && (0..chunks).all(|i| {
                self.posts
                    .get(&(kind.base() + i))
                    .is_some_and(|p| p.proven)
            })
    }

    /// Every chunk of the answer, or of the fallback, is posted and proven.
    pub fn settled(&self) -> bool {
        self.all_proven(PostKind::Answer, self.answer_chunks)
            || self.all_proven(PostKind::Fallback, self.fallback_chunks)
    }

    /// The watchdog rule for one obligation (see `WorkJournal::sweep`).
    pub(crate) fn sweep_one(
        &mut self,
        now: u64,
        policy: &ObligationPolicy,
        event_id: &str,
        room_id: &str,
    ) -> Option<SweepAction> {
        let (event_id, room_id) = (event_id.to_string(), room_id.to_string());
        let age = now.saturating_sub(self.received_at_ms);
        let has_posts = self.posts.keys().any(|o| PostKind::of_ordinal(*o).closes());
        if has_posts {
            // Posted, not yet proven: verify, never re-run or re-post.
            return (age >= policy.verify_after_ms).then_some(SweepAction::Verify {
                event_id,
                room_id,
            });
        }
        let running = self.turn_started_ms.is_some();
        let past_answer = now >= self.deadline_ms;
        let spent = !running && past_answer && self.reengages >= policy.max_reengages;
        if self.unsupported || self.fallback_due || age >= policy.fallback_deadline_ms || spent {
            self.fallback_due = true;
            return Some(SweepAction::Fallback {
                event_id,
                room_id,
                unsupported: self.unsupported,
                notify_operator: !self.unsupported && !self.operator_notified,
            });
        }
        if !past_answer {
            return None;
        }
        if running {
            if self.liveness_sent {
                return None;
            }
            self.liveness_sent = true;
            return Some(SweepAction::Liveness { event_id, room_id });
        }
        self.reengages += 1;
        Some(SweepAction::Reengage {
            event_id,
            room_id,
            attempt: self.reengages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txn_ids_are_deterministic_and_distinct() {
        assert_eq!(txn_id("$a", 0), txn_id("$a", 0));
        assert_ne!(txn_id("$a", 0), txn_id("$a", 1));
        assert_ne!(txn_id("$a", 0), txn_id("$b", 0));
        assert!(txn_id("$a", 0).starts_with(TXN_PREFIX));
    }
}
