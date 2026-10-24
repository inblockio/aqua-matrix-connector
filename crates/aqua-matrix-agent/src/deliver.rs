//! Obligation delivery: post an answer, a liveness note or a fallback for an
//! accepted peer message so that it is posted AT MOST ONCE and closed ON PROOF.
//!
//! Callable from the relay's `run_daemon` and from a poll-loop agent (Scribe):
//! both only need a [`WorkJournal`] on the client ([`AgentClient::attach_journal`]).
//!
//! The three layers against a duplicate, in the order they are consulted:
//!
//! 1. the journal records the event id of every post (`WorkJournal::posted`);
//!    a recorded post is never sent again;
//! 2. the Matrix transaction id is `txn_id(obligation event id, ordinal)`: a
//!    retry, a post-reauth pass and a post-restart repeat are the same event to
//!    the homeserver (this covers a lost send response, where layer 1 has
//!    nothing recorded);
//! 3. the own-event echo carries that transaction id in `unsigned`, which proves
//!    a post whose response was lost (`WorkJournal::observe_own_event`).
//!
//! Proof that closes an obligation: the own-event echo, OR the send response
//! confirmed by fetching the event from the room.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{EventId, OwnedTransactionId, RoomId, UserId};
use matrix_sdk::Room;

use crate::obligation::{txn_id, PostKind, SweepAction};
use crate::{retry_finalize, split_for_matrix, AgentClient, WorkJournal};

