use anyhow::{anyhow, Context, Result};
use clap::Parser;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{events::room::message::RoomMessageEventContent, OwnedDeviceId, OwnedUserId, UserId},
    Client, SessionMeta, SessionTokens,
};
use serde::Deserialize;
use siwx_oidc_auth::{authenticate, SiwxKey};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "aqua-matrix-hello")]
#[command(about = "MVP: authenticate via siwx-oidc, send a Matrix message")]
struct Args {
    /// Path to Ed25519 or P-256 PEM key file. Generated if missing.
    #[arg(long, env = "AGENT_KEY_FILE", default_value = "agent.pem")]
    key_file: PathBuf,

    /// siwx-oidc server URL
    #[arg(long, env = "SIWX_URL", default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,

    /// Matrix homeserver URL
    #[arg(long, env = "MATRIX_URL", default_value = "https://matrix.inblock.io")]
    matrix_url: String,

    /// OIDC client ID registered on the siwx-oidc server
    #[arg(long, env = "OIDC_CLIENT_ID", required_unless_present = "print_did")]
    client_id: Option<String>,

    /// OIDC redirect URI (must match the registered client)
    #[arg(long, env = "OIDC_REDIRECT_URI", required_unless_present = "print_did")]
    redirect_uri: Option<String>,

    /// Target Matrix user ID to send the message to
    #[arg(
        long,
        default_value = "@did-pkh-eip155-1-0x0000000000000000000000000000000000000000:matrix.inblock.io"
    )]
    target: String,

    /// Message to send
    #[arg(long, default_value = "Hello from aqua-matrix-hello agent!")]
    message: String,

    /// Print the agent's DID and exit
    #[arg(long)]
    print_did: bool,
}

#[derive(Deserialize)]
struct WhoAmI {
    user_id: String,
    device_id: String,
}

fn load_or_generate_key(path: &PathBuf) -> Result<SiwxKey> {
    if path.exists() {
        eprintln!("[1/6] Loading key from {}", path.display());
        SiwxKey::from_pem_file(path).context("failed to load key")
    } else {
        eprintln!("[1/6] Generating new Ed25519 key at {}", path.display());
        let key = SiwxKey::generate_ed25519();
        let pem = key.to_pem()?;
        std::fs::write(path, &pem).context("failed to write key file")?;
        Ok(key)
    }
}

async fn whoami(matrix_url: &str, access_token: &str) -> Result<WhoAmI> {
    let url = format!("{}/_matrix/client/v3/account/whoami", matrix_url);
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .context("whoami request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("whoami returned {status}: {body}");
    }

    resp.json().await.context("whoami JSON parse failed")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_hello=info".into()),
        )
        .init();

    let args = Args::parse();

    // Step 1: Load or generate agent key
    let key = load_or_generate_key(&args.key_file)?;
    let did = key.did();
    eprintln!("       Agent DID: {did}");

    if args.print_did {
        println!("{did}");
        return Ok(());
    }

    let client_id = args.client_id.as_deref().unwrap();
    let redirect_uri = args.redirect_uri.as_deref().unwrap();

    // Step 2: Authenticate via siwx-oidc (CAIP-122 headless flow)
    eprintln!("[2/6] Authenticating against {}...", args.siwx_url);
    let tokens = authenticate(&args.siwx_url, client_id, redirect_uri, &key)
        .await
        .context("siwx-oidc authentication failed")?;
    eprintln!(
        "       Access token acquired (expires in {}s)",
        tokens.expires_in.unwrap_or(0)
    );

    // Step 3: Resolve Matrix identity via whoami
    eprintln!("[3/6] Resolving Matrix identity...");
    let identity = whoami(&args.matrix_url, &tokens.access_token).await?;
    eprintln!("       Matrix user: {}", identity.user_id);
    eprintln!("       Device:      {}", identity.device_id);

    let user_id: OwnedUserId = identity
        .user_id
        .try_into()
        .map_err(|e| anyhow!("invalid user_id: {e}"))?;
    let device_id: OwnedDeviceId = identity.device_id.into();

    // Step 4: Build Matrix client and restore session
    eprintln!("[4/6] Connecting to Matrix at {}...", args.matrix_url);
    let client = Client::builder()
        .homeserver_url(&args.matrix_url)
        .build()
        .await
        .context("failed to build Matrix client")?;

    let session = matrix_sdk::authentication::matrix::MatrixSession {
        meta: SessionMeta {
            user_id,
            device_id,
        },
        tokens: SessionTokens {
            access_token: tokens.access_token,
            refresh_token: None,
        },
    };

    client
        .matrix_auth()
        .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
        .await
        .context("failed to restore Matrix session")?;

    eprintln!("       Session restored.");

    // Step 5: Initial sync so the client knows about existing rooms
    eprintln!("[5/6] Running initial sync...");
    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync failed")?;

    // Step 6: Find or create DM, send message
    let target: &UserId = args
        .target
        .as_str()
        .try_into()
        .map_err(|e| anyhow!("invalid target user_id: {e}"))?;

    let room = match client.get_dm_room(target) {
        Some(room) => {
            eprintln!("[6/6] Found existing DM: {}", room.room_id());
            room
        }
        None => {
            eprintln!("[6/6] Creating DM with {target}...");
            let room = client.create_dm(target).await.context("create_dm failed")?;
            eprintln!("       Created room: {}", room.room_id());
            room
        }
    };

    eprintln!("       Sending message...");
    let content = RoomMessageEventContent::text_plain(&args.message);
    room.send(content).await.context("failed to send message")?;

    println!("Sent to {target} in {}: {}", room.room_id(), args.message);
    Ok(())
}
