use anyhow::{Context, Result};
use aqua_matrix_agent::{did_from_key_file, load_dotenv, AgentClient, AgentConfig, RoomMention};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "aqua-matrix-agent",
    about = "Matrix agent: authenticate via siwx-oidc, send and read E2EE messages"
)]
struct Args {
    #[arg(long, env = "AGENT_KEY_FILE", default_value = "agent.pem")]
    key_file: PathBuf,

    #[arg(long, env = "SIWX_URL", default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,

    #[arg(long, env = "MATRIX_URL", default_value = "https://matrix.inblock.io")]
    matrix_url: String,

    #[arg(
        long,
        env = "OIDC_CLIENT_ID",
        help = "OIDC client ID (auto-registered if omitted)"
    )]
    client_id: Option<String>,

    #[arg(
        long,
        env = "OIDC_REDIRECT_URI",
        help = "OIDC redirect URI (defaults to http://localhost:0/callback)"
    )]
    redirect_uri: Option<String>,

    #[arg(
        long,
        env = "AGENT_TARGET",
        help = "Matrix user ID to message (set AGENT_TARGET, e.g. via a .env file; required for --message/--read)"
    )]
    target: Option<String>,

    #[arg(long, env = "AGENT_STORE_DIR")]
    store_dir: Option<PathBuf>,

    #[arg(
        long,
        env = "AGENT_DEVICE_ID",
        help = "Pin an explicit Matrix device_id (e.g. a role name like 'heartbeat'). Omit to derive a stable id from the agent DID."
    )]
    device_id: Option<String>,

    #[arg(long, help = "Message to send (omit to skip sending)")]
    message: Option<String>,

    #[arg(
        long,
        value_name = "MXID",
        requires = "message",
        help = "Also @mention this Matrix user in the --message (m.mentions + pill), so they are notified even in a mentions-only room"
    )]
    mention: Option<String>,

    #[arg(
        long,
        value_name = "TEXT",
        requires = "mention",
        help = "Visible text of the --mention pill (defaults to the MXID)"
    )]
    mention_name: Option<String>,

    #[arg(long, help = "Read recent messages from the DM room")]
    read: bool,

    #[arg(long, default_value = "20")]
    read_limit: u32,

    #[arg(long, help = "Print agent DID and exit")]
    print_did: bool,

    #[arg(
        long,
        env = "AGENT_DISPLAY_NAME",
        help = "Set the Matrix profile display name / alias (idempotent); applied on connect"
    )]
    display_name: Option<String>,

    #[arg(
        long,
        env = "AGENT_AVATAR",
        help = "Set the Matrix profile avatar from an image file (png/jpg/gif/webp); applied on connect (idempotent)"
    )]
    avatar: Option<String>,
}

impl Args {
    /// The user `--mention` names, with the pill text from `--mention-name`
    /// (the MXID itself when that is absent). `None` without `--mention`.
    fn mention(&self) -> Option<RoomMention<'_>> {
        self.mention.as_deref().map(|user_id| RoomMention {
            user_id,
            display: self.mention_name.as_deref().unwrap_or(user_id),
        })
    }
}

fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-matrix-agent")
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load instance config from a `.env` file before parsing args, so the
    // env-backed flags below (AGENT_TARGET, MATRIX_URL, …) can be file-driven.
    load_dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_agent=info".into()),
        )
        .init();

    let args = Args::parse();

    // Reject a malformed --mention before connecting, not after the
    // connect/sync has spent seconds of the access token's short life.
    if let (Some(msg), Some(m)) = (args.message.as_deref(), args.mention()) {
        aqua_matrix_agent::dm_content(msg, Some(&m))?;
    }

    if args.print_did {
        println!("{}", did_from_key_file(&args.key_file)?);
        return Ok(());
    }

    let config = AgentConfig {
        key_file: args.key_file.clone(),
        siwx_url: args.siwx_url.clone(),
        matrix_url: args.matrix_url.clone(),
        client_id: args.client_id.clone(),
        redirect_uri: args.redirect_uri.clone(),
        store_dir: args.store_dir.clone().unwrap_or_else(default_store_dir),
        device_id: args.device_id.clone(),
    };

    // One-shot CLI: connect once and exit. The long-running daemon modes moved
    // to their own binaries (aqua-matrix-heartbeat, aqua-matrix-claude-p) over
    // the aqua-matrix-relay lifecycle — this binary is now purely the
    // send/read/print-did tool documented in CLAUDE.md.
    let mut agent = AgentClient::connect(config).await?;

    let joined = agent.join_invited_rooms().await?;
    if !joined.is_empty() {
        agent.sync_once().await?;
    }

    // Set the profile display name (alias) when requested. Idempotent: the
    // homeserver PUT is skipped when the name already matches, so wiring this
    // into a per-send .env re-asserts the alias cheaply on every invocation.
    if let Some(ref name) = args.display_name {
        // Best-effort, mirroring the relay: a cosmetic profile write must never
        // block a send (this CLI is the critical-alert notify path).
        match agent.set_display_name(name).await {
            Ok(()) => println!("display name set to {name:?}"),
            Err(e) => eprintln!("warning: failed to set display name {name:?}: {e:#}"),
        }
    }

    if let Some(ref avatar) = args.avatar {
        // Best-effort, mirroring the display-name write above.
        match agent.set_avatar(std::path::Path::new(avatar)).await {
            Ok(()) => println!("avatar set from {avatar:?}"),
            Err(e) => eprintln!("warning: failed to set avatar {avatar:?}: {e:#}"),
        }
    }

    // --message / --read need a target; resolve it once with a clear error if
    // neither --target nor AGENT_TARGET (e.g. from .env) was provided.
    if args.message.is_some() || args.read {
        let target = args
            .target
            .as_deref()
            .context("no target set — pass --target or set AGENT_TARGET (see .env.example)")?;

        if let Some(ref msg) = args.message {
            // Self-healing send: siwx-oidc access tokens live only ~300s and the
            // restored matrix-sdk client can't refresh them itself, so a slow
            // connect() could leave us sending on an expired token (the live
            // M_UNKNOWN_TOKEN failure). send_dm_self_healing proactively rotates
            // a near-expiry token and re-auths-and-retries on a dead one, all
            // non-interactively from the persisted refresh token / the did:key.
            // --mention adds a real @mention (m.mentions.user_ids + pill);
            // without it the content is exactly the plain Markdown DM.
            let event_id = agent
                .send_dm_self_healing_with_mention(target, msg, args.mention().as_ref())
                .await?;
            println!("sent to {target}: {msg} (event: {event_id})");
        }

        if args.read {
            if args.message.is_some() {
                agent.sync_once().await?;
            }
            match agent.dm_room_id(target).await? {
                Some(room_id) => {
                    let messages = agent.messages(&room_id, args.read_limit).await?;
                    if messages.is_empty() {
                        println!("no messages found");
                    } else {
                        for msg in &messages {
                            println!("[{}] {}: {}", msg.timestamp_ms, msg.sender, msg.body);
                        }
                    }
                }
                None => println!("no DM room found with {target}"),
            }
        }
    }

    if args.message.is_none() && !args.read && args.display_name.is_none() && args.avatar.is_none()
    {
        println!("connected as {} ({})", agent.user_id(), agent.did());
        println!("use --message to send or --read to read messages");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "@alice:example.org";

    fn content_json(argv: &[&str]) -> serde_json::Value {
        let args = Args::try_parse_from(argv).unwrap();
        let msg = args.message.as_deref().unwrap();
        let content = aqua_matrix_agent::dm_content(msg, args.mention().as_ref()).unwrap();
        serde_json::to_value(&content).unwrap()
    }

    /// `--mention` makes the DM a real intentional mention: the MXID in
    /// `m.mentions.user_ids`, a matrix.to pill (with `--mention-name` as its
    /// text) in `formatted_body`, and the message text kept first.
    #[test]
    fn mention_flag_builds_user_ids_and_pill() {
        let v = content_json(&[
            "aqua-matrix-agent",
            "--message",
            "[x] CRITICAL: agent DOWN",
            "--mention",
            USER,
            "--mention-name",
            "Alice",
        ]);
        assert_eq!(v["m.mentions"]["user_ids"], serde_json::json!([USER]));
        let html = v["formatted_body"].as_str().unwrap();
        assert!(
            html.ends_with(&format!(
                "<a href=\"https://matrix.to/#/{USER}\">Alice</a></p>"
            )),
            "{html}"
        );
        assert!(v["body"]
            .as_str()
            .unwrap()
            .starts_with("[x] CRITICAL: agent DOWN"));
    }

    /// Without `--mention-name` the pill shows the MXID.
    #[test]
    fn mention_pill_defaults_to_the_mxid() {
        let v = content_json(&["aqua-matrix-agent", "--message", "hi", "--mention", USER]);
        let html = v["formatted_body"].as_str().unwrap();
        assert!(html.contains(&format!(">{USER}</a>")), "{html}");
    }

    /// No `--mention`: no `m.mentions` at all, and the content is exactly the
    /// plain Markdown DM the CLI always sent.
    #[test]
    fn no_mention_without_the_flag() {
        let v = content_json(&["aqua-matrix-agent", "--message", "[x] INFO: all good"]);
        assert!(v.get("m.mentions").is_none(), "{v}");
        use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
        let plain = RoomMessageEventContent::text_markdown("[x] INFO: all good");
        assert_eq!(v, serde_json::to_value(&plain).unwrap());
    }

    /// `--mention` needs a `--message`, `--mention-name` needs a `--mention`.
    #[test]
    fn mention_flags_require_their_parent() {
        assert!(Args::try_parse_from(["aqua-matrix-agent", "--mention", USER]).is_err());
        assert!(Args::try_parse_from([
            "aqua-matrix-agent",
            "--message",
            "m",
            "--mention-name",
            "A"
        ])
        .is_err());
    }
}
