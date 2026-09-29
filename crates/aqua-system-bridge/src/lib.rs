//! Core of the **Aqua System bridge**: a host-wide "system messages" channel
//! that lets every Claude Code session on this machine message allow-listed
//! people on Element (PR updates, notes, summaries) and read their replies.
//!
//! Two processes, one identity:
//!
//! - `aqua-system-bridged` (crate `aqua-system-bridged`) is the ONE long-running
//!   daemon. It owns the only matrix-sdk `Client` and crypto store for the
//!   "Aqua System" identity, writes inbound replies to a durable inbox, and
//!   serves requests on a unix socket (mode 600).
//! - `aqua-system-bridge-mcp` (this crate's binary) is a thin stdio MCP server,
//!   one per Claude session. It never touches Matrix or the crypto store; every
//!   tool call is one JSON line over the socket.
//!
//! Two Clients on one crypto store caused an OTK collision and Olm-deafness in
//! the Scribe (2026-09-27), which is why the split is strict.
//!
//! This library holds the Matrix-free pieces both halves share: the socket
//! protocol ([`proto`]), the allow-list ([`allowlist`]), the durable inbox
//! ([`inbox`]), the per-recipient rate limit ([`ratelimit`]) and formatting
//! helpers ([`format`]).

pub mod allowlist;
pub mod format;
pub mod inbox;
pub mod jsonrpc;
pub mod proto;
pub mod ratelimit;

use std::path::PathBuf;

/// Env var overriding the state directory (default `~/.aqua-system-bridge`).
pub const STATE_DIR_ENV: &str = "AQUA_SYSTEM_BRIDGE_DIR";
/// Env var overriding the socket path (default `<state dir>/bridge.sock`).
pub const SOCK_ENV: &str = "AQUA_SYSTEM_BRIDGE_SOCK";

/// Longest `wait_for_reply` the bridge honours (10 minutes).
pub const MAX_WAIT_SECS: u64 = 600;
/// Largest Markdown message body accepted by `send_message`, in bytes. Longer
/// content belongs in `send_file`.
pub const MAX_MESSAGE_BYTES: usize = 20_000;
/// Largest file accepted by `send_file`, in bytes (10 MiB).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Per-recipient send budget: at most this many sends ...
pub const RATE_LIMIT_COUNT: usize = 20;
/// ... per this many seconds (sliding window).
pub const RATE_LIMIT_WINDOW_SECS: u64 = 600;

/// The state directory: `$AQUA_SYSTEM_BRIDGE_DIR`, else `~/.aqua-system-bridge`.
/// Never under `/tmp` (a RAM-backed tmpfs on this host).
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(STATE_DIR_ENV) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".aqua-system-bridge")
}

/// The socket path: `$AQUA_SYSTEM_BRIDGE_SOCK`, else `<state dir>/bridge.sock`.
pub fn sock_path() -> PathBuf {
    if let Some(s) = std::env::var_os(SOCK_ENV) {
        return PathBuf::from(s);
    }
    state_dir().join("bridge.sock")
}
