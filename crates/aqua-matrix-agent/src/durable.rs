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

use serde::{Deserialize, Serialize};

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
}

/// Durable, process-shared journal of [`WorkItem`]s for one target. Cloneable via
/// `Arc`; all mutations persist to disk before returning.
pub struct WorkJournal {
    path: PathBuf,
    items: Mutex<Vec<WorkItem>>,
}

impl WorkJournal {
    const FILE_NAME: &'static str = "work-journal.json";

    /// Load the journal from `store_dir`, or start empty (and seed the file) when
    /// absent/unparsable. An unparsable file is logged and treated as empty
    /// rather than crashing the daemon — the worst case is replaying nothing,
    /// never a crash loop.
    pub fn load_or_empty(store_dir: &Path) -> Self {
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
        Self {
            path,
            items: Mutex::new(items),
        }
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
    pub fn enqueue(&self, item: WorkItem) -> bool {
        let mut items = self.items.lock().unwrap();
        if items.iter().any(|i| i.event_id == item.event_id) {
            return false;
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
        let dir = tmp_dir("format");
        let j = WorkJournal::load_or_empty(&dir);
        j.enqueue(item("$a"));
        j.enqueue(item("$b"));
        j.set_to_deliver("$b", "the answer");
        let on_disk = std::fs::read_to_string(dir.join(WorkJournal::FILE_NAME)).unwrap();
        assert_eq!(
            on_disk,
            r#"[
  {
    "event_id": "$a",
    "room_id": "!room:server",
    "ts_ms": 1,
    "msgtype": "m.text",
    "body": "body of $a",
    "state": "Pending"
  },
  {
    "event_id": "$b",
    "room_id": "!room:server",
    "ts_ms": 1,
    "msgtype": "m.text",
    "body": "body of $b",
    "state": {
      "ToDeliver": {
        "text": "the answer"
      }
    }
  }
]"#
        );
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
