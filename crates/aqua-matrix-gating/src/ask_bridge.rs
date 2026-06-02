//! Phase B daemon side — the bridge that backs the `ask_human` MCP tool.
//!
//! The MCP server (`aqua-matrix-ask-mcp`) owns **no** Matrix session; the daemon
//! does. So for each `claude` run that should be able to ask the human, the
//! daemon:
//!   1. binds a **per-run unix domain socket** and writes a one-server
//!      `--mcp-config` JSON pointing `claude` at the `aqua-matrix-ask-mcp`
//!      binary with `ASK_MCP_SOCK` set to that socket;
//!   2. runs an accept loop: each `ask_human` call opens the socket, sends one
//!      [`AskRequest`] line; the loop routes the question through the Phase A
//!      [`PendingMap::ask`] primitive (so the authenticated `target` answers it
//!      over the same Matrix channel) and writes back one [`AskReply`] line.
//!
//! Fail-closed end to end: [`PendingMap::ask`] returns `None` on
//! timeout/send-failure → we reply `denied`, and the MCP server turns that into
//! an `isError` tool result. A dropped [`AskBridge`] aborts the loop and unlinks
//! the socket + config file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aqua_matrix_ask_mcp::ipc::{self, AskReply};
use aqua_matrix_ask_mcp::SOCK_ENV;
use aqua_matrix_relay::AgentClient;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::pending::PendingMap;
use crate::ASK_SERVER_KEY;

/// A per-run `ask_human` bridge. Holds the bound socket, the generated
/// `--mcp-config` file, and the accept-loop task. Dropping it aborts the loop
/// and removes both files (RAII cleanup — no orphaned sockets per run).
pub struct AskBridge {
    sock_path: PathBuf,
    config_path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl AskBridge {
    /// Set up a bridge for one run targeting `target`. Binds the socket, writes
    /// the MCP config, and spawns the accept loop routing through `pending`.
    ///
    /// `timeout` bounds each `ask_human` wait (passed straight to
    /// [`PendingMap::ask`]); on timeout the model gets a fail-closed denial.
    // TODO: per-run socket auth token
    pub async fn setup(
        agent: &AgentClient,
        pending: &PendingMap,
        target: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let id = uuid::Uuid::new_v4();
        let dir = std::env::temp_dir();
        let sock_path = dir.join(format!("aqua-ask-{id}.sock"));
        let config_path = dir.join(format!("aqua-ask-{id}.json"));

        // A stale socket file at this exact path is impossible (fresh uuid), but
        // bind fails if it somehow exists — clear it defensively.
        let _ = tokio::fs::remove_file(&sock_path).await;
        let listener = UnixListener::bind(&sock_path)
            .map_err(|e| anyhow::anyhow!("ask-bridge: bind {}: {e}", sock_path.display()))?;

        let mcp_bin = ask_mcp_bin_path();
        let config = serde_json::json!({
            "mcpServers": {
                ASK_SERVER_KEY: {
                    "command": mcp_bin,
                    "args": [],
                    "env": { SOCK_ENV: sock_path.to_string_lossy() }
                }
            }
        });
        tokio::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)
            .await
            .map_err(|e| anyhow::anyhow!("ask-bridge: write config: {e}"))?;

        // The loop outlives this fn; give it owned clones. The "ask" closure
        // routes each question through the Phase A primitive — factoring it out
        // keeps the socket-serving layer (`accept_loop`) testable with a stub.
        let agent = agent.clone();
        let pending = pending.clone();
        let target = target.to_string();
        let ask = move |question: String| {
            let agent = agent.clone();
            let pending = pending.clone();
            let target = target.clone();
            async move {
                // Surface the question as a clearly-marked confirmation so the
                // user knows their next DM is the answer (mirrors Phase A's
                // `[confirm]` prefix).
                let q = format!("[ask] {question}\n\nReply with your answer.");
                pending.ask(&agent, &target, &q, timeout).await
            }
        };
        let task = tokio::spawn(async move {
            accept_loop(listener, ask).await;
        });

        Ok(Self { sock_path, config_path, task })
    }

    /// Path of the `--mcp-config` file to hand `claude`.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }
}

impl Drop for AskBridge {
    fn drop(&mut self) {
        self.task.abort();
        // Best-effort synchronous cleanup; the run is over.
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(&self.config_path);
    }
}

