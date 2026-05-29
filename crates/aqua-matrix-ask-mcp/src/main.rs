//! `aqua-matrix-ask-mcp` — a tiny stdio MCP server exposing one tool,
//! `ask_human(question) -> answer`.
//!
//! It speaks MCP JSON-RPC 2.0 over stdin/stdout (see [`aqua_matrix_ask_mcp::jsonrpc`]).
//! It owns **no** Matrix session. When `claude` calls `ask_human`, the server
//! opens the per-run unix socket named in `$ASK_MCP_SOCK` (set by the daemon),
//! writes the question as one JSON line, reads one JSON reply line, and returns
//! the human's answer to `claude` as the tool result.
//!
//! Fail-closed: any IPC error (no socket, connect refused, malformed/empty
//! reply) becomes an `isError` tool result telling the model it was **not**
//! granted — never a silent allow.

use std::io::Write as _;

use aqua_matrix_ask_mcp::ipc::{self, AskReply, AskRequest};
use aqua_matrix_ask_mcp::jsonrpc::{self, AskOutcome};
use aqua_matrix_ask_mcp::SOCK_ENV;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Round-trip one question to the daemon over the per-run unix socket.
/// Blocking-from-the-model's-view: this awaits the human's answer (the daemon
/// holds the connection open until `pending.ask` resolves). Fail-closed on any
/// error.
async fn ask_over_socket(sock_path: &str, question: &str) -> AskReply {
    let mut stream = match UnixStream::connect(sock_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("ask-mcp: cannot connect to {sock_path}: {e}");
            return AskReply::denied(format!("bridge unavailable ({e})"));
        }
    };

    let req = AskRequest { question: question.to_string() };
    if let Err(e) = stream.write_all(ipc::encode_request(&req).as_bytes()).await {
        tracing::warn!("ask-mcp: write failed: {e}");
        return AskReply::denied(format!("bridge write failed ({e})"));
    }
    // We've sent our single request; signal EOF on our side so the daemon can
    // read to end if it ever wants to, while we still read the reply.
    let _ = stream.flush().await;

    // Read one newline-delimited reply line. No timeout here: the daemon owns
    // the (bounded) `pending.ask` timeout and will always send a reply (granted
    // or denied) — if the daemon dies, the socket closes and read returns EOF.
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => {
            tracing::warn!("ask-mcp: daemon closed socket with no reply");
            AskReply::denied("bridge closed with no answer")
        }
        Ok(_) => match ipc::decode_reply(&line) {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!("ask-mcp: malformed reply {line:?}: {e}");
                AskReply::denied("bridge sent malformed answer")
            }
        },
        Err(e) => {
            tracing::warn!("ask-mcp: read failed: {e}");
            AskReply::denied(format!("bridge read failed ({e})"))
        }
    }
}

/// Map an IPC reply into the tool-result outcome surfaced to the model.
fn outcome_from_reply(reply: AskReply) -> AskOutcome {
    if reply.granted {
        AskOutcome { text: reply.answer, is_error: false }
    } else {
        AskOutcome {
            text: format!(
                "NOT GRANTED by the human operator: {}. Do NOT perform the action; \
stop and report this to the user.",
                reply.answer
            ),
            is_error: true,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr only — stdout is the JSON-RPC channel and must stay
    // clean. `claude --debug` surfaces this stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_ask_mcp=info".into()),
        )
        .init();

    let sock_path = std::env::var(SOCK_ENV).ok();
    if sock_path.is_none() {
        tracing::warn!("{SOCK_ENV} unset; ask_human will fail closed (deny) for every call");
    }

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    // stdout is sync to avoid interleaving partial writes; each response is one
    // line written atomically.
    let stdout = std::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("ask-mcp: ignoring non-JSON stdin line: {e}");
                continue;
            }
        };

        // The bridge closure performs the socket round-trip. `handle` only calls
        // it for a `tools/call` of our tool, so we do the (async) IPC outside
        // and pass a cached result in — but `handle` is sync, so instead we
        // detect the tool call here and resolve the answer first.
        let response = if is_ask_call(&req) {
            let question = req
                .get("params")
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("question"))
                .and_then(|q| q.as_str())
                .unwrap_or("");
            let reply = match &sock_path {
                Some(p) => ask_over_socket(p, question).await,
                None => AskReply::denied("no bridge socket configured"),
            };
            let outcome = outcome_from_reply(reply);
            jsonrpc::handle(&req, |_q| AskOutcome {
                text: outcome.text.clone(),
                is_error: outcome.is_error,
            })
        } else {
            jsonrpc::handle(&req, |_q| {
                // Unreachable: non-ask calls never invoke the closure.
                AskOutcome { text: String::new(), is_error: true }
            })
        };

        if let Some(resp) = response {
            let mut out = stdout.lock();
            let mut s = serde_json::to_string(&resp)?;
            s.push('\n');
            // Best-effort: if claude closed stdout we're done anyway.
            if out.write_all(s.as_bytes()).is_err() || out.flush().is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// Is this request a `tools/call` for our `ask_human` tool?
fn is_ask_call(req: &serde_json::Value) -> bool {
    req.get("method").and_then(|m| m.as_str()) == Some("tools/call")
        && req
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            == Some(aqua_matrix_ask_mcp::TOOL_NAME)
}
