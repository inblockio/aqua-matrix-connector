//! **aqua-messenger-matrix**: the Matrix side of `aqua-messenger`.
//!
//! - [`media`]: record an attachment's media reference at ingest; download it
//!   on demand (authenticated media, cache off), decrypt and verify SHA-256.
//! - [`inbound`]: engine-filtered, event-id-deduped ingest of inbound DMs and
//!   listed-room messages (live handler + history backfill), with reply and
//!   thread metadata; the joined-room survey.
//! - [`outbound`]: sends into a resolved room with `reply_to` threading.
//! - [`AgentMessenger`]: the in-process backend. An agent that embeds the
//!   connector gets the default messenger tooling with ONE call and keeps a
//!   single Matrix Client:
//!
//! ```no_run
//! # async fn demo(agent: &aqua_matrix_agent::AgentClient, owner: &str) -> anyhow::Result<()> {
//! use aqua_messenger_matrix::AgentMessenger;
//! // Once per process: owner-only allow-list, default limits and tools
//! // (no wait_for_reply).
//! let messenger = AgentMessenger::enable_default("/srv/my-agent/messenger".as_ref(), "My Agent", owner)?;
//! // Once per connect cycle, on the agent's own live Client:
//! messenger.attach(agent).await;
//! messenger.backfill(agent).await;
//! // Expose the tools to a `claude` subprocess (stdio server `aqua-messenger-mcp`):
//! let mcp = messenger.serve_mcp("/srv/my-agent/messenger.sock".as_ref(), "aqua-messenger-mcp".as_ref())?;
//! let _json = mcp.mcp_config(); // pass via --mcp-config; allow mcp.allowed_tools()
//! // Before the Client is dropped or rotated:
//! messenger.detach().await;
//! # Ok(()) }
//! ```

pub mod inbound;
pub mod inproc;
pub mod media;
pub mod outbound;

pub use inproc::{AgentMessenger, LiveClient, McpEndpoint, MessengerConfig, MESSENGER_SERVER_KEY};
