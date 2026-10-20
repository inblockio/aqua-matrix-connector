//! A seat's durable inbox: the messenger's `Inbox` (seq cursor that never goes
//! backwards, event-id dedupe, new/seen/processed states, atomic private
//! rewrite) plus a side table of the room messages that @mention the seat,
//! because `InboxEntry` has no field for it.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use aqua_messenger::inbox::{
    now_ms, Actor, Inbox, InboxEntry, InboxPolicy, NewEntry, Processed, Query, State,
};
use aqua_seat::paths::SeatPaths;
use aqua_seat::proto::{Item, Peer, Via, MAX_MESSAGE_BYTES};

/// Appended to a body cut at the size cap.
pub const TRUNC_MARK: &str = " [...truncated]";
/// The `room` label of roster-room entries (DM entries carry `None`).
pub const ROOM_LABEL: &str = "claude-seats";
/// Entries a seat keeps. Over the cap only read entries are evicted.
pub const MAX_ENTRIES: usize = 2_000;

pub fn policy() -> InboxPolicy {
    InboxPolicy {
        max_entries: MAX_ENTRIES,
        max_age: None::<Duration>,
        hard_cap: false,
        // No file sending in the spike: media is never recorded.
        accept_media: false,
        track_processed: true,
    }
}

pub struct SeatStore {
    inbox: Inbox,
    addressed: HashSet<String>,
    addressed_path: PathBuf,
    cursor_path: PathBuf,
    cursor: u64,
}

/// Cut `s` to at most `max` bytes on a char boundary; true when cut.
pub fn cap_body(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let keep = max.saturating_sub(TRUNC_MARK.len());
    let mut end = keep.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{}", &s[..end], TRUNC_MARK), true)
}

impl SeatStore {
    pub fn open(paths: &SeatPaths) -> Self {
        let addressed_path = paths.addressed();
        let addressed = std::fs::read_to_string(&addressed_path)
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let cursor_path = paths.listen_cursor();
        let cursor = std::fs::read_to_string(&cursor_path)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        Self {
            inbox: Inbox::load_with(paths.inbox(), policy(), now_ms()),
            addressed,
            addressed_path,
            cursor_path,
            cursor,
        }
    }

    /// Record an inbound message. Returns the new seq, `None` for a duplicate.
    pub fn ingest(&mut self, mut e: NewEntry, via: Via) -> Option<u64> {
        let (body, _) = cap_body(&e.body, MAX_MESSAGE_BYTES);
        e.body = body;
        e.room = (via != Via::Dm).then(|| ROOM_LABEL.to_string());
        let event_id = e.event_id.clone();
        let seq = self.inbox.ingest(e)?;
        if via == Via::Mention && self.addressed.insert(event_id.clone()) {
            // Written before anyone can read the entry, so a crash cannot
            // leave a mention that looks like chatter.
            if let Err(err) = self.append_addressed(&event_id) {
                tracing::error!("addressed.log append failed: {err}");
            }
        }
        Some(seq)
    }

