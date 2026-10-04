//! Socket side of the daemon: shared state, request validation (allow-list,
//! size caps, rate limit, sensitive-path guard) and the inbox/wait handlers.
//! Sends are validated here and executed by the Matrix cycle loop
//! ([`crate::matrix`]) on the one live Client.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aqua_system_bridge::allowlist::{is_valid_mxid, is_valid_room_id, AllowList, Target};
use aqua_system_bridge::attachments::{AttachmentPolicy, AttachmentStore};
use aqua_system_bridge::format;
use aqua_system_bridge::inbox::{
    Actor, Inbox, InboxEntry, InboxPolicy, MediaRef, NotTracked, Query, State,
};
use aqua_system_bridge::proto::{self, Request, Response};
use aqua_system_bridge::ratelimit::RateLimiter;
use aqua_system_bridge::{
    MAX_FILE_BYTES, MAX_MESSAGE_BYTES, MAX_WAIT_SECS, RATE_LIMIT_COUNT, RATE_LIMIT_WINDOW_SECS,
};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::OwnedEventId;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Notify};

/// How long a queued send may wait for the Matrix loop (reconnect, outage)
/// before it is abandoned unsent. The MCP side waits a little longer.
pub const SEND_DEADLINE: Duration = Duration::from_secs(170);
/// Largest request line accepted on the socket.
const MAX_REQUEST_BYTES: u64 = 256 * 1024;

pub enum SendKind {
    Text(String),
    File {
        path: PathBuf,
        caption: String,
    },
    /// Replace `original` (checked on the live Client to be the bridge's own
    /// text message in the destination room) with `content`.
    Edit {
        original: OwnedEventId,
        content: Box<RoomMessageEventContent>,
    },
    /// Download + decrypt an inbound attachment (`to_mxid` unused).
    Fetch {
        event_id: String,
        room_id: String,
        media: Option<MediaRef>,
        max_bytes: u64,
    },
}

/// Outcome of a command executed on the live Client.
pub enum CmdOk {
    /// A send: the new event id.
    Sent(String),
    /// A fetch: the decrypted, verified bytes and the mime type if known.
    Fetched {
        bytes: Vec<u8>,
        mimetype: Option<String>,
    },
}

/// Where a command goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// A person: their 1:1 DM (resolved by the Matrix loop, never a listed
    /// `[[rooms]]` entry), created if none exists.
    Person(String),
    /// A listed group room, by room id.
    Room(String),
    /// No destination (attachment fetches).
    Nobody,
}

/// What the bridge knows about one joined room, refreshed on every connect
/// (for `list_recipients` and the startup log).
#[derive(Debug, Clone, Default)]
pub struct SeenRoom {
    pub display_name: Option<String>,
    pub joined_members: u64,
    pub is_direct: bool,
}

pub struct SendCmd {
    pub to: Dest,
    pub kind: SendKind,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Result<CmdOk, String>>,
}

#[derive(Default, Clone)]
pub struct Status {
    pub connected: bool,
    pub did: Option<String>,
    pub user_id: Option<String>,
    pub device_id: Option<String>,
    pub last_error: Option<String>,
}

pub struct Shared {
    pub state_dir: PathBuf,
    pub allow: Mutex<AllowList>,
    pub inbox: Mutex<Inbox>,
    /// Woken (notify_waiters) after every successful ingest.
    pub inbox_changed: Notify,
    pub rate: Mutex<RateLimiter>,
    pub status: Mutex<Status>,
    /// Event ids of dropped (non-allow-listed) messages already logged, so a
    /// backfill does not re-log them every cycle.
    pub dropped: Mutex<std::collections::HashSet<String>>,
    /// Joined rooms as of the last connect (room id -> info).
    pub joined_rooms: Mutex<std::collections::BTreeMap<String, SeenRoom>>,
    pub cmd_tx: mpsc::Sender<SendCmd>,
    pub attach_policy: AttachmentPolicy,
    pub attachments: AttachmentStore,
    /// Serialises fetches so two sessions asking for the same entry do not
    /// download it twice.
    pub fetch_lock: tokio::sync::Mutex<()>,
}

impl Shared {
    pub fn new(
        state_dir: &Path,
        cmd_tx: mpsc::Sender<SendCmd>,
        attach_policy: AttachmentPolicy,
        inbox_policy: InboxPolicy,
    ) -> Self {
        Self {
            attach_policy,
            attachments: AttachmentStore::new(state_dir.join("attachments")),
            fetch_lock: tokio::sync::Mutex::new(()),
            state_dir: state_dir.to_path_buf(),
            allow: Mutex::new(AllowList::new(state_dir.join("allowlist.toml"))),
            inbox: Mutex::new(Inbox::load_with(
                state_dir.join("inbox.jsonl"),
                inbox_policy,
                aqua_system_bridge::inbox::now_ms(),
            )),
            inbox_changed: Notify::new(),
            rate: Mutex::new(RateLimiter::new(
                RATE_LIMIT_COUNT,
                Duration::from_secs(RATE_LIMIT_WINDOW_SECS),
            )),
            status: Mutex::new(Status::default()),
            dropped: Mutex::new(std::collections::HashSet::new()),
            joined_rooms: Mutex::new(std::collections::BTreeMap::new()),
            cmd_tx,
        }
    }

    /// Append one JSON line to `<state>/audit.jsonl` (sends, refusals, dropped
    /// inbound). Metadata only, never message bodies.
    pub fn audit(&self, mut v: serde_json::Value) {
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
}

/// How often the inbox policy is enforced while no request or message
/// triggers it (so an idle inbox still ages out).
pub const RETENTION_SWEEP: Duration = Duration::from_secs(300);

/// Enforce the inbox policy on a timer. Only worth running when an age bound is
/// configured: everything else is already enforced on load, on every ingest and
/// before every read.
pub async fn retention_loop(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(RETENTION_SWEEP);
    // After a Modern Standby the missed ticks collapse into one sweep.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        shared.inbox.lock().unwrap().enforce();
    }
}

