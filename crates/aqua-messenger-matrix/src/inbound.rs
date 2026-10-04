//! Inbound Matrix messages into the messenger inbox: a live event handler and
//! a history backfill, both deduped by event id and filtered by the engine
//! (a DM from an allow-listed person, or any member's message in a listed
//! `[[rooms]]` room; everything else is dropped and logged once). Reply and
//! thread metadata (`in_reply_to`, `thread_root`) is recorded with each
//! entry. Also: the joined-room survey the engine uses for `list_recipients`
//! and the group-room check.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use aqua_matrix_agent::AgentClient;
use aqua_messenger::inbox::NewEntry;
use aqua_messenger::{Engine, SeenRoom, Transport};
use matrix_sdk::{
    event_handler::EventHandlerHandle,
    room::{MessagesOptions, Room},
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::{
            receipt::ReceiptThread,
            room::message::{MessageType, OriginalSyncRoomMessageEvent},
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        },
        UInt,
    },
    RoomMemberships,
};

/// Per-room history scanned by [`backfill`].
pub const BACKFILL_LIMIT: u32 = 30;
pub const BACKFILL_TIMEOUT: Duration = Duration::from_secs(30);

/// `(kind, body, filename)` of a message.
pub fn describe_msgtype(msgtype: &MessageType) -> (&'static str, String, Option<String>) {
    let file_name = |f: &Option<String>, b: &str| Some(f.clone().unwrap_or_else(|| b.to_string()));
    match msgtype {
        MessageType::Text(t) => ("text", t.body.clone(), None),
        MessageType::Notice(n) => ("notice", n.body.clone(), None),
        MessageType::Emote(e) => ("emote", e.body.clone(), None),
        MessageType::File(f) => ("file", f.body.clone(), file_name(&f.filename, &f.body)),
        MessageType::Image(i) => ("image", i.body.clone(), file_name(&i.filename, &i.body)),
        MessageType::Audio(a) => ("audio", a.body.clone(), file_name(&a.filename, &a.body)),
        MessageType::Video(v) => ("video", v.body.clone(), file_name(&v.filename, &v.body)),
        other => ("other", other.body().to_string(), None),
    }
}

/// Record one inbound message if the engine accepts it (see module docs).
/// `group`: the room has more than two joined members. Returns true when it
/// was newly added to the inbox.
pub fn ingest_event<T: Transport>(
    engine: &Engine<T>,
    own: &str,
    ev: &OriginalSyncRoomMessageEvent,
    room_id: &str,
    group: bool,
) -> bool {
    let Some(acc) = engine.classify_inbound(
        own,
        ev.sender.as_str(),
        room_id,
        ev.event_id.as_str(),
        group,
    ) else {
        return false;
    };
    let (kind, body, filename) = describe_msgtype(&ev.content.msgtype);
    let (in_reply_to, thread_root) =
        aqua_matrix_agent::reply::reply_fields(ev.content.relates_to.as_ref());
    let entry = NewEntry {
        event_id: ev.event_id.to_string(),
        room_id: room_id.to_string(),
        sender: ev.sender.to_string(),
        sender_name: acc.sender_name,
        room: acc.room_name,
        ts_ms: u64::from(ev.origin_server_ts.0),
        kind: kind.to_string(),
        body,
        filename,
        media: crate::media::media_ref(&ev.content.msgtype),
        in_reply_to,
        thread_root,
    };
    engine.ingest(entry).is_some()
}

/// Joined members of `room`, from the full member list (fetched from the
/// server when lazy-loaded). The sync summary's `joined_members_count()` is 0
/// on our homeserver unless the filter lazy-loads members, so it cannot tell a
/// group from a DM; it is only the fallback when the member fetch fails.
pub async fn joined_member_count(room: &Room) -> u64 {
    match room.members(RoomMemberships::JOIN).await {
        Ok(m) => m.len() as u64,
        Err(e) => {
            tracing::debug!(room = %room.room_id(), "member list unavailable ({e:#}); using the sync summary count");
            room.joined_members_count()
        }
    }
}

/// Whether a message's room is a group: the member count from the last
/// connect's snapshot (or the room's summary for a room joined since).
pub fn is_group<T: Transport>(engine: &Engine<T>, room: &Room) -> bool {
    let snap = engine.snapshot_members(room.room_id().as_str());
    snap.unwrap_or(0).max(room.joined_members_count()) > 2
}

/// Register the live inbound handler on `agent`'s Client. The caller MUST
/// remove the returned handle before the Client is dropped/rotated
/// (`client.remove_event_handler`), or the handler keeps the Client alive.
pub fn register_message_handler<T: Transport>(
    agent: &AgentClient,
    engine: Arc<Engine<T>>,
) -> EventHandlerHandle {
    let own = agent.user_id().to_string();
    agent
        .client()
        .add_event_handler(move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let engine = engine.clone();
            let own = own.clone();
            async move {
                let group = is_group(&engine, &room);
                if ingest_event(&engine, &own, &ev, room.room_id().as_str(), group) {
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

/// Scan recent history of every joined room and ingest what the handler-less
/// catch-up syncs (or downtime) swallowed. Re-offering an ingested message is
/// a no-op. Returns how many were added.
pub async fn backfill<T: Transport>(agent: &AgentClient, engine: &Engine<T>) -> usize {
    let own = agent.user_id().to_string();
    let scan = async {
        let mut utd = 0usize;
        let mut found: Vec<(u64, String, bool, OriginalSyncRoomMessageEvent)> = Vec::new();
        for room in agent.client().joined_rooms() {
            let group = is_group(engine, &room);
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
                        group,
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
            return 0;
        }
    };
    if utd > 0 {
        tracing::debug!("backfill saw {utd} undecryptable event(s)");
    }
    found.sort_by_key(|(ts, _, _, _)| *ts);
    let added = found
        .iter()
        .filter(|(_, room_id, group, ev)| ingest_event(engine, &own, ev, room_id, *group))
        .count();
    if added > 0 {
        tracing::info!("backfill added {added} message(s) to the inbox");
    }
    added
}

/// Snapshot every joined room (display name, member count, DM flag) into the
/// engine, and log it on the first connect and whenever a room is new, so an
/// operator can learn which room id is which.
pub async fn survey_rooms<T: Transport>(agent: &AgentClient, engine: &Engine<T>, log_all: bool) {
    let mut map = BTreeMap::new();
    for room in agent.client().joined_rooms() {
        let id = room.room_id().to_string();
        let seen = SeenRoom {
            display_name: room.display_name().await.ok().map(|n| n.to_string()),
            joined_members: joined_member_count(&room).await,
            is_direct: room.is_direct().await.unwrap_or(false),
        };
        map.insert(id, seen);
    }
    let previous = engine.set_joined_rooms(map.clone());
    engine.with_allow(|allow| {
        for (id, r) in &map {
            if !log_all && previous.contains_key(id) {
                continue;
            }
            tracing::info!(
                room_id = %id,
                display_name = r.display_name.as_deref().unwrap_or("?"),
                joined_members = r.joined_members,
                is_direct = r.is_direct,
                listed = allow.room_by_id(id).map(|x| x.name.as_str()).unwrap_or("-"),
                "joined room"
            );
        }
    });
}
