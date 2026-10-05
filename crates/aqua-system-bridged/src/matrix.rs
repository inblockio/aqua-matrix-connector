//! The Matrix side: one Client at a time, rotated before token expiry.
//! See the crate docs in `main.rs` for the lifecycle.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aqua_matrix_agent::{
    connect_with_outage_retry, is_unknown_token, AgentClient, AgentConfig, ConnectOutcome,
    DeviceRole,
};
use aqua_messenger::allowlist::{invite_action, InviteAction};
use aqua_messenger::{Dest, ReplyRef, SeenRoom};
use aqua_messenger_matrix::inbound::{
    backfill, joined_member_count, register_message_handler, survey_rooms,
};
use matrix_sdk::{
    config::SyncSettings,
    event_handler::EventHandlerHandle,
    room::Room,
    ruma::{
        events::{
            direct::DirectEventContent,
            room::member::{MembershipState, StrippedRoomMemberEvent},
        },
        OwnedUserId, RoomId,
    },
    RoomState,
};
use serde_json::json;
use tokio::sync::{mpsc, Notify};

use crate::bridge::{CmdOk, Engine, SendCmd, SendKind, Target};

/// The daemon's shared state is the messenger engine (host profile).
type Shared = Engine;

const ROLE: &str = "aqua-system";
const REFRESH_GUARD_SECS: u64 = 30;
const MIN_CYCLE_SECS: u64 = 15;
const MAX_CONNECT_FAILURES: u32 = 3;
/// Upper bound on one send attempt (the connector's own retries included).
const SEND_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound on one attachment download + decrypt (50 MiB on a slow link).
const FETCH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(150);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn run(
    config: AgentConfig,
    shared: Arc<Shared>,
    mut rx: mpsc::Receiver<SendCmd>,
    shutdown: Arc<Notify>,
    display_name: String,
    key_file: PathBuf,
) {
    let mut first_cycle = true;
    // A send that hit M_UNKNOWN_TOKEN on the old Client, retried on the next.
    let mut carry: Option<SendCmd> = None;
    let role = config.device_role;
    if role == DeviceRole::Secondary {
        tracing::info!(
            "secondary device: invites, m.direct, the display name and new DM rooms stay with the primary bridge"
        );
    }
    loop {
        let agent = match connect_with_outage_retry(
            &config,
            ROLE,
            MAX_CONNECT_FAILURES,
            shutdown.notified(),
        )
        .await
        {
            ConnectOutcome::Connected(a) => a,
            ConnectOutcome::Shutdown => {
                tracing::info!("shutdown while connecting");
                return;
            }
            ConnectOutcome::Fatal(e) => {
                tracing::error!("{MAX_CONNECT_FAILURES} consecutive non-transient connect failures ({e:#}); exiting for systemd restart");
                std::process::exit(1);
            }
        };
        // connect() may have just generated the key: keep it owner-only.
        let _ = std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600));
        shared.set_status(|st| {
            st.connected = true;
            st.did = Some(agent.did().to_string());
            st.user_id = Some(agent.user_id().to_string());
            st.device_id = agent.device_id();
            st.last_error = None;
        });

        if let Err(e) = agent.sync_once_nowait().await {
            tracing::warn!("pre-join sync failed: {e:#}");
        }
        if role == DeviceRole::Primary {
            join_allowed_invites(&agent, &shared).await;
        }
        if let Err(e) = agent.sync_once_nowait().await {
            tracing::warn!("settle sync failed: {e:#}");
        }
        if role == DeviceRole::Primary {
            repair_m_direct(&agent, &shared).await;
        }
        survey_rooms(&agent, &shared, first_cycle).await;
        if first_cycle {
            if role == DeviceRole::Primary {
                match agent.set_display_name(&display_name).await {
                    Ok(()) => tracing::info!("display name set to {display_name:?}"),
                    Err(e) => tracing::warn!("set display name failed: {e:#}"),
                }
            }
            tracing::info!(did = %agent.did(), mxid = %agent.user_id(), "identity");
            first_cycle = false;
        }

        let exit = run_cycle(&agent, &shared, &mut rx, &mut carry, &shutdown, role).await;
        shared.set_status(|st| st.connected = false);
        if exit == "shutdown" {
            // Fail anything still queued so callers are not left hanging.
            if let Some(c) = carry.take() {
                let _ = c.done.send(Err("bridge shutting down; not sent".into()));
            }
            return;
        }
        tracing::info!("cycle ended ({exit}); reconnecting");
    }
}

