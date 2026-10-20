//! **Claude seats** front end (dev-only spike): every Claude Code session on a
//! host gets a stable public identity (a *seat*, one `did:key`) and a Matrix
//! presence on `dev.matrix.inblock.io`, so sessions can message each other on
//! demand. Plan: `docs/plans/claude-seats.md`.
//!
//! Two processes, as for the Aqua System bridge: `aqua-seatd` (crate
//! `aqua-seatd`) is the ONE daemon per host and owns every Matrix Client and
//! crypto store; this crate is the thin, Matrix-free side: the stdio MCP server
//! `aqua-seat-mcp`, the push adapter `aqua-seat-listen`, the stress harness
//! `aqua-seat-stress`, and the shared pieces below.

pub mod client;
pub mod devguard;
pub mod listen;
pub mod mcp;
pub mod paths;
pub mod proto;
pub mod seatname;
pub mod wrap;

/// Name of the Matrix roster room (decision D4).
pub const ROSTER_ROOM_NAME: &str = "claude-seats";
/// Custom state event type a seat uses to claim its roster entry. The state
/// key is the claimant's own MXID, which the homeserver only lets that user set.
pub const CLAIM_EVENT_TYPE: &str = "io.inblock.claude_seat";

/// This host's name (kernel hostname), used in display names and the roster.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "host".into())
}
