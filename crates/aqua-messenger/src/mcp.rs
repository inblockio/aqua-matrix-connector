//! The stdio MCP server loop, shared by every messenger front end.
//!
//! It maps MCP `tools/call` arguments to one [`Request`], hands it to a
//! [`Backend`] and renders the [`Response`] for the model (inbound content in
//! untrusted framing). Two backends exist:
//!
//! - [`SocketBackend`]: one JSON line per request over a unix socket to a
//!   process that owns the Matrix Client (the host `aqua-system-bridged`, or an
//!   embedded agent serving its engine with [`crate::server::serve`]);
//! - [`Engine`] itself (in-process), for callers that hold the engine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::engine::{Engine, Transport};
use crate::format::{self, Since};
use crate::inbox::InboxEntry;
use crate::jsonrpc::{self, Action};
use crate::proto::{self, Request, Response};

/// Timeout for sends and fetches: they may queue behind a reconnect (token
/// rotation, short network blip) before the Matrix side executes them.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(200);
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(20);

/// Something that answers messenger requests.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn call(&self, req: Request, timeout: Duration) -> Result<Response, String>;
}

/// A backend reached over a unix socket (one request per connection).
pub struct SocketBackend {
    sock: PathBuf,
    /// What the socket leads to, in errors ("the messenger backend").
    what: String,
    /// Shown when the socket cannot be reached ("systemctl --user status ...").
    hint: String,
}

impl SocketBackend {
    pub fn new(sock: PathBuf, hint: impl Into<String>) -> Self {
        Self {
            sock,
            what: "the messenger backend".into(),
            hint: hint.into(),
        }
    }

    /// Name the backend in error texts ("the Aqua System bridge daemon").
    pub fn named(mut self, what: impl Into<String>) -> Self {
        self.what = what.into();
        self
    }

    pub fn path(&self) -> &Path {
        &self.sock
    }
}

#[async_trait]
impl Backend for SocketBackend {
    async fn call(&self, req: Request, timeout: Duration) -> Result<Response, String> {
        let sock = &self.sock;
        let fut = async {
            let mut stream = UnixStream::connect(sock).await.map_err(|e| {
                format!(
                    "cannot reach {} at {} ({e}). {}",
                    self.what,
                    sock.display(),
                    self.hint
                )
            })?;
            stream
                .write_all(proto::encode_line(&req).as_bytes())
                .await
                .map_err(|e| format!("write: {e}"))?;
            let _ = stream.flush().await;
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => Err(format!("{} closed the connection with no reply", self.what)),
                Ok(_) => serde_json::from_str::<Response>(&line)
                    .map_err(|e| format!("malformed reply: {e}")),
                Err(e) => Err(format!("read: {e}")),
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(format!(
                "no reply from {} within {}s; the outcome is unknown (check read_inbox / its log before retrying a send)",
                self.what,
                timeout.as_secs()
            )),
        }
    }
}

#[async_trait]
impl<T: Transport> Backend for Engine<T> {
    async fn call(&self, req: Request, timeout: Duration) -> Result<Response, String> {
        tokio::time::timeout(timeout, self.handle(req))
            .await
            .map_err(|_| {
                format!(
                    "no result within {}s; the outcome is unknown",
                    timeout.as_secs()
                )
            })
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "host".into())
}

/// `<cwd basename>@<host>`: the default origin tag of a session.
pub fn default_origin() -> String {
    let cwd = std::env::current_dir().ok();
    let base = cwd
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("session")
        .to_string();
    format!("{base}@{}", hostname())
}

fn origin(args: &Value) -> String {
    match args
        .get("from_label")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some(l) if !l.is_empty() => format!("{l} ({})", default_origin()),
        _ => default_origin(),
    }
}

fn str_arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `reply_to` as a string: an inbox seq may arrive as a JSON number.
pub fn reply_to_arg(args: &Value) -> Result<Option<String>, String> {
    match args.get("reply_to") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|n| Some(n.to_string()))
            .ok_or_else(|| format!("reply_to {n} is not a valid inbox seq")),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => {
            crate::engine::parse_reply_to(s)?;
            Ok(Some(s.trim().to_string()))
        }
        Some(other) => Err(format!(
            "reply_to must be an inbox seq or a Matrix event id, not {other}"
        )),
    }
}

