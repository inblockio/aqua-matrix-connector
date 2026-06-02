//! aqua-matrix-gating — the connector-side confirmation/gating substrate.
//!
//! This crate is the agent/transport-agnostic core of the chat-confirmations
//! flow, extracted out of the Claude backend so any backend can reuse it. None
//! of it mentions `claude`. It provides three pieces:
//!
//! - [`PendingMap`] — the shared `ask_user` primitive: a pending-reply router
//!   that lets a run pause and ask the authenticated user a question over the
//!   same Matrix channel, resolving on their next DM.
//! - [`destructive`] — a small, table-driven matcher that decides whether a
//!   request is dangerous enough to gate behind a confirmation.
//! - [`AskBridge`] / [`accept_loop`] — the per-run `ask_human` unix-socket
//!   bridge backing the `aqua-matrix-ask-mcp` MCP tool.
//!
//! ## Cross-boundary invariant (connector ↔ agents)
//!
//! [`PendingMap`] alone does **NOT** serialize runs. The "one open question per
//! target" guarantee also needs the backend's per-target `run_lock`, which stays
//! **agents-side**. So the invariant is split across the crate boundary: this
//! connector crate provides the pending-reply router; the agent backend must
//! hold its own per-target run lock to ensure at most one question is open per
//! target at a time.

pub mod destructive;
mod ask_bridge;
mod pending;

pub use ask_bridge::{accept_loop, AskBridge};
pub use pending::PendingMap;

/// The MCP server key in the generated `ask_human` `--mcp-config`. The
/// fully-qualified tool name `claude` sees is therefore `mcp__ask__ask_human`
/// (see [`ASK_HUMAN_TOOL`]).
///
/// SINGLE SOURCE OF TRUTH: this used to be `ask_bridge::SERVER_KEY` and a
/// separate `ASK_HUMAN_TOOL` const in the backend's `main.rs`. They could drift;
/// now both live here and a unit test asserts they stay consistent.
pub const ASK_SERVER_KEY: &str = "ask";

/// The fully-qualified `ask_human` MCP tool name the backend merges into
/// `--allowedTools` when the ask bridge is up. Derived from [`ASK_SERVER_KEY`]
/// so the two cannot drift (the `derived_tool_name_matches_server_key` test
/// enforces `ASK_HUMAN_TOOL == "mcp__{ASK_SERVER_KEY}__ask_human"`).
pub const ASK_HUMAN_TOOL: &str = concat!("mcp__", "ask", "__ask_human");

/// System-prompt nudge appended to a run that carries the bridge: tells the
/// model the `ask_human` tool exists and *when* to reach for it. Advisory — the
/// model must choose to call it.
pub const ASK_SYSTEM_PROMPT: &str = "You have an `ask_human` tool (mcp__ask__ask_human) that puts a \
question to the authenticated human operator over the chat channel and blocks until they answer. \
Before any destructive or irreversible action (deleting files, `rm`/`rm -rf`, `git push --force`, \
`git reset --hard`, dropping data, or anything you cannot undo), you MUST call `ask_human` with the \
exact command and its blast radius, and proceed only if the answer authorises it. If it returns an \
error or a denial, do NOT perform the action — stop and report it.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_tool_name_matches_server_key() {
        // The single-source-of-truth guarantee: `ASK_HUMAN_TOOL` must always be
        // `mcp__<ASK_SERVER_KEY>__ask_human`. If `ASK_SERVER_KEY` ever changes,
        // this fails until `ASK_HUMAN_TOOL`'s `concat!` is updated to match.
        assert_eq!(ASK_HUMAN_TOOL, format!("mcp__{ASK_SERVER_KEY}__ask_human"));
    }
}
