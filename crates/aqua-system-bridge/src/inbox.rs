//! The durable inbox: inbound messages from allow-listed senders.
//!
//! Stored as one JSON object per line in `<state dir>/inbox.jsonl`, rewritten
//! atomically (temp file + rename, mode 600) on every change. Volume is tiny
//! (a handful of people replying to status messages), so a full rewrite is
//! simpler and safer than an append log with a side index.
//!
//! What the inbox may hold is an [`InboxPolicy`], set by the operator of a
//! bridge instance (daemon flags / environment). `InboxPolicy::default()` is the
//! long-standing behaviour: at most [`MAX_ENTRIES`] entries (only READ ones are
//! evicted over the cap), no age bound, media accepted. An instance can add:
//!
//! - an *age bound*: entries whose server timestamp is older than `max_age` are
//!   dropped, read or not, and an event that old is never ingested (so a
//!   backfill after a restart cannot resurrect it);
//! - a *hard cap*: over `max_entries`, the entries with the oldest server
//!   timestamp go first, read or not (stateless, so a re-offered evicted event
//!   is evicted again);
//! - *no media*: `file`/`image`/`audio`/`video` messages are refused at ingest
//!   and purged on load, so no media reference (for E2EE: the decryption key)
//!   is ever stored.
//!
//! The policy is enforced on load, on every ingest, and (by the daemon) before
//! every read and on a timer. `seq` never goes backwards: the high-water mark
//! is persisted next to the inbox (`inbox.seq`), so an inbox emptied by the age
//! bound does not restart numbering at 1 and make a session's `after_seq`
//! cursor miss new messages.
//!
//! Every entry has a handling [`State`], shared by all sessions on the host:
//! `new` (no session was shown it yet) -> `seen` (returned by a read) ->
//! `processed` (a session acted on it, or decided nothing needs doing, and
//! said so with a note). The state and its marks (who, which session, when)
//! are recorded state, kept on every instance. What an instance DOES with it
//! is policy: only with `track_processed` can entries be marked processed,
//! and only then does "open" (what a read without `since` returns) widen
//! from `new` to `new` + `seen`, so a message one session only looked at stays
//! open until some session handled it. Without it an instance behaves as
//! before the states (open = unread). On disk the `read` flag keeps mirroring
//! "not new", so a bridge binary from before the states still sees handled
//! messages as read after a rollback.
//!
//! Dedupe is by Matrix `event_id`, never by timestamp: the host clock on this
//! WSL box skews against the homeserver, so no host-clock watermark is used for
//! dedupe or delivery (memory `wsl-clock-skew-watermark-gotcha`). Timestamps
//! shown to sessions are the server's `origin_server_ts`. The one place the
//! host clock meets a server timestamp is the optional age bound, where a skew
//! of a few hours merely shifts the window by that much.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Default cap on held entries.
pub const MAX_ENTRIES: usize = 5_000;

/// What the inbox may hold. The `Default` keeps the behaviour from before
/// these knobs existed; an operator opts in to the stricter settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxPolicy {
    pub max_entries: usize,
    /// Drop entries older than this (server timestamp vs host clock), read or
    /// not. `None`: no age bound.
    pub max_age: Option<Duration>,
    /// Over `max_entries`: `false` evicts only READ entries (unread ones are
    /// kept until read), `true` evicts the oldest entries read or not.
    pub hard_cap: bool,
    /// `false`: media messages are refused at ingest and purged on load.
    pub accept_media: bool,
    /// Sessions mark entries `processed` (with a note) once handled, and the
    /// open view is `new` + `seen`. `false`: `mark_processed` is refused and
    /// the open view is `new` only (the behaviour from before the states).
    pub track_processed: bool,
}

impl Default for InboxPolicy {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_age: None,
            hard_cap: false,
            accept_media: true,
            track_processed: false,
        }
    }
}

impl InboxPolicy {
    /// The states still to be handled on this instance: what a read without
    /// `since` returns.
    pub fn open_states(&self) -> Vec<State> {
        if self.track_processed {
            vec![State::New, State::Seen]
        } else {
            vec![State::New]
        }
    }
}

/// [`Inbox::mark_processed`] on an instance without `track_processed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotTracked;

impl std::fmt::Display for NotTracked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this bridge does not track processed messages \
             (operator setting AQUA_SYSTEM_BRIDGE_INBOX_TRACK_PROCESSED is off)",
        )
    }
}

/// What one enforcement pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    pub removed: usize,
    /// Of `removed`, how many had never been shown to a session (`new`).
    pub unread: usize,
}

/// Where an inbox entry stands in its handling (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// No session has been shown it yet.
    New,
    /// Returned to a session by a read, not yet handled.
    Seen,
    /// A session acted on it (or decided nothing needs doing). Final.
    Processed,
}

impl State {
    pub const ALL: [State; 3] = [State::New, State::Seen, State::Processed];

