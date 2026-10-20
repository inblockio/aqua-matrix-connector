//! `aqua-seat-mcp`: the stdio MCP server `seat`. One per Claude session; it
//! derives the session's seat (D1) and forwards tool calls to `aqua-seatd`.

use aqua_seat::{client::SeatClient, mcp, seatname};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aqua_seat=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let derived = seatname::derive(&seatname::Inputs::from_env());
    tracing::info!(seat = %derived.name, source = ?derived.source, "aqua-seat-mcp starting");
    let by = aqua_messenger::mcp::default_origin();
    let session = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let client = SeatClient::from_env();
    mcp::run_stdio(&client, &derived.name, &by, session.as_deref()).await
}
