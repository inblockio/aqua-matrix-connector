//! The Matrix side: one Client at a time, rotated before token expiry.
//! See the crate docs in `main.rs` for the lifecycle.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aqua_matrix_agent::{
    connect_with_outage_retry, is_unknown_token, AgentClient, AgentConfig, ConnectOutcome,
};
use aqua_system_bridge::inbox::NewEntry;
use matrix_sdk::{
    config::SyncSettings,
    event_handler::EventHandlerHandle,
    room::{MessagesOptions, Room},
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::{
            receipt::ReceiptThread,
            room::{
                member::{MembershipState, StrippedRoomMemberEvent},
                message::{MessageType, OriginalSyncRoomMessageEvent},
            },
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        },
        UInt,
    },
};
use serde_json::json;
use tokio::sync::{mpsc, Notify};

use crate::bridge::{SendCmd, SendKind, Shared};

const ROLE: &str = "aqua-system";
const REFRESH_GUARD_SECS: u64 = 30;
const MIN_CYCLE_SECS: u64 = 15;
const MAX_CONNECT_FAILURES: u32 = 3;
/// Per-room history scanned at each cycle start to catch messages that arrived
/// while no handler was registered (the catch-up syncs, restarts, outages).
const BACKFILL_LIMIT: u32 = 30;
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on one send attempt (the connector's own retries included).
const SEND_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);

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
        {
            let mut st = shared.status.lock().unwrap();
            st.connected = true;
            st.did = Some(agent.did().to_string());
            st.user_id = Some(agent.user_id().to_string());
            st.device_id = agent.device_id();
            st.last_error = None;
        }

        if let Err(e) = agent.sync_once_nowait().await {
            tracing::warn!("pre-join sync failed: {e:#}");
        }
        join_allowed_invites(&agent, &shared).await;
        if let Err(e) = agent.sync_once_nowait().await {
            tracing::warn!("settle sync failed: {e:#}");
        }
        if first_cycle {
            match agent.set_display_name(&display_name).await {
                Ok(()) => tracing::info!("display name set to {display_name:?}"),
                Err(e) => tracing::warn!("set display name failed: {e:#}"),
            }
            tracing::info!(did = %agent.did(), mxid = %agent.user_id(), "identity");
            first_cycle = false;
        }

        let exit = run_cycle(&agent, &shared, &mut rx, &mut carry, &shutdown).await;
        shared.status.lock().unwrap().connected = false;
        if exit == "shutdown" {
            // Fail anything still queued so callers are not left hanging.
            if let Some(c) = carry.take() {
                let _ = c.reply.send(Err("bridge shutting down; not sent".into()));
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
) -> &'static str {
    let own = agent.user_id().to_string();
    let mut handles: Vec<EventHandlerHandle> = Vec::new();
    handles.push(register_message_handler(agent, shared.clone(), own.clone()));
    handles.push(register_invite_handler(agent, shared.clone(), own.clone()));

    backfill(agent, shared, &own).await;

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
            if let Some(retry) = execute(agent, cmd).await {
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
                    if let Some(retry) = execute(agent, cmd).await {
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
async fn execute(agent: &AgentClient, cmd: SendCmd) -> Option<SendCmd> {
    if Instant::now() > cmd.deadline {
        let _ = cmd.reply.send(Err(
            "the bridge could not reach Matrix before the send deadline (outage?); not sent".into(),
        ));
        return None;
    }
    let fut = async {
        match &cmd.kind {
            SendKind::Text(md) => agent.send_dm_chunked(&cmd.to_mxid, md).await,
            SendKind::File { path, caption } => {
                agent.send_file(&cmd.to_mxid, path, Some(caption)).await
            }
        }
    };
    let res = match tokio::time::timeout(SEND_ATTEMPT_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "send timed out after {}s (outcome unknown)",
            SEND_ATTEMPT_TIMEOUT.as_secs()
        )),
    };
    match res {
        Ok(id) => {
            let _ = cmd.reply.send(Ok(id));
            None
        }
        Err(e) if is_unknown_token(&e) => {
            tracing::warn!("send rejected with M_UNKNOWN_TOKEN; retrying on a fresh client");
            Some(cmd)
        }
        Err(e) => {
            let _ = cmd.reply.send(Err(format!("{e:#}")));
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
        handle_invite(agent, shared, room, inviter).await;
    }
}

async fn handle_invite(agent: &AgentClient, shared: &Arc<Shared>, room: Room, inviter: String) {
    let room_id = room.room_id().to_string();
    let allowed = shared.allow.lock().unwrap().by_mxid(&inviter).is_some();
    if !allowed {
        tracing::info!(%room_id, %inviter, "declining invite from a non-allow-listed user");
        shared.audit(json!({"event": "invite_declined", "from": inviter, "room": room_id}));
        if let Err(e) = room.leave().await {
            tracing::warn!(%room_id, "decline failed: {e:#}");
        }
        return;
    }
    match room.join().await {
        Ok(()) => {
            tracing::info!(%room_id, %inviter, "joined invite from allow-listed user");
            if let Err(e) = agent.mark_dm(&room_id, &inviter).await {
                tracing::warn!(%room_id, "mark_dm failed: {e:#}");
            }
        }
        Err(e) => tracing::warn!(%room_id, "join failed: {e:#}"),
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
                handle_invite(&agent, &shared, room, ev.sender.to_string()).await;
            }
        })
}

