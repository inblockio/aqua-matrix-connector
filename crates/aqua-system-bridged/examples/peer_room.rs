//! Local-stack e2e helper for `[[rooms]]`: act as a PEER that creates an
//! encrypted room and invites the bridge (`create`), posts in a room (`post`),
//! sends a file into a room (`post-file`), or prints a room's recent messages
//! (`read`). Never point this at production or at the bridge's own store: it
//! is a separate identity with its own `--store-dir`.
//!
//!   cargo run -p aqua-system-bridged --example peer_room -- \
//!     --key-file ~/.cache/system-bridge-test/peer/peer.pem \
//!     --store-dir ~/.cache/system-bridge-test/peer/store \
//!     --siwx-url http://localhost:18081 --matrix-url http://localhost:18080 \
//!     create --invite @bridge:localhost [--is-direct] --name "Daily Updates"

use std::path::PathBuf;

use aqua_matrix_agent::{AgentClient, AgentConfig};
use clap::{Parser, Subcommand};
use matrix_sdk::ruma::{
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    events::{room::encryption::RoomEncryptionEventContent, InitialStateEvent},
    OwnedUserId,
};

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
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an encrypted room and invite `invite` (flag the invite
    /// `is_direct` with --is-direct).
    Create {
        #[arg(long)]
        invite: String,
        #[arg(long)]
        is_direct: bool,
        #[arg(long)]
        name: String,
    },
    /// Post a text message in `room`.
    Post {
        #[arg(long)]
        room: String,
        #[arg(long)]
        text: String,
    },
    /// Send a local file as an (E2EE) attachment into `room`.
    PostFile {
        #[arg(long)]
        room: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Print the last messages in `room`.
    Read {
        #[arg(long)]
        room: String,
    },
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
    agent.sync_once().await?;
    match a.cmd {
        Cmd::Create {
            invite,
            is_direct,
            name,
        } => {
            let user: OwnedUserId = invite.as_str().try_into()?;
            let mut req = CreateRoomRequest::new();
            req.invite = vec![user];
            req.is_direct = is_direct;
            req.name = Some(name);
            req.initial_state = vec![InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any()];
            let room = agent.client().create_room(req).await?;
            println!("{}", room.room_id());
        }
        Cmd::Post { room, text } => {
            let id = agent.send_to_room(&room, &text).await?;
            let _ = agent.sync_once_nowait().await;
            println!("sent {id}");
        }
        Cmd::PostFile { room, file } => {
            let id = agent.send_media_to_room(&room, &file, None).await?;
            let _ = agent.sync_once_nowait().await;
            println!("sent {id}");
        }
        Cmd::Read { room } => {
            for m in agent.messages(&room, 10).await? {
                println!("{} {}: {}", m.timestamp_ms, m.sender, m.body);
            }
        }
    }
    Ok(())
}