async fn run_cycle(
    agent: &AgentClient,
    shared: &Arc<Shared>,
    rx: &mut mpsc::Receiver<SendCmd>,
    carry: &mut Option<SendCmd>,
    shutdown: &Notify,
    role: DeviceRole,
) -> &'static str {
    let own = agent.user_id().to_string();
    let mut handles: Vec<EventHandlerHandle> =
        vec![register_message_handler(agent, shared.clone())];
    if role == DeviceRole::Primary {
        handles.push(register_invite_handler(agent, shared.clone(), own.clone()));
    }

    backfill(agent, shared).await;

    let sync_client = agent.client().clone();
    aqua_matrix_agent::reload_olm_if_store_changed(&sync_client).await;
    let mut sync_task =
        tokio::spawn(async move { sync_client.sync(SyncSettings::default()).await });

    let now = unix_now();
    let ttl = agent
        .expires_at_unix()
        .saturating_sub(now)
        .saturating_sub(REFRESH_GUARD_SECS)
        .max(MIN_CYCLE_SECS);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
    tracing::info!("cycle starting (rotating in {ttl}s)");

    let exit = 'cycle: {
        if let Some(cmd) = carry.take() {
            if let Some(retry) = execute(agent, shared, cmd, role).await {
                *carry = Some(retry);
                break 'cycle "token-rejected";
            }
        }
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => break 'cycle "shutdown",
                _ = tokio::time::sleep_until(deadline) => break 'cycle "refresh-deadline",
                res = &mut sync_task => {
                    match res {
                        Ok(Ok(())) => tracing::warn!("sync returned Ok (unexpected)"),
                        Ok(Err(e)) => tracing::warn!("sync error: {e:#}"),
                        Err(e) => tracing::warn!("sync task join error: {e:#}"),
                    }
                    break 'cycle "sync-ended";
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break 'cycle "shutdown" };
                    if let Some(retry) = execute(agent, shared, cmd, role).await {
                        *carry = Some(retry);
                        break 'cycle "token-rejected";
                    }
                }
            }
        }
    };

    if !sync_task.is_finished() {
        sync_task.abort();
        let _ = sync_task.await;
    }
    // Break the Client <-> handler reference cycle so the old Client (and its
    // SQLite pools) drops before the next one is built (connector fd-leak fix).
    for h in handles {
        agent.client().remove_event_handler(h);
    }
    exit
}