/// Bind the socket (mode 600). Refuses to start if another daemon answers on
/// it (single-owner rule for the crypto store); removes a stale socket file.
pub fn bind_socket(path: &Path) -> anyhow::Result<UnixListener> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            anyhow::bail!(
                "another aqua-system-bridged is already listening on {}; refusing to start a second owner of the crypto store",
                path.display()
            );
        }
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

pub async fn serve(listener: UnixListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, shared).await {
                        tracing::warn!("socket connection error: {e:#}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("socket accept failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_conn(stream: UnixStream, shared: Arc<Shared>) -> anyhow::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => handle(req, &shared).await,
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    wr.write_all(proto::encode_line(&resp).as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}

/// Paths `send_file` refuses: the bridge's own state (key, store, tokens) and
/// the usual credential locations. Defence in depth: a confused or injected
/// session must not be able to mail out secrets, even to an allowed person.
fn sensitive_path(p: &Path, state_dir: &Path) -> Option<&'static str> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
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

/// A resolved `to`: display name, rate-limit key and destination.
#[derive(Debug)]
struct Resolved {
    name: String,
    /// MXID for a person, room id for a room (also the rate-limit key).
    id: String,
    dest: Dest,
}

fn resolve_to(shared: &Shared, to: &str) -> Result<Resolved, String> {
    let mut allow = shared.allow.lock().unwrap();
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
            if is_valid_room_id(t) && shared.joined_rooms.lock().unwrap().contains_key(t) {
                return Err(format!(
                    "REFUSED: the bridge has joined room {t} but it is not listed under [[rooms]] in {}; \
                     unlisted rooms are not sendable. Add a [[rooms]] entry (only with Tim's approval).",
                    allow.path().display()
                ));
            }
            Err(allow.refusal(to))
        }
    }
}

/// An inbox filter from a `from` argument.
#[derive(Debug)]
enum FromFilter {
    /// DMs from this MXID.
    Sender(String),
    /// Messages in this room.
    Room(String),
}