/// Where a post goes.
#[derive(Clone, Copy, Debug)]
pub enum Dest<'a> {
    /// The DM with this user (resolved, or created, like `send_dm`).
    Dm(&'a str),
    /// A room by id (the room the peer message arrived in).
    Room(&'a str),
}

impl AgentClient {
    /// Attach only the journal (a poll loop has no relay recycle signal).
    pub fn attach_journal(&mut self, journal: Arc<WorkJournal>) {
        self.journal = Some(journal);
    }

    /// The attached journal, if any.
    pub fn journal(&self) -> Option<&Arc<WorkJournal>> {
        self.journal.as_ref()
    }

    async fn dest_room(&self, dest: Dest<'_>) -> Result<Room> {
        match dest {
            Dest::Dm(target) => {
                let target: &UserId = target
                    .try_into()
                    .map_err(|e| anyhow!("invalid target: {e}"))?;
                self.ensure_dm_room(target).await
            }
            Dest::Room(room_id) => {
                let id: &RoomId = room_id
                    .try_into()
                    .map_err(|e| anyhow!("invalid room_id: {e}"))?;
                self.client
                    .get_room(id)
                    .ok_or_else(|| anyhow!("room {id} not found (agent not joined?)"))
            }
        }
    }

    /// Does the room hold the event `posted_id`? (A fetch: it posts nothing.)
    async fn event_exists(room: &Room, posted_id: &str) -> bool {
        let Ok(id) = <&EventId>::try_from(posted_id) else {
            return false;
        };
        room.event(id, None).await.is_ok()
    }

    /// Post `text` for the obligation `event_id` (`kind` picks answer, liveness
    /// or fallback), idempotently, and close the obligation once every chunk is
    /// proven. Safe to call again after any failure, restart or token rotation:
    /// already-posted chunks are skipped, the rest reuse their transaction ids.
    /// Returns the last chunk's event id. Without a journal it still sends with
    /// the deterministic ids (no bookkeeping).
    pub async fn deliver_obligation(
        &self,
        event_id: &str,
        dest: Dest<'_>,
        kind: PostKind,
        text: &str,
    ) -> Result<String> {
        let room = self.dest_room(dest).await?;
        let chunks = split_for_matrix(text, crate::STREAM_ROLLOVER_BYTES);
        if let Some(j) = &self.journal {
            j.declare_chunks(event_id, kind, chunks.len() as u32);
        }
        let mut last = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let ordinal = kind.base() + i as u32;
            // Layer 1: the journal's own record.
            if let Some(done) = self.journal.as_ref().and_then(|j| j.posted(event_id, ordinal)) {
                last = done.clone();
                if !self
                    .journal
                    .as_ref()
                    .is_some_and(|j| j.is_proven(event_id, ordinal))
                    && Self::event_exists(&room, &done).await
                {
                    if let Some(j) = &self.journal {
                        j.prove_post(event_id, ordinal);
                    }
                }
                continue;
            }
            // Layer 2: the same transaction id on every attempt.
            let posted = retry_finalize("deliver", txn_id(event_id, ordinal), |txn| {
                let room = room.clone();
                let content = RoomMessageEventContent::text_markdown(chunk);
                async move {
                    room.send(content)
                        .with_transaction_id(OwnedTransactionId::from(txn))
                        .await
                        .context("failed to send obligation post")
                        .map(|r| r.response.event_id.to_string())
                }
            })
            .await?;
            let proven = Self::event_exists(&room, &posted).await;
            if let Some(j) = &self.journal {
                j.record_post(event_id, ordinal, &posted, proven);
            }
            last = posted;
        }
        Ok(last)
    }

    /// Feed the agent's own events from its sync timeline here: an own event
    /// proves the post it is (by event id, or by the `unsigned.transaction_id`
    /// that proves a post whose send response was lost). Returns the
    /// obligations that closed.
    pub fn observe_own_event(&self, posted_id: &str, txn_id: Option<&str>) -> Vec<String> {
        self.journal
            .as_ref()
            .map(|j| j.observe_own_event(posted_id, txn_id))
            .unwrap_or_default()
    }

    /// Re-verify every unproven post by fetching its event. Returns how many
    /// obligations were left open (still unproven).
    pub async fn verify_open_posts(&self, room_of: impl Fn(&str) -> Option<String>) -> usize {
        let Some(j) = &self.journal else { return 0 };
        let mut still = 0;
        for (event_id, ordinal, posted_id) in j.unproven_posts() {
            let proven = match room_of(&event_id)
                .and_then(|r| <&RoomId>::try_from(r.as_str()).ok().and_then(|id| self.client.get_room(id)))
            {
                Some(room) => Self::event_exists(&room, &posted_id).await,
                None => false,
            };
            if proven {
                j.prove_post(&event_id, ordinal);
            } else {
                still += 1;
            }
        }
        still
    }

    /// Execute the sweep's actions that need no turn: liveness, fallback (and
    /// the operator DM), verification. `Reengage` actions are returned for the
    /// caller, who owns the turn: re-run it with the original message, calling
    /// `journal.begin_turn(event_id)` first and `end_turn` / `record_reply` after.
    ///
    /// `operator` is the MXID to tell when a fallback went out (`None` skips it).
    /// Failures are logged and retried by the next sweep.
    pub async fn execute_sweep(
        &self,
        actions: Vec<SweepAction>,
        operator: Option<&str>,
    ) -> Vec<SweepAction> {
        let Some(j) = self.journal.clone() else {
            return actions;
        };
        let mut left = Vec::new();
        for action in actions {
            match action {
                SweepAction::Liveness { event_id, room_id } => {
                    if let Err(e) = self
                        .deliver_obligation(
                            &event_id,
                            Dest::Room(&room_id),
                            PostKind::Liveness,
                            &j.policy().liveness_text,
                        )
                        .await
                    {
                        tracing::warn!("sweep: liveness post for {event_id} failed: {e:#}");
                    }
                }
                SweepAction::Fallback {
                    event_id,
                    room_id,
                    unsupported,
                    notify_operator,
                } => {
                    if let (true, Some(op)) = (notify_operator, operator) {
                        let key = format!("operator-fallback-{event_id}");
                        let note = format!(
                            "Delivery fallback sent for message {event_id} in {room_id}: \
                             no answer within the deadline."
                        );
                        match self.send_dm_chunked_keyed(op, &note, &key).await {
                            Ok(_) => j.mark_operator_notified(&event_id),
                            Err(e) => {
                                tracing::warn!("sweep: operator DM for {event_id} failed: {e:#}")
                            }
                        }
                    }
                    let text = if unsupported {
                        j.policy().unsupported_text.clone()
                    } else {
                        j.policy().fallback_text.clone()
                    };
                    if let Err(e) = self
                        .deliver_obligation(&event_id, Dest::Room(&room_id), PostKind::Fallback, &text)
                        .await
                    {
                        tracing::warn!("sweep: fallback for {event_id} failed (next sweep retries): {e:#}");
                    }
                }
                SweepAction::Verify { event_id, room_id } => {
                    self.verify_open_posts(|e| (e == event_id).then(|| room_id.clone()))
                        .await;
                }
                other @ SweepAction::Reengage { .. } => left.push(other),
            }
        }
        left
    }
}

