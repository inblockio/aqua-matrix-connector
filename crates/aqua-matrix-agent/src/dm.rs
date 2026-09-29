//! 1:1 DM resolution with an authoritative member list.
//!
//! **2026-09-29 bug D1.** `find_dm_room` used to skip group rooms with
//! `room.joined_members_count() > 2`. matrix-sdk-base 0.17 fills that count only
//! from the sync `summary` (`room_info.rs` `update_from_ruma_summary`), and
//! Synapse sends `summary` only on a `lazy_load_members` sync. The connector
//! syncs without that filter, so the count was 0 for EVERY room, the guard never
//! fired, and any room the agent shared with the target (a call room, a group)
//! was a DM candidate, newest wins. Observed in the local Scribe e2e: the
//! owner's "DM" resolved to the group call room.
//!
//! The fix never consults the summary counters. A room is the DM with `target`
//! only if its ACTIVE (joined or invited) member list, fetched from the server
//! when the store has not synced it yet, is a subset of `{self, target}` and
//! `target` is itself joined or invited. Among several true 1:1 rooms the choice
//! is deterministic: target joined over invited, then a room `m.direct` records
//! for `target`, then the most recent activity, then the room id. When no true
//! 1:1 exists, [`AgentClient::ensure_dm_room`] creates one; nothing ever falls
//! back to "any shared room".

use crate::AgentClient;
use anyhow::{anyhow, Result};
use matrix_sdk::ruma::events::direct::DirectUserIdentifier;
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use matrix_sdk::RoomMemberships;

/// What one joined room is with respect to a DM with `target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DmVerdict {
    /// A true 1:1 and `target` has joined: a usable DM.
    Joined,
    /// A true 1:1 and `target` is invited (a DM we created that the peer has
    /// not accepted yet). Usable, ranked below [`DmVerdict::Joined`].
    Invited,
    /// Some active member other than self and `target`: a group room. Never a
    /// DM, whatever `m.direct` says.
    NotOneToOne,
    /// `target` is not active here (left, banned, knocked, never joined): the
    /// 2026-06-06 liveness rule, a room the peer left can never exchange
    /// Megolm keys again. Never selected.
    TargetAbsent,
}

impl DmVerdict {
    /// Selection rank, `None` for a room that must never be chosen.
    pub(crate) fn rank(self) -> Option<u8> {
        match self {
            DmVerdict::Joined => Some(2),
            DmVerdict::Invited => Some(1),
            DmVerdict::NotOneToOne | DmVerdict::TargetAbsent => None,
        }
    }
}

/// Pure classification of one room from its ACTIVE (joined + invited) members.
/// `active` must come from the member list, never from the sync summary
/// counters (which read 0 on this connector's non-lazy sync, bug D1).
pub(crate) fn classify_dm_room(
    own: &UserId,
    target: &UserId,
    active: &[(OwnedUserId, MembershipState)],
) -> DmVerdict {
    if active
        .iter()
        .any(|(u, _)| u.as_str() != own.as_str() && u.as_str() != target.as_str())
    {
        return DmVerdict::NotOneToOne;
    }
    match active
        .iter()
        .find(|(u, _)| u.as_str() == target.as_str())
        .map(|(_, m)| m)
    {
        Some(MembershipState::Join) => DmVerdict::Joined,
        Some(MembershipState::Invite) => DmVerdict::Invited,
        _ => DmVerdict::TargetAbsent,
    }
}

/// One true-1:1 candidate for [`pick_dm_candidate`].
#[derive(Debug, Clone)]
pub(crate) struct DmCandidate {
    pub room_id: OwnedRoomId,
    pub verdict: DmVerdict,
    /// `m.direct` records this room for `target`.
    pub marked_direct: bool,
    /// Newest event timestamp (ms), 0 when unknown or not fetched.
    pub last_activity_ms: u64,
}

