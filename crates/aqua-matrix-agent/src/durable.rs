//! Durable per-target work-journal — the backbone of the delivery+inbox promise.
//!
//! Every accepted inbound message becomes a [`WorkItem`] that is persisted to
//! `<store>/work-journal.json` and only removed once its answer (or error) has
//! been **confirmed delivered**. This turns inbound into a durable inbox that is
//! processed until empty and survives a restart or a token-rotation delivery
//! failure:
//!
//! ```text
//!   Pending            -> the handler still owes an answer (replay on restart: re-run)
//!   ToDeliver { text } -> the handler produced an answer/error; it must be delivered
//!                         (redeliver on restart / next cycle with a fresh token — no re-run)
//!   (removed)          -> delivered and acknowledged by the homeserver  == done
//! ```
//!
//! The file is a single JSON array rewritten atomically (temp + rename) on every
//! mutation. Volume is tiny (one peer, a handful of in-flight messages), so the
//! whole-file rewrite is simpler and safer than append+compaction and never
//! leaves a torn file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::obligation::{
    Clock, Obligation, ObligationPolicy, Phase, PostKind, SweepAction, SystemClock,
};

/// State of a [`WorkItem`] in its lifecycle. `Pending` means the handler still
/// owes an answer (a restart must re-run it); `ToDeliver` means the answer/error
/// text exists and only delivery remains (a restart/next-cycle just resends it,
/// never re-running the handler).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum WorkState {
    Pending,
    ToDeliver { text: String },
}

/// One unit of durable work: an inbound message plus its delivery state.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WorkItem {
    /// Matrix event id of the inbound message — the stable, idempotent key.
    pub event_id: String,
    pub room_id: String,
    pub ts_ms: u64,
    pub msgtype: String,
    /// The inbound body (so a `Pending` replay can re-dispatch without the
    /// original sync event).
    pub body: String,
    pub state: WorkState,
    /// Who sent the message, recorded ONLY when the relay admitted someone other
    /// than its `target` in one of the handler's bound rooms
    /// (`MessageHandler::bound_rooms` in aqua-matrix-relay), so a replay can
    /// present the real author. `None` means `target` and is not written at all:
    /// a journal holding no such message is byte-identical to the format before
    /// this field existed, and `serde(default)` reads that format. There is no
    /// `deny_unknown_fields`, so an older binary still loads a journal that
    /// carries the field; it drops it on its next rewrite and replays the item
    /// as sent by `target`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Obligation state of the accepted peer message (deadline, attempts, posted
    /// event ids; see [`crate::obligation`]). `None` is a legacy item: the
    /// journal fills it in on load, so every item reaches the sweep with one.
    ///
    /// Compatibility, both directions. NEW reads OLD: absent field, defaults
    /// apply (`serde(default)`), the item becomes an open obligation received
    /// at its `ts_ms`. OLD reads NEW: the old `WorkItem` has no
    /// `deny_unknown_fields`, so a journal carrying this field still loads and
    /// `state` still means what it did (`Pending` = re-run, `ToDeliver` =
    /// resend); the old binary drops the field on its next rewrite, which only
    /// costs the obligation bookkeeping (attempts, posted ids): its resend then
    /// uses a random transaction id, so a rollback while an answer is posted
    /// but unproven can duplicate that one answer. `state` is deliberately NOT
    /// extended with new variants: an old binary could not parse them and
    /// would start with an EMPTY journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation: Option<Obligation>,
}

/// Durable, process-shared journal of [`WorkItem`]s for one target. Cloneable via
/// `Arc`; all mutations persist to disk before returning.
pub struct WorkJournal {
    path: PathBuf,
    items: Mutex<Vec<WorkItem>>,
    clock: Arc<dyn Clock>,
    policy: ObligationPolicy,
}

impl WorkJournal {
    const FILE_NAME: &'static str = "work-journal.json";

    /// Load the journal from `store_dir`, or start empty (and seed the file) when
    /// absent/unparsable. An unparsable file is logged and treated as empty
    /// rather than crashing the daemon — the worst case is replaying nothing,
    /// never a crash loop.
    pub fn load_or_empty(store_dir: &Path) -> Self {
        Self::load_with(store_dir, Arc::new(SystemClock), ObligationPolicy::default())
    }

