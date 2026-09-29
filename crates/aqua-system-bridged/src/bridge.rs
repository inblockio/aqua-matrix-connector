//! Socket side of the daemon: shared state, request validation (allow-list,
//! size caps, rate limit, sensitive-path guard) and the inbox/wait handlers.
//! Sends are validated here and executed by the Matrix cycle loop
//! ([`crate::matrix`]) on the one live Client.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aqua_system_bridge::allowlist::{is_valid_mxid, AllowList};
use aqua_system_bridge::format;
use aqua_system_bridge::inbox::{Inbox, Query};
use aqua_system_bridge::proto::{self, Request, Response};
use aqua_system_bridge::ratelimit::RateLimiter;
use aqua_system_bridge::{MAX_FILE_BYTES, MAX_MESSAGE_BYTES, MAX_WAIT_SECS, RATE_LIMIT_COUNT, RATE_LIMIT_WINDOW_SECS};
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
    File { path: PathBuf, caption: String },
}

pub struct SendCmd {
    pub to_mxid: String,
    pub kind: SendKind,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Result<String, String>>,
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
    pub cmd_tx: mpsc::Sender<SendCmd>,
}

impl Shared {
    pub fn new(state_dir: &Path, cmd_tx: mpsc::Sender<SendCmd>) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            allow: Mutex::new(AllowList::new(state_dir.join("allowlist.toml"))),
            inbox: Mutex::new(Inbox::load(state_dir.join("inbox.jsonl"))),
            inbox_changed: Notify::new(),
            rate: Mutex::new(RateLimiter::new(RATE_LIMIT_COUNT, Duration::from_secs(RATE_LIMIT_WINDOW_SECS))),
            status: Mutex::new(Status::default()),
            dropped: Mutex::new(std::collections::HashSet::new()),
            cmd_tx,
        }
    }

    /// Append one JSON line to `<state>/audit.jsonl` (sends, refusals, dropped
    /// inbound). Metadata only, never message bodies.
    pub fn audit(&self, mut v: serde_json::Value) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        v["host_ts"] = json!(format::fmt_ts_ms(now));
        let path = self.state_dir.join("audit.jsonl");
        let res = (|| -> std::io::Result<()> {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(&path)?;
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
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let state_dir = state_dir.canonicalize().unwrap_or_else(|_| state_dir.to_path_buf());
    if p.starts_with(&state_dir) {
        return Some("the bridge's own state directory");
    }
    for d in [".ssh", ".gnupg", ".config/gh", ".aws", ".docker", ".claude/.credentials.json", ".aqua-matrix-heartbeat"] {
        if !home.as_os_str().is_empty() && p.starts_with(home.join(d)) {
            return Some("a credentials location");
        }
    }
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if name == ".env" || name.starts_with(".env.") || name.ends_with(".env") || ["pem", "key", "p12", "pfx"].contains(&ext.as_str()) {
        return Some("a key or env file");
    }
    if name.contains("token") || name.contains("secret") || name == "hosts.yml" {
        return Some("a file named like a credential");
    }
    None
}

fn resolve_to(shared: &Shared, to: &str) -> Result<(String, String), String> {
    let mut allow = shared.allow.lock().unwrap();
    allow.reload(false);
    match allow.resolve(to) {
        Some(r) => Ok((r.name.clone(), r.mxid.clone())),
        None => Err(allow.refusal(to)),
    }
}

/// Resolve a `from` filter: an allow-listed name/MXID, or any well-formed MXID
/// (entries of since-removed people stay readable).
fn resolve_from(shared: &Shared, from: &str) -> Result<String, String> {
    let mut allow = shared.allow.lock().unwrap();
    allow.reload(false);
    if let Some(r) = allow.resolve(from) {
        return Ok(r.mxid.clone());
    }
    if is_valid_mxid(from.trim()) {
        return Ok(from.trim().to_string());
    }
    Err(format!("{from:?} is neither an allow-list name nor an MXID"))
}

async fn handle(req: Request, shared: &Arc<Shared>) -> Response {
    match req {
        Request::SendMessage { to, markdown, origin } => {
            if markdown.len() > MAX_MESSAGE_BYTES {
                return Response::err(format!(
                    "message is {} bytes; the cap is {MAX_MESSAGE_BYTES}. Send long content with send_file instead.",
                    markdown.len()
                ));
            }
            let body = format::tag_markdown(&markdown, &origin);
            let bytes = markdown.len();
            send(shared, &to, SendKind::Text(body), &origin, json!({"kind": "text", "bytes": bytes})).await
        }
        Request::SendFile { to, path, caption, origin } => {
            let p = match Path::new(&path).canonicalize() {
                Ok(p) => p,
                Err(e) => return Response::err(format!("cannot open {path}: {e}")),
            };
            if let Some(why) = sensitive_path(&p, &shared.state_dir) {
                shared.audit(json!({"event": "send_refused", "reason": "sensitive_path", "path": p}));
                return Response::err(format!("REFUSED: {} is {why}; the bridge never sends those.", p.display()));
            }
            let meta = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(e) => return Response::err(format!("cannot stat {}: {e}", p.display())),
            };
            if !meta.is_file() {
                return Response::err(format!("{} is not a regular file", p.display()));
            }
            if meta.len() > MAX_FILE_BYTES {
                return Response::err(format!("{} is {} bytes; the cap is {MAX_FILE_BYTES}", p.display(), meta.len()));
            }
            let filename = p.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
            let caption = format::tag_caption(caption.as_deref(), &filename, &origin);
            let info = json!({"kind": "file", "bytes": meta.len(), "filename": filename});
            send(shared, &to, SendKind::File { path: p, caption }, &origin, info).await
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
            Response::ok(json!({
                "recipients": recips,
                "allowlist_file": allow.path(),
                "allowlist_error": allow.load_error(),
                "rate_limit": format!("{RATE_LIMIT_COUNT} sends per {RATE_LIMIT_WINDOW_SECS}s per recipient"),
                "max_message_bytes": MAX_MESSAGE_BYTES,
                "max_file_bytes": MAX_FILE_BYTES,
                "note": "Confirm with Tim before messaging anyone other than Tim, unless he asked for it."
            }))
        }
        Request::ReadInbox { from, since_seq, since_ts_ms, unread_only, mark_read, limit } => {
            let sender = match from.as_deref().map(|f| resolve_from(shared, f)).transpose() {
                Ok(s) => s,
                Err(e) => return Response::err(e),
            };
            let mut inbox = shared.inbox.lock().unwrap();
            let entries = inbox.query(&Query { sender, since_seq, since_ts_ms, unread_only, limit });
            if mark_read {
                let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
                inbox.mark_read(&seqs);
            }
            Response::ok(json!({"entries": entries, "high_water": inbox.high_water(), "unread_remaining": inbox.unread_count()}))
        }
        Request::WaitForReply { from, timeout_s, after_seq } => {
            let sender = match resolve_from(shared, &from) {
                Ok(s) => s,
                Err(e) => return Response::err(e),
            };
            let secs = timeout_s.clamp(1, MAX_WAIT_SECS);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
            let q = Query { sender: Some(sender), since_seq: after_seq, unread_only: true, ..Default::default() };
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
                        return Response::ok(json!({"entries": entries, "waited_s": secs, "high_water": inbox.high_water()}));
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
async fn send(shared: &Arc<Shared>, to: &str, kind: SendKind, origin: &str, info: serde_json::Value) -> Response {
    let origin = format::sanitize_origin(origin);
    let (name, mxid) = match resolve_to(shared, to) {
        Ok(v) => v,
        Err(refusal) => {
            tracing::warn!(to, origin, "send refused: not on the allow-list");
            shared.audit(json!({"event": "send_refused", "reason": "not_allowlisted", "to": to, "origin": origin}));
            return Response::err(refusal);
        }
    };
    if let Err(wait) = shared.rate.lock().unwrap().try_acquire(&mxid, Instant::now()) {
        shared.audit(json!({"event": "send_refused", "reason": "rate_limited", "to": name, "origin": origin}));
        return Response::err(format!(
            "RATE LIMITED: {RATE_LIMIT_COUNT} messages per {} minutes to {name} already used; next slot in {}s. Batch your updates into fewer messages.",
            RATE_LIMIT_WINDOW_SECS / 60,
            wait.as_secs() + 1
        ));
    }
    let inbox_seq = shared.inbox.lock().unwrap().high_water();
    let (tx, rx) = oneshot::channel();
    let cmd = SendCmd { to_mxid: mxid.clone(), kind, deadline: Instant::now() + SEND_DEADLINE, reply: tx };
    if shared.cmd_tx.send(cmd).await.is_err() {
        shared.rate.lock().unwrap().release(&mxid);
        return Response::err("bridge Matrix loop is not running");
    }
    let outcome = match tokio::time::timeout(SEND_DEADLINE + Duration::from_secs(10), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("bridge dropped the request (shutting down?)".to_string()),
        Err(_) => Err("send did not complete in time; outcome unknown".to_string()),
    };
    match outcome {
        Ok(event_id) => {
            tracing::info!(to = %name, origin = %origin, event_id = %event_id, "sent");
            let mut a = json!({"event": "sent", "to": name, "origin": origin, "event_id": event_id});
            if let (Some(a), Some(i)) = (a.as_object_mut(), info.as_object()) {
                a.extend(i.clone());
            }
            shared.audit(a);
            Response::ok(json!({"event_id": event_id, "to_name": name, "to_mxid": mxid, "inbox_seq": inbox_seq}))
        }
        Err(e) => {
            shared.rate.lock().unwrap().release(&mxid);
            tracing::warn!(to = %name, origin = %origin, "send failed: {e}");
            shared.audit(json!({"event": "send_failed", "to": name, "origin": origin, "error": e}));
            Response::err(format!("NOT delivered to {name}: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_paths_are_refused() {
        let state = PathBuf::from("/home/u/.aqua-system-bridge");
        std::env::set_var("HOME", "/home/u");
        assert!(sensitive_path(Path::new("/home/u/.aqua-system-bridge/agent.pem"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/.ssh/id_ed25519"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/proj/.env"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/proj/prod.env"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/x/server.key"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/.aqua-matrix-heartbeat/claude-oauth-token"), &state).is_some());
        assert!(sensitive_path(Path::new("/home/u/proj/report.md"), &state).is_none());
        assert!(sensitive_path(Path::new("/home/u/proj/summary.pdf"), &state).is_none());
    }
}
