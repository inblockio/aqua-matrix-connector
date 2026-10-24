//! Admission of an inbound peer message: the order of "journal it" and "advance
//! the watermark", and the event-id dedupe that replaces the bare timestamp
//! gate (guaranteed-delivery GD-1).
//!
//! Three silent drops lived in `dispatch`:
//!
//! * the watermark advanced BEFORE the journal entry existed, so a crash in
//!   between lost the message for good ([`admit`] enqueues first);
//! * `ts <= watermark` dropped any event that arrived out of order (a later
//!   event with an earlier `origin_server_ts`), though it was never seen;
//! * an unknown msgtype returned without a trace.
//!
//! The dedupe key is the event id. [`SeenLog`] remembers the ids admitted in a
//! bounded, persisted ring; each room keeps a floor (its watermark when first
//! seen, raised whenever an id ages out of the ring). An event is a duplicate
//! when its id is journalled or seen, or when its timestamp is at or below the
//! room's floor (everything the old watermark had already accounted for).
//! Between the floor and the watermark an unseen id is NEW, however old its
//! timestamp.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aqua_matrix_agent::{WorkItem, WorkJournal, WorkState};
use serde::{Deserialize, Serialize};

use super::Watermark;

/// Admitted ids remembered per process lifetime AND across restarts.
const SEEN_CAP: usize = 512;

#[derive(Serialize, Deserialize, Default)]
struct SeenInner {
    /// room id -> timestamps at or below this are already accounted for.
    floors: HashMap<String, u64>,
    /// (event id, room id, origin_server_ts), oldest first.
    ids: VecDeque<(String, String, u64)>,
}

/// Bounded, persisted record of admitted event ids (`<store>/inbound-seen.json`).
/// A dedupe cache, not obligation state: that lives in the [`WorkJournal`].
pub(crate) struct SeenLog {
    path: PathBuf,
    inner: Mutex<SeenInner>,
}

impl SeenLog {
    const FILE_NAME: &'static str = "inbound-seen.json";

    pub(crate) fn load(store_dir: &Path) -> Self {
        let path = store_dir.join(Self::FILE_NAME);
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    fn persist(&self, inner: &SeenInner) {
        let tmp = self.path.with_extension("json.tmp");
        let res = serde_json::to_vec(inner)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = res {
            tracing::warn!("failed to persist {}: {e:#}", self.path.display());
        }
    }

    /// Already accounted for? `watermark_now` seeds a room's floor on first sight.
    pub(crate) fn is_duplicate(
        &self,
        event_id: &str,
        room_id: &str,
        ts_ms: u64,
        watermark_now: u64,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner.floors.contains_key(room_id) {
            inner.floors.insert(room_id.to_string(), watermark_now);
            self.persist(&inner);
        }
        ts_ms <= inner.floors[room_id] || inner.ids.iter().any(|(id, ..)| id == event_id)
    }

    pub(crate) fn record(&self, event_id: &str, room_id: &str, ts_ms: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.ids.iter().any(|(id, ..)| id == event_id) {
            return;
        }
        inner
            .ids
            .push_back((event_id.to_string(), room_id.to_string(), ts_ms));
        while inner.ids.len() > SEEN_CAP {
            if let Some((_, room, ts)) = inner.ids.pop_front() {
                let floor = inner.floors.entry(room).or_default();
                *floor = (*floor).max(ts);
            }
        }
        self.persist(&inner);
    }
}

/// One inbound peer message offered for admission.
pub(crate) struct Candidate {
    pub event_id: String,
    pub room_id: String,
    pub ts_ms: u64,
    pub msgtype: String,
    pub body: String,
    pub sender: Option<String>,
    /// `false` for a msgtype no handler can read: the obligation ends in a
    /// polite fallback instead of vanishing.
    pub supported: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Journalled (an open obligation) and the watermark advanced: dispatch it.
    Accepted,
    /// Seen before (or below the floor): do nothing.
    Duplicate,
}

/// Admit `c`: duplicate check, journal, remember, THEN advance the watermark.
/// `advance` runs last on purpose: a crash anywhere before it leaves the
/// message journalled (replayed on restart) or unaccepted (re-delivered by
/// sync/backfill, since the watermark did not move), never accepted-and-lost.
pub(crate) fn admit(
    journal: &WorkJournal,
    seen: &SeenLog,
    watermark: &Watermark,
    c: Candidate,
    advance: impl FnOnce(),
) -> Admission {
    if journal.contains(&c.event_id)
        || seen.is_duplicate(&c.event_id, &c.room_id, c.ts_ms, watermark.get())
    {
        return Admission::Duplicate;
    }
    let event_id = c.event_id.clone();
    let newly = journal.enqueue(WorkItem {
        event_id: c.event_id.clone(),
        room_id: c.room_id.clone(),
        ts_ms: c.ts_ms,
        msgtype: c.msgtype,
        body: c.body,
        state: WorkState::Pending,
        sender: c.sender,
        obligation: None,
    });
    if !newly {
        return Admission::Duplicate;
    }
    if !c.supported {
        journal.mark_unsupported(&event_id);
    }
    seen.record(&c.event_id, &c.room_id, c.ts_ms);
    advance();
    Admission::Accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_matrix_agent::{ManualClock, ObligationPolicy, SweepAction};
    use std::sync::Arc;

    fn store(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aqua-admit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn journal(dir: &Path) -> WorkJournal {
        WorkJournal::load_with(dir, Arc::new(ManualClock::new(0)), ObligationPolicy::default())
    }

    fn cand(id: &str, ts: u64) -> Candidate {
        Candidate {
            event_id: id.into(),
            room_id: "!dm:s".into(),
            ts_ms: ts,
            msgtype: "m.text".into(),
            body: format!("body {id}"),
            sender: None,
            supported: true,
        }
    }

    /// The watermark file after `admit`'s steps, read from disk.
    fn wm(dir: &Path) -> Watermark {
        let path = dir.join("inbound-watermark");
        if !path.exists() {
            std::fs::write(&path, "1").unwrap();
        }
        Watermark::load_or_seed_file(path)
    }

    /// Crash ordering. The process dies after the journal write and before the
    /// watermark moves: the restart finds the message journalled (replayed) and
    /// the watermark unmoved. The old order (advance, then enqueue) is the
    /// mirror image: watermark past the message, nothing journalled, lost.
    #[test]
    fn a_crash_between_enqueue_and_advance_keeps_the_message() {
        let dir = store("crash");
        let j = journal(&dir);
        let seen = SeenLog::load(&dir);
        let w = wm(&dir);
        w.advance(1_000);

        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admit(&j, &seen, &w, cand("$a", 5_000), || {
                assert!(j.contains("$a"), "journalled BEFORE the watermark moves");
                panic!("process killed");
            })
        }));
        assert!(crashed.is_err());

