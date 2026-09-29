//! The messenger **engine**: every policy decision behind the tools, shared by
//! the host bridge daemon and embedded agents.
//!
//! - target resolution: a person (DM) or an allow-listed group room, both
//!   first-class; anything else is refused with the list of allowed names;
//! - inbound filtering: a DM is kept when its sender is allow-listed, a room
//!   message when the ROOM is listed; everything else is dropped and logged
//!   once;
//! - per-target sliding-window rate limit, message/file size caps, the
//!   sensitive-path guard on `send_file`;
//! - `reply_to`: a reply must stay in the conversation of the message it
//!   answers (checked here against the inbox, and by the transport against
//!   the room);
//! - the durable inbox (`read_inbox`, `wait_for_reply` where enabled) and the
//!   attachment store (`fetch_attachment`: allow-list re-check at fetch time,
//!   cache, size cap, retention; the media key never leaves the engine);
//! - metadata-only audit log.
//!
//! Matrix itself sits behind [`Transport`]: the host daemon queues requests to
//! its cycle loop, an embedded agent calls its own live `AgentClient`
//! (crate `aqua-messenger-matrix`). Either way there is exactly ONE Matrix
//! Client per crypto store; the engine never builds one.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::allowlist::{is_valid_mxid, is_valid_room_id, AllowList, Target};
use crate::attachments::AttachmentStore;
use crate::format;
use crate::inbox::{Inbox, InboxEntry, MediaRef, NewEntry, Query};
use crate::jsonrpc;
use crate::profile::Profile;
use crate::proto::{Request, Response};
use crate::ratelimit::RateLimiter;

/// Where a send goes, after allow-list resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// A person: their 1:1 DM (resolved by the transport, never a listed
    /// `[[rooms]]` entry or a group room), created if none exists.
    Person(String),
    /// A listed group room, by room id (must already be joined).
    Room(String),
}

/// The message a send replies to, as far as the engine knows it. `room_id`,
/// `sender` and `thread_root` are known when the target is an inbox entry;
/// for a bare event id they are `None` and the transport loads the event from
/// the destination room (which also proves it is in that room).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplyRef {
    pub event_id: String,
    pub sender: Option<String>,
    pub room_id: Option<String>,
    pub thread_root: Option<String>,
}

/// A delivered send.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sent {
    /// Event id of the (last) event sent.
    pub event_id: String,
    /// The thread the message went into, if it was a threaded reply.
    pub thread_root: Option<String>,
}

impl From<String> for Sent {
    fn from(event_id: String) -> Self {
        Self {
            event_id,
            thread_root: None,
        }
    }
}

/// What the engine asks of Matrix. Errors are human-readable strings shown to
/// the model (never secrets).
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Send Markdown to `to` (chunked below the event size cap; a reply
    /// relation only on the first chunk, later chunks stay in the thread).
    async fn send_text(
        &self,
        to: &Dest,
        markdown: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String>;
    /// Upload `path` as an (E2EE) attachment to `to`.
    async fn send_file(
        &self,
        to: &Dest,
        path: &Path,
        caption: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String>;
    /// Download, decrypt and verify an inbound attachment.
    async fn fetch(&self, req: FetchRequest) -> Result<Fetched, String>;
}

/// One attachment download.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub event_id: String,
    pub room_id: String,
    /// The reference recorded at ingest (`None` for entries older than the
    /// attachment feature: the transport resolves the event from the room).
    pub media: Option<MediaRef>,
    pub max_bytes: u64,
}

/// A downloaded attachment: verified plaintext and the mime type if known.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub mimetype: Option<String>,
}

/// Connection status reported by `status` (set by whoever owns the Client).
#[derive(Default, Clone, Debug)]
pub struct Status {
    pub connected: bool,
    pub did: Option<String>,
    pub user_id: Option<String>,
    pub device_id: Option<String>,
    pub last_error: Option<String>,
}

/// What the Matrix side knows about one joined room, refreshed on every
/// connect (for `list_recipients`, the unlisted-room refusal and the
/// group-room check on inbound messages).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeenRoom {
    pub display_name: Option<String>,
    pub joined_members: u64,
    pub is_direct: bool,
}

/// How an inbound message is recorded (see [`Engine::classify_inbound`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    /// `[[rooms]]` name for a listed-room message, `None` for a DM.
    pub room_name: Option<String>,
    /// Allow-list name of the sender, if listed.
    pub sender_name: Option<String>,
}

