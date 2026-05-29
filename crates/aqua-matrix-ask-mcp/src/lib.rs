//! Shared wire contracts for the Phase B `ask_human` bridge.
//!
//! Two small surfaces live here so the MCP server binary ([`crate::main`]) and
//! the daemon (`aqua-matrix-claude-p`, which owns the Matrix session) agree on
//! exactly one definition of each:
//!
//! - [`ipc`] — the daemon ⇄ MCP-server protocol over a **per-run unix domain
//!   socket**. Newline-delimited JSON, one request → one response.
//! - [`jsonrpc`] — the minimal subset of MCP's JSON-RPC 2.0 (over the server's
//!   stdio) that `claude` actually exercises headless: `initialize`,
//!   `notifications/initialized`, `tools/list`, `tools/call`.
//!
//! The whole point: the MCP subprocess does **not** talk to Matrix. It receives
//! a `tools/call` from `claude` on stdin, forwards the question to the daemon
//! over the unix socket, and returns the human's answer back to `claude` as the
//! tool result. The daemon does the actual `pending.ask(...)` (Phase A seam).
//!
//! ## Fail-closed
//!
//! Every error path (socket missing, malformed frame, daemon denies/times out)
//! resolves to a **deny** at the daemon and to a clear "not granted" tool result
//! at the server — never a silent allow.

pub mod ipc;
pub mod jsonrpc;

/// The name of the single tool this server exposes. The fully-qualified name
/// `claude` sees is `mcp__<server>__ask_human`; the daemon wires the server in
/// under the key `ask`, so `--allowedTools mcp__ask__ask_human`.
pub const TOOL_NAME: &str = "ask_human";

/// Env var the daemon sets on the spawned MCP subprocess: the path of the
/// per-run unix socket the server connects to for each `ask_human` call.
pub const SOCK_ENV: &str = "ASK_MCP_SOCK";
