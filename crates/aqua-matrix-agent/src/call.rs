//! Call **signaling** for [`AgentClient`].
//!
//! ## Scope: signaling, not media
//!
//! matrix-sdk 0.17 ships no WebRTC/LiveKit media stack, so an agent can *ring* a
//! peer and *observe* call events, but it cannot place or carry the actual
//! audio/video stream of a call. [`ring_call`](AgentClient::ring_call) emits the
//! same `m.call.notify` (MSC4075) signal Element Call uses to make a peer's
//! Element X show an incoming call; joining the media would need an embedded
//! WebRTC engine (a separate, much larger effort). Inbound call *detection* is
//! surfaced through the relay's `on_call` seam.
//!
//! `m.call.notify` / `ApplicationType` are deprecated in ruma in favour of
//! `m.rtc.notification`, but they remain what current Element clients emit and
//! honour, so this module uses them deliberately — hence the module-wide
//! `allow(deprecated)`.
#![allow(deprecated)]

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use matrix_sdk::ruma::events::call::notify::{ApplicationType, CallNotifyEventContent};
use matrix_sdk::ruma::events::rtc::notification::NotificationType;
use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::UserId;

use crate::AgentClient;

impl AgentClient {
    /// Ring `target`: send an `m.call.notify` (MSC4075) with `NotificationType::Ring`
    /// into the DM, mentioning the peer — the same event Element Call emits to make
    /// a recipient's Element X show an incoming call.
    ///
    /// **Best-effort SIGNALING only.** This announces/rings a call; it does NOT
    /// open a media stream (matrix-sdk has no WebRTC). Whether a given client
    /// surfaces a ring with no accompanying MatrixRTC session is up to the client.
    /// Returns the event id of the notify.
    pub async fn ring_call(&self, target: &str) -> Result<String> {
        let user: &UserId = target
            .try_into()
            .map_err(|e| anyhow!("invalid target: {e}"))?;
        let room = self.ensure_dm_room(user).await?;
        let content = CallNotifyEventContent::new(
            new_call_id(),
            ApplicationType::Call,
            NotificationType::Ring,
            Mentions::with_user_ids([user.to_owned()]),
        );
        let resp = room
            .send(content)
            .await
            .context("failed to send call ring")?;
        Ok(resp.response.event_id.to_string())
    }
}

/// A fresh, collision-resistant call id. We have no RNG in the default feature
/// set, so derive it from the wall clock in nanoseconds — unique enough for a
/// ring (a real MatrixRTC session id would come from the media layer we don't
/// embed).
fn new_call_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("aqua-{nanos}")
}