/// Paths `send_file` refuses: the messenger's own state (key, store, tokens)
/// and the usual credential locations. Defence in depth: a confused or
/// injected session must not be able to mail out secrets, even to an allowed
/// person.
pub fn sensitive_path(p: &Path, state_dir: &Path) -> Option<&'static str> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    sensitive_path_in(p, state_dir, &home)
}

fn sensitive_path_in(p: &Path, state_dir: &Path, home: &Path) -> Option<&'static str> {
    let state_dir = state_dir
        .canonicalize()
        .unwrap_or_else(|_| state_dir.to_path_buf());
    if p.starts_with(&state_dir) {
        return Some("the bridge's own state directory");
    }
    for d in [
        ".ssh",
        ".gnupg",
        ".config/gh",
        ".aws",
        ".docker",
        ".claude/.credentials.json",
        ".aqua-matrix-heartbeat",
        ".aqua-system-bridge",
    ] {
        if !home.as_os_str().is_empty() && p.starts_with(home.join(d)) {
            return Some("a credentials location");
        }
    }
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".env")
        || ["pem", "key", "p12", "pfx"].contains(&ext.as_str())
    {
        return Some("a key or env file");
    }
    if name.contains("token") || name.contains("secret") || name == "hosts.yml" {
        return Some("a file named like a credential");
    }
    None
}

/// A parsed `reply_to` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyArg {
    /// An inbox sequence number.
    Seq(u64),
    /// A Matrix event id (`$...`).
    EventId(String),
}

/// Parse `reply_to`: a decimal inbox seq or a Matrix event id.
pub fn parse_reply_to(s: &str) -> Result<ReplyArg, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(ReplyArg::Seq(n));
    }
    if s.len() > 1
        && s.len() <= 255
        && s.starts_with('$')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Ok(ReplyArg::EventId(s.to_string()));
    }
    Err(format!(
        "reply_to {s:?} is neither an inbox seq (e.g. \"12\") nor a Matrix event id (\"$...\")"
    ))
}

/// A resolved `to`: display name, rate-limit key and destination.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub name: String,
    /// MXID for a person, room id for a room (also the rate-limit key).
    pub id: String,
    pub dest: Dest,
}

/// An inbox filter from a `from` argument.
#[derive(Debug, Clone, PartialEq)]
pub enum FromFilter {
    /// DMs from this MXID.
    Sender(String),
    /// Messages in this room.
    Room(String),
}

pub struct Engine<T: Transport> {
    profile: Profile,
    state_dir: PathBuf,
    allow: Mutex<AllowList>,
    inbox: Mutex<Inbox>,
    /// Woken (notify_waiters) after every successful ingest.
    inbox_changed: Notify,
    rate: Mutex<RateLimiter>,
    status: Mutex<Status>,
    /// Keys of dropped inbound messages already logged (event ids, and
    /// `room:<id>` for unlisted group rooms), so a backfill does not re-log
    /// them every cycle.
    dropped: Mutex<HashSet<String>>,
    /// Joined rooms as of the last connect (room id -> info).
    joined_rooms: Mutex<BTreeMap<String, SeenRoom>>,
    attachments: AttachmentStore,
    /// Serialises fetches so two callers asking for the same entry do not
    /// download it twice.
    fetch_lock: tokio::sync::Mutex<()>,
    transport: T,
}

impl<T: Transport> Engine<T> {
    /// `state_dir` holds `inbox.jsonl`, `audit.jsonl` and `attachments/`.
    pub fn new(profile: Profile, state_dir: &Path, allow: AllowList, transport: T) -> Self {
        let l = profile.limits;
        Self {
            state_dir: state_dir.to_path_buf(),
            allow: Mutex::new(allow),
            inbox: Mutex::new(Inbox::load(state_dir.join("inbox.jsonl"))),
            inbox_changed: Notify::new(),
            rate: Mutex::new(RateLimiter::new(
                l.rate_count,
                Duration::from_secs(l.rate_window_secs),
            )),
            status: Mutex::new(Status::default()),
            dropped: Mutex::new(HashSet::new()),
            joined_rooms: Mutex::new(BTreeMap::new()),
            attachments: AttachmentStore::new(state_dir.join("attachments")),
            fetch_lock: tokio::sync::Mutex::new(()),
            transport,
            profile,
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// "the bridge" on the host, "this agent" when embedded (texts only).
    fn who_am_i(&self) -> &'static str {
        if self.profile.default_to.is_none() {
            "the bridge"
        } else {
            "this agent"
        }
    }

    pub fn set_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }

    /// Run `f` on the allow-list after a hot reload (mtime check).
    pub fn with_allow<R>(&self, f: impl FnOnce(&AllowList) -> R) -> R {
        let mut allow = self.allow.lock().unwrap();
        allow.reload(false);
        f(&allow)
    }

