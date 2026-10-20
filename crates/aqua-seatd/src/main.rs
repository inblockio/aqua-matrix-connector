//! `aqua-seatd`: the Claude seats daemon. Dev only.
//!
//! Defaults point at `dev.siwx.inblock.io` and `dev.matrix.inblock.io`; any
//! non-dev host is refused. State lives under `~/.local/state/claude-seats/`
//! (`CLAUDE_SEATS_STATE`), the socket at `$XDG_RUNTIME_DIR/claude-seats/seatd.sock`
//! (`CLAUDE_SEATS_SOCK`). No `.env` file is read.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use aqua_seat::{devguard, paths};
use aqua_seatd::daemon::{Config, Daemon};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "aqua-seatd", about = "Claude seats daemon (dev-only spike)")]
struct Args {
    /// siwx-oidc provider (dev only).
    #[arg(long, env = "CLAUDE_SEATS_SIWX_URL", default_value = devguard::DEFAULT_SIWX_URL)]
    siwx_url: String,
    /// Matrix homeserver (dev only).
    #[arg(long, env = "CLAUDE_SEATS_MATRIX_URL", default_value = devguard::DEFAULT_MATRIX_URL)]
    matrix_url: String,
    /// State root (default ~/.local/state/claude-seats).
    #[arg(long, env = paths::STATE_ENV)]
    state_dir: Option<PathBuf>,
    /// Socket path (default $XDG_RUNTIME_DIR/claude-seats/seatd.sock).
    #[arg(long, env = paths::SOCK_ENV)]
    sock: Option<PathBuf>,
    /// Host name used in display names and the roster (default: kernel hostname).
    #[arg(long, env = "CLAUDE_SEATS_HOST")]
    host: Option<String>,
    /// A seat nobody touched for this long stops syncing.
    #[arg(long, env = "CLAUDE_SEATS_IDLE_DETACH_SECS", default_value_t = 900)]
    idle_detach_secs: u64,
    /// Sends allowed per recipient per window.
    #[arg(long, env = "CLAUDE_SEATS_RATE_COUNT", default_value_t = aqua_seat::proto::RATE_LIMIT_COUNT)]
    rate_count: usize,
    #[arg(long, env = "CLAUDE_SEATS_RATE_WINDOW_SECS", default_value_t = aqua_seat::proto::RATE_LIMIT_WINDOW_SECS)]
    rate_window_secs: u64,
    /// How long an attach waits for login and the roster join.
    #[arg(long, env = "CLAUDE_SEATS_ATTACH_TIMEOUT_SECS", default_value_t = 180)]
    attach_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aqua_seatd=info,aqua_matrix_agent=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    devguard::require_dev(&args.siwx_url).context("--siwx-url")?;
    devguard::require_dev(&args.matrix_url).context("--matrix-url")?;

    let root = args.state_dir.unwrap_or_else(paths::state_root);
    let sock = args.sock.unwrap_or_else(paths::sock_path);
    paths::ensure_private_dir(&root)?;
    if let Some(dir) = sock.parent() {
        paths::ensure_private_dir(dir)?;
    }

    // One daemon per state root: two would mean two Clients on one store.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::daemon_lock(&root))
        .context("opening the daemon lock")?;
    if lock.try_lock().is_err() {
        bail!(
            "another aqua-seatd already owns {}; refusing to start a second one",
            root.display()
        );
    }

    let listener = aqua_messenger::server::bind_socket(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    let cfg = Config {
        root: root.clone(),
        sock: sock.clone(),
        siwx_url: args.siwx_url,
        matrix_url: args.matrix_url,
        host: args.host.unwrap_or_else(aqua_seat::hostname),
        idle_detach: Duration::from_secs(args.idle_detach_secs),
        rate_count: args.rate_count,
        rate_window: Duration::from_secs(args.rate_window_secs),
        attach_timeout: Duration::from_secs(args.attach_timeout_secs),
    };
    tracing::info!(
        state = %root.display(), sock = %sock.display(), host = %cfg.host,
        siwx = %cfg.siwx_url, matrix = %cfg.matrix_url,
        "aqua-seatd starting (dev only)"
    );
    let d = Daemon::new(cfg);
    let server = tokio::spawn(aqua_seatd::server::serve(listener, d.clone()));
    let idle = tokio::spawn(d.clone().idle_loop());

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = d.shutdown.notified() => tracing::info!("shutdown requested"),
    }
    server.abort();
    idle.abort();
    d.detach_all().await;
    let _ = std::fs::remove_file(&sock);
    drop(lock);
    Ok(())
}
