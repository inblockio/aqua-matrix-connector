//! One-shot cross-signing probe: connect as an identity and print, from the
//! keys the homeserver publishes, whether each device is signed by its owner's
//! self-signing key. That is the check behind Element's "Encrypted by a device
//! not verified by its owner" warning.
//!
//! Env (mirrors the one-shot CLI): `AGENT_KEY_FILE`, `AGENT_STORE_DIR`
//! (required); `AGENT_CHECK_USERS` (comma-separated MXIDs to check besides the
//! identity itself), `SIWX_URL` / `MATRIX_URL` / `OIDC_CLIENT_ID` /
//! `OIDC_REDIRECT_URI` / `AGENT_DEVICE_ID` optional. Set `AGENT_DEVICE_ID` to
//! the identity's pinned device if it has one, or the connect re-keys the store
//! onto the derived device.
//!
//! The connect itself signs the identity's own device if needed (see
//! `sign_own_device_if_needed`); everything else is read-only. Never point it
//! at a store a running daemon owns (two Clients on one store).
//!
//! Run: `cargo run --example cross_signing_probe`

use anyhow::{Context, Result};
use aqua_matrix_agent::{AgentClient, AgentConfig};
use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::Client;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .init();

    let env = |k: &str| std::env::var(k).ok();
    let config = AgentConfig {
        key_file: env("AGENT_KEY_FILE").context("set AGENT_KEY_FILE")?.into(),
        siwx_url: env("SIWX_URL").unwrap_or_else(|| "https://siwx-oidc.inblock.io".into()),
        matrix_url: env("MATRIX_URL").unwrap_or_else(|| "https://matrix.inblock.io".into()),
        client_id: env("OIDC_CLIENT_ID"),
        redirect_uri: env("OIDC_REDIRECT_URI"),
        store_dir: env("AGENT_STORE_DIR")
            .context("set AGENT_STORE_DIR")?
            .into(),
        device_id: env("AGENT_DEVICE_ID"),
        device_role: Default::default(),
    };

    let agent = AgentClient::connect(config).await?;
    let client = agent.client();
    let own: OwnedUserId = client.user_id().context("not logged in")?.to_owned();
    let own_device = client.device_id().map(|d| d.to_string());
    println!("own device: {}", own_device.as_deref().unwrap_or("?"));

    let mut users = vec![own];
    for u in env("AGENT_CHECK_USERS").unwrap_or_default().split(',') {
        let u = u.trim();
        if !u.is_empty() {
            users.push(u.try_into().with_context(|| format!("invalid MXID {u}"))?);
        }
    }
    for user in &users {
        report(client, user).await?;
    }
    Ok(())
}

async fn report(client: &Client, user: &OwnedUserId) -> Result<()> {
    // Query the homeserver so the published keys, not a stale cache, are checked.
    let identity = client.encryption().request_user_identity(user).await?;
    match &identity {
        Some(id) => println!(
            "{user}: master key {}, identity verified here: {}",
            id.master_key()
                .get_first_key()
                .map(|k| k.to_base64())
                .unwrap_or_else(|| "?".into()),
            id.is_verified(),
        ),
        None => println!("{user}: NO cross-signing identity published"),
    }
    let devices = client.encryption().get_user_devices(user).await?;
    for device in devices.devices() {
        println!(
            "  device {} ({}): signed by owner: {}",
            device.device_id(),
            device.display_name().unwrap_or("-"),
            device.is_cross_signed_by_owner(),
        );
    }
    Ok(())
}