/// Deterministic choice among candidates: rooms that are not a usable 1:1 are
/// dropped first (defence in depth, callers should not pass them), then target
/// joined over invited, `m.direct`-recorded over not, newest activity, and the
/// room id as the final stable tie-break. `None` means "no true DM exists":
/// the caller creates one, it never falls back to another shared room.
pub(crate) fn pick_dm_candidate(candidates: Vec<DmCandidate>) -> Option<OwnedRoomId> {
    let mut usable: Vec<(u8, DmCandidate)> = candidates
        .into_iter()
        .filter_map(|c| c.verdict.rank().map(|r| (r, c)))
        .collect();
    usable.sort_by(|(ra, a), (rb, b)| {
        rb.cmp(ra)
            .then(b.marked_direct.cmp(&a.marked_direct))
            .then(b.last_activity_ms.cmp(&a.last_activity_ms))
            .then(a.room_id.cmp(&b.room_id))
    });
    usable.into_iter().next().map(|(_, c)| c.room_id)
}

/// Does `m.direct` (as mirrored into the room info) record `room` for `target`?
pub(crate) fn is_marked_direct(room: &matrix_sdk::Room, target: &UserId) -> bool {
    room.direct_targets()
        .contains(<&DirectUserIdentifier>::from(target))
}

/// The room's ACTIVE members and whether that list is authoritative.
///
/// `Room::members` fetches `/members` from the server once when the store has
/// not synced the list (the same request the pre-fix `get_member` already made
/// per room), so the answer is authoritative. On a fetch error it falls back to
/// the store: this connector syncs WITHOUT lazy-loading, so a joined room's full
/// state arrives in the sync and the store list is complete in practice. The
/// flag lets destructive callers (m.direct rewrites) act only on authoritative
/// data. `None` when even the store read fails.
pub(crate) async fn active_members(
    room: &matrix_sdk::Room,
) -> Option<(Vec<(OwnedUserId, MembershipState)>, bool)> {
    let to_pairs = |ms: Vec<matrix_sdk::room::RoomMember>| {
        ms.into_iter()
            .map(|m| (m.user_id().to_owned(), m.membership().clone()))
            .collect::<Vec<_>>()
    };
    match room.members(RoomMemberships::ACTIVE).await {
        Ok(ms) => Some((to_pairs(ms), true)),
        Err(e) => {
            tracing::warn!(
                room_id = %room.room_id(),
                "dm: member fetch failed, classifying from the store: {e:#}"
            );
            match room.members_no_sync(RoomMemberships::ACTIVE).await {
                Ok(ms) => Some((to_pairs(ms), false)),
                Err(e) => {
                    tracing::warn!(room_id = %room.room_id(), "dm: store member read failed: {e:#}");
                    None
                }
            }
        }
    }
}

impl AgentClient {
    /// Classify `room` for a DM with `target` from its member list. The bool is
    /// whether the member list was authoritative (see [`active_members`]).
    pub(crate) async fn dm_verdict(
        &self,
        room: &matrix_sdk::Room,
        target: &UserId,
    ) -> Option<(DmVerdict, bool)> {
        let own = self.client.user_id()?.to_owned();
        let (active, authoritative) = active_members(room).await?;
        Some((classify_dm_room(&own, target, &active), authoritative))
    }