fn entries_of(data: &Value) -> Vec<InboxEntry> {
    serde_json::from_value(data.get("entries").cloned().unwrap_or(json!([]))).unwrap_or_default()
}

/// Map one tool call to a request (or an argument error for the model).
pub fn build_request(
    name: &str,
    args: &Value,
    max_wait_secs: u64,
) -> Result<(Request, Duration), String> {
    Ok(match name {
        jsonrpc::T_SEND_MESSAGE => {
            let md = args.get("markdown").and_then(Value::as_str).unwrap_or("");
            (
                Request::SendMessage {
                    to: str_arg(args, "to").unwrap_or("").into(),
                    markdown: md.into(),
                    origin: origin(args),
                    reply_to: reply_to_arg(args)?,
                },
                SEND_TIMEOUT,
            )
        }
        jsonrpc::T_SEND_FILE => {
            let Some(path) = str_arg(args, "path") else {
                return Err("send_file needs `path`".into());
            };
            let p = PathBuf::from(path);
            let abs = if p.is_absolute() {
                p
            } else {
                std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
            };
            let abs = abs
                .canonicalize()
                .map_err(|e| format!("cannot open {}: {e}", abs.display()))?;
            (
                Request::SendFile {
                    to: str_arg(args, "to").unwrap_or("").into(),
                    path: abs.to_string_lossy().into_owned(),
                    caption: str_arg(args, "caption").map(String::from),
                    origin: origin(args),
                    reply_to: reply_to_arg(args)?,
                },
                SEND_TIMEOUT,
            )
        }
        jsonrpc::T_LIST_RECIPIENTS => (Request::ListRecipients, QUICK_TIMEOUT),
        jsonrpc::T_READ_INBOX => {
            let (mut since_seq, mut since_ts_ms) = (None, None);
            if let Some(s) = args.get("since").and_then(|v| {
                v.as_u64()
                    .map(|n| n.to_string())
                    .or_else(|| v.as_str().map(String::from))
            }) {
                match format::parse_since(&s)? {
                    Since::Seq(n) => since_seq = Some(n),
                    Since::TsMs(t) => since_ts_ms = Some(t),
                }
            }
            let unread_only = since_seq.is_none() && since_ts_ms.is_none();
            (
                Request::ReadInbox {
                    from: str_arg(args, "from").map(String::from),
                    since_seq,
                    since_ts_ms,
                    unread_only,
                    mark_read: args
                        .get("mark_read")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    limit: Some(
                        args.get("limit")
                            .and_then(Value::as_u64)
                            .unwrap_or(50)
                            .clamp(1, 500) as usize,
                    ),
                },
                QUICK_TIMEOUT,
            )
        }
        jsonrpc::T_WAIT_FOR_REPLY => {
            let Some(from) = str_arg(args, "from") else {
                return Err("wait_for_reply needs `from`".into());
            };
            let t = args
                .get("timeout_s")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, max_wait_secs);
            (
                Request::WaitForReply {
                    from: from.into(),
                    timeout_s: t,
                    after_seq: args.get("after_seq").and_then(Value::as_u64),
                },
                Duration::from_secs(t + 30),
            )
        }
        jsonrpc::T_FETCH_ATTACHMENT => {
            let seq = args
                .get("inbox_seq")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .ok_or_else(|| "fetch_attachment needs an integer `inbox_seq`".to_string())?;
            (Request::FetchAttachment { inbox_seq: seq }, SEND_TIMEOUT)
        }
        other => return Err(format!("unknown tool {other}")),
    })
}