/// Run one send on the live Client. Returns the command back when the token
/// was rejected (to retry on the next cycle's fresh Client); otherwise replies.
async fn execute(
    agent: &AgentClient,
    shared: &Shared,
    cmd: SendCmd,
    role: DeviceRole,
) -> Option<SendCmd> {
    if Instant::now() > cmd.deadline {
        let _ = cmd.done.send(Err(
            "the bridge could not reach Matrix before the send deadline (outage?); not sent".into(),
        ));
        return None;
    }
    // Downloads run here, inline, on the one live Client: spawning them would
    // keep this Client alive into the next cycle next to its successor.
    let (limit, what) = match cmd.kind {
        SendKind::Fetch { .. } => (FETCH_ATTEMPT_TIMEOUT, "fetch"),
        _ => (SEND_ATTEMPT_TIMEOUT, "send"),
    };
    let fut = async {
        let reply = cmd.reply.as_ref();
        match &cmd.kind {
            SendKind::Text(md) => {
                let room_id = destination_room(
                    agent,
                    shared,
                    &cmd.to,
                    reply,
                    MissingDm::for_new_message(role),
                )
                .await?;
                aqua_messenger_matrix::outbound::send_text(agent, &room_id, md, reply)
                    .await
                    .map(CmdOk::Sent)
            }
            SendKind::File { path, caption } => {
                let room_id = destination_room(
                    agent,
                    shared,
                    &cmd.to,
                    reply,
                    MissingDm::for_new_message(role),
                )
                .await?;
                aqua_messenger_matrix::outbound::send_file(agent, &room_id, path, caption, reply)
                    .await
                    .map(CmdOk::Sent)
            }
            SendKind::Edit { original, content } => {
                let room_id =
                    destination_room(agent, shared, &cmd.to, None, MissingDm::Refuse).await?;
                crate::edit::ensure_editable(agent.client(), &room_id, original).await?;
                agent
                    .send_content_to_room(&room_id, (**content).clone())
                    .await
                    .map(|id| CmdOk::Sent(id.into()))
            }
            SendKind::Fetch {
                event_id,
                room_id,
                media,
                max_bytes,
            } => aqua_messenger_matrix::media::fetch(
                agent.client(),
                room_id,
                event_id,
                media.clone(),
                *max_bytes,
            )
            .await
            .map(|(bytes, mimetype)| CmdOk::Fetched { bytes, mimetype }),
        }
    };
    let res = match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "{what} timed out after {}s (outcome unknown)",
            limit.as_secs()
        )),
    };
    match res {
        Ok(ok) => {
            let _ = cmd.done.send(Ok(ok));
            None
        }
        Err(e) if is_unknown_token(&e) => {
            tracing::warn!("send rejected with M_UNKNOWN_TOKEN; retrying on a fresh client");
            Some(cmd)
        }
        Err(e) => {
            let _ = cmd.done.send(Err(format!("{e:#}")));
            None
        }
    }
}

/// Join invites already pending at connect time, from allow-listed people only;
/// decline the rest.
async fn join_allowed_invites(agent: &AgentClient, shared: &Arc<Shared>) {
    for room in agent.client().invited_rooms() {
        let inviter = match room.invite_details().await {
            Ok(inv) => inv.inviter_id.to_string(),
            Err(e) => {
                tracing::warn!(room = %room.room_id(), "invite details unavailable: {e:#}");
                continue;
            }
        };
        // For an invited room this reads `is_direct` from our own stripped
        // member event (the invite itself).
        let is_direct = room.is_direct().await.unwrap_or(false);
        handle_invite(agent, shared, room, inviter, is_direct).await;
    }
}

async fn handle_invite(
    agent: &AgentClient,
    shared: &Arc<Shared>,
    room: Room,
    inviter: String,
    is_direct: bool,
) {
    let room_id = room.room_id().to_string();
    let (allowed, listed) = shared.with_allow(|allow| {
        (
            allow.by_mxid(&inviter).is_some(),
            allow.room_by_id(&room_id).map(|r| r.name.clone()),
        )
    });
    let action = invite_action(allowed, is_direct, listed.is_some());
    if action == InviteAction::Decline {
        tracing::info!(%room_id, %inviter, "declining invite from a non-allow-listed user");
        shared.audit(json!({"event": "invite_declined", "from": inviter, "room": room_id}));
        if let Err(e) = room.leave().await {
            tracing::warn!(%room_id, "decline failed: {e:#}");
        }
        return;
    }
    match room.join().await {
        Ok(()) => {
            let seen = SeenRoom {
                display_name: room.name(),
                joined_members: joined_member_count(&room).await,
                is_direct: action == InviteAction::JoinAsDm,
            };
            shared.note_joined_room(&room_id, seen);
            if action == InviteAction::JoinAsDm {
                tracing::info!(%room_id, %inviter, "joined DM invite from allow-listed user");
                if let Err(e) = agent.mark_dm(&room_id, &inviter).await {
                    tracing::warn!(%room_id, "mark_dm failed: {e:#}");
                }
            } else {
                // A group room: never recorded in m.direct. Sendable only once
                // it is listed under [[rooms]].
                tracing::info!(
                    %room_id,
                    %inviter,
                    listed = listed.as_deref().unwrap_or("-"),
                    "joined GROUP room invite from allow-listed user (not marked as DM{})",
                    if listed.is_some() { "" } else { "; not sendable until listed under [[rooms]]" }
                );
                shared.audit(json!({"event": "group_joined", "from": inviter, "room": room_id, "listed": listed}));
            }
        }
        Err(e) => tracing::warn!(%room_id, "join failed: {e:#}"),
    }
}

