//! **aqua-seatd**: the Claude seats daemon (dev-only spike). See
//! `docs/plans/claude-seats.md`.
//!
//! One daemon per host owns one matrix-sdk Client and crypto store per seat
//! (never two Clients on one store). Front ends (`aqua-seat-mcp`,
//! `aqua-seat-listen`, `aqua-seat-stress`, all in crate `aqua-seat-mcp`) only
//! talk to its unix socket.

#![recursion_limit = "256"]

pub mod classify;
pub mod daemon;
pub mod hostkey;
pub mod hosts;
pub mod keys;
pub mod knock;
pub mod meta;
pub mod roster;
pub mod seat;
pub mod server;
pub mod store;