fn register_message_handler(
    agent: &AgentClient,
    shared: Arc<Shared>,
    own: String,
) -> EventHandlerHandle {
    agent
        .client()
        .add_event_handler(move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let shared = shared.clone();
            let own = own.clone();
            async move {
                if ingest(&shared, &own, &ev, room.room_id().as_str()) {
                    let event_id = ev.event_id.clone();
                    tokio::spawn(async move {
                        let _ = room
                            .send_single_receipt(
                                ReceiptType::Read,
                                ReceiptThread::Unthreaded,
                                event_id,
                            )
                            .await;
                    });
                }
            }
        })
}

/// Record one inbound message if it comes from an allow-listed sender.
/// Returns true when it was newly added to the inbox.
fn ingest(shared: &Shared, own: &str, ev: &OriginalSyncRoomMessageEvent, room_id: &str) -> bool {
    let sender = ev.sender.as_str();
    if sender.eq_ignore_ascii_case(own) {
        return false;
    }
    let name = shared
        .allow
        .lock()
        .unwrap()
        .by_mxid(sender)
        .map(|r| r.name.clone());
    let Some(name) = name else {
        let event_id = ev.event_id.to_string();
        if shared.dropped.lock().unwrap().insert(event_id.clone()) {
            tracing::info!(%sender, room = %room_id, "dropped message from non-allow-listed sender");
            shared.audit(json!({"event": "inbound_dropped", "from": sender, "room": room_id, "event_id": event_id}));
        }
        return false;
    };
    let (kind, body, filename) = match &ev.content.msgtype {
        MessageType::Text(t) => ("text", t.body.clone(), None),
        MessageType::Notice(n) => ("notice", n.body.clone(), None),
        MessageType::Emote(e) => ("emote", e.body.clone(), None),
        MessageType::File(f) => (
            "file",
            f.body.clone(),
            Some(f.filename.clone().unwrap_or_else(|| f.body.clone())),
        ),
        MessageType::Image(i) => (
            "image",
            i.body.clone(),
            Some(i.filename.clone().unwrap_or_else(|| i.body.clone())),
        ),
        MessageType::Audio(a) => (
            "audio",
            a.body.clone(),
            Some(a.filename.clone().unwrap_or_else(|| a.body.clone())),
        ),
        MessageType::Video(v) => (
            "video",
            v.body.clone(),
            Some(v.filename.clone().unwrap_or_else(|| v.body.clone())),
        ),
        other => ("other", other.body().to_string(), None),
    };
    let entry = NewEntry {
        event_id: ev.event_id.to_string(),
        room_id: room_id.to_string(),
        sender: sender.to_string(),
        sender_name: Some(name.clone()),
        ts_ms: u64::from(ev.origin_server_ts.0),
        kind: kind.to_string(),
        body,
        filename,
    };
    let seq = shared.inbox.lock().unwrap().ingest(entry);
    match seq {
        Some(seq) => {
            tracing::info!(from = %name, seq, kind, "inbox: new message");
            shared.inbox_changed.notify_waiters();
            true
        }
        None => false,
    }
}

/// Scan recent history of every joined room and ingest what the handler-less
/// catch-up syncs (or downtime) swallowed. Dedupe is by event id, so
/// re-offering an already-ingested message is a no-op.
async fn backfill(agent: &AgentClient, shared: &Arc<Shared>, own: &str) {
    let scan = async {
        let mut utd = 0usize;
        let mut found: Vec<(u64, String, OriginalSyncRoomMessageEvent)> = Vec::new();
        for room in agent.client().joined_rooms() {
            let mut opts = MessagesOptions::backward();
            opts.limit = UInt::from(BACKFILL_LIMIT);
            let resp = match room.messages(opts).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(room = %room.room_id(), "backfill failed: {e:#}");
                    continue;
                }
            };
            for event in resp.chunk {
                if event.kind.is_utd() {
                    utd += 1;
                    continue;
                }
                let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                    msg,
                ))) = event.raw().deserialize()
                else {
                    continue;
                };
                if let Some(orig) = msg.as_original() {
                    found.push((
                        u64::from(orig.origin_server_ts.0),
                        room.room_id().to_string(),
                        orig.clone(),
                    ));
                }
            }
        }
        (found, utd)
    };
    let (mut found, utd) = match tokio::time::timeout(BACKFILL_TIMEOUT, scan).await {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!("backfill exceeded {}s; skipped", BACKFILL_TIMEOUT.as_secs());
            return;
        }
    };
    if utd > 0 {
        tracing::debug!("backfill saw {utd} undecryptable event(s)");
    }
    found.sort_by_key(|(ts, _, _)| *ts);
    let mut added = 0;
    for (_, room_id, ev) in &found {
        if ingest(shared, own, ev, room_id) {
            added += 1;
        }
    }
    if added > 0 {
        tracing::info!("backfill added {added} message(s) to the inbox");
    }
}