/// What a command does when the person has no DM with the bridge yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingDm {
    /// Create one (a new message).
    Create,
    /// Fail (an edit: without a DM there is nothing to edit).
    Refuse,
    /// Fail (a secondary device: the primary bridge creates DM rooms).
    LeaveToPrimary,
}

impl MissingDm {
    /// The policy of a new message: only the primary creates DM rooms.
    fn for_new_message(role: DeviceRole) -> Self {
        match role {
            DeviceRole::Primary => MissingDm::Create,
            DeviceRole::Secondary => MissingDm::LeaveToPrimary,
        }
    }
}

/// The room a send goes to. A listed room must already be joined. A person's
/// DM is resolved here (never through `m.direct` alone, never to a listed
/// `[[rooms]]` entry or a group room), and created when none exists if
/// `missing` allows it.
async fn destination_room(
    agent: &AgentClient,
    shared: &Shared,
    to: &Target,
    reply: Option<&ReplyRef>,
    missing: MissingDm,
) -> anyhow::Result<String> {
    let Target::Dest(dest) = to else {
        anyhow::bail!("internal error: send without a destination");
    };
    match dest {
        Dest::Room(id) => {
            let rid = <&RoomId>::try_from(id.as_str())
                .map_err(|e| anyhow::anyhow!("invalid room id {id}: {e}"))?;
            match agent.client().get_room(rid).map(|r| r.state()) {
                Some(RoomState::Joined) => Ok(id.clone()),
                Some(RoomState::Invited) => anyhow::bail!(
                    "the bridge has a pending invite to room {id} but has not joined it yet (invites are joined on the next reconnect, within ~5 minutes)"
                ),
                _ => anyhow::bail!(
                    "the bridge is not a member of room {id}; invite the Aqua System identity to it first"
                ),
            }
        }
        Dest::Person(mxid) => {
            let prefer = reply.and_then(|r| r.room_id.as_deref());
            dm_room_for(agent, shared, mxid, prefer, missing).await
        }
    }
}

/// Resolve (or create, if `missing` allows) the 1:1 DM with `mxid`.
/// Candidates: joined rooms with at most two joined members, not listed under
/// `[[rooms]]`, where the person is joined (or invited). Preference: the room
/// of the message being replied to (`prefer`, when it is a candidate), then
/// rooms recorded in `m.direct`, then the person joined over invited, then the
/// connector's own pick (most recent activity), then room id order.
async fn dm_room_for(
    agent: &AgentClient,
    shared: &Shared,
    mxid: &str,
    prefer: Option<&str>,
    missing: MissingDm,
) -> anyhow::Result<String> {
    let target =
        OwnedUserId::try_from(mxid).map_err(|e| anyhow::anyhow!("invalid MXID {mxid}: {e}"))?;
    let listed = listed_room_ids(shared);
    let connector_pick = agent.dm_room_id(mxid).await.ok().flatten();
    let mut best: Option<((bool, bool, u8, bool), String)> = None;
    for room in agent.client().joined_rooms() {
        let id = room.room_id().to_string();
        if listed.contains(&id) || joined_member_count(&room).await > 2 {
            continue;
        }
        let Some(member) = room.get_member(&target).await.ok().flatten() else {
            continue;
        };
        let rank = match member.membership() {
            MembershipState::Join => 2u8,
            MembershipState::Invite => 1u8,
            _ => continue,
        };
        let is_direct = room.is_direct().await.unwrap_or(false);
        let key = (
            prefer == Some(id.as_str()),
            is_direct,
            rank,
            connector_pick.as_deref() == Some(id.as_str()),
        );
        let better = match &best {
            None => true,
            Some((k, bid)) => key > *k || (key == *k && id < *bid),
        };
        if better {
            best = Some((key, id));
        }
    }
    if let Some((_, id)) = best {
        return Ok(id);
    }
    if missing == MissingDm::LeaveToPrimary {
        anyhow::bail!(
            "this secondary bridge has no DM with {mxid} and leaves creating one to the primary bridge"
        );
    }
    if missing == MissingDm::Refuse {
        anyhow::bail!("the bridge has no DM with {mxid}, so there is no message there to edit");
    }
    let room = agent
        .client()
        .create_dm(&target)
        .await
        .map_err(|e| anyhow::anyhow!("create_dm failed: {e}"))?;
    match room.latest_encryption_state().await {
        Ok(state) if state.is_encrypted() => {}
        _ => {
            if let Err(e) = room.enable_encryption().await {
                tracing::warn!("failed to enable encryption on fresh DM: {e:#}");
            }
        }
    }
    let id = room.room_id().to_string();
    if let Err(e) = agent.mark_dm(&id, mxid).await {
        tracing::warn!(room_id = %id, "mark_dm on new DM failed: {e:#}");
    }
    tracing::info!(room_id = %id, to = %mxid, "created a new DM room");
    Ok(id)
}

