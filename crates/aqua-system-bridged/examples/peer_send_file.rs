//! Local-stack e2e helper: act as an allow-listed PEER and send one file as an
//! (E2EE) attachment to the bridge identity, so `fetch_attachment` can be
//! tested end to end. Never point this at production or at the bridge's own
//! store: it is a separate identity with its own `--store-dir`.
//!
//!   cargo run -p aqua-system-bridged --example peer_send_file -- \
//!     --key-file ~/.cache/system-bridge-test/peer/peer.pem \
//!     --store-dir ~/.cache/system-bridge-test/peer/store \
//!     --siwx-url http://localhost:18081 --matrix-url http://localhost:18080 \
//!     --target @bridge:localhost --file ~/.cache/system-bridge-test/blob.bin

use std::path::PathBuf;

use aqua_matrix_agent::{AgentClient, AgentConfig};
use clap::Parser;

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
    #[arg(long, default_value = "")]
    target: String,
    #[arg(long)]
    file: Option<PathBuf>,
    /// Print this peer's MXID and exit (registers the account on first use).
    #[arg(long)]
    whoami: bool,
    /// Send a text first and wait this long, so the target can join the DM
    /// before the attachment is sent.
    #[arg(long, default_value_t = 0)]
    warmup_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a = Args::parse();
    for u in [&a.siwx_url, &a.matrix_url] {
        anyhow::ensure!(
            u.contains("localhost") || u.contains("127.0.0.1"),
            "local stack only: {u}"
        );
    }
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
    if a.whoami {
        println!("mxid {}", agent.user_id());
        return Ok(());
    }
    let file = a
        .file
        .ok_or_else(|| anyhow::anyhow!("--file is required"))?;
    agent.sync_once().await?;
    let _ = agent.join_invited_rooms().await;
    agent.sync_once().await?;
    if a.warmup_secs > 0 {
        agent.send_dm(&a.target, "hello (e2e warm-up)").await?;
        for _ in 0..a.warmup_secs {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let _ = agent.sync_once_nowait().await;
        }
    }
    let id = agent.send_file(&a.target, &file, None).await?;
    // One more sync so the room key reaches the bridge's device promptly.
    let _ = agent.sync_once_nowait().await;
    println!("sent {id}");
    Ok(())
}
