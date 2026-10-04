//! `aqua-system-bridged`: the Aqua System bridge daemon.
//!
//! Owns the ONLY matrix-sdk `Client` + crypto store of the "Aqua System"
//! identity (two Clients on one crypto store caused the 2026-09-27 Scribe OTK
//! collision / Olm-deafness). Local sessions talk to it through a unix socket
//! (mode 600) via `aqua-system-bridge-mcp`; they never touch Matrix.
//!
//! Lifecycle, mirroring `aqua-matrix-relay::run_daemon` but multi-peer:
//! connect via siwx-oidc (outage-aware, `connect_with_outage_retry`), catch-up
//! syncs, join invites from allow-listed people only, register handlers,
//! backfill recent history into the inbox, stream-sync until ~30 s before the
//! access token expires, then REMOVE the handlers (no Client leak, no fd
//! growth) and rotate to a fresh Client. Exactly one Client exists at a time:
//! outbound sends are executed by the cycle loop itself on the current Client
//! (a send that hits `M_UNKNOWN_TOKEN` is carried into the next cycle rather
//! than rebuilding a second Client in place). SIGTERM/SIGINT exit cleanly.

#![recursion_limit = "256"]

mod bridge;
mod edit;
mod matrix;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use aqua_matrix_agent::AgentConfig;
use clap::Parser;
use tokio::sync::Notify;

#[derive(Parser, Debug)]
#[command(
    name = "aqua-system-bridged",
    about = "Aqua System Matrix bridge daemon"
)]
struct Args {
    /// State directory (key, crypto store, inbox, allow-list, logs).
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_DIR")]
    state_dir: Option<PathBuf>,
    /// Unix socket path (default: <state dir>/bridge.sock).
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_SOCK")]
    sock: Option<PathBuf>,
    /// siwx-oidc provider URL.
    #[arg(
        long,
        env = "AQUA_SYSTEM_BRIDGE_SIWX_URL",
        default_value = "https://siwx-oidc.inblock.io"
    )]
    siwx_url: String,
    /// Matrix homeserver URL.
    #[arg(
        long,
        env = "AQUA_SYSTEM_BRIDGE_MATRIX_URL",
        default_value = "https://matrix.inblock.io"
    )]
    matrix_url: String,
    /// Display name published for the identity.
    #[arg(
        long,
        env = "AQUA_SYSTEM_BRIDGE_DISPLAY_NAME",
        default_value = "Aqua System"
    )]
    display_name: String,
    /// Size cap for one fetched inbound attachment, in bytes (default 50 MiB).
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_ATTACHMENT_MAX_BYTES", default_value_t = aqua_system_bridge::attachments::DEFAULT_MAX_BYTES)]
    attachment_max_bytes: u64,
    /// Fetched attachments older than this many days are deleted (default 14).
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_ATTACHMENT_RETENTION_DAYS", default_value_t = aqua_system_bridge::attachments::DEFAULT_RETENTION_DAYS)]
    attachment_retention_days: u64,
    /// Inbox: drop messages older than this many hours, read or not
    /// (0 = no age bound, the default).
    #[arg(
        long,
        env = "AQUA_SYSTEM_BRIDGE_INBOX_MAX_AGE_HOURS",
        default_value_t = 0
    )]
    inbox_max_age_hours: u64,
    /// Inbox: hold at most this many messages.
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_INBOX_MAX_ENTRIES", default_value_t = aqua_system_bridge::inbox::MAX_ENTRIES as u64, value_parser = clap::value_parser!(u64).range(1..))]
    inbox_max_entries: u64,
    /// Inbox: over the cap, evict the oldest messages read or not (default:
    /// only read messages are evicted, unread ones are kept until read).
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_INBOX_HARD_CAP")]
    inbox_hard_cap: bool,
    /// What to do with inbound files, images, audio and video: `fetch` records
    /// them and lets a session download one on request (default), `refuse`
    /// records nothing and downloads nothing.
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_INBOUND_MEDIA", value_enum, default_value_t = InboundMedia::Fetch)]
    inbound_media: InboundMedia,
    /// Inbox: sessions mark messages processed (with a note) once handled,
    /// and a read without `since` returns every message not yet processed
    /// (default: off, a read returns unread messages and nothing is ever
    /// marked processed).
    #[arg(long, env = "AQUA_SYSTEM_BRIDGE_INBOX_TRACK_PROCESSED")]
    inbox_track_processed: bool,
    /// Print the identity (DID, and MXID once logged in) and exit.
    #[arg(long)]
    print_identity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum InboundMedia {
    Fetch,
    Refuse,
}