    /// Resolve THE 1:1 direct room for `target`: the single resolver behind
    /// `dm_room_id`, `ensure_dm_room` (so `send_dm*`, every media send and
    /// `ring_call`), `typing_guard` and `reply_stream`.
    ///
    /// Scans JOINED rooms only and keeps a room only when its active member
    /// list is `{self, target}` with `target` joined or invited (see the module
    /// doc for why the summary count cannot be used). A room the target left is
    /// never selected (2026-06-06 liveness rule), and neither is a group room,
    /// even when `m.direct` lists it. matrix-sdk's `get_dm_room` is not used: it
    /// trusts `m.direct` alone and returns the first match in undefined order.
    pub(crate) async fn find_dm_room(&self, target: &UserId) -> Option<matrix_sdk::Room> {
        let mut rooms: Vec<(matrix_sdk::Room, DmCandidate)> = Vec::new();
        for room in self.client.joined_rooms() {
            let Some((verdict, _)) = self.dm_verdict(&room, target).await else {
                continue;
            };
            if verdict.rank().is_none() {
                continue; // group room, or the target is gone
            }
            let candidate = DmCandidate {
                room_id: room.room_id().to_owned(),
                verdict,
                marked_direct: is_marked_direct(&room, target),
                last_activity_ms: 0,
            };
            rooms.push((room, candidate));
        }
        if rooms.len() > 1 {
            // Genuine multiple live 1:1 rooms (rare): read the newest activity
            // for the tie-break. Cheap (limit=1) and only on this path.
            for (room, c) in rooms.iter_mut() {
                c.last_activity_ms = self
                    .messages(room.room_id().as_str(), 1)
                    .await
                    .ok()
                    .and_then(|m| m.iter().map(|x| x.timestamp_ms).max())
                    .unwrap_or(0);
            }
        }
        let chosen = pick_dm_candidate(rooms.iter().map(|(_, c)| c.clone()).collect())?;
        rooms
            .into_iter()
            .find(|(r, _)| r.room_id() == chosen)
            .map(|(r, _)| r)
    }