    /// [`load_or_empty`](Self::load_or_empty) with an injected clock and policy
    /// (tests; a caller with other deadlines).
    pub fn load_with(store_dir: &Path, clock: Arc<dyn Clock>, policy: ObligationPolicy) -> Self {
        let path = store_dir.join(Self::FILE_NAME);
        let items = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Vec<WorkItem>>(&s) {
                Ok(v) => {
                    if !v.is_empty() {
                        tracing::info!(
                            "work-journal restored: {} item(s) from {}",
                            v.len(),
                            path.display()
                        );
                    }
                    v
                }
                Err(e) => {
                    tracing::warn!(
                        "work-journal at {} is unparsable ({e:#}); starting empty",
                        path.display()
                    );
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        let mut items = items;
        for it in items.iter_mut() {
            if it.obligation.is_none() {
                it.obligation = Some(Obligation::open(it.ts_ms, &policy));
            }
        }
        Self {
            path,
            items: Mutex::new(items),
            clock,
            policy,
        }
    }

    /// The policy (deadlines, fallback texts) this journal sweeps with.
    pub fn policy(&self) -> &ObligationPolicy {
        &self.policy
    }

    /// Atomic best-effort write: temp file + rename in the same dir, so a crash
    /// mid-write can never leave a torn journal (worst case the previous state
    /// survives). A write failure is a WARN, never fatal.
    fn persist(path: &Path, items: &[WorkItem]) {
        let tmp = path.with_extension("json.tmp");
        let res = serde_json::to_vec_pretty(items)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = res {
            tracing::warn!(
                "failed to persist work-journal to {}: {e:#}",
                path.display()
            );
        }
    }

    /// Append a new `Pending` item. Idempotent: a duplicate `event_id` (the relay
    /// re-seeing the same event, or a startup replay racing a fresh sync) is
    /// ignored. Returns `true` if it was newly enqueued.
    pub fn enqueue(&self, mut item: WorkItem) -> bool {
        let mut items = self.items.lock().unwrap();
        if items.iter().any(|i| i.event_id == item.event_id) {
            return false;
        }
        if item.obligation.is_none() {
            item.obligation = Some(Obligation::open(self.clock.now_ms(), &self.policy));
        }
        items.push(item);
        Self::persist(&self.path, &items);
        true
    }

    /// Transition an item to `ToDeliver { text }` (the handler produced an
    /// answer/error). No-op if the item is gone (already delivered).
    pub fn set_to_deliver(&self, event_id: &str, text: &str) {
        let mut items = self.items.lock().unwrap();
        if let Some(it) = items.iter_mut().find(|i| i.event_id == event_id) {
            it.state = WorkState::ToDeliver {
                text: text.to_string(),
            };
            if let Some(o) = it.obligation.as_mut() {
                o.turn_started_ms = None;
            }
            Self::persist(&self.path, &items);
        }
    }

    /// Remove an item — its answer/error has been confirmed delivered. Idempotent.
    pub fn mark_done(&self, event_id: &str) {
        let mut items = self.items.lock().unwrap();
        let before = items.len();
        items.retain(|i| i.event_id != event_id);
        if items.len() != before {
            Self::persist(&self.path, &items);
        }
    }

    /// Items still owed an answer (`Pending`) — replayed to the handler on startup.
    pub fn pending_work(&self) -> Vec<WorkItem> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.state == WorkState::Pending)
            .cloned()
            .collect()
    }