    pub fn as_str(self) -> &'static str {
        match self {
            State::New => "new",
            State::Seen => "seen",
            State::Processed => "processed",
        }
    }
}

/// Who moved an entry into a state, when, and (for `processed`) why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mark {
    /// Origin label of the session (cwd basename and host, or a caller label).
    pub by: String,
    /// The Claude Code session id, when the session's MCP server knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Host clock, milliseconds since the epoch.
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The session acting on the inbox (a read that marks entries seen, or a
/// `mark_processed`). Labels are stored as given; the daemon sanitizes them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Actor {
    pub by: String,
    pub session: Option<String>,
}

impl Actor {
    fn mark(&self, at_ms: u64, note: Option<String>) -> Mark {
        Mark {
            by: self.by.clone(),
            session: self.session.clone(),
            at_ms,
            note,
        }
    }
}

/// Result of [`Inbox::mark_processed`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Processed {
    /// Seqs moved to `processed` by this call.
    pub marked: Vec<u64>,
    /// Seqs that were already processed, with the earlier (kept) mark.
    pub already: Vec<(u64, Mark)>,
    /// Requested seqs not (or no longer) in the inbox.
    pub missing: Vec<u64>,
}

/// Milliseconds since the epoch on the host clock.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True for the inbox `kind`s that carry media.
pub fn is_media_kind(kind: &str) -> bool {
    crate::attachments::MEDIA_KINDS.contains(&kind)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxEntry {
    /// Monotonic local sequence number (cursor for `since` / `after_seq`).
    pub seq: u64,
    pub event_id: String,
    pub room_id: String,
    /// Sender MXID as delivered by the homeserver.
    pub sender: String,
    /// Allow-list name of the sender at ingest time.
    #[serde(default)]
    pub sender_name: Option<String>,
    /// `[[rooms]]` name of the group room the message was posted in (at
    /// ingest time); `None` for a DM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Server timestamp (`origin_server_ts`), milliseconds since the epoch.
    pub ts_ms: u64,
    /// `text`, `notice`, `emote`, `file`, `image`, `audio`, `video`.
    pub kind: String,
    /// Message text, or the caption / filename of an attachment.
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Media reference of an attachment (fetched on demand with
    /// `fetch_attachment`). Never shown to sessions: for an E2EE attachment it
    /// holds the file's decryption key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaRef>,
    /// `state() != New`, kept on disk for binaries that predate the states.
    #[serde(default)]
    pub read: bool,
    /// The first session shown this entry. `None` with `read` set: read
    /// before the states existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<Mark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processed: Option<Mark>,
}

/// Where an attachment's bytes live, as recorded from the event at ingest
/// time. `source` is the Matrix `MediaSource` JSON (`{"url": ...}` or
/// `{"file": <EncryptedFile>}`), kept opaque here so this crate stays
/// Matrix-free.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaRef {
    pub source: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// Size declared by the sender (unverified until downloaded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl InboxEntry {
    /// True when the entry is an attachment `fetch_attachment` can download.
    pub fn has_attachment(&self) -> bool {
        crate::attachments::MEDIA_KINDS.contains(&self.kind.as_str())
    }

    pub fn state(&self) -> State {
        if self.processed.is_some() {
            State::Processed
        } else if self.read || self.seen.is_some() {
            State::Seen
        } else {
            State::New
        }
    }

    /// A copy safe to hand to a session process: the media reference keeps
    /// its declared metadata but loses the source (and so the file key).
    pub fn public(&self) -> Self {
        let mut e = self.clone();
        if let Some(m) = e.media.as_mut() {
            m.source = serde_json::Value::Null;
        }
        e
    }
}

/// A new inbound message, before it gets a `seq`.
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    pub sender_name: Option<String>,
    /// `[[rooms]]` name for a group-room message, `None` for a DM.
    pub room: Option<String>,
    pub ts_ms: u64,
    pub kind: String,
    pub body: String,
    pub filename: Option<String>,
    pub media: Option<MediaRef>,
}

/// Filter for [`Inbox::query`].
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Sender MXID (already resolved from a name), ASCII case-insensitive.
    /// Matches DIRECT messages only: a person's posts in group rooms are
    /// found with `room_id` (so a group-room message is never taken for a
    /// reply to a DM).
    pub sender: Option<String>,
    /// Only messages posted in this room (exact room id).
    pub room_id: Option<String>,
    pub since_seq: Option<u64>,
    pub since_ts_ms: Option<u64>,
    /// Only entries in one of these states; `None` = any state.
    pub states: Option<Vec<State>>,
    pub limit: Option<usize>,
}

pub struct Inbox {
    path: PathBuf,
    entries: Vec<InboxEntry>,
    /// Event ids already ingested, with their server timestamp. Evicted ids
    /// stay (so an in-process backfill cannot re-ingest them); with an age
    /// bound, ids older than it are forgotten (ingest refuses them anyway).
    seen: HashMap<String, u64>,
    next_seq: u64,
    /// The high-water mark last written to `inbox.seq`.
    persisted_seq: u64,
    policy: InboxPolicy,
}