    /// Replace the joined-room snapshot (after a connect); returns the
    /// previous one.
    pub fn set_joined_rooms(&self, map: BTreeMap<String, SeenRoom>) -> BTreeMap<String, SeenRoom> {
        std::mem::replace(&mut *self.joined_rooms.lock().unwrap(), map)
    }

    /// Record one room joined since the last snapshot.
    pub fn note_joined_room(&self, room_id: &str, seen: SeenRoom) {
        self.joined_rooms
            .lock()
            .unwrap()
            .insert(room_id.to_string(), seen);
    }

    /// Joined-member count of `room_id` as of the last snapshot.
    pub fn snapshot_members(&self, room_id: &str) -> Option<u64> {
        self.joined_rooms
            .lock()
            .unwrap()
            .get(room_id)
            .map(|s| s.joined_members)
    }

    /// Append one JSON line to `<state>/audit.jsonl`. Metadata only, never
    /// message bodies.
    pub fn audit(&self, mut v: Value) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        v["host_ts"] = json!(format::fmt_ts_ms(now));
        let path = self.state_dir.join("audit.jsonl");
        let res = (|| -> std::io::Result<()> {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .open(&path)?;
            f.write_all(format!("{v}\n").as_bytes())
        })();
        if let Err(e) = res {
            tracing::warn!("audit write failed: {e}");
        }
    }

    /// Delete fetched attachments past retention; returns how many.
    pub fn prune_attachments(&self) -> usize {
        self.attachments
            .prune(self.profile.attachments.retention_days, SystemTime::now())
    }

    // ---- inbound side (called by the Matrix adapter) --------------------

    /// Decide whether an inbound message is recorded: any member's message in
    /// a listed `[[rooms]]` room, or a DM from an allow-listed sender.
    /// Messages in unlisted group rooms (`group`: more than two joined
    /// members) are dropped, logged once per room; DMs from other senders are
    /// dropped, logged once per event. Our own messages are ignored.
    pub fn classify_inbound(
        &self,
        own: &str,
        sender: &str,
        room_id: &str,
        event_id: &str,
        group: bool,
    ) -> Option<Accepted> {
        if sender.eq_ignore_ascii_case(own) {
            return None;
        }
        let (room_name, sender_name) = self.with_allow(|a| {
            (
                a.room_by_id(room_id).map(|r| r.name.clone()),
                a.by_mxid(sender).map(|r| r.name.clone()),
            )
        });
        if room_name.is_none() && group {
            if self
                .dropped
                .lock()
                .unwrap()
                .insert(format!("room:{room_id}"))
            {
                tracing::info!(room = %room_id, "dropping messages in an unlisted group room (not under [[rooms]])");
                self.audit(json!({"event": "inbound_dropped_unlisted_room", "room": room_id}));
            }
            return None;
        }
        if room_name.is_none() && sender_name.is_none() {
            if self.dropped.lock().unwrap().insert(event_id.to_string()) {
                tracing::info!(%sender, room = %room_id, "dropped message from non-allow-listed sender");
                self.audit(json!({"event": "inbound_dropped", "from": sender, "room": room_id, "event_id": event_id}));
            }
            return None;
        }
        Some(Accepted {
            room_name,
            sender_name,
        })
    }

    /// Record an inbound message (event-id deduped). Returns the new seq.
    pub fn ingest(&self, entry: NewEntry) -> Option<u64> {
        let kind = entry.kind.clone();
        let from = entry
            .sender_name
            .clone()
            .unwrap_or_else(|| entry.sender.clone());
        let room = entry.room.clone().unwrap_or_else(|| "-".into());
        let seq = self.inbox.lock().unwrap().ingest(entry)?;
        tracing::info!(%from, %room, seq, kind, "inbox: new message");
        self.inbox_changed.notify_waiters();
        Some(seq)
    }

    /// Whether `event_id` is already in the inbox.
    pub fn has_event(&self, event_id: &str) -> bool {
        self.inbox.lock().unwrap().contains(event_id)
    }

    // ---- request side ----------------------------------------------------

    /// Resolve a `to` argument to an allow-listed person or listed room.
    pub fn resolve_to(&self, to: &str) -> Result<Resolved, String> {
        let mut allow = self.allow.lock().unwrap();
        allow.reload(false);
        match allow.resolve_target(to) {
            Some(Target::Person(r)) => Ok(Resolved {
                name: r.name.clone(),
                id: r.mxid.clone(),
                dest: Dest::Person(r.mxid.clone()),
            }),
            Some(Target::Room(r)) => Ok(Resolved {
                name: r.name.clone(),
                id: r.room_id.clone(),
                dest: Dest::Room(r.room_id.clone()),
            }),
            None => {
                let t = to.trim();
                if is_valid_room_id(t) && self.joined_rooms.lock().unwrap().contains_key(t) {
                    return Err(format!(
                        "REFUSED: {} has joined room {t} but it is not listed under [[rooms]] in {}; \
                         unlisted rooms are not sendable. Add a [[rooms]] entry (only with {}'s approval).",
                        self.who_am_i(),
                        allow.path_display(),
                        self.profile.approver
                    ));
                }
                Err(allow.refusal(to))
            }
        }
    }

    /// Resolve a `from` filter: an allow-listed person (name/MXID), a listed
    /// room (name/room id), or any well-formed MXID / room id (entries of
    /// since-removed people and rooms stay readable).
    pub fn resolve_from(&self, from: &str) -> Result<FromFilter, String> {
        let mut allow = self.allow.lock().unwrap();
        allow.reload(false);
        match allow.resolve_target(from) {
            Some(Target::Person(r)) => return Ok(FromFilter::Sender(r.mxid.clone())),
            Some(Target::Room(r)) => return Ok(FromFilter::Room(r.room_id.clone())),
            None => {}
        }
        let f = from.trim();
        if is_valid_mxid(f) {
            return Ok(FromFilter::Sender(f.to_string()));
        }
        if is_valid_room_id(f) {
            return Ok(FromFilter::Room(f.to_string()));
        }
        Err(format!(
            "{from:?} is neither an allow-list name, an MXID, a [[rooms]] name nor a room id"
        ))
    }

    /// Resolve `reply_to` for a send to `to`. An inbox entry must belong to
    /// that conversation: for a room, posted in that room; for a person, a DM
    /// they sent. A bare event id not in the inbox is passed on; the transport
    /// loads it from the destination room, which fails for an event that is
    /// not there.
    pub fn resolve_reply(&self, reply_to: &str, to: &Resolved) -> Result<ReplyRef, String> {
        let arg = parse_reply_to(reply_to)?;
        let entry = {
            let inbox = self.inbox.lock().unwrap();
            match &arg {
                ReplyArg::Seq(n) => Some(inbox.get(*n).cloned().ok_or_else(|| {
                    format!("reply_to: no inbox entry with seq {n} (see read_inbox)")
                })?),
                ReplyArg::EventId(id) => inbox.get_by_event_id(id).cloned(),
            }
        };
        let Some(e) = entry else {
            let ReplyArg::EventId(id) = arg else {
                unreachable!("a seq either resolves or errors above")
            };
            return Ok(ReplyRef {
                event_id: id,
                ..Default::default()
            });
        };
        let where_ = match &e.room {
            Some(r) => format!("room {r}"),
            None => format!(
                "the DM with {}",
                e.sender_name.clone().unwrap_or_else(|| e.sender.clone())
            ),
        };
        let fits = match &to.dest {
            Dest::Room(rid) => &e.room_id == rid,
            Dest::Person(mxid) => e.room.is_none() && e.sender.eq_ignore_ascii_case(mxid),
        };
        if !fits {
            return Err(format!(
                "REFUSED: reply_to {} (inbox seq {}) is a message in {where_}, not in the conversation with {}; \
                 a reply must go where the original was posted (set `to` accordingly, or drop reply_to).",
                reply_to.trim(),
                e.seq,
                to.name
            ));
        }
        Ok(ReplyRef {
            event_id: e.event_id,
            sender: Some(e.sender),
            room_id: Some(e.room_id),
            thread_root: e.thread_root,
        })
    }

    /// `to` as given, or the profile's default recipient.
    fn to_or_default(&self, to: &str, tool: &str, what: &str) -> Result<String, String> {
        let to = to.trim();
        if !to.is_empty() {
            return Ok(to.to_string());
        }
        self.profile
            .default_to
            .clone()
            .ok_or_else(|| format!("{tool} needs `to` and {what}"))
    }

    fn tool_enabled(&self, tool: &str) -> Result<(), Response> {
        if self.profile.has_tool(tool) {
            Ok(())
        } else {
            Err(Response::err(format!(
                "{tool} is not enabled for this messenger"
            )))
        }
    }

    /// Answer one request. Never panics on bad input; errors are for the model.
    pub async fn handle(&self, req: Request) -> Response {
        let l = self.profile.limits;
        match req {
            Request::Describe => Response::ok(jsonrpc::describe(&self.profile)),
            Request::SendMessage {
                to,
                markdown,
                origin,
                reply_to,
            } => {
                if let Err(r) = self.tool_enabled(jsonrpc::T_SEND_MESSAGE) {
                    return r;
                }
                let to = match self.to_or_default(&to, "send_message", "a non-empty `markdown`") {
                    Ok(t) => t,
                    Err(e) => return Response::err(e),
                };
                if markdown.trim().is_empty() {
                    return Response::err("send_message needs a non-empty `markdown`");
                }
                if markdown.len() > l.max_message_bytes {
                    return Response::err(format!(
                        "message is {} bytes; the cap is {}. Send long content with send_file instead.",
                        markdown.len(),
                        l.max_message_bytes
                    ));
                }
                let body = if self.profile.tag_origin {
                    format::tag_markdown(&markdown, &origin)
                } else {
                    markdown.clone()
                };
                let bytes = markdown.len();
                self.send(
                    &to,
                    Outgoing::Text(body),
                    &origin,
                    reply_to.as_deref(),
                    json!({"kind": "text", "bytes": bytes}),
                )
                .await
            }
            Request::SendFile {
                to,
                path,
                caption,
                origin,
                reply_to,
            } => {
                if let Err(r) = self.tool_enabled(jsonrpc::T_SEND_FILE) {
                    return r;
                }
                let to = match self.to_or_default(&to, "send_file", "`path`") {
                    Ok(t) => t,
                    Err(e) => return Response::err(e),
                };
                let p = match Path::new(&path).canonicalize() {
                    Ok(p) => p,
                    Err(e) => return Response::err(format!("cannot open {path}: {e}")),
                };
                if let Some(why) = sensitive_path(&p, &self.state_dir) {
                    self.audit(
                        json!({"event": "send_refused", "reason": "sensitive_path", "path": p}),
                    );
                    return Response::err(format!(
                        "REFUSED: {} is {why}; {} never sends those.",
                        p.display(),
                        self.who_am_i()
                    ));
                }
                let meta = match std::fs::metadata(&p) {
                    Ok(m) => m,
                    Err(e) => return Response::err(format!("cannot stat {}: {e}", p.display())),
                };
                if !meta.is_file() {
                    return Response::err(format!("{} is not a regular file", p.display()));
                }
                if meta.len() > l.max_file_bytes {
                    return Response::err(format!(
                        "{} is {} bytes; the cap is {}",
                        p.display(),
                        meta.len(),
                        l.max_file_bytes
                    ));
                }
                let filename = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string();
                let caption = if self.profile.tag_origin {
                    format::tag_caption(caption.as_deref(), &filename, &origin)
                } else {
                    caption
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| filename.clone())
                };
                let info = json!({"kind": "file", "bytes": meta.len(), "filename": filename});
                self.send(
                    &to,
                    Outgoing::File { path: p, caption },
                    &origin,
                    reply_to.as_deref(),
                    info,
                )
                .await
            }
            Request::ListRecipients => self.list_recipients(),
            Request::ReadInbox {
                from,
                since_seq,
                since_ts_ms,
                unread_only,
                mark_read,
                limit,
            } => {
                if let Err(r) = self.tool_enabled(jsonrpc::T_READ_INBOX) {
                    return r;
                }
                let filter = match from.as_deref().map(|f| self.resolve_from(f)).transpose() {
                    Ok(s) => s,
                    Err(e) => return Response::err(e),
                };
                let mut q = Query {
                    since_seq,
                    since_ts_ms,
                    unread_only,
                    limit,
                    ..Default::default()
                };
                apply_from(&mut q, filter);
                let mut inbox = self.inbox.lock().unwrap();
                let entries = inbox.query(&q);
                if mark_read {
                    let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                    inbox.mark_read(&seqs);
                }
                let entries: Vec<_> = entries.iter().map(InboxEntry::public).collect();
                Response::ok(
                    json!({"entries": entries, "high_water": inbox.high_water(), "unread_remaining": inbox.unread_count()}),
                )
            }
            Request::WaitForReply {
                from,
                timeout_s,
                after_seq,
            } => {
                if let Err(r) = self.tool_enabled(jsonrpc::T_WAIT_FOR_REPLY) {
                    return r;
                }
                let filter = match self.resolve_from(&from) {
                    Ok(s) => s,
                    Err(e) => return Response::err(e),
                };
                let secs = timeout_s.clamp(1, l.max_wait_secs);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
                let mut q = Query {
                    since_seq: after_seq,
                    unread_only: true,
                    ..Default::default()
                };
                apply_from(&mut q, Some(filter));
                loop {
                    // Register interest BEFORE checking, so an ingest between
                    // the check and the await cannot be missed.
                    let notified = self.inbox_changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    {
                        let mut inbox = self.inbox.lock().unwrap();
                        let entries = inbox.query(&q);
                        if !entries.is_empty() {
                            let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                            inbox.mark_read(&seqs);
                            let entries: Vec<_> = entries.iter().map(InboxEntry::public).collect();
                            return Response::ok(
                                json!({"entries": entries, "waited_s": secs, "high_water": inbox.high_water()}),
                            );
                        }
                    }
                    tokio::select! {
                        _ = &mut notified => continue,
                        _ = tokio::time::sleep_until(deadline) => {
                            return Response::ok(json!({"entries": [], "waited_s": secs, "timed_out": true}));
                        }
                    }
                }
            }
            Request::FetchAttachment { inbox_seq } => {
                if let Err(r) = self.tool_enabled(jsonrpc::T_FETCH_ATTACHMENT) {
                    return r;
                }
                self.fetch_attachment(inbox_seq).await
            }
            Request::Status => {
                let st = self.status.lock().unwrap().clone();
                let inbox = self.inbox.lock().unwrap();
                Response::ok(json!({
                    "connected": st.connected,
                    "did": st.did,
                    "mxid": st.user_id,
                    "device_id": st.device_id,
                    "last_error": st.last_error,
                    "inbox_entries": inbox.len(),
                    "inbox_unread": inbox.unread_count(),
                    "inbox_high_water": inbox.high_water(),
                }))
            }
        }
    }

    fn list_recipients(&self) -> Response {
        let l = self.profile.limits;
        let mut allow = self.allow.lock().unwrap();
        allow.reload(false);
        let mut rate = self.rate.lock().unwrap();
        let now = Instant::now();
        let recips: Vec<_> = allow
            .recipients()
            .iter()
            .map(|r| json!({"name": r.name, "mxid": r.mxid, "note": r.note, "sends_left_in_window": rate.remaining(&r.mxid, now)}))
            .collect();
        let joined = self.joined_rooms.lock().unwrap();
        let rooms: Vec<_> = allow
            .rooms()
            .iter()
            .map(|r| {
                let seen = joined.get(&r.room_id);
                json!({
                    "name": r.name,
                    "room_id": r.room_id,
                    "note": r.note,
                    "joined": seen.is_some(),
                    "display_name": seen.and_then(|s| s.display_name.clone()),
                    "joined_members": seen.map(|s| s.joined_members),
                    "sends_left_in_window": rate.remaining(&r.room_id, now),
                })
            })
            .collect();
        let unlisted = joined
            .iter()
            .filter(|(id, s)| !s.is_direct && !allow.is_listed_room(id))
            .count();
        let note = match &self.profile.default_to {
            None => format!(
                "Confirm with {a} before messaging anyone other than {a}, or posting in a room, unless he asked for it. A room post is read by everyone in that room.",
                a = self.profile.approver
            ),
            Some(o) => format!(
                "`to` defaults to {o} (your owner). Additional people or rooms are explicit configuration only. A room post is read by everyone in that room."
            ),
        };
        Response::ok(json!({
            "recipients": recips,
            "rooms": rooms,
            "unlisted_non_dm_rooms_joined": unlisted,
            "allowlist_file": allow.path(),
            "allowlist_error": allow.load_error(),
            "rate_limit": format!("{} sends per {}s per person or room", l.rate_count, l.rate_window_secs),
            "max_message_bytes": l.max_message_bytes,
            "max_file_bytes": l.max_file_bytes,
            "note": note
        }))
    }

    /// Validate target, reply and rate limit, hand the send to the transport
    /// and report its outcome.
    async fn send(
        &self,
        to: &str,
        out: Outgoing,
        origin: &str,
        reply_to: Option<&str>,
        info: Value,
    ) -> Response {
        let origin = format::sanitize_origin(origin);
        let target = match self.resolve_to(to) {
            Ok(v) => v,
            Err(refusal) => {
                tracing::warn!(to, origin, "send refused: not on the allow-list");
                self.audit(json!({"event": "send_refused", "reason": "not_allowlisted", "to": to, "origin": origin}));
                return Response::err(refusal);
            }
        };
        let reply = match reply_to.map(str::trim).filter(|r| !r.is_empty()) {
            None => None,
            Some(r) => match self.resolve_reply(r, &target) {
                Ok(rr) => Some(rr),
                Err(e) => {
                    self.audit(json!({"event": "send_refused", "reason": "reply_to", "to": target.name, "origin": origin}));
                    return Response::err(e);
                }
            },
        };
        let Resolved {
            name,
            id: key,
            dest,
        } = target;
        let l = self.profile.limits;
        if let Err(wait) = self.rate.lock().unwrap().try_acquire(&key, Instant::now()) {
            self.audit(json!({"event": "send_refused", "reason": "rate_limited", "to": name, "origin": origin}));
            return Response::err(format!(
                "RATE LIMITED: {} messages per {} minutes to {name} already used; next slot in {}s. Batch your updates into fewer messages.",
                l.rate_count,
                l.rate_window_secs / 60,
                wait.as_secs() + 1
            ));
        }
        let inbox_seq = self.inbox.lock().unwrap().high_water();
        let is_room = matches!(dest, Dest::Room(_));
        let outcome = match &out {
            Outgoing::Text(md) => self.transport.send_text(&dest, md, reply.as_ref()).await,
            Outgoing::File { path, caption } => {
                self.transport
                    .send_file(&dest, path, caption, reply.as_ref())
                    .await
            }
        };
        match outcome {
            Ok(sent) => {
                tracing::info!(to = %name, origin = %origin, event_id = %sent.event_id, "sent");
                let mut a = json!({"event": "sent", "to": name, "room": is_room, "origin": origin, "event_id": sent.event_id});
                if let Some(r) = &reply {
                    a["reply_to"] = json!(r.event_id);
                }
                if let (Some(a), Some(i)) = (a.as_object_mut(), info.as_object()) {
                    a.extend(i.clone());
                }
                self.audit(a);
                let mut data = json!({"event_id": sent.event_id, "to_name": name, "to_kind": if is_room { "room" } else { "person" }, "inbox_seq": inbox_seq});
                data[if is_room { "to_room_id" } else { "to_mxid" }] = json!(key);
                if let Some(r) = &reply {
                    data["reply_to"] = json!(r.event_id);
                }
                if let Some(t) = &sent.thread_root {
                    data["thread_root"] = json!(t);
                }
                Response::ok(data)
            }
            Err(e) => {
                self.rate.lock().unwrap().release(&key);
                tracing::warn!(to = %name, origin = %origin, "send failed: {e}");
                self.audit(
                    json!({"event": "send_failed", "to": name, "origin": origin, "error": e}),
                );
                Response::err(format!("NOT delivered to {name}: {e}"))
            }
        }
    }

    /// Whether an entry's attachment may be fetched now: it was posted in a
    /// listed `[[rooms]]` room (by any member), or sent by a person who is on
    /// the allow-list. Re-checked at fetch time, not only at ingest, so
    /// removing a person or room from the list also stops downloads (and cache
    /// hits) of what they sent before. A broken allow-list refuses everything.
    fn fetch_permitted(&self, entry: &InboxEntry) -> Result<(), String> {
        let mut allow = self.allow.lock().unwrap();
        allow.reload(false);
        if let Some(e) = allow.load_error() {
            return Err(format!(
                "REFUSED: the allow-list failed to load ({e}); attachments cannot be fetched until it is fixed"
            ));
        }
        if allow.is_listed_room(&entry.room_id) || allow.by_mxid(&entry.sender).is_some() {
            return Ok(());
        }
        Err(format!(
            "REFUSED: inbox entry {} is from {} in a room that is not under [[rooms]], and that \
             sender is not (or no longer) on the allow-list; its attachment is not fetched",
            entry.seq, entry.sender
        ))
    }

    /// Serve the cached copy, or download + decrypt + verify via the
    /// transport and store it (mode 600) under `<state>/attachments/`.
    async fn fetch_attachment(&self, inbox_seq: u64) -> Response {
        let _guard = self.fetch_lock.lock().await;
        let label = self.profile.label.clone();
        let entry = match self.inbox.lock().unwrap().get(inbox_seq).cloned() {
            Some(e) => e,
            None => {
                return Response::err(format!(
                    "no inbox entry with seq {inbox_seq} (see read_inbox)"
                ))
            }
        };
        if !entry.has_attachment() {
            return Response::err(format!(
                "inbox entry {inbox_seq} is a {} message, not an attachment",
                entry.kind
            ));
        }
        if let Err(e) = self.fetch_permitted(&entry) {
            self.audit(json!({"event": "fetch_refused", "reason": "not_allowed", "inbox_seq": inbox_seq, "from": entry.sender, "room": entry.room_id}));
            return Response::err(e);
        }
        let raw_name = entry.filename.clone().unwrap_or_else(|| entry.body.clone());
        let policy = self.profile.attachments;
        if let Some(f) = self
            .attachments
            .lookup(inbox_seq, &entry.event_id, &raw_name)
        {
            return Response::ok(
                json!({"framed": format::frame_attachment(&entry, &f, true, &label), "path": f.path, "sha256": f.sha256, "cached": true}),
            );
        }
        if let Some(declared) = entry.media.as_ref().and_then(|m| m.size) {
            if let Err(e) = policy.check_size(declared, "declared") {
                self.audit(json!({"event": "fetch_refused", "reason": "too_large", "inbox_seq": inbox_seq, "declared": declared}));
                return Response::err(e);
            }
        }
        let req = FetchRequest {
            event_id: entry.event_id.clone(),
            room_id: entry.room_id.clone(),
            media: entry.media.clone(),
            max_bytes: policy.max_bytes,
        };
        let fetched = match self.transport.fetch(req).await {
            Ok(f) => f,
            Err(e) => {
                self.audit(json!({"event": "fetch_failed", "inbox_seq": inbox_seq, "error": e}));
                return Response::err(format!("attachment {inbox_seq} NOT fetched: {e}"));
            }
        };
        let mimetype = fetched
            .mimetype
            .or_else(|| entry.media.as_ref().and_then(|m| m.mimetype.clone()));
        let stored = self.attachments.store(
            &policy,
            inbox_seq,
            &entry.event_id,
            &raw_name,
            mimetype,
            &fetched.bytes,
        );
        drop(fetched.bytes);
        let removed = self.prune_attachments();
        match stored {
            Ok(f) => {
                tracing::info!(
                    inbox_seq,
                    size = f.size,
                    removed_old = removed,
                    "attachment fetched"
                );
                self.audit(json!({"event": "fetched", "inbox_seq": inbox_seq, "bytes": f.size, "sha256": f.sha256}));
                Response::ok(
                    json!({"framed": format::frame_attachment(&entry, &f, false, &label), "path": f.path, "sha256": f.sha256, "cached": false}),
                )
            }
            Err(e) => {
                self.audit(json!({"event": "fetch_failed", "inbox_seq": inbox_seq, "error": e}));
                Response::err(format!("attachment {inbox_seq} NOT stored: {e}"))
            }
        }
    }
}

