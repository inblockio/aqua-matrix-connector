//! aqua-matrix-heartbeat — the ops agent for this host.
//!
//! Periodic status DMs (host facts + a snoop of the active Claude Code
//! transcript) plus a `#shell` command channel, both delivered over the
//! generic [`aqua_matrix_relay`] daemon. The Matrix/auth lifecycle lives in the
//! relay; everything here is the host-specific telemetry and command set, which
//! deliberately stays OUT of the reusable reference.
//!
//! Thin `main`: parse args → build [`AgentConfig`] → hand an [`OpsHandler`] to
//! [`run_daemon`].
use std::path::PathBuf;
use std::time::Duration;

use aqua_matrix_relay::{load_dotenv, run_daemon, AgentConfig};
use clap::Parser;

mod ops;
use ops::OpsHandler;

#[derive(Parser)]
#[command(
    name = "aqua-matrix-heartbeat",
    about = "Ops agent: periodic status DMs + `#shell` command channel"
)]
struct Args {
    #[arg(long, env = "AGENT_KEY_FILE", default_value = "heartbeat.pem")]
    key_file: PathBuf,

    #[arg(long, env = "SIWX_URL", default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,

    #[arg(long, env = "MATRIX_URL", default_value = "https://matrix.inblock.io")]
    matrix_url: String,

    #[arg(long, env = "OIDC_CLIENT_ID", help = "OIDC client ID (auto-registered if omitted)")]
    client_id: Option<String>,

    #[arg(long, env = "OIDC_REDIRECT_URI", help = "OIDC redirect URI (defaults to http://localhost:0/callback)")]
    redirect_uri: Option<String>,

    #[arg(
        long,
        env = "AGENT_TARGET",
        help = "Matrix user ID to report to and accept `#shell` commands from (set AGENT_TARGET, e.g. via this instance's .env file)"
    )]
    target: String,

    #[arg(long, env = "AGENT_STORE_DIR")]
    store_dir: Option<PathBuf>,

    #[arg(
        long,
        default_value = "600",
        help = "Status interval in seconds (default 600 = 10 minutes)"
    )]
    interval: u64,
}

fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-matrix-heartbeat")
}

#[tokio::main]
async fn main() {
    // Load this instance's config from its `.env` before parsing args (see
    // `load_dotenv`): AGENT_TARGET et al. become file-driven and per-instance.
    load_dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_agent=info,aqua_matrix_relay=info,aqua_matrix_heartbeat=info".into()),
        )
        .init();

    let args = Args::parse();
    let interval = Duration::from_secs(args.interval);
    let store_dir = args.store_dir.unwrap_or_else(default_store_dir);
    let config = AgentConfig {
        key_file: args.key_file,
        siwx_url: args.siwx_url,
        matrix_url: args.matrix_url,
        client_id: args.client_id,
        redirect_uri: args.redirect_uri,
        store_dir: store_dir.clone(),
    };

    run_daemon(config, &args.target, OpsHandler::new(interval, &store_dir)).await;
}