/// Resolve a `from` filter: an allow-listed person (name/MXID), a listed room
/// (name/room id), or any well-formed MXID / room id (entries of since-removed
/// people and rooms stay readable).
fn resolve_from(shared: &Shared, from: &str) -> Result<FromFilter, String> {
    let mut allow = shared.allow.lock().unwrap();
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

fn apply_from(q: &mut Query, f: Option<FromFilter>) {
    match f {
        Some(FromFilter::Sender(s)) => q.sender = Some(s),
        Some(FromFilter::Room(r)) => q.room_id = Some(r),
        None => {}
    }
}

/// The session behind a read or `mark_processed`, with its labels sanitized
/// (they are stored and shown to every later reader).
fn actor(by: Option<String>, session: Option<String>) -> Actor {
    Actor {
        by: format::sanitize_origin(by.as_deref().unwrap_or("")),
        session: format::sanitize_session(session.as_deref()),
    }
}

async fn handle(req: Request, shared: &Arc<Shared>) -> Response {
    match req {
        Request::SendMessage {
            to,
            markdown,
            origin,
        } => {
            if let Err(e) = check_message_size(&markdown) {
                return Response::err(e);
            }
            let body = format::tag_markdown(&markdown, &origin);
            let bytes = markdown.len();
            send(
                shared,
                &to,
                SendKind::Text(body),
                &origin,
                json!({"kind": "text", "bytes": bytes}),
            )
            .await
        }
        Request::EditMessage {
            to,
            event_id,
            markdown,
            origin,
        } => edit_message(shared, &to, &event_id, &markdown, &origin).await,
        Request::SendFile {
            to,
            path,
            caption,
            origin,
        } => {
            let p = match Path::new(&path).canonicalize() {
                Ok(p) => p,
                Err(e) => return Response::err(format!("cannot open {path}: {e}")),
            };
            if let Some(why) = sensitive_path(&p, &shared.state_dir) {
                shared
                    .audit(json!({"event": "send_refused", "reason": "sensitive_path", "path": p}));
                return Response::err(format!(
                    "REFUSED: {} is {why}; the bridge never sends those.",
                    p.display()
                ));
            }
            let meta = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(e) => return Response::err(format!("cannot stat {}: {e}", p.display())),
            };
            if !meta.is_file() {
                return Response::err(format!("{} is not a regular file", p.display()));
            }
            if meta.len() > MAX_FILE_BYTES {
                return Response::err(format!(
                    "{} is {} bytes; the cap is {MAX_FILE_BYTES}",
                    p.display(),
                    meta.len()
                ));
            }
            let filename = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();
            let caption = format::tag_caption(caption.as_deref(), &filename, &origin);
            let info = json!({"kind": "file", "bytes": meta.len(), "filename": filename});
            send(
                shared,
                &to,
                SendKind::File { path: p, caption },
                &origin,
                info,
            )
            .await
        }
        Request::ListRecipients => {
            let mut allow = shared.allow.lock().unwrap();
            allow.reload(false);
            let mut rate = shared.rate.lock().unwrap();
            let now = Instant::now();
            let recips: Vec<_> = allow
                .recipients()
                .iter()
                .map(|r| json!({"name": r.name, "mxid": r.mxid, "note": r.note, "sends_left_in_window": rate.remaining(&r.mxid, now)}))
                .collect();
            let joined = shared.joined_rooms.lock().unwrap();
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
            Response::ok(json!({
                "recipients": recips,
                "rooms": rooms,
                "unlisted_non_dm_rooms_joined": unlisted,
                "allowlist_file": allow.path(),
                "allowlist_error": allow.load_error(),
                "rate_limit": format!("{RATE_LIMIT_COUNT} sends per {RATE_LIMIT_WINDOW_SECS}s per person or room"),
                "max_message_bytes": MAX_MESSAGE_BYTES,
                "max_file_bytes": MAX_FILE_BYTES,
                "note": "Confirm with Tim before messaging anyone other than Tim, or posting in a room, unless he asked for it. A room post is read by everyone in that room."
            }))
        }
        Request::ReadInbox {
            from,
            since_seq,
            since_ts_ms,
            unread_only,
            mark_read,
            limit,
            by,
            session,
        } => {
            let filter = match from.as_deref().map(|f| resolve_from(shared, f)).transpose() {
                Ok(s) => s,
                Err(e) => return Response::err(e),
            };
            let mut inbox = shared.inbox.lock().unwrap();
            // Never hand out what the inbox policy already dropped.
            inbox.enforce();
            let policy = inbox.policy();
            let mut q = Query {
                since_seq,
                since_ts_ms,
                states: unread_only.then(|| policy.open_states()),
                limit,
                ..Default::default()
            };
            apply_from(&mut q, filter);
            // Shown with the state they had when read: a `new` entry tells
            // this session that no other session has looked at it yet.
            let entries = inbox.query(&q);
            if mark_read {
                let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                inbox.mark_seen(&seqs, &actor(by, session));
            }
            let entries: Vec<_> = entries.iter().map(|e| e.public()).collect();
            Response::ok(json!({
                "entries": entries,
                "high_water": inbox.high_water(),
                "unread_remaining": inbox.unread_count(),
                "open_remaining": inbox.open_count(),
                "track_processed": policy.track_processed,
            }))
        }
        Request::MarkProcessed {
            seqs,
            up_to_seq,
            from,
            note,
            by,
            session,
        } => {
            let Some(note) = format::sanitize_note(&note) else {
                return Response::err(
                    "mark_processed needs a non-empty note (what was done, or why nothing needs doing)",
                );
            };
            if from.is_some() && up_to_seq.is_none() {
                return Response::err(
                    "`from` narrows `up_to_seq`; give up_to_seq as well, or list the seqs",
                );
            }
            let filter = match from.as_deref().map(|f| resolve_from(shared, f)).transpose() {
                Ok(s) => s,
                Err(e) => return Response::err(e),
            };
            let who = actor(by, session);
            let mut inbox = shared.inbox.lock().unwrap();
            if !inbox.policy().track_processed {
                return Response::err(NotTracked.to_string());
            }
            inbox.enforce();
            let mut targets = seqs;
            if let Some(up_to) = up_to_seq {
                let mut q = Query {
                    states: Some(inbox.policy().open_states()),
                    ..Default::default()
                };
                apply_from(&mut q, filter);
                targets.extend(
                    inbox
                        .query(&q)
                        .iter()
                        .map(|e| e.seq)
                        .filter(|&s| s <= up_to),
                );
            }
            if targets.is_empty() {
                return Response::ok(json!({
                    "marked": [], "already_processed": [], "missing": [],
                    "open_remaining": inbox.open_count(),
                    "note": "nothing open matched; nothing changed",
                }));
            }
            let r = match inbox.mark_processed(&targets, &who, &note) {
                Ok(r) => r,
                Err(e) => return Response::err(e.to_string()),
            };
            let already: Vec<_> = r
                .already
                .iter()
                .map(|(seq, m)| json!({"seq": seq, "processed": format::mark_json(m)}))
                .collect();
            shared.audit(json!({
                "event": "inbox_processed",
                "seqs": r.marked,
                "by": who.by,
                "session": who.session,
            }));
            Response::ok(json!({
                "marked": r.marked,
                "already_processed": already,
                "missing": r.missing,
                "open_remaining": inbox.open_count(),
            }))
        }
        Request::WaitForReply {
            from,
            timeout_s,
            after_seq,
            by,
            session,
        } => {
            let who = actor(by, session);
            let filter = match resolve_from(shared, &from) {
                Ok(s) => s,
                Err(e) => return Response::err(e),
            };
            let secs = timeout_s.clamp(1, MAX_WAIT_SECS);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
            let mut q = Query {
                since_seq: after_seq,
                states: Some(vec![State::New]),
                ..Default::default()
            };
            apply_from(&mut q, Some(filter));
            loop {
                // Register interest BEFORE checking, so an ingest between the
                // check and the await cannot be missed.
                let notified = shared.inbox_changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut inbox = shared.inbox.lock().unwrap();
                    inbox.enforce();
                    let entries = inbox.query(&q);
                    if !entries.is_empty() {
                        let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                        inbox.mark_seen(&seqs, &who);
                        let entries: Vec<_> = entries.iter().map(|e| e.public()).collect();
                        return Response::ok(json!({
                            "entries": entries,
                            "waited_s": secs,
                            "high_water": inbox.high_water(),
                            "track_processed": inbox.policy().track_processed,
                        }));
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
        Request::FetchAttachment { inbox_seq } => fetch_attachment(shared, inbox_seq).await,
        Request::Status => {
            let st = shared.status.lock().unwrap().clone();
            let inbox = shared.inbox.lock().unwrap();
            Response::ok(json!({
                "connected": st.connected,
                "did": st.did,
                "mxid": st.user_id,
                "device_id": st.device_id,
                "last_error": st.last_error,
                "inbox_entries": inbox.len(),
                "inbox_max_entries": inbox.policy().max_entries,
                "inbox_max_age_hours": inbox.policy().max_age.map(|a| a.as_secs() / 3600),
                "inbox_hard_cap": inbox.policy().hard_cap,
                "inbox_accept_media": inbox.policy().accept_media,
                "inbox_track_processed": inbox.policy().track_processed,
                "inbox_unread": inbox.unread_count(),
                "inbox_new": inbox.count(State::New),
                "inbox_seen": inbox.count(State::Seen),
                "inbox_processed": inbox.count(State::Processed),
                "inbox_high_water": inbox.high_water(),
            }))
        }
    }
}

fn check_message_size(markdown: &str) -> Result<(), String> {
    if markdown.len() > MAX_MESSAGE_BYTES {
        return Err(format!(
            "message is {} bytes; the cap is {MAX_MESSAGE_BYTES}. Send long content with send_file instead.",
            markdown.len()
        ));
    }
    Ok(())
}

/// `edit_message`: validate and build the replacement here (size caps before
/// any rate-limit slot is taken), then deliver it like a send. Whether the
/// original may be edited is checked on the live Client in the destination
/// room ([`crate::edit::ensure_editable`]).
async fn edit_message(
    shared: &Arc<Shared>,
    to: &str,
    event_id: &str,
    markdown: &str,
    origin: &str,
) -> Response {
    if let Err(e) = check_message_size(markdown) {
        return Response::err(e);
    }
    let original = match OwnedEventId::try_from(event_id.trim()) {
        Ok(id) => id,
        Err(e) => return Response::err(format!("invalid event id {event_id:?}: {e}")),
    };
    let content =
        crate::edit::replacement(&format::tag_markdown(markdown, origin), original.clone());
    if let Err(e) = crate::edit::check_size(&content) {
        return Response::err(e);
    }
    let info = json!({"kind": "edit", "bytes": markdown.len(), "replaces": original.as_str()});
    let kind = SendKind::Edit {
        original: original.clone(),
        content: Box::new(content),
    };
    match deliver(shared, to, kind, origin, info).await {
        Ok(d) => Response::ok(json!({"event_id": d.event_id, "replaces": original.as_str()})),
        Err(e) => Response::err(e),
    }
}

/// [`deliver`] a new message and answer with the send response shape.
async fn send(
    shared: &Arc<Shared>,
    to: &str,
    kind: SendKind,
    origin: &str,
    info: serde_json::Value,
) -> Response {
    let d = match deliver(shared, to, kind, origin, info).await {
        Ok(d) => d,
        Err(e) => return Response::err(e),
    };
    let mut data = json!({"event_id": d.event_id, "to_name": d.name, "to_kind": if d.is_room { "room" } else { "person" }, "inbox_seq": d.inbox_seq});
    data[if d.is_room { "to_room_id" } else { "to_mxid" }] = json!(d.id);
    Response::ok(data)
}

/// A command the Matrix loop executed.
struct Delivered {
    event_id: String,
    name: String,
    /// MXID for a person, room id for a room.
    id: String,
    is_room: bool,
    /// Inbox high-water mark when the command was queued.
    inbox_seq: u64,
}

/// Validate recipient + rate limit, queue the command for the Matrix loop and
/// wait for its outcome. Every outcome is audited; a failure releases the
/// rate-limit slot and comes back as the caller-facing error text.
async fn deliver(
    shared: &Arc<Shared>,
    to: &str,
    kind: SendKind,
    origin: &str,
    info: serde_json::Value,
) -> Result<Delivered, String> {
    let origin = format::sanitize_origin(origin);
    let Resolved {
        name,
        id: key,
        dest,
    } = match resolve_to(shared, to) {
        Ok(v) => v,
        Err(refusal) => {
            tracing::warn!(to, origin, "send refused: not on the allow-list");
            shared.audit(json!({"event": "send_refused", "reason": "not_allowlisted", "to": to, "origin": origin}));
            return Err(refusal);
        }
    };
    if let Err(wait) = shared
        .rate
        .lock()
        .unwrap()
        .try_acquire(&key, Instant::now())
    {
        shared.audit(json!({"event": "send_refused", "reason": "rate_limited", "to": name, "origin": origin}));
        return Err(format!(
            "RATE LIMITED: {RATE_LIMIT_COUNT} messages per {} minutes to {name} already used; next slot in {}s. Batch your updates into fewer messages.",
            RATE_LIMIT_WINDOW_SECS / 60,
            wait.as_secs() + 1
        ));
    }
    let inbox_seq = shared.inbox.lock().unwrap().high_water();
    let (tx, rx) = oneshot::channel();
    let is_room = matches!(dest, Dest::Room(_));
    let cmd = SendCmd {
        to: dest,
        kind,
        deadline: Instant::now() + SEND_DEADLINE,
        reply: tx,
    };
    if shared.cmd_tx.send(cmd).await.is_err() {
        shared.rate.lock().unwrap().release(&key);
        return Err("bridge Matrix loop is not running".to_string());
    }
    let outcome = match tokio::time::timeout(SEND_DEADLINE + Duration::from_secs(10), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("bridge dropped the request (shutting down?)".to_string()),
        Err(_) => Err("send did not complete in time; outcome unknown".to_string()),
    };
    let outcome = outcome.and_then(|ok| match ok {
        CmdOk::Sent(id) => Ok(id),
        CmdOk::Fetched { .. } => {
            Err("internal error: send answered with a fetch result".to_string())
        }
    });
    match outcome {
        Ok(event_id) => {
            tracing::info!(to = %name, origin = %origin, event_id = %event_id, "sent");
            shared.audit(with_info(
                json!({"event": "sent", "to": name, "room": is_room, "origin": origin, "event_id": event_id}),
                &info,
            ));
            Ok(Delivered {
                event_id,
                name,
                id: key,
                is_room,
                inbox_seq,
            })
        }
        Err(e) => {
            shared.rate.lock().unwrap().release(&key);
            tracing::warn!(to = %name, origin = %origin, "send failed: {e}");
            shared.audit(with_info(
                json!({"event": "send_failed", "to": name, "origin": origin, "error": e}),
                &info,
            ));
            Err(format!("NOT delivered to {name}: {e}"))
        }
    }
}

fn with_info(mut record: serde_json::Value, info: &serde_json::Value) -> serde_json::Value {
    if let (Some(r), Some(i)) = (record.as_object_mut(), info.as_object()) {
        r.extend(i.clone());
    }
    record
}

/// Whether an entry's attachment may be fetched now: it was posted in a
/// listed `[[rooms]]` room (by any member), or sent by a person who is on the
/// allow-list. Re-checked at fetch time, not only at ingest, so removing a
/// person or room from the list also stops downloads (and cache hits) of what
/// they sent before. A broken allow-list refuses everything.
fn fetch_permitted(shared: &Shared, entry: &InboxEntry) -> Result<(), String> {
    let mut allow = shared.allow.lock().unwrap();
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

/// `fetch_attachment`: serve the cached copy, or download + decrypt + verify
/// on the live Client and store it (mode 600) under `<state>/attachments/`.
async fn fetch_attachment(shared: &Arc<Shared>, inbox_seq: u64) -> Response {
    if !shared.inbox.lock().unwrap().policy().accept_media {
        return Response::err(
            "REFUSED: this bridge is configured not to accept inbound media \
             (AQUA_SYSTEM_BRIDGE_INBOUND_MEDIA=refuse); there is nothing to fetch",
        );
    }
    let _guard = shared.fetch_lock.lock().await;
    let entry = match shared.inbox.lock().unwrap().get(inbox_seq).cloned() {
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
    if let Err(e) = fetch_permitted(shared, &entry) {
        shared.audit(json!({"event": "fetch_refused", "reason": "not_allowed", "inbox_seq": inbox_seq, "from": entry.sender, "room": entry.room_id}));
        return Response::err(e);
    }
    let raw_name = entry.filename.clone().unwrap_or_else(|| entry.body.clone());
    let policy = shared.attach_policy;
    if let Some(f) = shared
        .attachments
        .lookup(inbox_seq, &entry.event_id, &raw_name)
    {
        return Response::ok(
            json!({"framed": format::frame_attachment(&entry, &f, true), "path": f.path, "sha256": f.sha256, "cached": true}),
        );
    }
    if let Some(declared) = entry.media.as_ref().and_then(|m| m.size) {
        if let Err(e) = policy.check_size(declared, "declared") {
            shared.audit(json!({"event": "fetch_refused", "reason": "too_large", "inbox_seq": inbox_seq, "declared": declared}));
            return Response::err(e);
        }
    }
    let (tx, rx) = oneshot::channel();
    let cmd = SendCmd {
        to: Dest::Nobody,
        kind: SendKind::Fetch {
            event_id: entry.event_id.clone(),
            room_id: entry.room_id.clone(),
            media: entry.media.clone(),
            max_bytes: policy.max_bytes,
        },
        deadline: Instant::now() + SEND_DEADLINE,
        reply: tx,
    };
    if shared.cmd_tx.send(cmd).await.is_err() {
        return Response::err("bridge Matrix loop is not running");
    }
    let outcome = match tokio::time::timeout(SEND_DEADLINE + Duration::from_secs(10), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("bridge dropped the request (shutting down?)".to_string()),
        Err(_) => Err("fetch did not complete in time".to_string()),
    };
    let (bytes, mimetype) = match outcome {
        Ok(CmdOk::Fetched { bytes, mimetype }) => (bytes, mimetype),
        Ok(CmdOk::Sent(_)) => {
            return Response::err("internal error: fetch answered with a send result")
        }
        Err(e) => {
            shared.audit(json!({"event": "fetch_failed", "inbox_seq": inbox_seq, "error": e}));
            return Response::err(format!("attachment {inbox_seq} NOT fetched: {e}"));
        }
    };
    let mimetype = mimetype.or_else(|| entry.media.as_ref().and_then(|m| m.mimetype.clone()));
    let stored = shared.attachments.store(
        &policy,
        inbox_seq,
        &entry.event_id,
        &raw_name,
        mimetype,
        &bytes,
    );
    drop(bytes);
    let removed = shared
        .attachments
        .prune(policy.retention_days, SystemTime::now());
    match stored {
        Ok(f) => {
            tracing::info!(
                inbox_seq,
                size = f.size,
                removed_old = removed,
                "attachment fetched"
            );
            shared.audit(json!({"event": "fetched", "inbox_seq": inbox_seq, "bytes": f.size, "sha256": f.sha256}));
            Response::ok(
                json!({"framed": format::frame_attachment(&entry, &f, false), "path": f.path, "sha256": f.sha256, "cached": false}),
            )
        }
        Err(e) => {
            shared.audit(json!({"event": "fetch_failed", "inbox_seq": inbox_seq, "error": e}));
            Response::err(format!("attachment {inbox_seq} NOT stored: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_dir_with(allowlist: &str, tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asb-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("allowlist.toml"), allowlist).unwrap();
        dir
    }

    fn shared_with(allowlist: &str, tag: &str) -> Shared {
        let (tx, _rx) = mpsc::channel(1);
        Shared::new(
            &state_dir_with(allowlist, tag),
            tx,
            AttachmentPolicy::default(),
            InboxPolicy::default(),
        )
    }

    /// A Shared whose Matrix loop is a stub: every edit is answered with event
    /// `$new:x`, except originals named `$foreign...`, which it refuses the
    /// way `edit::check_editable` does. Returns what the loop was asked to do.
    fn shared_with_stub_loop(allowlist: &str, tag: &str) -> (Arc<Shared>, Arc<Mutex<Vec<String>>>) {
        let (tx, mut rx) = mpsc::channel::<SendCmd>(4);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let SendKind::Edit { original, content } = &cmd.kind else {
                    let _ = cmd.reply.send(Err("stub only handles edits".into()));
                    continue;
                };
                log.lock()
                    .unwrap()
                    .push(format!("{original} in {:?}: {}", cmd.to, content.body()));
                let outcome = if original.as_str().starts_with("$foreign") {
                    Err(format!("REFUSED: event {original} was sent by @tim:x"))
                } else {
                    Ok(CmdOk::Sent("$new:x".into()))
                };
                let _ = cmd.reply.send(outcome);
            }
        });
        let sh = Shared::new(
            &state_dir_with(allowlist, tag),
            tx,
            AttachmentPolicy::default(),
            InboxPolicy::default(),
        );
        (Arc::new(sh), seen)
    }

    fn edit(to: &str, event_id: &str, markdown: &str) -> Request {
        Request::EditMessage {
            to: to.into(),
            event_id: event_id.into(),
            markdown: markdown.into(),
            origin: "trains@nuc10".into(),
        }
    }

    #[tokio::test]
    async fn edit_answers_with_the_contract_and_counts_as_one_send() {
        let (sh, seen) = shared_with_stub_loop(LIST, "edit-ok");
        let r = handle(edit("daily-updates", "$orig:x", "# Report v2"), &sh).await;
        assert!(r.ok, "{r:?}");
        assert_eq!(r.data, json!({"event_id": "$new:x", "replaces": "$orig:x"}));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["$orig:x in Room(\"!daily:x\"): * # Report v2\n\n<sub>via `trains@nuc10`</sub>"]
        );
        let left = sh
            .rate
            .lock()
            .unwrap()
            .remaining("!daily:x", Instant::now());
        assert_eq!(left, RATE_LIMIT_COUNT - 1);
        let audit = std::fs::read_to_string(sh.state_dir.join("audit.jsonl")).unwrap();
        let last: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert_eq!(last["event"], "sent");
        assert_eq!(last["kind"], "edit");
        assert_eq!(last["event_id"], "$new:x");
        assert_eq!(last["replaces"], "$orig:x");
        assert!(
            !audit.contains("Report v2"),
            "audit holds no bodies: {audit}"
        );
    }

    #[tokio::test]
    async fn refused_edits_take_no_rate_slot() {
        let (sh, seen) = shared_with_stub_loop(LIST, "edit-refused");
        let e = handle(edit("mallory", "$o:x", "x"), &sh)
            .await
            .error
            .unwrap();
        assert!(e.starts_with("REFUSED"), "{e}");
        let e = handle(edit("tim", "not-an-event-id", "x"), &sh)
            .await
            .error
            .unwrap();
        assert!(e.contains("invalid event id"), "{e}");
        let too_long = "x".repeat(MAX_MESSAGE_BYTES + 1);
        let e = handle(edit("tim", "$o:x", &too_long), &sh)
            .await
            .error
            .unwrap();
        assert!(e.contains("the cap is 20000"), "{e}");
        // within the send_message cap, but too large as one edit event
        let prose = "word ".repeat(MAX_MESSAGE_BYTES / 5);
        let e = handle(edit("tim", "$o:x", &prose), &sh)
            .await
            .error
            .unwrap();
        assert!(e.contains("the edit would be"), "{e}");
        assert!(seen.lock().unwrap().is_empty());
        // refused on the Matrix side: the slot is released again
        let e = handle(edit("daily-updates", "$foreign:x", "x"), &sh)
            .await
            .error
            .unwrap();
        assert!(
            e.starts_with("NOT delivered to daily-updates: REFUSED"),
            "{e}"
        );
        let mut rate = sh.rate.lock().unwrap();
        assert_eq!(rate.remaining("!daily:x", Instant::now()), RATE_LIMIT_COUNT);
        assert_eq!(rate.remaining("@tim:x", Instant::now()), RATE_LIMIT_COUNT);
    }

    const LIST: &str = r#"
[[recipients]]
name = "tim"
mxid = "@tim:x"

[[rooms]]
name = "daily-updates"
room_id = "!daily:x"
"#;

    #[test]
    fn to_resolves_person_room_or_refuses() {
        let sh = shared_with(LIST, "to");
        let p = resolve_to(&sh, "tim").unwrap();
        assert_eq!(p.dest, Dest::Person("@tim:x".into()));
        assert_eq!(p.id, "@tim:x");
        let r = resolve_to(&sh, "daily-updates").unwrap();
        assert_eq!(r.dest, Dest::Room("!daily:x".into()));
        assert_eq!(r.id, "!daily:x");
        assert_eq!(resolve_to(&sh, "!daily:x").unwrap().name, "daily-updates");
        assert!(resolve_to(&sh, "mallory")
            .unwrap_err()
            .starts_with("REFUSED"));
        // a joined but unlisted room is refused with a specific reason
        sh.joined_rooms
            .lock()
            .unwrap()
            .insert("!internal:x".into(), SeenRoom::default());
        let e = resolve_to(&sh, "!internal:x").unwrap_err();
        assert!(e.contains("not listed under [[rooms]]"), "{e}");
    }

    #[test]
    fn from_resolves_to_sender_or_room_filter() {
        let sh = shared_with(LIST, "from");
        assert!(matches!(resolve_from(&sh, "tim"), Ok(FromFilter::Sender(s)) if s == "@tim:x"));
        assert!(
            matches!(resolve_from(&sh, "daily-updates"), Ok(FromFilter::Room(r)) if r == "!daily:x")
        );
        // since-removed people/rooms stay readable by id
        assert!(matches!(
            resolve_from(&sh, "@old:x"),
            Ok(FromFilter::Sender(_))
        ));
        assert!(matches!(
            resolve_from(&sh, "!old:x"),
            Ok(FromFilter::Room(_))
        ));
        assert!(resolve_from(&sh, "nobody").is_err());
    }

    fn add_file(sh: &Shared, ev: &str, room_id: &str, sender: &str, room: Option<&str>) -> u64 {
        let e = aqua_system_bridge::inbox::NewEntry {
            event_id: ev.into(),
            room_id: room_id.into(),
            sender: sender.into(),
            sender_name: None,
            room: room.map(Into::into),
            ts_ms: 1,
            kind: "file".into(),
            body: "a.pdf".into(),
            filename: Some("a.pdf".into()),
            media: Some(MediaRef {
                source: json!({"file": {"url": "mxc://x/secretmedia", "key": {"k": "SECRETKEY"}}}),
                mimetype: Some("application/pdf".into()),
                size: Some(10),
            }),
        };
        sh.inbox.lock().unwrap().ingest(e).unwrap()
    }

    #[tokio::test]
    async fn fetch_gate_allows_listed_rooms_and_people_only() {
        let sh = Arc::new(shared_with(LIST, "fetchgate"));
        // any member of a listed room; an allow-listed person (MXID case-insensitive)
        let room = add_file(&sh, "$r", "!daily:x", "@stranger:x", Some("daily-updates"));
        let tim = add_file(&sh, "$t", "!dm:x", "@TIM:x", None);
        // a non-listed sender outside a listed room (e.g. removed since ingest)
        let other = add_file(&sh, "$o", "!dm2:x", "@stranger:x", None);
        for seq in [room, tim] {
            let r = fetch_attachment(&sh, seq).await;
            let e = r.error.unwrap();
            // passed the gate; failed only because no Matrix loop runs here
            assert!(e.contains("Matrix loop is not running"), "{seq}: {e}");
        }
        let e = fetch_attachment(&sh, other).await.error.unwrap();
        assert!(e.starts_with("REFUSED"), "{e}");
        // removing tim from the list stops fetches of what he sent earlier
        std::fs::write(
            sh.state_dir.join("allowlist.toml"),
            "[[rooms]]\nname = \"daily-updates\"\nroom_id = \"!daily:x\"\n",
        )
        .unwrap();
        sh.allow.lock().unwrap().reload(true);
        let e = fetch_attachment(&sh, tim).await.error.unwrap();
        assert!(e.starts_with("REFUSED"), "{e}");
    }

    #[tokio::test]
    async fn inbox_output_never_carries_the_media_source() {
        let sh = Arc::new(shared_with(LIST, "public"));
        add_file(&sh, "$r", "!daily:x", "@stranger:x", Some("daily-updates"));
        let r = handle(
            Request::ReadInbox {
                from: Some("daily-updates".into()),
                since_seq: None,
                since_ts_ms: None,
                unread_only: false,
                mark_read: false,
                limit: None,
                by: None,
                session: None,
            },
            &sh,
        )
        .await;
        let out = serde_json::to_string(&r).unwrap();
        assert!(out.contains("a.pdf"), "{out}");
        assert!(
            !out.contains("SECRETKEY") && !out.contains("secretmedia"),
            "{out}"
        );
        let r = handle(
            Request::WaitForReply {
                from: "daily-updates".into(),
                timeout_s: 1,
                after_seq: None,
                by: None,
                session: None,
            },
            &sh,
        )
        .await;
        let out = serde_json::to_string(&r).unwrap();
        assert!(out.contains("a.pdf"), "{out}");
        assert!(
            !out.contains("SECRETKEY") && !out.contains("secretmedia"),
            "{out}"
        );
    }

    fn shared_with_inbox_policy(policy: InboxPolicy, tag: &str) -> Arc<Shared> {
        let (tx, _rx) = mpsc::channel(1);
        Arc::new(Shared::new(
            &state_dir_with(LIST, tag),
            tx,
            AttachmentPolicy::default(),
            policy,
        ))
    }

    fn read_all() -> Request {
        Request::ReadInbox {
            from: None,
            since_seq: None,
            since_ts_ms: None,
            unread_only: false,
            mark_read: false,
            limit: None,
            by: None,
            session: None,
        }
    }

    fn add_text(sh: &Shared, ev: &str, room_id: &str, sender: &str, room: Option<&str>) -> u64 {
        let e = aqua_system_bridge::inbox::NewEntry {
            event_id: ev.into(),
            room_id: room_id.into(),
            sender: sender.into(),
            sender_name: None,
            room: room.map(Into::into),
            ts_ms: 1,
            kind: "text".into(),
            body: format!("body {ev}"),
            filename: None,
            media: None,
        };
        sh.inbox.lock().unwrap().ingest(e).unwrap()
    }

    /// A read as the MCP server sends it: `open` = no `since` (unread_only),
    /// otherwise a history read of everything.
    fn read(from: Option<&str>, open: bool, by: Option<&str>) -> Request {
        Request::ReadInbox {
            from: from.map(Into::into),
            since_seq: None,
            since_ts_ms: None,
            unread_only: open,
            mark_read: true,
            limit: None,
            by: by.map(Into::into),
            session: by.map(|_| "ABC-123".into()),
        }
    }

    fn processed(
        seqs: &[u64],
        up_to: Option<u64>,
        from: Option<&str>,
        note: &str,
        by: &str,
    ) -> Request {
        Request::MarkProcessed {
            seqs: seqs.to_vec(),
            up_to_seq: up_to,
            from: from.map(Into::into),
            note: note.into(),
            by: Some(by.into()),
            session: None,
        }
    }

    fn seqs_of(r: &Response) -> Vec<u64> {
        assert!(r.ok, "{r:?}");
        r.data["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn default_instance_keeps_unread_semantics_and_refuses_processing() {
        let sh = Arc::new(shared_with(LIST, "untracked"));
        let dm1 = add_text(&sh, "$d1", "!dm:x", "@tim:x", None);
        let r = handle(read(None, true, Some("repo@host")), &sh).await;
        assert_eq!(seqs_of(&r), vec![dm1]);
        assert_eq!(r.data["track_processed"], false);
        // read = gone from the open view, exactly as before the states
        assert!(seqs_of(&handle(read(None, true, None), &sh).await).is_empty());
        // ... but who read it is recorded state
        let seen = sh.inbox.lock().unwrap().get(dm1).unwrap().seen.clone();
        assert_eq!(seen.unwrap().by, "repo@host");
        let e = handle(processed(&[dm1], None, None, "done", "repo@host"), &sh).await;
        assert!(
            e.error
                .as_deref()
                .unwrap()
                .contains("does not track processed"),
            "{e:?}"
        );
        let e = handle(processed(&[], Some(9), None, "done", "repo@host"), &sh).await;
        assert!(!e.ok);
        assert_eq!(sh.inbox.lock().unwrap().count(State::Processed), 0);
        let st = handle(Request::Status, &sh).await.data;
        assert_eq!(st["inbox_track_processed"], false);
    }

    #[tokio::test]
    async fn handling_states_through_the_socket_ops() {
        let tracking = InboxPolicy {
            track_processed: true,
            ..InboxPolicy::default()
        };
        let sh = shared_with_inbox_policy(tracking, "states");
        let dm1 = add_text(&sh, "$d1", "!dm:x", "@tim:x", None);
        let r1 = add_text(&sh, "$r1", "!daily:x", "@bob:x", Some("daily-updates"));
        let r2 = add_text(&sh, "$r2", "!daily:x", "@bob:x", Some("daily-updates"));
        let dm2 = add_text(&sh, "$d2", "!dm:x", "@tim:x", None);

        // an MCP server from before the states (no by/session): its unread
        // read gets the instance's open view, and seen messages stay open
        let old = handle(read(Some("tim"), true, None), &sh).await;
        assert_eq!(seqs_of(&old), vec![dm1, dm2]);
        assert_eq!(
            seqs_of(&handle(read(Some("tim"), true, None), &sh).await),
            vec![dm1, dm2]
        );

        let r = handle(read(None, true, Some("repo@host")), &sh).await;
        assert_eq!(seqs_of(&r), vec![dm1, r1, r2, dm2]);
        assert_eq!(r.data["open_remaining"], 4);
        assert_eq!(r.data["track_processed"], true);
        {
            let ib = sh.inbox.lock().unwrap();
            assert_eq!(
                ib.get(dm1).unwrap().seen.as_ref().unwrap().by,
                "unknown session"
            );
            let m = ib.get(r1).unwrap().seen.clone().unwrap();
            assert_eq!(
                (m.by.as_str(), m.session.as_deref()),
                ("repo@host", Some("abc-123"))
            );
        }

        // settle the room backlog in one call, then one DM
        let p = handle(
            processed(
                &[],
                Some(r2),
                Some("daily-updates"),
                "room chatter, no action",
                "repo@host",
            ),
            &sh,
        )
        .await;
        assert!(p.ok, "{p:?}");
        assert_eq!(p.data["marked"], json!([r1, r2]));
        assert_eq!(p.data["open_remaining"], 2);
        let p = handle(
            processed(&[dm1], None, None, "answered in DM", "repo@host"),
            &sh,
        )
        .await;
        assert_eq!(p.data["marked"], json!([dm1]));

        // a second session cannot redo or overwrite it
        let p = handle(
            processed(&[dm1, 99], None, None, "answered again", "other@host"),
            &sh,
        )
        .await;
        assert_eq!(p.data["marked"], json!([]));
        assert_eq!(p.data["missing"], json!([99]));
        assert_eq!(p.data["already_processed"][0]["seq"], dm1);
        assert_eq!(
            p.data["already_processed"][0]["processed"]["by"],
            "repo@host"
        );
        assert_eq!(
            p.data["already_processed"][0]["processed"]["note"],
            "answered in DM"
        );

        // open reads no longer return processed messages; history still does
        assert_eq!(
            seqs_of(&handle(read(None, true, Some("x@h")), &sh).await),
            vec![dm2]
        );
        let all = handle(read(None, false, Some("x@h")), &sh).await;
        assert_eq!(seqs_of(&all), vec![dm1, r1, r2, dm2]);
        assert_eq!(
            all.data["entries"][0]["processed"]["note"],
            "answered in DM"
        );

        // refusals
        let e = handle(processed(&[dm2], None, None, " \n ", "repo@host"), &sh).await;
        assert!(e.error.unwrap().contains("non-empty note"));
        let e = handle(processed(&[dm2], None, Some("tim"), "x", "repo@host"), &sh).await;
        assert!(e.error.unwrap().contains("up_to_seq"));
        let e = handle(
            processed(&[], Some(9), Some("nobody here"), "x", "repo@host"),
            &sh,
        )
        .await;
        assert!(!e.ok);

        let st = handle(Request::Status, &sh).await.data;
        assert_eq!(
            (
                st["inbox_new"].as_u64(),
                st["inbox_seen"].as_u64(),
                st["inbox_processed"].as_u64()
            ),
            (Some(0), Some(1), Some(3))
        );
        let audit = std::fs::read_to_string(sh.state_dir.join("audit.jsonl")).unwrap();
        assert!(audit.contains("\"inbox_processed\""), "{audit}");
    }

    #[tokio::test]
    async fn media_refusing_instance_refuses_fetch_and_reports_it() {
        let refuse = InboxPolicy {
            accept_media: false,
            ..InboxPolicy::default()
        };
        let sh = shared_with_inbox_policy(refuse, "nomedia");
        let e = fetch_attachment(&sh, 1).await.error.unwrap();
        assert!(
            e.starts_with("REFUSED") && e.contains("not to accept inbound media"),
            "{e}"
        );
        let data = handle(Request::Status, &sh).await.data;
        assert_eq!(data["inbox_accept_media"], false);
        // the default instance reports media accepted and keeps the fetch path
        let open = Arc::new(shared_with(LIST, "media-default"));
        assert_eq!(
            handle(Request::Status, &open).await.data["inbox_accept_media"],
            true
        );
        let e = fetch_attachment(&open, 1).await.error.unwrap();
        assert!(e.contains("no inbox entry"), "{e}");
    }

    #[tokio::test]
    async fn read_inbox_never_returns_entries_past_the_age_bound() {
        use aqua_system_bridge::inbox::{now_ms, NewEntry};
        const H: u64 = 3_600_000;
        let policy = InboxPolicy {
            max_age: Some(Duration::from_secs(24 * 3600)),
            ..InboxPolicy::default()
        };
        let sh = shared_with_inbox_policy(policy, "age");
        let now = now_ms();
        let entry = |id: &str, age_ms: u64| NewEntry {
            event_id: id.into(),
            room_id: "!dm:x".into(),
            sender: "@tim:x".into(),
            sender_name: Some("tim".into()),
            room: None,
            ts_ms: now - age_ms,
            kind: "text".into(),
            body: id.into(),
            filename: None,
            media: None,
        };
        {
            let mut ib = sh.inbox.lock().unwrap();
            // already past 24 h: refused outright
            assert_eq!(ib.ingest(entry("$stale", 30 * H)), None);
            assert!(ib.ingest(entry("$fresh", H)).is_some());
            // 30 h old, but ingested "as of" 29 h ago, i.e. inside the bound
            // then and aged out since (what happens between two sweeps)
            assert!(ib
                .ingest_at(entry("$aging", 30 * H), now - 29 * H)
                .is_some());
            assert_eq!(ib.len(), 2);
        }
        let out = serde_json::to_string(&handle(read_all(), &sh).await).unwrap();
        assert!(out.contains("$fresh") && !out.contains("$aging"), "{out}");
        assert_eq!(sh.inbox.lock().unwrap().len(), 1);
        let data = handle(Request::Status, &sh).await.data;
        assert_eq!(data["inbox_max_age_hours"], 24);
        assert_eq!(data["inbox_max_entries"], 5000);
        // the default instance has no age bound
        let open = Arc::new(shared_with(LIST, "age-default"));
        assert!(handle(Request::Status, &open).await.data["inbox_max_age_hours"].is_null());
    }

    #[test]
    fn broken_allowlist_fails_closed_for_rooms_too() {
        let bad = format!("{LIST}\n[[rooms]]\nname = \"x\"\nroom_id = \"not-a-room\"\n");
        let sh = shared_with(&bad, "bad");
        assert!(resolve_to(&sh, "tim").is_err());
        assert!(resolve_to(&sh, "daily-updates").is_err());
    }

    #[test]
    fn sensitive_paths_are_refused() {
        let state = PathBuf::from("/home/u/.aqua-system-bridge");
        std::env::set_var("HOME", "/home/u");
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
        assert!(sensitive_path(Path::new("/home/u/proj/report.md"), &state).is_none());
        assert!(sensitive_path(Path::new("/home/u/proj/summary.pdf"), &state).is_none());
    }
}
