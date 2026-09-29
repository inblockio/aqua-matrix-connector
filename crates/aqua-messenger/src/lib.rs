//! **aqua-messenger**: the transport-agnostic messaging toolset every agent
//! built on aqua-matrix-connector can expose to its model as MCP tools
//! (`send_message`, `send_file`, `list_recipients`, `read_inbox`,
//! `fetch_attachment`, and `wait_for_reply` where a profile enables it), with
//! one shared policy:
//!
//! - allow-list of people AND rooms ([`allowlist`]): the host bridge reads a
//!   file; an embedded agent defaults to its OWNER only, anything more is
//!   explicit config;
//! - per-target rate limit ([`ratelimit`]), size caps ([`profile::Limits`]),
//!   sensitive-path guard;
//! - reply threading (`reply_to`, [`engine::ReplyRef`]); inbox entries carry
//!   `event_id`, `in_reply_to` and `thread_root`;
//! - durable inbox ([`inbox`]) and on-demand attachment store ([`attachments`]);
//! - inbound content always framed as untrusted data ([`format`]);
//! - MCP tool annotations on every tool ([`jsonrpc::annotations`]).
//!
//! Matrix-free by design: the [`engine::Engine`] talks to Matrix through the
//! [`engine::Transport`] trait. Implementations live elsewhere so this crate
//! (and the stdio MCP binary `aqua-messenger-mcp`) never pull in matrix-sdk:
//! the host daemon `aqua-system-bridged` queues requests to its cycle loop, and
//! `aqua-messenger-matrix` drives an embedding agent's own live `AgentClient`.
//! Either way there is exactly one Matrix Client per crypto store.
//!
//! Front ends: [`mcp::run_stdio`] over a [`mcp::SocketBackend`] (the process
//! owning the Client serves its engine with [`server::serve`]), or the engine
//! directly in-process ([`mcp::Backend`] is implemented for it).

pub mod allowlist;
pub mod attachments;
pub mod engine;
pub mod format;
pub mod inbox;
pub mod jsonrpc;
pub mod mcp;
pub mod profile;
pub mod proto;
pub mod ratelimit;
pub mod server;

pub use engine::{
    Dest, Engine, FetchRequest, Fetched, ReplyRef, SeenRoom, Sent, Status, Transport,
};
pub use profile::{Limits, Profile};

/// Env var naming the socket `aqua-messenger-mcp` connects to.
pub const SOCK_ENV: &str = "AQUA_MESSENGER_SOCK";