/// Render a successful response for the model. `wait` = the backend offers
/// `wait_for_reply` (the host); otherwise the send text points at
/// `read_inbox` instead of a tool that does not exist.
pub fn render(name: &str, d: &Value, label: &str, wait: bool) -> String {
    match name {
        jsonrpc::T_SEND_MESSAGE | jsonrpc::T_SEND_FILE => {
            let room = d["to_kind"].as_str() == Some("room");
            let reply = match (d["reply_to"].as_str(), d["thread_root"].as_str()) {
                (Some(r), Some(t)) => format!(" as a reply to {r} (in thread {t})"),
                (Some(r), None) => format!(" as a reply to {r}"),
                _ => String::new(),
            };
            let cursor = if wait {
                format!(
                    "inbox_seq={} (pass as after_seq to wait_for_reply to wait for an answer to this message).",
                    d["inbox_seq"]
                )
            } else {
                format!(
                    "inbox_seq={} (answers arrive after this seq; read_inbox shows them, with in_reply_to set when they reply to this event).",
                    d["inbox_seq"]
                )
            };
            format!(
                "Delivered to {}{} ({}) as Matrix event {}{reply}. {cursor}",
                if room { "room " } else { "" },
                d["to_name"].as_str().unwrap_or("?"),
                if room {
                    d["to_room_id"].as_str()
                } else {
                    d["to_mxid"].as_str()
                }
                .unwrap_or("?"),
                d["event_id"].as_str().unwrap_or("?"),
            )
        }
        jsonrpc::T_LIST_RECIPIENTS => serde_json::to_string_pretty(d).unwrap_or_default(),
        jsonrpc::T_READ_INBOX => {
            let entries = entries_of(d);
            if entries.is_empty() {
                format!(
                    "No matching messages in the {label} inbox (inbox high-water seq {}).",
                    d["high_water"]
                )
            } else {
                format::frame_entries(
                    &entries,
                    &format!(
                        "{} message(s); inbox high-water seq {}.",
                        entries.len(),
                        d["high_water"]
                    ),
                    label,
                )
            }
        }
        jsonrpc::T_WAIT_FOR_REPLY => {
            let entries = entries_of(d);
            if entries.is_empty() {
                format!(
                    "No reply within {}s. Nothing new from that person yet; you can wait again or check read_inbox later.",
                    d["waited_s"]
                )
            } else {
                format::frame_entries(&entries, "Reply received (marked read).", label)
            }
        }
        jsonrpc::T_FETCH_ATTACHMENT => d["framed"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| d.to_string()),
        _ => d.to_string(),
    }
}

/// Run one tool call end to end; returns (text, is_error).
pub async fn call_tool<B: Backend + ?Sized>(
    backend: &B,
    desc: &Value,
    name: &str,
    args: &Value,
) -> (String, bool) {
    let max_wait = desc["max_wait_secs"].as_u64().unwrap_or(600);
    let (req, timeout) = match build_request(name, args, max_wait) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let resp = match backend.call(req, timeout).await {
        Ok(r) => r,
        Err(e) => return (e, true),
    };
    if !resp.ok {
        return (
            resp.error
                .unwrap_or_else(|| "the messenger reported an unspecified error".into()),
            true,
        );
    }
    let label = desc["label"].as_str().unwrap_or("messenger");
    let wait = jsonrpc::desc_has_tool(desc, jsonrpc::T_WAIT_FOR_REPLY);
    (render(name, &resp.data, label, wait), false)
}

/// Fetch the backend's tool surface, falling back to `fallback` (a
/// [`jsonrpc::describe`] document) when the backend is unreachable or too old
/// to answer `describe`.
pub async fn describe_or<B: Backend + ?Sized>(backend: &B, fallback: Value) -> Value {
    match backend
        .call(Request::Describe, Duration::from_secs(5))
        .await
    {
        Ok(r) if r.ok && r.data["tools"].is_array() => r.data,
        Ok(r) => {
            tracing::info!(
                "backend has no describe op ({:?}); using the built-in tool list",
                r.error
            );
            fallback
        }
        Err(e) => {
            tracing::warn!("backend unreachable at startup ({e}); using the built-in tool list");
            fallback
        }
    }
}

/// Serve MCP over stdin/stdout until stdin closes. Logs go to stderr.
pub async fn run_stdio<B: Backend + ?Sized>(backend: &B, fallback: Value) -> anyhow::Result<()> {
    use std::io::Write as _;
    let desc = describe_or(backend, fallback).await;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let stdout = std::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("ignoring non-JSON stdin line: {e}");
                continue;
            }
        };
        let resp = match jsonrpc::classify(&req, &desc) {
            Action::None => continue,
            Action::Reply(r) => r,
            Action::Call { id, name, args } => {
                let (text, is_error) = call_tool(backend, &desc, &name, &args).await;
                jsonrpc::tool_result(id, &text, is_error)
            }
        };
        let mut out = stdout.lock();
        let mut s = serde_json::to_string(&resp)?;
        s.push('\n');
        if out.write_all(s.as_bytes()).is_err() || out.flush().is_err() {
            break;
        }
    }
    Ok(())
}