/// Accept one connection per `ask_human` call and route its question through
/// `ask` (which the daemon backs with [`PendingMap::ask`]). Connections are
/// handled sequentially: the per-target run lock plus `claude` blocking on each
/// tool result mean at most one question is ever in flight, so there is no
/// overlap to parallelise.
///
/// `ask` is `async fn(question) -> Option<answer>`: `Some` grants with the
/// human's answer, `None` is the fail-closed deny (timeout / send failure).
pub async fn accept_loop<F, Fut>(listener: UnixListener, ask: F)
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                if let Err(e) = handle_conn(stream, &ask).await {
                    tracing::warn!("ask-bridge: connection error: {e:#}");
                }
            }
            Err(e) => {
                tracing::warn!("ask-bridge: accept failed: {e}");
                // A transient accept error shouldn't kill the bridge; yield and
                // retry. (On listener close the task is aborted via Drop.)
                tokio::task::yield_now().await;
            }
        }
    }
}

/// One request/response: read an [`AskRequest`] line, resolve it via `ask`,
/// write the [`AskReply`] line back. `None` from `ask` is mapped to a
/// fail-closed denial.
async fn handle_conn<F, Fut>(stream: UnixStream, ask: &F) -> anyhow::Result<()>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        anyhow::bail!("ask-bridge: peer closed before sending a request");
    }
    let req = ipc::decode_request(&line)?;
    tracing::info!("ask-bridge: ask_human: {}", req.question);

    let reply = match ask(req.question).await {
        Some(answer) => AskReply::granted(answer),
        None => AskReply::denied("no answer within the timeout (fail-closed deny)"),
    };

    let mut stream = reader.into_inner();
    stream.write_all(ipc::encode_reply(&reply).as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Resolve the `aqua-matrix-ask-mcp` binary: it is built into the same
/// `target/<profile>/` directory as this daemon, so look beside `current_exe`
/// first, then fall back to a bare name on `PATH`.
fn ask_mcp_bin_path() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("aqua-matrix-ask-mcp");
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "aqua-matrix-ask-mcp".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the socket-serving layer (`accept_loop` + `handle_conn`) end to
    /// end against a stub `ask`, exactly as the real MCP server would: write one
    /// `AskRequest` line, read one `AskReply` line. The Matrix leg (`pending.ask`
    /// → `send_dm`) is substituted by the stub, since that half is Phase A's and
    /// needs a live room.
    async fn roundtrip<F, Fut>(question: &str, ask: F) -> AskReply
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Option<String>> + Send,
    {
        let id = uuid::Uuid::new_v4();
        let sock = std::env::temp_dir().join(format!("aqua-ask-test-{id}.sock"));
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move { accept_loop(listener, ask).await });

        let mut client = UnixStream::connect(&sock).await.unwrap();
        let req = ipc::AskRequest { question: question.to_string() };
        client
            .write_all(ipc::encode_request(&req).as_bytes())
            .await
            .unwrap();
        client.flush().await.unwrap();

        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        server.abort();
        let _ = std::fs::remove_file(&sock);
        ipc::decode_reply(&line).unwrap()
    }

    #[tokio::test]
    async fn granted_answer_round_trips_over_the_socket() {
        let reply = roundtrip("ok to rm /tmp/x?", |q| async move {
            assert_eq!(q, "ok to rm /tmp/x?");
            Some("yes, go ahead".to_string())
        })
        .await;
        assert!(reply.granted);
        assert_eq!(reply.answer, "yes, go ahead");
    }

    #[tokio::test]
    async fn none_from_ask_becomes_a_fail_closed_denial() {
        // `None` models a timeout / send failure on the daemon side.
        let reply = roundtrip("ok?", |_q| async move { None }).await;
        assert!(!reply.granted, "no answer must map to a denial");
        assert!(!reply.answer.is_empty(), "denial carries a reason");
    }

    #[test]
    fn mcp_bin_path_falls_back_to_bare_name_when_no_sibling() {
        // In a normal test run there is no `aqua-matrix-ask-mcp` beside the test
        // binary, so we exercise the PATH fallback branch deterministically.
        assert_eq!(ask_mcp_bin_path(), "aqua-matrix-ask-mcp");
    }
}
