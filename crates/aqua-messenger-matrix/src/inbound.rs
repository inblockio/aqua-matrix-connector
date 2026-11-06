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

use aqua_matrix_agent::{AgentClient, DmObserver};
use aqua_messenger::allowlist::{count_joined, Observer};
use aqua_messenger::inbox::NewEntry;
use aqua_messenger::{Engine, SeenRoom, Transport};
use matrix_sdk::ruma::events::room::member::MembershipState;
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

/// Members of `room` that count toward "is this a DM": the joined members
/// minus the `observers` bound to this room (read-only members such as the
/// inbox reader; see `[[observers]]` in the allow-list). Every site that
/// decides DM versus group counts through here. A room with no observer
/// bound to it takes exactly the path of [`joined_member_count`].
///
/// A room an observer is bound to is counted from the member list with
/// invited members included, as the connector's own 1:1 check does (see
/// [`observed_count`]); if no member list can be read the room counts as
/// [`GROUP_IF_UNKNOWN`].
pub async fn counted_members(room: &Room, observers: &[Observer]) -> u64 {
    counted_members_opt(room, observers)
        .await
        .unwrap_or(GROUP_IF_UNKNOWN)
}

/// [`counted_members`], `None` for an observed room whose members could not be
/// read at all. For a caller that must not act on a guess (the `m.direct`
/// repair removes entries).
pub async fn counted_members_opt(room: &Room, observers: &[Observer]) -> Option<u64> {
    let room_id = room.room_id().as_str();
    // never discount this client itself, whatever a row says
    let observers = usable_observers(observers, room.own_user_id().as_str());
    if !observers.iter().any(|o| o.room_id == room_id) {
        return Some(joined_member_count(room).await);
    }
    match observed_count(room, &observers).await {
        LiveCount::Live(n) | LiveCount::Cached(n) => Some(n),
        LiveCount::Unknown => None,
    }
}

/// How many of a room's members count: the JOINED and INVITED ones, minus
/// the observers bound to it (the connector's 1:1 check counts invited members
/// too, so a pending invitee knocks a room out of the DM here as well).
pub fn count_active_members<'a>(
    room_id: &str,
    members: impl IntoIterator<Item = (&'a str, &'a MembershipState)>,
    observers: &[Observer],
) -> u64 {
    count_joined(
        room_id,
        members
            .into_iter()
            .filter(|(_, m)| matches!(**m, MembershipState::Join | MembershipState::Invite))
            .map(|(u, _)| u),
        observers,
    )
}

/// What the live count of a room an observer row has named came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveCount {
    /// From the server's member list.
    Live(u64),
    /// The server fetch failed; from the local store.
    Cached(u64),
    /// Neither source answered.
    Unknown,
}

/// The group verdict for a tracked room from its live count. The server's
/// answer decides alone (a snapshot can be stale in both directions). When
/// only the local store answered, the snapshot still counts and the larger of
/// the two wins; when nobody answered the room is treated as a group, never as
/// a DM: a dropped message comes back with the next backfill, a message filed
/// into the wrong room does not.
pub fn tracked_group(snapshot: Option<u64>, count: LiveCount) -> bool {
    match count {
        LiveCount::Live(n) => n > 2,
        LiveCount::Cached(n) => snapshot.unwrap_or(0).max(n) > 2,
        LiveCount::Unknown => true,
    }
}

/// Whether a room is counted from the member list with invited members
/// included: a row names it now (`bound`), or one did earlier in this process
/// (`tracked`). STUB (red commit): only a current row.
pub fn counts_members_live(bound: bool, _tracked: bool) -> bool {
    bound
}

/// The snapshot value for a measured count. A count that failed is stored as
/// 0, which every decision reads as "no snapshot" (they take `unwrap_or(0)`);
/// it is never stored as [`GROUP_IF_UNKNOWN`], which would beat a later
/// measured 2. STUB (red commit): stores the group sentinel.
pub fn snapshot_count(count: Option<u64>) -> u64 {
    count.unwrap_or(GROUP_IF_UNKNOWN)
}

/// What [`counted_members`] reports for an observed room whose members could
/// not be read at all: group-sized, so nothing is filed or sent as a DM.
pub const GROUP_IF_UNKNOWN: u64 = 3;

