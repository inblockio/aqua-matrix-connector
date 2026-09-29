//! `aqua-system-bridge-mcp`: stdio MCP server, one per Claude Code session.
//!
//! Speaks MCP JSON-RPC over stdin/stdout and forwards each tool call as one
//! JSON line to the `aqua-system-bridged` daemon's unix socket. It never opens
//! a Matrix client or the crypto store (one Client per crypto store, always).
//! Logs go to stderr; stdout is the protocol channel.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aqua_system_bridge::format::{self, Since};
use aqua_system_bridge::inbox::InboxEntry;
use aqua_system_bridge::jsonrpc::{self, Action};
use aqua_system_bridge::proto::{self, Request, Response};
use aqua_system_bridge::MAX_WAIT_SECS;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Socket timeout for sends: they may queue behind a reconnect (token
/// rotation, short network blip) before the daemon executes them.
const SEND_TIMEOUT: Duration = Duration::from_secs(200);
const QUICK_TIMEOUT: Duration = Duration::from_secs(20);

async fn roundtrip(sock: &Path, req: &Request, timeout: Duration) -> Result<Response, String> {
    let fut = async {
        let mut stream = UnixStream::connect(sock).await.map_err(|e| {
            format!(
                "cannot reach the Aqua System bridge daemon at {} ({e}). Is the service running? \
                 Check: systemctl --user status aqua-system-bridge",
                sock.display()
            )
        })?;
        stream.write_all(proto::encode_line(req).as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        let _ = stream.flush().await;
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => Err("bridge closed the connection with no reply".to_string()),
            Ok(_) => serde_json::from_str::<Response>(&line).map_err(|e| format!("malformed reply: {e}")),
            Err(e) => Err(format!("read: {e}")),
        }
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "no reply from the bridge within {}s; the outcome is unknown (check read_inbox / the daemon journal before retrying a send)",
            timeout.as_secs()
        )),
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

fn default_origin() -> String {
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
    match args.get("from_label").and_then(Value::as_str).map(str::trim) {
        Some(l) if !l.is_empty() => format!("{l} ({})", default_origin()),
        _ => default_origin(),
    }
}

fn str_arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn entries_of(data: &Value) -> Vec<InboxEntry> {
    serde_json::from_value(data.get("entries").cloned().unwrap_or(json!([]))).unwrap_or_default()
}

/// Run one tool call; returns (text, is_error).
async fn call_tool(sock: &Path, name: &str, args: &Value) -> (String, bool) {
    let (req, timeout) = match name {
        jsonrpc::T_SEND_MESSAGE => {
            let (Some(to), Some(md)) = (str_arg(args, "to"), args.get("markdown").and_then(Value::as_str)) else {
                return ("send_message needs `to` and a non-empty `markdown`".into(), true);
            };
            if md.trim().is_empty() {
                return ("send_message needs a non-empty `markdown`".into(), true);
            }
            (Request::SendMessage { to: to.into(), markdown: md.into(), origin: origin(args) }, SEND_TIMEOUT)
        }
        jsonrpc::T_SEND_FILE => {
            let (Some(to), Some(path)) = (str_arg(args, "to"), str_arg(args, "path")) else {
                return ("send_file needs `to` and `path`".into(), true);
            };
            let p = PathBuf::from(path);
            let abs = if p.is_absolute() { p } else { std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p) };
            let abs = match abs.canonicalize() {
                Ok(a) => a,
                Err(e) => return (format!("cannot open {}: {e}", abs.display()), true),
            };
            (
                Request::SendFile {
                    to: to.into(),
                    path: abs.to_string_lossy().into_owned(),
                    caption: str_arg(args, "caption").map(String::from),
                    origin: origin(args),
                },
                SEND_TIMEOUT,
            )
        }
        jsonrpc::T_LIST_RECIPIENTS => (Request::ListRecipients, QUICK_TIMEOUT),
        jsonrpc::T_READ_INBOX => {
            let (mut since_seq, mut since_ts_ms) = (None, None);
            if let Some(s) = args.get("since").and_then(|v| v.as_u64().map(|n| n.to_string()).or_else(|| v.as_str().map(String::from))) {
                match format::parse_since(&s) {
                    Ok(Since::Seq(n)) => since_seq = Some(n),
                    Ok(Since::TsMs(t)) => since_ts_ms = Some(t),
                    Err(e) => return (e, true),
                }
            }
            let unread_only = since_seq.is_none() && since_ts_ms.is_none();
            (
                Request::ReadInbox {
                    from: str_arg(args, "from").map(String::from),
                    since_seq,
                    since_ts_ms,
                    unread_only,
                    mark_read: args.get("mark_read").and_then(Value::as_bool).unwrap_or(true),
                    limit: Some(args.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 500) as usize),
                },
                QUICK_TIMEOUT,
            )
        }
        jsonrpc::T_WAIT_FOR_REPLY => {
            let Some(from) = str_arg(args, "from") else {
                return ("wait_for_reply needs `from`".into(), true);
            };
            let t = args.get("timeout_s").and_then(Value::as_u64).unwrap_or(120).clamp(1, MAX_WAIT_SECS);
            (
                Request::WaitForReply { from: from.into(), timeout_s: t, after_seq: args.get("after_seq").and_then(Value::as_u64) },
                Duration::from_secs(t + 30),
            )
        }
        other => return (format!("unknown tool {other}"), true),
    };

    let resp = match roundtrip(sock, &req, timeout).await {
        Ok(r) => r,
        Err(e) => return (e, true),
    };
    if !resp.ok {
        return (resp.error.unwrap_or_else(|| "bridge reported an unspecified error".into()), true);
    }
    let d = &resp.data;
    let text = match name {
        jsonrpc::T_SEND_MESSAGE | jsonrpc::T_SEND_FILE => format!(
            "Delivered to {} ({}) as Matrix event {}. inbox_seq={} (pass as after_seq to wait_for_reply to wait for an answer to this message).",
            d["to_name"].as_str().unwrap_or("?"),
            d["to_mxid"].as_str().unwrap_or("?"),
            d["event_id"].as_str().unwrap_or("?"),
            d["inbox_seq"]
        ),
        jsonrpc::T_LIST_RECIPIENTS => serde_json::to_string_pretty(d).unwrap_or_default(),
        jsonrpc::T_READ_INBOX => {
            let entries = entries_of(d);
            if entries.is_empty() {
                format!("No matching messages in the Aqua System inbox (inbox high-water seq {}).", d["high_water"])
            } else {
                format::frame_entries(&entries, &format!("{} message(s); inbox high-water seq {}.", entries.len(), d["high_water"]))
            }
        }
        jsonrpc::T_WAIT_FOR_REPLY => {
            let entries = entries_of(d);
            if entries.is_empty() {
                format!("No reply within {}s. Nothing new from that person yet; you can wait again or check read_inbox later.", d["waited_s"])
            } else {
                format::frame_entries(&entries, "Reply received (marked read).")
            }
        }
        _ => d.to_string(),
    };
    (text, false)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,aqua_system_bridge=info".into()),
        )
        .init();
    let sock = aqua_system_bridge::sock_path();

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
        let resp = match jsonrpc::classify(&req) {
            Action::None => continue,
            Action::Reply(r) => r,
            Action::Call { id, name, args } => {
                let (text, is_error) = call_tool(&sock, &name, &args).await;
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