fn apply_from(q: &mut Query, f: Option<FromFilter>) {
    match f {
        Some(FromFilter::Sender(s)) => q.sender = Some(s),
        Some(FromFilter::Room(r)) => q.room_id = Some(r),
        None => {}
    }
}

enum Outgoing {
    Text(String),
    File { path: PathBuf, caption: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_paths_are_refused() {
        let state = PathBuf::from("/home/u/.aqua-system-bridge");
        let home = Path::new("/home/u");
        let sensitive_path = |p: &Path, s: &Path| sensitive_path_in(p, s, home);
        assert!(
            sensitive_path(Path::new("/home/u/.aqua-system-bridge/agent.pem"), &state).is_some()
        );
        assert!(sensitive_path(Path::new("/home/u/.ssh/id_ed25519"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/proj/.env"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/proj/prod.env"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/x/server.key"), &state).is_some());
        assert!(sensitive_path(
            Path::new("/home/u/.aqua-matrix-heartbeat/claude-oauth-token"),
            &state
        )
        .is_some());
        let agent_state = PathBuf::from("/home/u/.my-agent/messenger");
        assert!(sensitive_path(
            Path::new("/home/u/.my-agent/messenger/inbox.jsonl"),
            &agent_state
        )
        .is_some());
        assert!(sensitive_path(
            Path::new("/home/u/.aqua-system-bridge/inbox.jsonl"),
            &agent_state
        )
        .is_some());
        assert!(sensitive_path(Path::new("/home/u/proj/report.md"), &state).is_none());
        assert!(sensitive_path(Path::new("/home/u/proj/summary.pdf"), &state).is_none());
    }

    #[test]
    fn reply_to_parses_seq_or_event_id() {
        assert_eq!(parse_reply_to("12"), Ok(ReplyArg::Seq(12)));
        assert_eq!(parse_reply_to(" 7 "), Ok(ReplyArg::Seq(7)));
        assert_eq!(
            parse_reply_to("$abc:server"),
            Ok(ReplyArg::EventId("$abc:server".into()))
        );
        assert!(parse_reply_to("$").is_err());
        assert!(parse_reply_to("$a b").is_err());
        assert!(parse_reply_to("abc").is_err());
        assert!(parse_reply_to("-1").is_err());
        assert!(parse_reply_to(&format!("${}", "a".repeat(300))).is_err());
    }
}
