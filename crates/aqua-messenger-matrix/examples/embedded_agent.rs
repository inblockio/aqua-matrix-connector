//! Minimal embedded agent: shows how an agent built on the connector gets the
//! default messenger tooling with ONE call, on its OWN live Client (no second
//! Client on its crypto store), and serves it to a stdio MCP server.
//!
//! Local stack only (used by the messenger e2e):
//!
//!   cargo run -p aqua-messenger-matrix --example embedded_agent -- \
//!     --key-file ~/.cache/messenger-e2e/agent/agent.pem \
//!     --store-dir ~/.cache/messenger-e2e/agent/store \
//!     --siwx-url http://localhost:18081 --matrix-url http://localhost:18080 \
//!     --owner @peer:localhost --run-secs 120
//!
//! It prints its MXID, the `--mcp-config` JSON and the `--allowedTools`
//! entries, then syncs until `--run-secs` pass. Drive the tools with
//! `AQUA_MESSENGER_SOCK=<sock> aqua-messenger-mcp` (stdio JSON-RPC).

#![recursion_limit = "256"]

use std::path::PathBuf;
use std::time::Duration;

use aqua_matrix_agent::{AgentClient, AgentConfig};
use aqua_messenger_matrix::AgentMessenger;
use clap::Parser;
use matrix_sdk::config::SyncSettings;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    key_file: PathBuf,
    #[arg(long)]
    store_dir: PathBuf,
    #[arg(long)]
    siwx_url: String,
    #[arg(long)]
    matrix_url: String,
    /// The agent's owner (the only allow-listed person by default).
    #[arg(long)]
    owner: String,
    #[arg(long, default_value_t = 120)]
    run_secs: u64,
    #[arg(long, default_value = "aqua-messenger-mcp")]
    mcp_binary: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_messenger=info,aqua_messenger_matrix=info")
        .init();
    let a = Args::parse();
    for u in [&a.siwx_url, &a.matrix_url] {
        anyhow::ensure!(
            u.contains("localhost") || u.contains("127.0.0.1"),
            "local stack only: {u}"
        );
    }
    let messenger_dir = a.store_dir.with_file_name("messenger");
    let sock = a.store_dir.with_file_name("messenger.sock");

    // (1) ONE call: default tooling, owner-only allow-list.
    let messenger = AgentMessenger::enable_default(&messenger_dir, "E2E Agent", &a.owner)?;

    let agent = AgentClient::connect(AgentConfig {
        key_file: a.key_file,
        siwx_url: a.siwx_url,
        matrix_url: a.matrix_url,
        client_id: None,
        redirect_uri: None,
        store_dir: a.store_dir,
        device_id: None,
    })
    .await?;
    println!("agent mxid {}", agent.user_id());

    // (2) Per connect cycle: attach the agent's own Client.
    agent.sync_once_nowait().await?;
    messenger.attach(&agent).await;
    messenger.backfill(&agent).await;

    // (3) Serve the tools to `claude` via the stdio server.
    let mcp = messenger.serve_mcp(&sock, &a.mcp_binary)?;
    println!("mcp-config {}", mcp.mcp_config());
    println!("allowed-tools {}", mcp.allowed_tools().join(","));
    println!("ready");

    // The agent's own loop: join invites from the owner, keep syncing.
    let client = agent.client().clone();
    let sync = tokio::spawn(async move { client.sync(SyncSettings::default()).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(a.run_secs);
    while tokio::time::Instant::now() < deadline {
        for room in agent.client().invited_rooms() {
            if let Ok(inv) = room.invite_details().await {
                if inv.inviter_id.as_str().eq_ignore_ascii_case(&a.owner) {
                    let _ = room.join().await;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    sync.abort();
    let _ = sync.await;
    // (4) Before the Client goes away.
    messenger.detach().await;
    drop(mcp);
    println!("done");
    Ok(())
}
