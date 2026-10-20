//! A seat on another host joins the hub's roster room by knocking (D9/D10):
//! `knock` with `reason` = this host's signed endorsement of the seat.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use aqua_matrix_agent::AgentClient;

use crate::daemon::Daemon;
use crate::endorse::{make_reason, Endorsement};
use crate::hostkey;
use crate::seat::{unix_now, RoomFile, SeatShared};

/// Send one knock for `rf`'s room. The endorsement is made fresh each time
/// (its timestamp is only good for ten minutes).
pub async fn knock_for_roster(
    d: &Arc<Daemon>,
    shared: &Arc<SeatShared>,
    agent: &AgentClient,
    rf: &RoomFile,
) -> Result<()> {
    let key = hostkey::load_signing_key(&d.cfg.root)?;
    let e = Endorsement {
        seat: shared.seat.clone(),
        seat_did: agent.did().to_string(),
        host: d.cfg.host.clone(),
        host_did: d.cfg.host_did.clone(),
        ts: unix_now(),
    };
    let reason = make_reason(&key, &e);
    let server = agent
        .user_id()
        .split_once(':')
        .map(|(_, s)| s)
        .ok_or_else(|| anyhow!("malformed own user id"))?;
    let via: matrix_sdk::ruma::OwnedServerName = server
        .try_into()
        .map_err(|e| anyhow!("bad server name {server}: {e}"))?;
    let target: matrix_sdk::ruma::OwnedRoomOrAliasId = rf
        .room_id
        .as_str()
        .try_into()
        .map_err(|e| anyhow!("bad room id: {e}"))?;
    agent
        .client()
        .knock(target, Some(reason), vec![via])
        .await
        .context("knocking at the roster room")?;
    tracing::info!(seat = %shared.seat, host = %d.cfg.host, "knocked at the roster room with a host endorsement");
    Ok(())
}