fn ensure_private_dir(p: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        // journald under systemd: no ANSI colour codes in the journal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,matrix_sdk=warn,matrix_sdk_crypto=warn,matrix_sdk_base=warn".into()
            }),
        )
        .init();
    let args = Args::parse();
    let state = args
        .state_dir
        .clone()
        .unwrap_or_else(aqua_system_bridge::state_dir);
    let sock = args
        .sock
        .clone()
        .unwrap_or_else(|| state.join("bridge.sock"));
    let store = state.join("store");
    let key_file = state.join("agent.pem");

    if args.print_identity {
        if key_file.exists() {
            println!("did: {}", aqua_matrix_agent::did_from_key_file(&key_file)?);
        } else {
            println!("did: (no key yet at {})", key_file.display());
        }
        let cfg =
            aqua_matrix_agent::ConfigFile::load(&store.join("config.toml")).unwrap_or_default();
        match cfg.session {
            Some(s) => println!("mxid: {}\ndevice_id: {}", s.user_id, s.device_id),
            None => println!("mxid: (not logged in yet)"),
        }
        return Ok(());
    }

    ensure_private_dir(&state)?;
    ensure_private_dir(&store)?;
    if key_file.exists() {
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600))?;
    }

    let config = AgentConfig {
        key_file: key_file.clone(),
        siwx_url: args.siwx_url.clone(),
        matrix_url: args.matrix_url.clone(),
        client_id: None,
        redirect_uri: None,
        store_dir: store,
        device_id: None,
    };

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(64);
    // The host profile: all six tools (the only profile with wait_for_reply),
    // origin tags, the file allow-list of people and [[rooms]].
    let mut profile = aqua_messenger::Profile::host();
    profile.attachments = aqua_system_bridge::attachments::AttachmentPolicy {
        max_bytes: args.attachment_max_bytes,
        retention_days: args.attachment_retention_days,
    };
    profile = profile.with_inbox(aqua_messenger::inbox::InboxPolicy {
        max_entries: usize::try_from(args.inbox_max_entries).unwrap_or(usize::MAX),
        max_age: (args.inbox_max_age_hours > 0)
            .then(|| std::time::Duration::from_secs(args.inbox_max_age_hours.saturating_mul(3600))),
        hard_cap: args.inbox_hard_cap,
        accept_media: args.inbound_media == InboundMedia::Fetch,
        track_processed: args.inbox_track_processed,
    });
    let inbox_policy = profile.inbox;
    let allow = aqua_messenger::allowlist::AllowList::new(state.join("allowlist.toml"));
    let shared = Arc::new(aqua_messenger::Engine::new(
        profile,
        &state,
        allow,
        bridge::QueueTransport::new(cmd_tx),
    ));
    tracing::info!(
        max_entries = inbox_policy.max_entries,
        max_age_hours = args.inbox_max_age_hours,
        hard_cap = inbox_policy.hard_cap,
        accept_media = inbox_policy.accept_media,
        track_processed = inbox_policy.track_processed,
        held = shared.inbox_len(),
        "inbox policy in force"
    );
    let pruned = shared.prune_attachments();
    if pruned > 0 {
        tracing::info!(pruned, "removed fetched attachments past retention");
    }

    let listener = aqua_messenger::server::bind_socket(&sock)?;
    tracing::info!(sock = %sock.display(), state = %state.display(), matrix = %args.matrix_url, "aqua-system-bridged starting");
    let server = tokio::spawn(aqua_messenger::server::serve(listener, shared.clone()));
    let sweeper = inbox_policy
        .max_age
        .map(|_| tokio::spawn(aqua_messenger::engine::retention_loop(shared.clone())));

    let shutdown = Arc::new(Notify::new());
    spawn_shutdown_listener(shutdown.clone());

    matrix::run(
        config,
        shared,
        cmd_rx,
        shutdown,
        args.display_name.clone(),
        key_file,
    )
    .await;

    server.abort();
    if let Some(h) = sweeper {
        h.abort();
    }
    let _ = std::fs::remove_file(&sock);
    tracing::info!("aqua-system-bridged stopped");
    Ok(())
}

fn spawn_shutdown_listener(shutdown: Arc<Notify>) {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async move {
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!("failed to install signal handlers");
            return;
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = int.recv() => tracing::info!("SIGINT received"),
        }
        shutdown.notify_one();
    });
}