fn listed_room_ids(shared: &Shared) -> std::collections::HashSet<String> {
    shared.with_allow(|allow| allow.rooms().iter().map(|r| r.room_id.clone()).collect())
}

/// Remove group rooms from our `m.direct`: every `[[rooms]]` entry and every
/// joined room with more than two joined members (2026-09-29 incident: two
/// group-room invites were recorded as Tim's DMs). Runs on every connect;
/// writes only when something changed. Best-effort.
async fn repair_m_direct(agent: &AgentClient, shared: &Shared) {
    let listed = listed_room_ids(shared);
    let mut groups = std::collections::HashSet::new();
    for r in agent.client().joined_rooms() {
        if joined_member_count(&r).await > 2 {
            groups.insert(r.room_id().to_string());
        }
    }
    let raw = match agent
        .client()
        .account()
        .fetch_account_data_static::<DirectEventContent>()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("m.direct repair: fetch failed: {e:#}");
            return;
        }
    };
    let content = match raw.map(|r| r.deserialize()).transpose() {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("m.direct repair: unparsable m.direct, left alone: {e:#}");
            return;
        }
    };
    let Some((fixed, removed)) = aqua_system_bridge::direct::repair_direct(&content.0, |r| {
        listed.contains(r) || groups.contains(r)
    }) else {
        tracing::debug!("m.direct repair: nothing to remove");
        return;
    };
    if let Err(e) = agent
        .client()
        .account()
        .set_account_data(DirectEventContent(fixed))
        .await
    {
        tracing::warn!("m.direct repair: write failed: {e:#}");
        return;
    }
    for (user, room) in &removed {
        let reason = if listed.contains(room.as_str()) {
            "listed [[rooms]] entry"
        } else {
            "group room (>2 joined members)"
        };
        tracing::info!(user = %user, room_id = %room, reason, "m.direct repair: removed a group room from a DM entry");
        shared.audit(json!({"event": "m_direct_removed", "user": user.to_string(), "room": room.to_string(), "reason": reason}));
    }
}

fn register_invite_handler(
    agent: &AgentClient,
    shared: Arc<Shared>,
    own: String,
) -> EventHandlerHandle {
    let agent_c = agent.clone();
    agent
        .client()
        .add_event_handler(move |ev: StrippedRoomMemberEvent, room: Room| {
            let agent = agent_c.clone();
            let shared = shared.clone();
            let own = own.clone();
            async move {
                if ev.state_key.as_str() != own || ev.content.membership != MembershipState::Invite
                {
                    return;
                }
                let is_direct = ev.content.is_direct.unwrap_or(false);
                handle_invite(&agent, &shared, room, ev.sender.to_string(), is_direct).await;
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_primary_creates_dm_rooms_for_new_messages() {
        assert_eq!(
            MissingDm::for_new_message(DeviceRole::Primary),
            MissingDm::Create
        );
        assert_eq!(
            MissingDm::for_new_message(DeviceRole::Secondary),
            MissingDm::LeaveToPrimary
        );
    }
}