impl Inbox {
    /// Load `path` (missing file = empty inbox) under the default policy.
    pub fn load(path: PathBuf) -> Self {
        Self::load_with(path, InboxPolicy::default(), now_ms())
    }

    /// Load `path` and enforce `policy` as of `now_ms` (what no longer fits is
    /// dropped and the file rewritten). Unparsable lines are skipped with a
    /// warning rather than discarding the whole inbox.
    pub fn load_with(path: PathBuf, policy: InboxPolicy, now_ms: u64) -> Self {
        let mut entries = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for (i, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<InboxEntry>(line) {
                    Ok(e) => entries.push(e),
                    Err(e) => tracing::warn!("inbox line {} unparsable, skipped: {e}", i + 1),
                }
            }
        }
        let persisted_seq = std::fs::read_to_string(seq_path(&path))
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let high = entries
            .iter()
            .map(|e| e.seq)
            .max()
            .unwrap_or(0)
            .max(persisted_seq);
        let seen = entries
            .iter()
            .map(|e| (e.event_id.clone(), e.ts_ms))
            .collect();
        let mut inbox = Self {
            path,
            entries,
            seen,
            next_seq: high + 1,
            persisted_seq,
            policy,
        };
        inbox.enforce_at(now_ms);
        inbox
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn policy(&self) -> InboxPolicy {
        self.policy
    }

    pub fn contains(&self, event_id: &str) -> bool {
        self.seen.contains_key(event_id)
    }