    fn append_addressed(&self, event_id: &str) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.addressed_path)?;
        writeln!(f, "{event_id}")
    }

    pub fn contains(&self, event_id: &str) -> bool {
        self.inbox.contains(event_id)
    }

    pub fn via_of(&self, e: &InboxEntry) -> Via {
        if e.room.is_none() {
            Via::Dm
        } else if self.addressed.contains(&e.event_id) {
            Via::Mention
        } else {
            Via::Room
        }
    }

    pub fn is_direct(&self, e: &InboxEntry) -> bool {
        self.via_of(e).is_direct()
    }

    /// Entries for `seat_inbox`: with `since`, every entry after it; without,
    /// everything still open (new + seen). Oldest first, the newest `limit`.
    pub fn read(&self, since: Option<u64>, addressed_only: bool, limit: usize) -> Vec<InboxEntry> {
        let q = Query {
            since_seq: since,
            states: since.is_none().then(|| policy().open_states()),
            ..Default::default()
        };
        let mut v = self.inbox.query(&q);
        if addressed_only {
            v.retain(|e| self.is_direct(e));
        }
        if v.len() > limit {
            v.drain(..v.len() - limit);
        }
        v
    }

    /// Entries no session has been shown yet, after `after_seq`.
    pub fn new_entries(&self, after_seq: Option<u64>, addressed_only: bool) -> Vec<InboxEntry> {
        let q = Query {
            since_seq: after_seq,
            states: Some(vec![State::New]),
            ..Default::default()
        };
        let mut v = self.inbox.query(&q);
        if addressed_only {
            v.retain(|e| self.is_direct(e));
        }
        v
    }

    pub fn mark_seen(&mut self, seqs: &[u64], who: &Actor) {
        self.inbox.mark_seen(seqs, who);
    }

    pub fn mark_processed(&mut self, seqs: &[u64], who: &Actor, note: &str) -> Processed {
        // `track_processed` is always on for a seat.
        self.inbox
            .mark_processed(seqs, who, note)
            .unwrap_or_default()
    }

    /// Unread direct-address messages (state `new`).
    pub fn unread_direct(&self) -> usize {
        self.new_entries(None, true).len()
    }

    /// The oldest unread direct-address message a listener was not woken for.
    pub fn next_unannounced_direct(&self) -> Option<InboxEntry> {
        self.new_entries(Some(self.cursor), true).into_iter().next()
    }

    pub fn listen_cursor(&self) -> u64 {
        self.cursor
    }

    /// Remember that a listener was woken for everything up to `seq`.
    pub fn advance_listen_cursor(&mut self, seq: u64) {
        if seq > self.cursor {
            self.cursor = seq;
            if let Err(e) =
                aqua_seat::paths::write_private_atomic(&self.cursor_path, seq.to_string().as_bytes())
            {
                tracing::error!("listen.cursor write failed: {e:#}");
            }
        }
    }

    pub fn high_water(&self) -> u64 {
        self.inbox.high_water()
    }

    pub fn len(&self) -> usize {
        self.inbox.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inbox.is_empty()
    }

    /// Sessions' view of `entries`, the senders resolved against `peers`.
    pub fn items(&self, entries: &[InboxEntry], peers: &[Peer]) -> Vec<Item> {
        entries
            .iter()
            .map(|e| {
                let via = self.via_of(e);
                let peer = peers.iter().find(|p| p.mxid.eq_ignore_ascii_case(&e.sender));
                Item {
                    seq: e.seq,
                    event_id: e.event_id.clone(),
                    from_mxid: e.sender.clone(),
                    from_seat: peer.map(|p| p.seat.clone()),
                    from_did: peer.map(|p| p.did.clone()),
                    from_host: peer.map(|p| p.host.clone()),
                    ts_ms: e.ts_ms,
                    via,
                    direct: via.is_direct(),
                    body: e.body.clone(),
                    truncated: e.body.ends_with(TRUNC_MARK),
                    in_reply_to: e.in_reply_to.clone(),
                    state: e.state().as_str().to_string(),
                    processed_note: e.processed.as_ref().and_then(|m| m.note.clone()),
                }
            })
            .collect()
    }

    /// Event id for a `reply_to` that is an inbox seq.
    pub fn event_id_of_seq(&self, seq: u64) -> Option<String> {
        self.inbox.get(seq).map(|e| e.event_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(event: &str, sender: &str, body: &str) -> NewEntry {
        NewEntry {
            event_id: event.into(),
            room_id: "!r:x".into(),
            sender: sender.into(),
            sender_name: None,
            room: None,
            ts_ms: now_ms(),
            kind: "text".into(),
            body: body.into(),
            filename: None,
            media: None,
            in_reply_to: None,
            thread_root: None,
        }
    }

    fn actor() -> Actor {
        Actor {
            by: "test".into(),
            session: None,
        }
    }

    fn open(t: &tempfile::TempDir) -> SeatStore {
        let p = SeatPaths::new(t.path(), "seat-t").unwrap();
        p.ensure_dir().unwrap();
        SeatStore::open(&p)
    }

    #[test]
    fn dm_and_mention_are_direct_chatter_is_not() {
        let t = tempfile::tempdir().unwrap();
        let mut s = open(&t);
        let dm = s.ingest(entry("$1", "@a:x", "dm"), Via::Dm).unwrap();
        let men = s.ingest(entry("$2", "@a:x", "mention"), Via::Mention).unwrap();
        let room = s.ingest(entry("$3", "@a:x", "chatter"), Via::Room).unwrap();
        assert_eq!((dm, men, room), (1, 2, 3));
        let direct: Vec<u64> = s.read(None, true, 100).iter().map(|e| e.seq).collect();
        assert_eq!(direct, [1, 2]);
        assert_eq!(s.read(None, false, 100).len(), 3);
        assert_eq!(s.unread_direct(), 2);
    }

    #[test]
    fn duplicates_are_ignored_and_the_mention_flag_survives_a_reload() {
        let t = tempfile::tempdir().unwrap();
        {
            let mut s = open(&t);
            assert!(s.ingest(entry("$m", "@a:x", "hi"), Via::Mention).is_some());
            assert!(s.ingest(entry("$m", "@a:x", "hi"), Via::Mention).is_none());
        }
        let s = open(&t);
        let e = s.read(None, false, 10).remove(0);
        assert_eq!(s.via_of(&e), Via::Mention);
        assert!(s.is_direct(&e));
    }

    #[test]
    fn reading_marks_seen_and_processed_closes_the_entry() {
        let t = tempfile::tempdir().unwrap();
        let mut s = open(&t);
        s.ingest(entry("$1", "@a:x", "one"), Via::Dm);
        s.ingest(entry("$2", "@a:x", "two"), Via::Dm);
        assert_eq!(s.new_entries(None, false).len(), 2);
        s.mark_seen(&[1], &actor());
        assert_eq!(s.new_entries(None, false).len(), 1);
        // seen is still open, processed is not
        assert_eq!(s.read(None, false, 10).len(), 2);
        let r = s.mark_processed(&[1], &actor(), "done");
        assert_eq!(r.marked, [1]);
        let open_now: Vec<u64> = s.read(None, false, 10).iter().map(|e| e.seq).collect();
        assert_eq!(open_now, [2]);
        // `since` shows processed entries too, with the note
        let all = s.read(Some(0), false, 10);
        let items = s.items(&all, &[]);
        assert_eq!(items[0].state, "processed");
        assert_eq!(items[0].processed_note.as_deref(), Some("done"));
        // final: a second mark keeps the first
        assert_eq!(s.mark_processed(&[1], &actor(), "again").already.len(), 1);
    }

    #[test]
    fn listen_cursor_announces_each_direct_message_once() {
        let t = tempfile::tempdir().unwrap();
        let mut s = open(&t);
        s.ingest(entry("$1", "@a:x", "chatter"), Via::Room);
        assert!(s.next_unannounced_direct().is_none());
        s.ingest(entry("$2", "@a:x", "dm"), Via::Dm);
        let e = s.next_unannounced_direct().unwrap();
        assert_eq!(e.seq, 2);
        s.advance_listen_cursor(e.seq);
        assert!(s.next_unannounced_direct().is_none());
        // still unread for the nudge
        assert_eq!(s.unread_direct(), 1);
        s.ingest(entry("$3", "@a:x", "dm2"), Via::Dm);
        assert_eq!(s.next_unannounced_direct().unwrap().seq, 3);
        // cursor persists
        drop(s);
        assert_eq!(open(&t).listen_cursor(), 2);
    }

    #[test]
    fn items_resolve_the_sender_against_the_roster() {
        let t = tempfile::tempdir().unwrap();
        let mut s = open(&t);
        s.ingest(entry("$1", "@Known:x", "hi"), Via::Dm);
        s.ingest(entry("$2", "@stranger:x", "yo"), Via::Room);
        let peers = vec![Peer {
            seat: "alpha".into(),
            did: "did:key:z6MkA".into(),
            host: "h".into(),
            mxid: "@known:x".into(),
        }];
        let items = s.items(&s.read(None, false, 10), &peers);
        assert_eq!(items[0].from_seat.as_deref(), Some("alpha"));
        assert_eq!(items[0].from_host.as_deref(), Some("h"));
        assert!(items[0].direct && items[0].via == Via::Dm);
        assert_eq!(items[1].from_seat, None);
        assert_eq!(items[1].via, Via::Room);
    }

    #[test]
    fn long_bodies_are_cut_on_a_char_boundary() {
        let (b, cut) = cap_body("short", 100);
        assert!(!cut && b == "short");
        let s = "é".repeat(30_000);
        let (b, cut) = cap_body(&s, MAX_MESSAGE_BYTES);
        assert!(cut && b.len() <= MAX_MESSAGE_BYTES && b.ends_with(TRUNC_MARK));
        let t = tempfile::tempdir().unwrap();
        let mut st = open(&t);
        st.ingest(entry("$big", "@a:x", &s), Via::Dm);
        let items = st.items(&st.read(None, false, 1), &[]);
        assert!(items[0].truncated && items[0].body.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn media_is_not_recorded() {
        let t = tempfile::tempdir().unwrap();
        let mut s = open(&t);
        let mut e = entry("$f", "@a:x", "pic");
        e.kind = "image".into();
        assert!(s.ingest(e, Via::Dm).is_none());
    }
}
