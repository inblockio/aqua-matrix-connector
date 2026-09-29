//! The durable inbox: inbound messages from allow-listed senders.
//!
//! Stored as one JSON object per line in `<state dir>/inbox.jsonl`, rewritten
//! atomically (temp file + rename, mode 600) on every change. Volume is tiny
//! (a handful of people replying to status messages), so a full rewrite is
//! simpler and safer than an append log with a side index.
//!
//! Dedupe is by Matrix `event_id`, never by timestamp: the host clock on this
//! WSL box skews against the homeserver, so no host-clock watermark is used
//! anywhere (memory `wsl-clock-skew-watermark-gotcha`). Timestamps shown to
//! sessions are the server's `origin_server_ts`.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Keep at most this many entries; the oldest READ entries go first.
pub const MAX_ENTRIES: usize = 5_000;

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
    #[serde(default)]
    pub read: bool,
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
    pub unread_only: bool,
    pub limit: Option<usize>,
}

pub struct Inbox {
    path: PathBuf,
    entries: Vec<InboxEntry>,
    seen: HashSet<String>,
    next_seq: u64,
}

impl Inbox {
    /// Load `path` (missing file = empty inbox). Unparsable lines are skipped
    /// with a warning rather than discarding the whole inbox.
    pub fn load(path: PathBuf) -> Self {
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
        let seen = entries.iter().map(|e| e.event_id.clone()).collect();
        let next_seq = entries.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        Self {
            path,
            entries,
            seen,
            next_seq,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn contains(&self, event_id: &str) -> bool {
        self.seen.contains(event_id)
    }

    /// The highest seq assigned so far (0 when empty). A session can pass this
    /// as `after_seq` to wait only for replies that arrive after its send.
    pub fn high_water(&self) -> u64 {
        self.next_seq - 1
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn unread_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.read).count()
    }

    /// Add a message unless its event id was already ingested. Returns the new
    /// entry's seq, or `None` for a duplicate. Persists on success.
    pub fn ingest(&mut self, new: NewEntry) -> Option<u64> {
        if self.seen.contains(&new.event_id) {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.seen.insert(new.event_id.clone());
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
        });
        self.prune();
        self.persist();
        Some(seq)
    }

    /// Drop the oldest read entries beyond [`MAX_ENTRIES`] (unread ones are
    /// kept until read). `seen` keeps the pruned ids so a backfill cannot
    /// re-ingest them in this process.
    fn prune(&mut self) {
        let mut excess = self.entries.len().saturating_sub(MAX_ENTRIES);
        if excess == 0 {
            return;
        }
        self.entries.retain(|e| {
            if excess > 0 && e.read {
                excess -= 1;
                false
            } else {
                true
            }
        });
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
            .filter(|e| !q.unread_only || !e.read)
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

    /// Mark the given seqs read. Persists if anything changed.
    pub fn mark_read(&mut self, seqs: &[u64]) {
        let set: HashSet<u64> = seqs.iter().copied().collect();
        let mut changed = false;
        for e in self.entries.iter_mut() {
            if set.contains(&e.seq) && !e.read {
                e.read = true;
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
    }

    /// Atomic rewrite: temp file (mode 600) + rename.
    fn persist(&self) {
        if let Err(e) = self.try_persist() {
            tracing::error!(path = %self.path.display(), "inbox persist failed: {e:#}");
        }
    }

    fn try_persist(&self) -> std::io::Result<()> {
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
    fn query_filters_and_mark_read() {
        let p = tmp_path("query");
        let mut ib = Inbox::load(p.clone());
        ib.ingest(msg("$1", "@t:x", 100));
        ib.ingest(msg("$2", "@k:x", 200));
        ib.ingest(msg("$3", "@T:x", 300));
        let q = Query {
            sender: Some("@t:x".into()),
            unread_only: true,
            ..Default::default()
        };
        let got = ib.query(&q);
        assert_eq!(got.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 3]);
        ib.mark_read(&[1]);
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
}
