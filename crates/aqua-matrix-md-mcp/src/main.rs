//! `aqua-matrix-md-mcp`: stdio MCP server exposing one tool,
//! `send_markdown_file(filename, markdown) -> delivered`.
//!
//! It speaks MCP JSON-RPC 2.0 over stdin/stdout (see [`aqua_matrix_md_mcp::jsonrpc`]).
//! It owns **no** Matrix session. When `claude` calls `send_markdown_file`, the
//! server opens the per-run unix socket named in `$MD_MCP_SOCK` (set by the
//! daemon), writes the `{filename, markdown}` pair as one JSON line, reads one
//! JSON reply line, and returns the delivery result to `claude` as the tool
//! result.
//!
//! Fail-closed: any IPC error (no socket, connect refused, malformed/empty
//! reply) becomes a tool result telling the model the file was NOT delivered and
//! to paste the document inline instead, so content is never silently dropped.

use std::io::Write as _;

use aqua_matrix_md_mcp::ipc::{self, MdReply, MdRequest};
use aqua_matrix_md_mcp::jsonrpc::{self, MdOutcome};
use aqua_matrix_md_mcp::SOCK_ENV;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Round-trip one delivery request to the daemon over the per-run unix socket.
/// Returns the daemon's [`MdReply`], or an `Err(reason)` for a transport failure
/// (the daemon's handler never ran, so nothing was delivered).
async fn deliver_over_socket(
    sock_path: &str,
    filename: &str,
    markdown: &str,
) -> Result<MdReply, String> {
    let mut stream = UnixStream::connect(sock_path)
        .await
        .map_err(|e| format!("connect {sock_path}: {e}"))?;

    let req = MdRequest { filename: filename.to_string(), markdown: markdown.to_string() };
    stream
        .write_all(ipc::encode_request(&req).as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let _ = stream.flush().await;

    // Read one newline-delimited reply line. The daemon owns the bounded
    // delivery work and always writes a reply; if it dies the socket closes and
    // read returns EOF.
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => Err("bridge closed with no reply".to_string()),
        Ok(_) => ipc::decode_reply(&line).map_err(|e| format!("malformed reply: {e}")),
        Err(e) => Err(format!("read: {e}")),
    }
}

/// Map a delivery result into the tool-result outcome surfaced to the model.
///
/// The three cases drive distinct model behavior, and they are deliberately
/// consistent with the daemon-injected system prompt (no double-delivery):
///   - delivered  → confirm briefly; not an error.
///   - not delivered, but the daemon already sent the content inline → tell the
///     model the content reached the user another way and NOT to resend; not an
///     error.
///   - transport failure (handler never ran, nothing sent) → instruct the model
///     to paste the document inline now; flagged `isError` so it acts.
fn outcome_from_result(res: Result<MdReply, String>) -> MdOutcome {
    match res {
        Ok(reply) if reply.delivered => MdOutcome {
            text: format!("Delivered to the user as an attached markdown file ({}).", reply.detail),
            is_error: false,
        },
        Ok(reply) => MdOutcome {
            text: format!(
                "Could not attach the file ({}). The content was sent to the user inline instead, so do NOT resend it; just briefly note it could not be attached as a file.",
                reply.detail
            ),
            is_error: false,
        },
        Err(e) => MdOutcome {
            text: format!(
                "Could not reach the delivery bridge ({e}); nothing was sent to the user. Paste the full Markdown document inline in a fenced code block now so the user still receives it."
            ),
            is_error: true,
        },
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr only. stdout is the JSON-RPC channel and must stay
    // clean. `claude --debug` surfaces this stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_md_mcp=info".into()),
        )
        .init();

    let sock_path = std::env::var(SOCK_ENV).ok();
    if sock_path.is_none() {
        tracing::warn!("{SOCK_ENV} unset; send_markdown_file will fail closed for every call");
    }

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let stdout = std::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("md-mcp: ignoring non-JSON stdin line: {e}");
                continue;
            }
        };

        // `handle` is sync, so resolve the (async) socket round-trip first and
        // pass a cached outcome in. `handle` only invokes the closure for a
        // `tools/call` of our tool.
        let response = if is_md_call(&req) {
            let args = req.get("params").and_then(|p| p.get("arguments"));
            let filename = args
                .and_then(|a| a.get("filename"))
                .and_then(|f| f.as_str())
                .unwrap_or("");
            let markdown = args
                .and_then(|a| a.get("markdown"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let outcome = match &sock_path {
                Some(p) => outcome_from_result(deliver_over_socket(p, filename, markdown).await),
                None => MdOutcome {
                    text: "No delivery bridge configured; nothing was sent. Paste the full Markdown document inline in a fenced code block now.".to_string(),
                    is_error: true,
                },
            };
            jsonrpc::handle(&req, |_f, _m| MdOutcome {
                text: outcome.text.clone(),
                is_error: outcome.is_error,
            })
        } else {
            jsonrpc::handle(&req, |_f, _m| {
                // Unreachable: non-delivery calls never invoke the closure.
                MdOutcome { text: String::new(), is_error: true }
            })
        };

        if let Some(resp) = response {
            let mut out = stdout.lock();
            let mut s = serde_json::to_string(&resp)?;
            s.push('\n');
            if out.write_all(s.as_bytes()).is_err() || out.flush().is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// Is this request a `tools/call` for our `send_markdown_file` tool?
fn is_md_call(req: &serde_json::Value) -> bool {
    req.get("method").and_then(|m| m.as_str()) == Some("tools/call")
        && req
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            == Some(aqua_matrix_md_mcp::TOOL_NAME)
}