/// Count the members of a room an observer row names (now or earlier): joined
/// and invited, observers left out, from the server's member list, else the
/// local store, else unknown.
pub async fn observed_count(room: &Room, observers: &[Observer]) -> LiveCount {
    let room_id = room.room_id().as_str();
    let observers = usable_observers(observers, room.own_user_id().as_str());
    let count = |ms: &[matrix_sdk::room::RoomMember]| {
        count_active_members(
            room_id,
            ms.iter().map(|m| (m.user_id().as_str(), m.membership())),
            &observers,
        )
    };
    match room.members(RoomMemberships::ACTIVE).await {
        Ok(ms) => LiveCount::Live(count(&ms)),
        Err(e) => {
            tracing::debug!(room = %room_id, "member list unavailable ({e:#}); trying the cached list");
            match room.members_no_sync(RoomMemberships::ACTIVE).await {
                Ok(ms) => LiveCount::Cached(count(&ms)),
                Err(_) => LiveCount::Unknown,
            }
        }
    }
}

/// The group verdict for a room an observer row has named (`tracked`: now or
/// earlier in this process) or not. The connect-time snapshot of a tracked
/// room can be stale in both directions (a row added or removed while the
/// observer is joined), so only the live count decides there. Any other room
/// keeps the old rule.
pub fn group_verdict(tracked: bool, snapshot: Option<u64>, current: u64) -> bool {
    if tracked {
        current > 2
    } else {
        group_from_counts(snapshot, current)
    }
}

/// The observers that may be applied: a row naming `own` (this client's own
/// account) is dropped, a client is never its own observer. Without this a
/// wrong row would make a three-member group look like a DM.
pub fn usable_observers(observers: &[Observer], own: &str) -> Vec<Observer> {
    observers
        .iter()
        .filter(|o| !o.mxid.eq_ignore_ascii_case(own))
        .cloned()
        .collect()
}

/// Read the observers in force from the engine's allow-list, drop a row that
/// names this client itself (logged and audited once), and hand the rest to
/// the connector so its own 1:1 checks (`mark_dm`, `dm_room_id`,
/// `is_one_to_one_dm`) leave them out too. Call before any DM decision.
pub fn sync_dm_observers<T: Transport>(agent: &AgentClient, engine: &Engine<T>) -> Vec<Observer> {
    let all = engine.with_allow(|a| a.observers().to_vec());
    let own = agent.user_id().to_string();
    let usable = usable_observers(&all, &own);
    if usable.len() != all.len() {
        for o in all.iter().filter(|o| o.mxid.eq_ignore_ascii_case(&own)) {
            if engine.note_once(&format!("own-observer:{}:{}", o.name, o.room_id)) {
                tracing::error!(name = %o.name, room = %o.room_id, "observer row names this bridge's own account; ignored");
                engine.audit(serde_json::json!({"event": "observer_ignored", "name": o.name, "room": o.room_id, "reason": "own_account"}));
            }
        }
    }
    agent.set_dm_observers(
        usable
            .iter()
            .map(|o| DmObserver {
                room_id: o.room_id.clone(),
                user_id: o.mxid.clone(),
            })
            .collect(),
    );
    usable
}

/// The group verdict from the last connect's snapshot and a current count.
pub fn group_from_counts(snapshot: Option<u64>, current: u64) -> bool {
    snapshot.unwrap_or(0).max(current) > 2
}