    /// The highest seq assigned so far (0 when none ever). A session can pass
    /// this as `after_seq` to wait only for replies that arrive after its send.
    pub fn high_water(&self) -> u64 {
        self.next_seq - 1
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries no session has been shown yet (`new`).
    pub fn unread_count(&self) -> usize {
        self.count(State::New)
    }

    pub fn count(&self, state: State) -> usize {
        self.entries.iter().filter(|e| e.state() == state).count()
    }

    /// Entries still to be handled under this instance's policy.
    pub fn open_count(&self) -> usize {
        let open = self.policy.open_states();
        self.entries
            .iter()
            .filter(|e| open.contains(&e.state()))
            .count()
    }

    /// Add a message unless its event id was already ingested, it is media on
    /// an instance that refuses media, or it is already outside the age bound.
    /// Returns the new entry's seq, or `None` when it was not kept. Persists on
    /// success.
    pub fn ingest(&mut self, new: NewEntry) -> Option<u64> {
        self.ingest_at(new, now_ms())
    }

    pub fn ingest_at(&mut self, new: NewEntry, now_ms: u64) -> Option<u64> {
        if self.seen.contains_key(&new.event_id)
            || (!self.policy.accept_media && is_media_kind(&new.kind))
            || new.ts_ms < self.cutoff(now_ms)
        {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.seen.insert(new.event_id.clone(), new.ts_ms);
        self.entries.push(InboxEntry {
            seq,
            event_id: new.event_id,
            room_id: new.room_id,
            sender: new.sender,
            sender_name: new.sender_name,
            room: new.room,
            ts_ms: new.ts_ms,
            kind: new.kind,
            body: new.body,
            filename: new.filename,
            media: new.media,
            read: false,
            seen: None,
            processed: None,
        });
        // With a hard cap the entry just added can itself be the oldest.
        let pruned = self.prune_at(now_ms);
        let kept = self.entries.iter().rev().any(|e| e.seq == seq);
        if kept || pruned.removed > 0 {
            self.persist();
        }
        log_pruned(pruned);
        kept.then_some(seq)
    }

    /// Drop whatever the policy no longer allows, as of now. Persists and logs
    /// when something was removed.
    pub fn enforce(&mut self) -> Pruned {
        self.enforce_at(now_ms())
    }

    pub fn enforce_at(&mut self, now_ms: u64) -> Pruned {
        let pruned = self.prune_at(now_ms);
        if pruned.removed > 0 {
            self.persist();
        }
        log_pruned(pruned);
        pruned
    }

    /// Oldest server timestamp still inside the age bound (0 = no bound).
    fn cutoff(&self, now_ms: u64) -> u64 {
        self.policy
            .max_age
            .map_or(0, |a| now_ms.saturating_sub(a.as_millis() as u64))
    }

    /// Remove media entries (if refused), entries past the age bound and, if
    /// still over the cap, the oldest entries by server timestamp (ties: lowest
    /// seq): without a hard cap only entries a session was shown, processed
    /// ones before seen (still open) ones; with a hard cap any, by age alone
    /// (so a re-offered evicted event is evicted again). In memory only; the
    /// caller persists.
    fn prune_at(&mut self, now_ms: u64) -> Pruned {
        let before = self.entries.len();
        let unread_before = self.unread_count();
        let cutoff = self.cutoff(now_ms);
        let accept_media = self.policy.accept_media;
        self.entries
            .retain(|e| (accept_media || !is_media_kind(&e.kind)) && e.ts_ms >= cutoff);
        let excess = self.entries.len().saturating_sub(self.policy.max_entries);
        if excess > 0 {
            let hard = self.policy.hard_cap;
            let mut order: Vec<(bool, u64, u64)> = self
                .entries
                .iter()
                .filter(|e| hard || e.state() != State::New)
                .map(|e| (!hard && e.state() == State::Seen, e.ts_ms, e.seq))
                .collect();
            order.sort_unstable();
            let drop: HashSet<u64> = order.iter().take(excess).map(|&(_, _, seq)| seq).collect();
            self.entries.retain(|e| !drop.contains(&e.seq));
        }
        if self.policy.max_age.is_some() {
            self.seen.retain(|_, ts| *ts >= cutoff);
        }
        Pruned {
            removed: before - self.entries.len(),
            unread: unread_before - self.unread_count(),
        }
    }

    /// Entries matching `q`, oldest first, capped at `q.limit` (newest kept).
    pub fn query(&self, q: &Query) -> Vec<InboxEntry> {
        let mut out: Vec<InboxEntry> = self
            .entries
            .iter()
            .filter(|e| {
                q.sender
                    .as_ref()
                    .is_none_or(|s| e.room.is_none() && e.sender.eq_ignore_ascii_case(s))
            })
            .filter(|e| q.room_id.as_ref().is_none_or(|r| &e.room_id == r))
            .filter(|e| q.since_seq.is_none_or(|s| e.seq > s))
            .filter(|e| q.since_ts_ms.is_none_or(|t| e.ts_ms > t))
            .filter(|e| q.states.as_ref().is_none_or(|s| s.contains(&e.state())))
            .cloned()
            .collect();
        out.sort_by_key(|e| e.seq);
        if let Some(limit) = q.limit {
            if out.len() > limit {
                out.drain(..out.len() - limit);
            }
        }
        out
    }

    /// The entry with this seq, if still in the inbox.
    pub fn get(&self, seq: u64) -> Option<&InboxEntry> {
        self.entries.iter().find(|e| e.seq == seq)
    }

    /// Move the given seqs from `new` to `seen`, recording `who` as the first
    /// session shown them. Entries already seen or processed are left alone.
    /// Persists if anything changed.
    pub fn mark_seen(&mut self, seqs: &[u64], who: &Actor) {
        let set: HashSet<u64> = seqs.iter().copied().collect();
        let now = now_ms();
        let mut changed = false;
        for e in self.entries.iter_mut() {
            if set.contains(&e.seq) && e.state() == State::New {
                e.read = true;
                e.seen = Some(who.mark(now, None));
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
    }

    /// Mark the given seqs `processed` by `who` with `note` (what was done, or
    /// why nothing needs doing). Final: an entry already processed keeps its
    /// first mark and is reported in `already`. Persists if anything changed.
    /// Refused unless the policy tracks processing.
    pub fn mark_processed(
        &mut self,
        seqs: &[u64],
        who: &Actor,
        note: &str,
    ) -> Result<Processed, NotTracked> {
        if !self.policy.track_processed {
            return Err(NotTracked);
        }
        let now = now_ms();
        let mut out = Processed::default();
        let mut wanted: Vec<u64> = seqs.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        for seq in wanted {
            let Some(e) = self.entries.iter_mut().find(|e| e.seq == seq) else {
                out.missing.push(seq);
                continue;
            };
            if let Some(m) = &e.processed {
                out.already.push((seq, m.clone()));
                continue;
            }
            e.read = true;
            e.processed = Some(who.mark(now, Some(note.to_string())));
            out.marked.push(seq);
        }
        if !out.marked.is_empty() {
            self.persist();
        }
        Ok(out)
    }

    /// Atomic rewrite: temp file (mode 600) + rename. The seq high-water mark
    /// is written first, so a crash can leave it ahead of the entries but
    /// never behind them.
    fn persist(&mut self) {
        if let Err(e) = self.try_persist() {
            tracing::error!(path = %self.path.display(), "inbox persist failed: {e:#}");
        }
    }

    fn try_persist(&mut self) -> std::io::Result<()> {
        let high = self.high_water();
        if high != self.persisted_seq {
            write_atomic(&seq_path(&self.path), high.to_string().as_bytes())?;
            self.persisted_seq = high;
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut f = open_private(&tmp)?;
            for e in &self.entries {
                let line = serde_json::to_string(e).map_err(std::io::Error::other)?;
                f.write_all(line.as_bytes())?;
                f.write_all(b"\n")?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

/// `<inbox>.seq`: the persisted seq high-water mark.
fn seq_path(inbox: &Path) -> PathBuf {
    inbox.with_extension("seq")
}

fn log_pruned(p: Pruned) {
    if p.removed > 0 {
        tracing::info!(
            removed = p.removed,
            unread = p.unread,
            "inbox: pruned entries outside the inbox policy"
        );
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = open_private(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Create/truncate a file readable only by its owner.
pub fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies on creation; force it for a pre-existing file too.
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asb-inbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("inbox.jsonl")
    }

    fn msg(id: &str, sender: &str, ts: u64) -> NewEntry {
        NewEntry {
            event_id: id.into(),
            room_id: "!r:x".into(),
            sender: sender.into(),
            sender_name: Some("tim".into()),
            room: None,
            ts_ms: ts,
            kind: "text".into(),
            body: format!("body {id}"),
            filename: None,
            media: None,
        }
    }

    #[test]
    fn public_copy_drops_the_media_source() {
        let mut m = msg("$f", "@t:x", 1);
        m.kind = "file".into();
        m.media = Some(MediaRef {
            source: serde_json::json!({"file": {"url": "mxc://x/y", "key": {"k": "SECRET"}}}),
            mimetype: Some("application/pdf".into()),
            size: Some(3),
        });
        let mut ib = Inbox::load(tmp_path("public"));
        let seq = ib.ingest(m).unwrap();
        let e = ib.get(seq).unwrap();
        assert!(e.media.as_ref().unwrap().source.get("file").is_some());
        let p = e.public();
        let out = serde_json::to_string(&p).unwrap();
        assert!(!out.contains("SECRET") && !out.contains("mxc://"), "{out}");
        assert_eq!(p.media.as_ref().unwrap().size, Some(3));
        assert!(p.has_attachment());
    }

    #[test]
    fn ingest_dedupes_by_event_id_and_persists() {
        let p = tmp_path("dedupe");
        let mut ib = Inbox::load(p.clone());
        assert_eq!(ib.ingest(msg("$a", "@t:x", 10)), Some(1));
        assert_eq!(ib.ingest(msg("$a", "@t:x", 10)), None);
        assert_eq!(ib.ingest(msg("$b", "@t:x", 5)), Some(2));
        let reloaded = Inbox::load(p.clone());
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains("$a"));
        assert_eq!(reloaded.high_water(), 2);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn query_filters_and_mark_seen() {
        let p = tmp_path("query");
        let mut ib = Inbox::load(p.clone());
        ib.ingest(msg("$1", "@t:x", 100));
        ib.ingest(msg("$2", "@k:x", 200));
        ib.ingest(msg("$3", "@T:x", 300));
        let q = Query {
            sender: Some("@t:x".into()),
            states: Some(vec![State::New]),
            ..Default::default()
        };
        let got = ib.query(&q);
        assert_eq!(got.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 3]);
        ib.mark_seen(&[1], &Actor::default());
        assert_eq!(ib.query(&q).len(), 1);
        assert_eq!(ib.unread_count(), 2);
        let since = Query {
            since_seq: Some(1),
            ..Default::default()
        };
        assert_eq!(ib.query(&since).len(), 2);
        let ts = Query {
            since_ts_ms: Some(150),
            ..Default::default()
        };
        assert_eq!(ib.query(&ts).len(), 2);
        let lim = Query {
            limit: Some(1),
            ..Default::default()
        };
        assert_eq!(ib.query(&lim)[0].seq, 3);
        // read state survives reload
        let re = Inbox::load(p);
        assert_eq!(re.unread_count(), 2);
    }

    #[test]
    fn room_filter_and_dm_only_sender_filter() {
        let p = tmp_path("rooms");
        let mut ib = Inbox::load(p.clone());
        ib.ingest(msg("$dm", "@t:x", 1));
        let mut g = msg("$grp", "@t:x", 2);
        g.room_id = "!g:x".into();
        g.room = Some("daily-updates".into());
        ib.ingest(g);
        let by_room = ib.query(&Query {
            room_id: Some("!g:x".into()),
            ..Default::default()
        });
        assert_eq!(by_room.len(), 1);
        assert_eq!(by_room[0].room.as_deref(), Some("daily-updates"));
        // a person filter matches their DMs, never their group-room posts
        let by_sender = ib.query(&Query {
            sender: Some("@t:x".into()),
            ..Default::default()
        });
        assert_eq!(
            by_sender
                .iter()
                .map(|e| e.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["$dm"]
        );
        // the room field survives a reload; old lines without it load as DMs
        let re = Inbox::load(p);
        assert_eq!(re.get(2).unwrap().room.as_deref(), Some("daily-updates"));
        assert!(re.get(1).unwrap().room.is_none());
    }

    // ---- policy knobs (the defaults are exercised by the tests above) ----

    /// A fixed "now" so these tests never depend on the host clock.
    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 3_600_000;

    /// A text message `age_ms` old at [`NOW`].
    fn aged(id: &str, age_ms: u64) -> NewEntry {
        let mut m = msg(id, "@t:x", NOW - age_ms);
        m.body = format!("body {id}");
        m
    }

    fn media(id: &str, kind: &str) -> NewEntry {
        let mut m = aged(id, 10);
        m.kind = kind.into();
        m.filename = Some("a.bin".into());
        m.media = Some(MediaRef {
            source: serde_json::json!({"file": {"url": "mxc://x/y", "key": {"k": "SECRETKEY"}}}),
            mimetype: None,
            size: Some(1),
        });
        m
    }

    fn with(policy: InboxPolicy, tag: &str) -> (Inbox, PathBuf) {
        let p = tmp_path(tag);
        (Inbox::load_with(p.clone(), policy, NOW), p)
    }

    #[test]
    fn default_policy_keeps_the_long_standing_behaviour() {
        let d = InboxPolicy::default();
        assert_eq!(d.max_entries, 5_000);
        assert_eq!(d.max_age, None);
        assert!(!d.hard_cap);
        assert!(d.accept_media);
        // no age bound: a year-old message is still taken; media is accepted
        let (mut ib, _) = with(d, "defaults");
        assert!(ib.ingest_at(aged("$old", 365 * 24 * HOUR), NOW).is_some());
        assert!(ib.ingest_at(media("$img", "image"), NOW).is_some());
        assert_eq!(ib.enforce_at(NOW + 400 * 24 * HOUR), Pruned::default());
        assert_eq!(ib.len(), 2);
    }

    #[test]
    fn soft_cap_evicts_read_entries_only() {
        let policy = InboxPolicy {
            max_entries: 2,
            ..InboxPolicy::default()
        };
        let (mut ib, _) = with(policy, "soft");
        ib.ingest_at(aged("$1", 3000), NOW);
        ib.ingest_at(aged("$2", 2000), NOW);
        ib.mark_seen(&[1], &Actor::default());
        assert_eq!(ib.ingest_at(aged("$3", 1000), NOW), Some(3));
        // the read one went; both unread ones stay
        assert_eq!(
            ib.query(&Query::default())
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        // over the cap with only unread entries: nothing is evicted
        assert_eq!(ib.ingest_at(aged("$4", 500), NOW), Some(4));
        assert_eq!(ib.len(), 3);
    }

    #[test]
    fn hard_cap_evicts_oldest_by_server_time_read_or_not() {
        let policy = InboxPolicy {
            max_entries: 3,
            hard_cap: true,
            ..InboxPolicy::default()
        };
        let (mut ib, p) = with(policy, "hard");
        for (i, id) in ["$1", "$2", "$3"].iter().enumerate() {
            ib.ingest_at(aged(id, (10 - i as u64) * 1000), NOW);
        }
        ib.mark_seen(&[1], &Actor::default());
        assert_eq!(ib.ingest_at(aged("$4", 500), NOW), Some(4));
        assert_eq!(ib.len(), 3);
        let held = |ib: &Inbox| {
            ib.query(&Query::default())
                .into_iter()
                .map(|e| e.event_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(held(&ib), vec!["$2", "$3", "$4"]);
        // the cap holds for UNREAD entries too: the oldest unread goes next
        assert_eq!(ib.ingest_at(aged("$5", 400), NOW), Some(5));
        assert_eq!(held(&ib), vec!["$3", "$4", "$5"]);
        // an event older than everything held is not kept
        assert_eq!(ib.ingest_at(aged("$0", 20_000), NOW), None);
        assert_eq!(ib.len(), 3);
        // a restart (no in-memory memory of evicted ids) evicts a re-offered one again
        let mut re = Inbox::load_with(p, policy, NOW);
        assert_eq!(re.ingest_at(aged("$1", 10_000), NOW), None);
        assert_eq!(held(&re), vec!["$3", "$4", "$5"]);
        // a smaller cap on load trims what is already there
        let two = InboxPolicy {
            max_entries: 2,
            ..policy
        };
        assert_eq!(Inbox::load_with(re.path().to_path_buf(), two, NOW).len(), 2);
    }

    #[test]
    fn age_bound_drops_old_entries_read_or_not_and_refuses_old_events() {
        let policy = InboxPolicy {
            max_age: Some(Duration::from_secs(24 * 3600)),
            ..InboxPolicy::default()
        };
        let (mut ib, p) = with(policy, "age");
        ib.ingest_at(aged("$old-read", 23 * HOUR), NOW);
        ib.ingest_at(aged("$old-unread", 23 * HOUR + 1000), NOW);
        ib.ingest_at(aged("$fresh", HOUR), NOW);
        ib.mark_seen(&[1], &Actor::default());
        // an event already past the bound is never ingested (backfill after a restart)
        assert_eq!(ib.ingest_at(aged("$ancient", 25 * HOUR), NOW), None);
        assert!(!ib.contains("$ancient"));
        assert_eq!(ib.len(), 3);
        // two hours later the 23 h-old pair is past 24 h, read and unread alike
        let later = NOW + 2 * HOUR;
        assert_eq!(
            ib.enforce_at(later),
            Pruned {
                removed: 2,
                unread: 1
            }
        );
        assert_eq!(ib.len(), 1);
        assert!(ib.contains("$fresh"));
        // the ids of pruned entries are forgotten (memory stays bounded) and on disk
        assert!(!ib.contains("$old-read"));
        assert_eq!(Inbox::load_with(p, policy, later).len(), 1);
        // nothing left to prune: no-op
        assert_eq!(ib.enforce_at(later), Pruned::default());
    }

    #[test]
    fn seq_never_goes_backwards_even_when_everything_expires() {
        let policy = InboxPolicy {
            max_age: Some(Duration::from_secs(24 * 3600)),
            ..InboxPolicy::default()
        };
        let (mut ib, p) = with(policy, "seq");
        for i in 0..3 {
            ib.ingest_at(aged(&format!("$e{i}"), HOUR), NOW);
        }
        assert_eq!(ib.high_water(), 3);
        // a week later everything has aged out; a restart must not renumber from 1
        let later = NOW + 7 * 24 * HOUR;
        assert_eq!(ib.enforce_at(later).removed, 3);
        assert!(ib.is_empty());
        let mut re = Inbox::load_with(p.clone(), policy, later);
        assert!(re.is_empty());
        assert_eq!(re.high_water(), 3);
        let mut fresh = aged("$new", 0);
        fresh.ts_ms = later;
        assert_eq!(re.ingest_at(fresh, later), Some(4));
        // also when only an entries file written before inbox.seq existed is there
        std::fs::remove_file(seq_path(&p)).unwrap();
        assert_eq!(Inbox::load_with(p, policy, later).high_water(), 4);
    }

    #[test]
    fn refusing_media_stores_nothing_and_purges_old_media_on_load() {
        let refuse = InboxPolicy {
            accept_media: false,
            ..InboxPolicy::default()
        };
        let (mut ib, p) = with(refuse, "nomedia");
        for kind in crate::attachments::MEDIA_KINDS {
            assert_eq!(
                ib.ingest_at(media(&format!("${kind}"), kind), NOW),
                None,
                "{kind}"
            );
        }
        assert!(ib.is_empty());
        assert!(ib.ingest_at(aged("$txt", 5), NOW).is_some());
        // an inbox written under the default policy, with an E2EE media entry in it
        let (mut open, p2) = with(InboxPolicy::default(), "nomedia-legacy");
        assert!(open.ingest_at(media("$img", "image"), NOW).is_some());
        assert!(open.ingest_at(aged("$txt", 5), NOW).is_some());
        assert!(std::fs::read_to_string(&p2).unwrap().contains("SECRETKEY"));
        // the same file loaded by an instance that refuses media: purged, key off disk
        let strict = Inbox::load_with(p2.clone(), refuse, NOW);
        assert_eq!(strict.len(), 1);
        assert!(strict.get(2).is_some() && strict.get(1).is_none());
        assert!(!std::fs::read_to_string(&p2).unwrap().contains("SECRETKEY"));
        assert_eq!(strict.high_water(), 2);
        let _ = p;
    }

    // ---- handling states ----

    fn tracking() -> InboxPolicy {
        InboxPolicy {
            track_processed: true,
            ..InboxPolicy::default()
        }
    }

    fn actor(by: &str) -> Actor {
        Actor {
            by: by.into(),
            session: Some(format!("sess-{by}")),
        }
    }

    fn seqs_in(ib: &Inbox, states: &[State]) -> Vec<u64> {
        ib.query(&Query {
            states: Some(states.to_vec()),
            ..Default::default()
        })
        .iter()
        .map(|e| e.seq)
        .collect()
    }

    #[test]
    fn states_go_new_seen_processed_and_processed_is_final() {
        let p = tmp_path("states");
        let mut ib = Inbox::load_with(p.clone(), tracking(), now_ms());
        for (i, id) in ["$1", "$2", "$3"].iter().enumerate() {
            ib.ingest(msg(id, "@t:x", 10 + i as u64));
        }
        let open = [State::New, State::Seen];
        assert_eq!(seqs_in(&ib, &open), vec![1, 2, 3]);

        ib.mark_seen(&[1], &actor("a"));
        let e1 = ib.get(1).unwrap();
        assert_eq!(e1.state(), State::Seen);
        assert_eq!(e1.seen.as_ref().unwrap().by, "a");
        assert_eq!(e1.seen.as_ref().unwrap().session.as_deref(), Some("sess-a"));

        // processed straight from new works too; unknown seqs are reported
        let r = ib
            .mark_processed(&[2, 1, 99, 1], &actor("b"), "replied in DM")
            .unwrap();
        assert_eq!(r.marked, vec![1, 2]);
        assert_eq!(r.missing, vec![99]);
        assert!(r.already.is_empty());
        assert_eq!(ib.get(2).unwrap().state(), State::Processed);
        assert!(ib.get(2).unwrap().seen.is_none());

        // a second session cannot overwrite the first disposition
        let again = ib
            .mark_processed(&[1], &actor("c"), "did it again")
            .unwrap();
        assert!(again.marked.is_empty());
        assert_eq!(again.already.len(), 1);
        let (seq, kept) = &again.already[0];
        assert_eq!(*seq, 1);
        assert_eq!(kept.by, "b");
        assert_eq!(kept.note.as_deref(), Some("replied in DM"));

        // seeing a processed entry does not demote it
        ib.mark_seen(&[1, 2], &actor("d"));
        assert_eq!(ib.get(2).unwrap().state(), State::Processed);
        assert_eq!(ib.get(1).unwrap().seen.as_ref().unwrap().by, "a");

        assert_eq!(seqs_in(&ib, &open), vec![3]);
        assert_eq!(seqs_in(&ib, &[State::Processed]), vec![1, 2]);
        assert_eq!(
            (
                ib.count(State::New),
                ib.count(State::Seen),
                ib.count(State::Processed)
            ),
            (1, 0, 2)
        );

        // all of it survives a reload, also by an instance that does not track
        let re = Inbox::load(p);
        assert_eq!(seqs_in(&re, &[State::Processed]), vec![1, 2]);
        let m = re.get(1).unwrap().processed.clone().unwrap();
        assert_eq!(
            (m.by.as_str(), m.session.as_deref(), m.note.as_deref()),
            ("b", Some("sess-b"), Some("replied in DM"))
        );
        assert_eq!(re.unread_count(), 1);
    }

    #[test]
    fn legacy_read_flag_loads_as_seen_and_stays_readable_by_old_binaries() {
        let p = tmp_path("legacy");
        let line = |seq: u64, read: bool| {
            format!(
                r#"{{"seq":{seq},"event_id":"$e{seq}","room_id":"!r:x","sender":"@t:x","sender_name":"tim","ts_ms":{seq},"kind":"text","body":"b","read":{read}}}"#
            )
        };
        std::fs::write(&p, format!("{}\n{}\n", line(1, true), line(2, false))).unwrap();
        let mut ib = Inbox::load_with(p.clone(), tracking(), now_ms());
        assert_eq!(ib.get(1).unwrap().state(), State::Seen);
        assert!(ib.get(1).unwrap().seen.is_none());
        assert_eq!(ib.get(2).unwrap().state(), State::New);

        ib.mark_processed(&[2], &actor("a"), "done").unwrap();
        // what a pre-states binary deserializes: `read` must be set for handled entries
        let text = std::fs::read_to_string(&p).unwrap();
        let l2 = text.lines().find(|l| l.contains("\"$e2\"")).unwrap();
        let v: serde_json::Value = serde_json::from_str(l2).unwrap();
        assert_eq!(v["read"], true);
        assert_eq!(v["processed"]["note"], "done");
    }

    #[test]
    fn soft_cap_evicts_processed_before_still_open_entries() {
        let policy = InboxPolicy {
            max_entries: 2,
            ..tracking()
        };
        let (mut ib, _) = with(policy, "soft-states");
        ib.ingest_at(aged("$1", 3000), NOW);
        ib.ingest_at(aged("$2", 2000), NOW);
        ib.mark_seen(&[1], &actor("a"));
        ib.mark_processed(&[2], &actor("a"), "done").unwrap();
        // $1 is older, but only seen (open work); the processed $2 goes first
        assert_eq!(ib.ingest_at(aged("$3", 1000), NOW), Some(3));
        assert_eq!(seqs_in(&ib, &State::ALL), vec![1, 3]);
    }

    #[test]
    fn without_tracking_open_means_unread_and_processing_is_refused() {
        let d = InboxPolicy::default();
        assert!(!d.track_processed);
        assert_eq!(d.open_states(), vec![State::New]);
        assert_eq!(tracking().open_states(), vec![State::New, State::Seen]);
        let mut ib = Inbox::load(tmp_path("untracked"));
        ib.ingest(msg("$1", "@t:x", 1));
        ib.ingest(msg("$2", "@t:x", 2));
        ib.mark_seen(&[1], &actor("a"));
        assert_eq!(ib.open_count(), 1);
        assert_eq!(
            ib.mark_processed(&[2], &actor("a"), "done"),
            Err(NotTracked)
        );
        assert_eq!(ib.get(2).unwrap().state(), State::New);
        // the read is still recorded as state, with its provenance
        assert_eq!(ib.get(1).unwrap().seen.as_ref().unwrap().by, "a");
    }
}