        let j2 = journal(&dir);
        assert_eq!(j2.startable_work().len(), 1, "the message survives the crash");
        assert_eq!(wm(&dir).get(), 1_000, "and the watermark never passed it");
    }

    /// An event that arrives after a LATER one, with an earlier timestamp, is
    /// new (it used to be dropped as `ts <= watermark`), exactly once.
    #[test]
    fn out_of_order_timestamp_is_accepted_once() {
        let dir = store("ooo");
        let j = journal(&dir);
        let seen = SeenLog::load(&dir);
        let w = wm(&dir);
        w.advance(1_000);
        assert_eq!(admit(&j, &seen, &w, cand("$later", 3_000), || w.advance(3_000)), Admission::Accepted);
        assert_eq!(w.get(), 3_000);
        assert_eq!(admit(&j, &seen, &w, cand("$earlier", 2_000), || w.advance(2_000)), Admission::Accepted);
        assert_eq!(w.get(), 3_000, "the watermark never moves back");
        // Redelivery of either, and anything at or below the old floor.
        assert_eq!(admit(&j, &seen, &w, cand("$earlier", 2_000), || {}), Admission::Duplicate);
        assert_eq!(admit(&j, &seen, &w, cand("$ancient", 900), || {}), Admission::Duplicate);
        assert_eq!(j.len(), 2);
    }

    /// The dedupe survives a restart even after the obligations closed.
    #[test]
    fn closed_event_stays_a_duplicate_across_a_restart() {
        let dir = store("restart-dedupe");
        {
            let j = journal(&dir);
            let seen = SeenLog::load(&dir);
            let w = wm(&dir);
            w.advance(1_000);
            admit(&j, &seen, &w, cand("$a", 2_000), || w.advance(2_000));
            j.mark_done("$a");
        }
        let (j, seen, w) = (journal(&dir), SeenLog::load(&dir), wm(&dir));
        assert_eq!(admit(&j, &seen, &w, cand("$a", 2_000), || {}), Admission::Duplicate);
        assert!(j.is_empty());
    }

    #[test]
    fn an_aged_out_id_raises_the_floor_instead_of_being_forgotten() {
        let dir = store("floor");
        let seen = SeenLog::load(&dir);
        assert!(!seen.is_duplicate("$first", "!r", 10_000, 9_000));
        for i in 0..SEEN_CAP {
            seen.record(&format!("$e{i}"), "!r", 10_000 + i as u64);
        }
        seen.record("$first", "!r", 10_000);
        for i in 0..2 {
            seen.record(&format!("$more{i}"), "!r", 20_000 + i);
        }
        // "$first" aged out of the ring; its timestamp is now below the floor.
        assert!(seen.is_duplicate("$first", "!r", 10_000, 99_999));
    }

    /// An unknown msgtype opens an obligation that ends in a polite fallback:
    /// no turn is started for it, the sweep emits the fallback at once.
    #[test]
    fn unknown_msgtype_opens_an_obligation_that_ends_in_a_fallback() {
        let dir = store("unsupported");
        let j = journal(&dir);
        let seen = SeenLog::load(&dir);
        let w = wm(&dir);
        let c = Candidate { supported: false, msgtype: "m.location".into(), body: String::new(), ..cand("$loc", 4_000) };
        assert_eq!(admit(&j, &seen, &w, c, || w.advance(4_000)), Admission::Accepted);
        assert!(j.startable_work().is_empty());
        assert!(matches!(
            j.sweep()[..],
            [SweepAction::Fallback { unsupported: true, .. }]
        ));
    }
}