    /// Is `room_id` a joined room whose active members are exactly this agent
    /// and `target` (target joined or invited)? The check a consumer must use
    /// before treating a room as "the DM with `target`", instead of
    /// `Room::joined_members_count`, which reads 0 on this connector's
    /// non-lazy sync (bug D1). `false` for unknown or non-joined rooms.
    pub async fn is_one_to_one_dm(&self, room_id: &str, target: &str) -> Result<bool> {
        let room_id: &RoomId = room_id
            .try_into()
            .map_err(|e| anyhow!("invalid room_id: {e}"))?;
        let target: &UserId = target
            .try_into()
            .map_err(|e| anyhow!("invalid target: {e}"))?;
        let Some(room) = self.client.get_room(room_id) else {
            return Ok(false);
        };
        if room.state() != matrix_sdk::RoomState::Joined {
            return Ok(false);
        }
        Ok(matches!(
            self.dm_verdict(&room, target).await,
            Some((v, _)) if v.rank().is_some()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }
    fn rid(s: &str) -> OwnedRoomId {
        RoomId::parse(s).unwrap()
    }
    const ME: &str = "@scribe:matrix.inblock.io";
    const OWNER: &str = "@owner:matrix.inblock.io";
    const OTHER: &str = "@gawain:matrix.inblock.io";

    fn cand(room: &str, verdict: DmVerdict, marked: bool, ts: u64) -> DmCandidate {
        DmCandidate {
            room_id: rid(room),
            verdict,
            marked_direct: marked,
            last_activity_ms: ts,
        }
    }

    #[test]
    fn group_room_with_three_joined_is_never_a_dm() {
        // The D1 call room: summary absent, so the SDK count read 0. The member
        // list says three joined, which makes it a group whatever the count says.
        let active = vec![
            (uid(ME), MembershipState::Join),
            (uid(OWNER), MembershipState::Join),
            (uid(OTHER), MembershipState::Join),
        ];
        let v = classify_dm_room(&uid(ME), &uid(OWNER), &active);
        assert_eq!(v, DmVerdict::NotOneToOne);
        assert_eq!(v.rank(), None);
    }

    #[test]
    fn group_room_with_a_pending_third_invite_is_not_a_dm() {
        let active = vec![
            (uid(ME), MembershipState::Join),
            (uid(OWNER), MembershipState::Join),
            (uid(OTHER), MembershipState::Invite),
        ];
        assert_eq!(
            classify_dm_room(&uid(ME), &uid(OWNER), &active),
            DmVerdict::NotOneToOne
        );
    }

    #[test]
    fn true_one_to_one_is_a_dm_joined_over_invited() {
        let joined = vec![
            (uid(ME), MembershipState::Join),
            (uid(OWNER), MembershipState::Join),
        ];
        assert_eq!(
            classify_dm_room(&uid(ME), &uid(OWNER), &joined),
            DmVerdict::Joined
        );
        let invited = vec![
            (uid(ME), MembershipState::Join),
            (uid(OWNER), MembershipState::Invite),
        ];
        assert_eq!(
            classify_dm_room(&uid(ME), &uid(OWNER), &invited),
            DmVerdict::Invited
        );
        assert!(DmVerdict::Joined.rank() > DmVerdict::Invited.rank());
    }

    #[test]
    fn room_the_target_left_is_not_chosen() {
        // The peer left: only the agent is still active (1 member).
        let active = vec![(uid(ME), MembershipState::Join)];
        let v = classify_dm_room(&uid(ME), &uid(OWNER), &active);
        assert_eq!(v, DmVerdict::TargetAbsent);
        assert_eq!(v.rank(), None);
        assert_eq!(
            pick_dm_candidate(vec![cand("!left:x", v, true, 99)]),
            None,
            "an m.direct-recorded room the peer left is never selected"
        );
    }

    #[test]
    fn one_to_one_with_someone_else_is_not_a_dm_with_target() {
        let active = vec![
            (uid(ME), MembershipState::Join),
            (uid(OTHER), MembershipState::Join),
        ];
        assert_eq!(
            classify_dm_room(&uid(ME), &uid(OWNER), &active),
            DmVerdict::NotOneToOne
        );
    }

    #[test]
    fn pick_prefers_the_true_dm_over_a_newer_marked_group_room() {
        // D1 shape: the group call room is newest and even polluted into
        // m.direct; the true 1:1 must still win.
        let picked = pick_dm_candidate(vec![
            cand("!call:x", DmVerdict::NotOneToOne, true, 2_000),
            cand("!dm:x", DmVerdict::Joined, false, 1_000),
        ]);
        assert_eq!(picked, Some(rid("!dm:x")));
    }

    #[test]
    fn no_true_dm_yields_none_so_the_caller_creates_one() {
        // Only group rooms and dead rooms are shared: no fallback to "any
        // shared room"; ensure_dm_room turns None into create_dm.
        assert_eq!(
            pick_dm_candidate(vec![
                cand("!call:x", DmVerdict::NotOneToOne, true, 3_000),
                cand("!dead:x", DmVerdict::TargetAbsent, true, 2_000),
            ]),
            None
        );
        assert_eq!(pick_dm_candidate(vec![]), None);
    }

    #[test]
    fn pick_order_is_joined_then_m_direct_then_newest_then_room_id() {
        // Joined beats a newer invited room.
        assert_eq!(
            pick_dm_candidate(vec![
                cand("!inv:x", DmVerdict::Invited, true, 9_000),
                cand("!join:x", DmVerdict::Joined, false, 1),
            ]),
            Some(rid("!join:x"))
        );
        // m.direct-recorded beats a newer unrecorded 1:1.
        assert_eq!(
            pick_dm_candidate(vec![
                cand("!new:x", DmVerdict::Joined, false, 9_000),
                cand("!marked:x", DmVerdict::Joined, true, 1),
            ]),
            Some(rid("!marked:x"))
        );
        // Then newest activity.
        assert_eq!(
            pick_dm_candidate(vec![
                cand("!old:x", DmVerdict::Joined, true, 1),
                cand("!new:x", DmVerdict::Joined, true, 2),
            ]),
            Some(rid("!new:x"))
        );
        // Then a stable room-id order, independent of input order.
        for order in [["!b:x", "!a:x"], ["!a:x", "!b:x"]] {
            assert_eq!(
                pick_dm_candidate(
                    order
                        .iter()
                        .map(|r| cand(r, DmVerdict::Joined, true, 5))
                        .collect()
                ),
                Some(rid("!a:x"))
            );
        }
    }
}
