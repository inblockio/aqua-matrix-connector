//! A seat on another host joins the hub's roster room by knocking (D9/D10).
//! Filled in by S5.

use std::sync::Arc;

use anyhow::{bail, Result};
use aqua_matrix_agent::AgentClient;

use crate::daemon::Daemon;
use crate::seat::{RoomFile, SeatShared};

pub async fn knock_for_roster(
    _d: &Arc<Daemon>,
    _shared: &Arc<SeatShared>,
    _agent: &AgentClient,
    _rf: &RoomFile,
) -> Result<()> {
    bail!("joining the roster from another host needs the endorsed knock (S5)")
}