/// Whether a message's room is a group: the member count from the last
/// connect's snapshot (or the room's summary for a room joined since). A room
/// an observer row names (now, or earlier in this process) is counted live
/// from its member list without the observer: the sync summary cannot tell
/// the observer apart and the snapshot can be stale (see [`tracked_group`]).
pub async fn is_group<T: Transport>(engine: &Engine<T>, room: &Room) -> bool {
    let room_id = room.room_id().as_str();
    let (bound, tracked) =
        engine.with_allow(|a| (a.observers_in_room(room_id), a.was_observed(room_id)));
    let snapshot = engine.snapshot_members(room_id);
    if tracked {
        tracked_group(snapshot, observed_count(room, &bound).await)
    } else {
        group_verdict(false, snapshot, room.joined_members_count())
    }
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
                let group = is_group(&engine, &room).await;
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
            let group = is_group(engine, &room).await;
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
    let observers = sync_dm_observers(agent, engine);
    let mut map = BTreeMap::new();
    for room in agent.client().joined_rooms() {
        let id = room.room_id().to_string();
        let seen = SeenRoom {
            display_name: room.display_name().await.ok().map(|n| n.to_string()),
            joined_members: counted_members(&room, &observers).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_invitee_knocks_an_observed_room_out_of_the_dm() {
        use MembershipState::*;
        let observers = [obs("@reader:x", "!dm:x")];
        let dm = [
            ("@bridge:x", &Join),
            ("@tim:x", &Join),
            ("@reader:x", &Join),
        ];
        assert_eq!(count_active_members("!dm:x", dm, &observers), 2);
        // an invited stranger counts, like the connector's own 1:1 check
        let invited = [
            ("@bridge:x", &Join),
            ("@tim:x", &Join),
            ("@reader:x", &Join),
            ("@mallory:x", &Invite),
        ];
        assert_eq!(count_active_members("!dm:x", invited, &observers), 3);
        // an observer that is only invited does not count either
        let observer_invited = [
            ("@bridge:x", &Join),
            ("@tim:x", &Join),
            ("@reader:x", &Invite),
        ];
        assert_eq!(
            count_active_members("!dm:x", observer_invited, &observers),
            2
        );
        // people who left, were banned or knocked are not members
        let gone = [
            ("@bridge:x", &Join),
            ("@tim:x", &Join),
            ("@old:x", &Leave),
            ("@bad:x", &Ban),
        ];
        assert_eq!(count_active_members("!dm:x", gone, &observers), 2);
        // an observer bound to another room is an ordinary member here
        let elsewhere = [
            ("@bridge:x", &Join),
            ("@tim:x", &Join),
            ("@reader:x", &Join),
        ];
        assert_eq!(count_active_members("!other:x", elsewhere, &observers), 3);
    }

    #[test]
    fn every_tracked_room_counts_invited_members() {
        // a room a row named earlier keeps the invite-aware count after the row is gone
        assert!(counts_members_live(false, true));
        assert!(counts_members_live(true, true));
        assert!(counts_members_live(true, false));
        // a room no row ever named takes the old joined-only count
        assert!(!counts_members_live(false, false));
    }

    #[test]
    fn a_failed_count_is_never_stored_as_a_snapshot() {
        assert_eq!(snapshot_count(Some(2)), 2);
        assert_eq!(snapshot_count(Some(0)), 0);
        // unknown stays "no snapshot": a later measured 2 decides, on every path
        assert_eq!(snapshot_count(None), 0);
        let snap = Some(snapshot_count(None));
        assert!(!tracked_group(snap, LiveCount::Cached(2)));
        assert!(!group_verdict(false, snap, 2));
        // a measured group count still sticks
        assert!(tracked_group(Some(snapshot_count(Some(3))), LiveCount::Cached(2)));
    }

    #[test]
    fn a_failed_live_count_errs_toward_group() {
        // the store answered: the snapshot still counts
        assert!(tracked_group(Some(3), LiveCount::Cached(2)));
        assert!(tracked_group(Some(2), LiveCount::Cached(3)));
        assert!(!tracked_group(Some(2), LiveCount::Cached(2)));
        assert!(!tracked_group(None, LiveCount::Cached(2)));
        // nobody answered: never 0, never a DM
        assert!(tracked_group(None, LiveCount::Unknown));
        assert!(tracked_group(Some(2), LiveCount::Unknown));
        // the server answered: a stale snapshot is ignored, as before
        assert!(!tracked_group(Some(3), LiveCount::Live(2)));
        assert!(tracked_group(Some(2), LiveCount::Live(3)));
    }

    fn obs(mxid: &str, room: &str) -> Observer {
        Observer {
            name: "o".into(),
            mxid: mxid.into(),
            room_id: room.into(),
            note: None,
        }
    }

    #[test]
    fn a_tracked_room_ignores_its_stale_snapshot() {
        // row added after a snapshot of 3: the live count (2) decides
        assert!(!group_verdict(true, Some(3), 2));
        // row removed while the reader is joined: snapshot 2 is stale, live 3 decides
        assert!(group_verdict(true, Some(2), 3));
        assert!(!group_verdict(true, Some(9), 2));
        assert!(!group_verdict(true, None, 2));
    }

    #[test]
    fn an_untracked_room_keeps_the_old_rule() {
        assert!(group_verdict(false, Some(3), 0));
        assert!(group_verdict(false, None, 3));
        assert!(group_verdict(false, Some(2), 3));
        assert!(!group_verdict(false, Some(2), 0));
        assert!(!group_verdict(false, None, 2));
    }

    #[test]
    fn the_clients_own_account_is_never_an_observer() {
        let rows = [
            obs("@bridge:x", "!dm:x"),
            obs("@Reader:x", "!dm:x"),
            obs("@BRIDGE:x", "!group:x"),
        ];
        let kept = usable_observers(&rows, "@bridge:x");
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].mxid, "@Reader:x");
        // no row names the client: nothing is dropped
        assert_eq!(usable_observers(&rows, "@someone:x").len(), 3);
    }

    #[test]
    fn group_verdict_is_the_larger_of_snapshot_and_current_count() {
        // today's rule: more than two joined members is a group
        assert!(!group_from_counts(None, 0));
        assert!(!group_from_counts(Some(2), 0));
        assert!(!group_from_counts(None, 2));
        assert!(group_from_counts(Some(3), 0));
        assert!(group_from_counts(None, 3));
        assert!(group_from_counts(Some(2), 3));
        // a DM with one observer counts 2 on both sides: not a group
        assert!(!group_from_counts(Some(2), 2));
    }
}
