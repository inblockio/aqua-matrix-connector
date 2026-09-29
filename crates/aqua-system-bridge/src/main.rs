//! `aqua-system-bridge-mcp`: stdio MCP server, one per Claude Code session.
//!
//! Speaks MCP JSON-RPC over stdin/stdout and forwards each tool call as one
//! JSON line to the `aqua-system-bridged` daemon's unix socket. It never opens
//! a Matrix client or the crypto store (one Client per crypto store, always).
//! Logs go to stderr; stdout is the protocol channel. The tool loop is the
//! shared `aqua_messenger::mcp`; tool texts come from the daemon (`describe`),
//! with the host profile as fallback for an older daemon.

use aqua_messenger::mcp::{run_stdio, SocketBackend};
use aqua_messenger::{jsonrpc, Profile};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_system_bridge=info,aqua_messenger=info".into()),
        )
        .init();
    let backend = SocketBackend::new(
        aqua_system_bridge::sock_path(),
        "Is the service running? Check: systemctl --user status aqua-system-bridge",
    )
    .named("the Aqua System bridge daemon");
    run_stdio(&backend, jsonrpc::describe(&Profile::host())).await
}
