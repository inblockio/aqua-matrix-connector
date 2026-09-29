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
use aqua_system_bridge::inbox::{Inbox, MediaRef, Query};
use aqua_system_bridge::proto::{self, Request, Response};
use aqua_system_bridge::ratelimit::RateLimiter;
use aqua_system_bridge::{
    MAX_FILE_BYTES, MAX_MESSAGE_BYTES, MAX_WAIT_SECS, RATE_LIMIT_COUNT, RATE_LIMIT_WINDOW_SECS,
};
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
    ) -> Self {
        Self {
            attach_policy,
            attachments: AttachmentStore::new(state_dir.join("attachments")),
            fetch_lock: tokio::sync::Mutex::new(()),
            state_dir: state_dir.to_path_buf(),
            allow: Mutex::new(AllowList::new(state_dir.join("allowlist.toml"))),
            inbox: Mutex::new(Inbox::load(state_dir.join("inbox.jsonl"))),
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

async fn handle(req: Request, shared: &Arc<Shared>) -> Response {
    match req {
        Request::SendMessage {
            to,
            markdown,
            origin,
        } => {
            if markdown.len() > MAX_MESSAGE_BYTES {
                return Response::err(format!(
                    "message is {} bytes; the cap is {MAX_MESSAGE_BYTES}. Send long content with send_file instead.",
                    markdown.len()
                ));
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
        } => {
            let filter = match from.as_deref().map(|f| resolve_from(shared, f)).transpose() {
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
            let mut inbox = shared.inbox.lock().unwrap();
            let entries = inbox.query(&q);
            if mark_read {
                let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                inbox.mark_read(&seqs);
            }
            Response::ok(
                json!({"entries": entries, "high_water": inbox.high_water(), "unread_remaining": inbox.unread_count()}),
            )
        }
        Request::WaitForReply {
            from,
            timeout_s,
            after_seq,
        } => {
            let filter = match resolve_from(shared, &from) {
                Ok(s) => s,
                Err(e) => return Response::err(e),
            };
            let secs = timeout_s.clamp(1, MAX_WAIT_SECS);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
            let mut q = Query {
                since_seq: after_seq,
                unread_only: true,
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
                    let entries = inbox.query(&q);
                    if !entries.is_empty() {
                        let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                        inbox.mark_read(&seqs);
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
                "inbox_unread": inbox.unread_count(),
                "inbox_high_water": inbox.high_water(),
            }))
        }
    }
}

/// Validate recipient + rate limit, queue the send for the Matrix loop and
/// wait for its outcome.
async fn send(
    shared: &Arc<Shared>,
    to: &str,
    kind: SendKind,
    origin: &str,
    info: serde_json::Value,
) -> Response {
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
            return Response::err(refusal);
        }
    };
    if let Err(wait) = shared
        .rate
        .lock()
        .unwrap()
        .try_acquire(&key, Instant::now())
    {
        shared.audit(json!({"event": "send_refused", "reason": "rate_limited", "to": name, "origin": origin}));
        return Response::err(format!(
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
        return Response::err("bridge Matrix loop is not running");
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
            let mut a = json!({"event": "sent", "to": name, "room": is_room, "origin": origin, "event_id": event_id});
            if let (Some(a), Some(i)) = (a.as_object_mut(), info.as_object()) {
                a.extend(i.clone());
            }
            shared.audit(a);
            let mut data = json!({"event_id": event_id, "to_name": name, "to_kind": if is_room { "room" } else { "person" }, "inbox_seq": inbox_seq});
            data[if is_room { "to_room_id" } else { "to_mxid" }] = json!(key);
            Response::ok(data)
        }
        Err(e) => {
            shared.rate.lock().unwrap().release(&key);
            tracing::warn!(to = %name, origin = %origin, "send failed: {e}");
            shared.audit(json!({"event": "send_failed", "to": name, "origin": origin, "error": e}));
            Response::err(format!("NOT delivered to {name}: {e}"))
        }
    }
}

/// `fetch_attachment`: serve the cached copy, or download + decrypt + verify
/// on the live Client and store it (mode 600) under `<state>/attachments/`.
async fn fetch_attachment(shared: &Arc<Shared>, inbox_seq: u64) -> Response {
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

    fn shared_with(allowlist: &str, tag: &str) -> Shared {
        let dir = std::env::temp_dir().join(format!("asb-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("allowlist.toml"), allowlist).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        Shared::new(&dir, tx, AttachmentPolicy::default())
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