    /// Items whose answer/error is produced but not yet delivered (`ToDeliver`) —
    /// redelivered at each cycle start with a fresh token.
    pub fn pending_deliveries(&self) -> Vec<WorkItem> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .filter(|i| matches!(i.state, WorkState::ToDeliver { .. }))
            .cloned()
            .collect()
    }


    // ---------------------------------------------------------------------
    // Obligation API. One concept over the same items: an accepted peer
    // message is OWED a delivered answer or a delivered fallback; it leaves the
    // journal (closed) only on proof the agent's own post reached the room.
    // ---------------------------------------------------------------------

    /// Run `f` on the item `event_id`, persist if it returns `true`, and close
    /// (remove) the item when its obligation is settled. `None` if absent.
    fn with_item<R>(
        &self,
        event_id: &str,
        f: impl FnOnce(&mut WorkItem, u64) -> (R, bool),
    ) -> Option<R> {
        let now = self.clock.now_ms();
        let mut items = self.items.lock().unwrap();
        let idx = items.iter().position(|i| i.event_id == event_id)?;
        let (r, changed) = f(&mut items[idx], now);
        let settled = items[idx].obligation.as_ref().is_some_and(|o| o.settled());
        if settled {
            items.remove(idx);
        }
        if changed || settled {
            Self::persist(&self.path, &items);
        }
        Some(r)
    }

    /// A snapshot of one item.
    pub fn get(&self, event_id: &str) -> Option<WorkItem> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.event_id == event_id)
            .cloned()
    }

    /// Whether `event_id` is an obligation still open in this journal.
    pub fn contains(&self, event_id: &str) -> bool {
        self.get(event_id).is_some()
    }

    /// The obligation's phase (`None` once closed).
    pub fn phase(&self, event_id: &str) -> Option<Phase> {
        self.get(event_id)
            .and_then(|i| i.obligation.map(|o| o.phase()))
    }

    /// A turn for `event_id` starts now: counts an attempt and marks the turn
    /// running (in memory only: after a restart no turn runs). Returns `false`
    /// when the obligation is gone, already answered, or a turn already runs.
    pub fn begin_turn(&self, event_id: &str) -> bool {
        self.with_item(event_id, |it, now| {
            let o = it.obligation.get_or_insert_with(Obligation::default);
            if o.turn_started_ms.is_some() || !matches!(o.phase(), Phase::Open) {
                return (false, false);
            }
            o.turn_started_ms = Some(now);
            o.attempts += 1;
            (true, true)
        })
        .unwrap_or(false)
    }

    /// The running turn for `event_id` ended without an answer (it failed): the
    /// watchdog may now re-engage it.
    pub fn end_turn(&self, event_id: &str) {
        self.with_item(event_id, |it, _| {
            if let Some(o) = it.obligation.as_mut() {
                o.turn_started_ms = None;
            }
            ((), false)
        });
    }

    /// `Pending` items no turn is running for: what a (re)start or a cycle
    /// start must hand to the handler. Replaces "pending_work, once per
    /// process": a turn still running in THIS process is skipped, so replaying
    /// every cycle cannot double-run it. Unsupported-message obligations are
    /// excluded (they are answered by the fallback, not a turn).
    pub fn startable_work(&self) -> Vec<WorkItem> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.state == WorkState::Pending)
            .filter(|i| {
                i.obligation.as_ref().is_some_and(|o| {
                    o.turn_started_ms.is_none() && !o.unsupported && !o.fallback_due
                })
            })
            .cloned()
            .collect()
    }

    /// Mark `event_id` as a message no turn can answer (an unknown msgtype): the
    /// next sweep emits its fallback at once.
    pub fn mark_unsupported(&self, event_id: &str) {
        self.with_item(event_id, |it, _| {
            if let Some(o) = it.obligation.as_mut() {
                o.unsupported = true;
                o.fallback_due = true;
            }
            ((), true)
        });
    }

    /// Declare how many chunks the `kind` post of `event_id` has, so the
    /// obligation knows when every one is proven.
    pub(crate) fn declare_chunks(&self, event_id: &str, kind: PostKind, n: u32) {
        self.with_item(event_id, |it, _| {
            if let Some(o) = it.obligation.as_mut() {
                match kind {
                    PostKind::Answer => o.answer_chunks = n,
                    PostKind::Fallback => o.fallback_chunks = n,
                    PostKind::Liveness => {}
                }
            }
            ((), true)
        });
    }

    /// The event id already posted for `(event_id, ordinal)`: the FIRST
    /// duplicate check of every send (the transaction id is the second).
    pub fn posted(&self, event_id: &str, ordinal: u32) -> Option<String> {
        self.get(event_id)?
            .obligation?
            .posts
            .get(&ordinal)
            .map(|p| p.event_id.clone())
    }

    /// Whether `(event_id, ordinal)` is posted AND proven.
    pub(crate) fn is_proven(&self, event_id: &str, ordinal: u32) -> bool {
        self.get(event_id)
            .and_then(|i| i.obligation)
            .and_then(|o| o.posts.get(&ordinal).map(|p| p.proven))
            .unwrap_or(false)
    }

    /// Record that `(event_id, ordinal)` was posted as `posted_id`. `proven`
    /// when the post is already confirmed. Closes the obligation if that was
    /// the last unproven post. Returns whether the obligation is now closed.
    pub fn record_post(&self, event_id: &str, ordinal: u32, posted_id: &str, proven: bool) -> bool {
        self.with_item(event_id, |it, _| {
            let o = it.obligation.get_or_insert_with(Obligation::default);
            let p = o.posts.entry(ordinal).or_default();
            p.event_id = posted_id.to_string();
            p.proven |= proven;
            ((), true)
        });
        !self.contains(event_id)
    }

    /// Prove a recorded post (the event was fetched, or its echo came back).
    pub fn prove_post(&self, event_id: &str, ordinal: u32) {
        self.with_item(event_id, |it, _| {
            if let Some(p) = it
                .obligation
                .as_mut()
                .and_then(|o| o.posts.get_mut(&ordinal))
            {
                p.proven = true;
            }
            ((), true)
        });
    }

    /// Close on the own-event echo: the agent's sync timeline carried its own
    /// event `posted_id`, and/or an event whose `unsigned.transaction_id` is
    /// `txn_id` (which proves a post whose send response was lost). Returns the
    /// obligation event ids that closed.
    pub fn observe_own_event(&self, posted_id: &str, txn_id: Option<&str>) -> Vec<String> {
        let candidates: Vec<(String, Vec<(u32, bool)>)> = self
            .items
            .lock()
            .unwrap()
            .iter()
            .filter_map(|i| {
                let o = i.obligation.as_ref()?;
                let hits = o
                    .posts
                    .iter()
                    .map(|(ord, p)| (*ord, p.event_id == posted_id))
                    .chain(
                        txn_id
                            .filter(|t| t.starts_with(crate::obligation::TXN_PREFIX))
                            .into_iter()
                            .flat_map(|t| {
                                crate::obligation::candidate_ordinals()
                                    .filter(move |ord| {
                                        crate::obligation::txn_id(&i.event_id, *ord) == t
                                    })
                                    .map(|ord| (ord, true))
                                    .collect::<Vec<_>>()
                            }),
                    )
                    .filter(|(_, hit)| *hit)
                    .collect::<Vec<_>>();
                (!hits.is_empty()).then(|| (i.event_id.clone(), hits))
            })
            .collect();
        let mut closed = Vec::new();
        for (event_id, hits) in candidates {
            self.with_item(&event_id, |it, _| {
                if let Some(o) = it.obligation.as_mut() {
                    for (ord, _) in &hits {
                        let p = o.posts.entry(*ord).or_default();
                        if p.event_id.is_empty() {
                            p.event_id = posted_id.to_string();
                        }
                        p.proven = true;
                    }
                }
                ((), true)
            });
            if !self.contains(&event_id) {
                closed.push(event_id);
            }
        }
        closed
    }

    /// Unproven posts, as `(obligation event id, ordinal, posted event id)`:
    /// what a sweep re-verifies by fetching the event.
    pub fn unproven_posts(&self) -> Vec<(String, u32, String)> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .flat_map(|i| {
                i.obligation.iter().flat_map(move |o| {
                    o.posts
                        .iter()
                        .filter(|(ord, p)| !p.proven && PostKind::of_ordinal(**ord).closes())
                        .map(move |(ord, p)| (i.event_id.clone(), *ord, p.event_id.clone()))
                })
            })
            .collect()
    }

    /// The operator DM for `event_id`'s fallback went out.
    pub fn mark_operator_notified(&self, event_id: &str) {
        self.with_item(event_id, |it, _| {
            if let Some(o) = it.obligation.as_mut() {
                o.operator_notified = true;
            }
            ((), true)
        });
    }

    /// The watchdog sweep (run it every ~30 s). Pure bookkeeping against the
    /// journal's clock: it returns what the CALLER must do and never touches
    /// the network. Per open obligation, with `age` = now - received:
    ///
    /// * unsupported message: `Fallback` at once;
    /// * `age >= fallback_deadline` (10 min), or past the answer deadline with
    ///   no turn running and the re-engage budget (2) spent: `Fallback`
    ///   (repeated each sweep until the fallback is posted; the txn id makes the
    ///   repeat the same event; `notify_operator` until the DM went out);
    /// * past the answer deadline (5 min) with a turn running: `Liveness`,
    ///   once (it never closes anything);
    /// * past the answer deadline with no turn running: `Reengage` (counted
    ///   when emitted; the caller runs the turn and calls
    ///   [`begin_turn`](Self::begin_turn));
    /// * a post recorded but unproven for over a minute: `Verify`.
    pub fn sweep(&self) -> Vec<SweepAction> {
        let now = self.clock.now_ms();
        let mut items = self.items.lock().unwrap();
        let mut out = Vec::new();
        let mut changed = false;
        for it in items.iter_mut() {
            let (event_id, room_id) = (it.event_id.clone(), it.room_id.clone());
            let Some(o) = it.obligation.as_mut() else {
                continue;
            };
            let before = o.clone();
            if let Some(a) = o.sweep_one(now, &self.policy, &event_id, &room_id) {
                out.push(a);
            }
            changed |= *o != before;
        }
        if changed {
            Self::persist(&self.path, &items);
        }
        out
    }

    /// Open obligations and the age (ms) of the oldest, for the status file and
    /// the probe.
    pub fn open_stats(&self) -> (usize, u64) {
        let now = self.clock.now_ms();
        let items = self.items.lock().unwrap();
        let oldest = items
            .iter()
            .filter_map(|i| i.obligation.as_ref())
            .map(|o| now.saturating_sub(o.received_at_ms))
            .max()
            .unwrap_or(0);
        (items.len(), oldest)
    }

    /// Whether the inbox is fully drained.
    pub fn is_empty(&self) -> bool {
        self.items.lock().unwrap().is_empty()
    }

    /// Number of outstanding items (any state).
    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("aqua-journal-test-{tag}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn item(id: &str) -> WorkItem {
        WorkItem {
            event_id: id.to_string(),
            room_id: "!room:server".to_string(),
            ts_ms: 1,
            msgtype: "m.text".to_string(),
            body: format!("body of {id}"),
            state: WorkState::Pending,
            sender: None,
            obligation: None,
        }
    }

    #[test]
    fn enqueue_dedupes_by_event_id() {
        let dir = tmp_dir("dedupe");
        let j = WorkJournal::load_or_empty(&dir);
        assert!(j.enqueue(item("$a")));
        assert!(
            !j.enqueue(item("$a")),
            "duplicate event_id must not re-enqueue"
        );
        assert!(j.enqueue(item("$b")));
        assert_eq!(j.len(), 2);
    }

    #[test]
    fn lifecycle_pending_to_deliver_to_done() {
        let dir = tmp_dir("lifecycle");
        let j = WorkJournal::load_or_empty(&dir);
        j.enqueue(item("$a"));
        assert_eq!(j.pending_work().len(), 1);
        assert_eq!(j.pending_deliveries().len(), 0);

        j.set_to_deliver("$a", "the answer");
        assert_eq!(j.pending_work().len(), 0);
        let d = j.pending_deliveries();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].state,
            WorkState::ToDeliver {
                text: "the answer".into()
            }
        );

        j.mark_done("$a");
        assert!(j.is_empty());
    }

    #[test]
    fn survives_reload_replaying_unfinished_work() {
        let dir = tmp_dir("reload");
        {
            let j = WorkJournal::load_or_empty(&dir);
            j.enqueue(item("$pending"));
            j.enqueue(item("$answered"));
            j.set_to_deliver("$answered", "cached reply");
            j.enqueue(item("$done"));
            j.mark_done("$done");
        }
        // Simulate a restart: a fresh journal loads the same file.
        let j2 = WorkJournal::load_or_empty(&dir);
        assert_eq!(
            j2.len(),
            2,
            "done item must not survive; the other two must"
        );
        let pending = j2.pending_work();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, "$pending");
        assert_eq!(pending[0].body, "body of $pending");
        let deliveries = j2.pending_deliveries();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].event_id, "$answered");
        assert_eq!(
            deliveries[0].state,
            WorkState::ToDeliver {
                text: "cached reply".into()
            }
        );
    }

    /// The on-disk bytes of an item without a recorded sender, exactly as every
    /// binary before the `sender` field wrote them. Every live agent without
    /// bound rooms journals only such items, so this must never move.
    #[test]
    fn item_without_sender_keeps_the_pre_sender_format() {
        assert_eq!(
            serde_json::to_string(&item("$a")).unwrap(),
            r#"{"event_id":"$a","room_id":"!room:server","ts_ms":1,"msgtype":"m.text","body":"body of $a","state":"Pending"}"#
        );
        // Through the journal the item gains ONLY the `obligation` key (the
        // sender key stays absent): every other byte of the old format holds.
        let dir = tmp_dir("format");
        let j = WorkJournal::load_with(
            &dir,
            Arc::new(crate::ManualClock::new(5_000)),
            ObligationPolicy::default(),
        );
        j.enqueue(item("$a"));
        j.enqueue(item("$b"));
        j.set_to_deliver("$b", "the answer");
        let on_disk = std::fs::read_to_string(dir.join(WorkJournal::FILE_NAME)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        let keys = |i: usize| -> Vec<String> {
            let mut k: Vec<String> = v[i].as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let old = ["body", "event_id", "msgtype", "room_id", "state", "ts_ms"];
        for i in 0..2 {
            let mut want: Vec<String> = old.iter().map(|s| s.to_string()).collect();
            want.push("obligation".into());
            want.sort();
            assert_eq!(keys(i), want, "{on_disk}");
        }
        assert_eq!(v[0]["state"], "Pending");
        assert_eq!(v[1]["state"]["ToDeliver"]["text"], "the answer");
        assert_eq!(v[0]["obligation"]["received_at_ms"], 5_000);
        assert_eq!(v[0]["obligation"]["deadline_ms"], 5_000 + 300_000);
    }

    /// A recorded sender is written, survives a restart and comes back as-is.
    #[test]
    fn recorded_sender_round_trips_through_a_restart() {
        let dir = tmp_dir("sender");
        {
            let j = WorkJournal::load_or_empty(&dir);
            j.enqueue(WorkItem {
                sender: Some("@collaborator:server".to_string()),
                ..item("$from-collaborator")
            });
            j.enqueue(item("$from-target"));
        }
        let on_disk = std::fs::read_to_string(dir.join(WorkJournal::FILE_NAME)).unwrap();
        assert_eq!(on_disk.matches("\"sender\"").count(), 1, "{on_disk}");
        let pending = WorkJournal::load_or_empty(&dir).pending_work();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].sender.as_deref(), Some("@collaborator:server"));
        assert_eq!(pending[1].sender, None);
    }

    /// A journal written before the field existed (the file every live agent
    /// holds today) loads, every item without a sender.
    #[test]
    fn pre_sender_journal_loads_without_a_sender() {
        let dir = tmp_dir("old-format");
        std::fs::write(
            dir.join(WorkJournal::FILE_NAME),
            r#"[
  {
    "event_id": "$old",
    "room_id": "!room:server",
    "ts_ms": 1,
    "msgtype": "m.text",
    "body": "asked before the upgrade",
    "state": "Pending"
  },
  {
    "event_id": "$answered",
    "room_id": "!room:server",
    "ts_ms": 2,
    "msgtype": "m.text",
    "body": "q",
    "state": {
      "ToDeliver": {
        "text": "a"
      }
    }
  }
]"#,
        )
        .unwrap();
        let j = WorkJournal::load_or_empty(&dir);
        assert_eq!(j.len(), 2, "an old-format journal must not load empty");
        assert_eq!(j.pending_work()[0].sender, None);
        assert_eq!(j.pending_work()[0].body, "asked before the upgrade");
        assert_eq!(j.pending_deliveries()[0].sender, None);
    }

    /// Rollback: the item shape of a binary from before the field (same derive,
    /// no `deny_unknown_fields`) still reads a journal that carries a sender.
    #[test]
    fn pre_sender_binary_reads_a_journal_with_a_sender() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct PreSenderWorkItem {
            event_id: String,
            room_id: String,
            ts_ms: u64,
            msgtype: String,
            body: String,
            state: WorkState,
        }
        let json = serde_json::to_string(&vec![WorkItem {
            sender: Some("@collaborator:server".to_string()),
            ..item("$a")
        }])
        .unwrap();
        let old: Vec<PreSenderWorkItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].event_id, "$a");
    }

    #[test]
    fn mark_done_is_idempotent_and_unparsable_file_is_empty() {
        let dir = tmp_dir("idem");
        let j = WorkJournal::load_or_empty(&dir);
        j.enqueue(item("$a"));
        j.mark_done("$a");
        j.mark_done("$a"); // no panic, no-op
        assert!(j.is_empty());

        // Corrupt the file → loads empty, not a crash.
        std::fs::write(dir.join(WorkJournal::FILE_NAME), "{not json").unwrap();
        let j2 = WorkJournal::load_or_empty(&dir);
        assert!(j2.is_empty());
    }
}
