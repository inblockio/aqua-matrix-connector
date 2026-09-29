//! `aqua-messenger-mcp`: the generic stdio MCP server for an embedded agent's
//! messenger. Connects to the socket in `$AQUA_MESSENGER_SOCK`, served by the
//! agent process that owns the Matrix Client (see
//! `aqua_messenger_matrix::AgentMessenger::serve_mcp`). Tool names, texts and
//! limits come from the backend (`describe`); it never touches Matrix.

use aqua_messenger::mcp::{run_stdio, SocketBackend};
use aqua_messenger::{jsonrpc, Profile, SOCK_ENV};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_messenger=info".into()),
        )
        .init();
    let Some(sock) = std::env::var_os(SOCK_ENV) else {
        anyhow::bail!("{SOCK_ENV} is not set; this server is started by the agent's --mcp-config");
    };
    let backend = SocketBackend::new(sock.into(), "Is the agent running?");
    let fallback = jsonrpc::describe(&Profile::embedded_agent("this agent", "your owner"));
    run_stdio(&backend, fallback).await
}
