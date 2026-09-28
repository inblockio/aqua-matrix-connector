//! Long-lived MatrixRTC membership: keep this agent's `call.member` valid for a
//! call of any length, and make sure a crash cannot leave it behind.
//!
//! ## Why this exists
//!
//! [`set_rtc_member`](AgentClient::set_rtc_member) publishes one
//! `org.matrix.msc3401.call.member` state event with ruma's default
//! `expires = 4 h`. Element clients compute a session membership's absolute
//! expiry as `(created_ts ?? origin_server_ts) + expires` and drop an expired
//! member from the call (matrix-js-sdk `CallMembership.getAbsoluteExpiry` /
//! `isExpired`, and `isValidMembership` in `MatrixRTCSession.ts`). An agent that
//! stays in a call longer than 4 h therefore vanishes from every Element client
//! unless it re-sends the event; and an agent that crashes stays listed as a
//! ghost member until those 4 h run out.
//!
//! ## What Element Call does (matrix-js-sdk 42.x `MembershipManager.ts`)
//!
//! - **Expiry refresh** (`UpdateExpiry`): after the join it re-sends its
//!   membership every `min(membershipEventExpiryMs, 1 h) - 5 s`, keeping
//!   `created_ts` (its own membership's `createdTs()`) and growing `expires`
//!   (`membershipEventExpiryMs * iteration`), so the absolute expiry keeps
//!   moving ahead while the membership chain keeps its identity.
//! - **Dead-man switch** (MSC4140 delayed events): before the join it schedules
//!   an EMPTY `call.member` for its own state key with `delay = 8 s`
//!   (`_unstable_sendDelayedStateEvent`) and restarts that timer every 5 s. If
//!   the client dies, the homeserver sends the leave. A restart answered with
//!   `M_NOT_FOUND` means the delayed event is gone (it fired, or was lost), and
//!   the client re-schedules it. On hang-up it asks the server to send the
//!   delayed leave now (`_unstable_sendScheduledDelayedEvent`), falling back to
//!   sending the empty state event itself.
//! - `created_ts` matters beyond expiry: `RTCEncryptionManager` keys "have I
//!   shared my media key with this member" on `(user, device, createdTs)`, so a
//!   membership re-sent with a NEW `created_ts` reads as a rejoin (keys are
//!   re-shared), and one that comes back after a leave with the OLD `created_ts`
//!   reads as "already shared" (keys are not re-sent). A refresh keeps the old
//!   one; a rejoin after our leave fired must take a new one.
//!
//! [`hold_rtc_member`](AgentClient::hold_rtc_member) implements the same two
//! mechanisms for a server-side agent, with timings suited to a daemon
//! ([`RtcMemberTiming`]): a 60 s dead-man delay restarted every 15 s (instead
//! of 8 s / 5 s, so a short network blip does not drop the agent from the call),
//! and an hourly refresh that keeps the membership valid for 4 h beyond "now".
//!
//! ## Clocks
//!
//! No wall-clock reading is used. `created_ts` is the homeserver's
//! `origin_server_ts` of our own join event, and elapsed time is measured with
//! the monotonic [`tokio::time::Instant`], so a skewed host clock (WSL) cannot
//! shorten or lengthen the membership.
//!
//! ## Tokens
//!
//! siwx-oidc access tokens live ~5 min and this keeper runs for hours on its
//! own clone of the [`AgentClient`]. Before every request it rotates the token
//! in place when it is near expiry ([`AgentClient::reauth_token_only`], the
//! same no-sync rotation the Scribe's in-call poll client uses), and it retries
//! once after an `M_UNKNOWN_TOKEN`. It never syncs, so it cannot consume
//! to-device messages or touch the Olm account.

use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::ruma::api::client::delayed_events::{
    delayed_state_event, update_delayed_event, DelayParameters,
};
use matrix_sdk::ruma::api::client::room::get_room_event;
use matrix_sdk::ruma::api::client::state::get_state_event_for_key;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::call::member::{CallMemberEventContent, CallMemberStateKey};
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, UInt};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::call::rtc_member_content;
use crate::{AgentClient, TOKEN_REFRESH_MARGIN};

/// Timings for [`AgentClient::hold_rtc_member`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcMemberTiming {
    /// How long each published membership stays valid beyond the moment it is
    /// sent. 4 h is Element Call's `DEFAULT_EXPIRE_DURATION`.
    pub expiry: Duration,
    /// How often the membership is re-sent. Must be shorter than `expiry`; the
    /// difference is the head-room for retries (and for a paused host).
    pub refresh_every: Duration,
    /// Dead-man delay of the MSC4140 delayed leave: how long after the last
    /// successful restart the homeserver removes us if we go silent.
    pub leave_delay: Duration,
    /// How often the delayed leave is restarted. Must be shorter than
    /// `leave_delay`, with room for at least one retry.
    pub leave_restart_every: Duration,
    /// Back-off after a failed refresh / restart / schedule attempt.
    pub retry_after_error: Duration,
}

impl Default for RtcMemberTiming {
    fn default() -> Self {
        Self {
            expiry: Duration::from_secs(4 * 3600),
            refresh_every: Duration::from_secs(3600),
            leave_delay: Duration::from_secs(60),
            leave_restart_every: Duration::from_secs(15),
            retry_after_error: Duration::from_secs(10),
        }
    }
}

impl RtcMemberTiming {
    fn validate(&self) -> Result<()> {
        if self.expiry.is_zero()
            || self.refresh_every.is_zero()
            || self.leave_delay.is_zero()
            || self.leave_restart_every.is_zero()
            || self.retry_after_error.is_zero()
        {
            return Err(anyhow!("RtcMemberTiming: every duration must be non-zero"));
        }
        if self.refresh_every >= self.expiry {
            return Err(anyhow!(
                "RtcMemberTiming: refresh_every ({:?}) must be shorter than expiry ({:?})",
                self.refresh_every,
                self.expiry
            ));
        }
        if self.leave_restart_every >= self.leave_delay {
            return Err(anyhow!(
                "RtcMemberTiming: leave_restart_every ({:?}) must be shorter than leave_delay ({:?})",
                self.leave_restart_every,
                self.leave_delay
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure schedule (no I/O, time passed in as "monotonic offset since start")
// ---------------------------------------------------------------------------

/// State of the dead-man switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaveState {
    /// The homeserver does not offer MSC4140 (or refuses our delay): no
    /// dead-man switch for this session; the 4 h expiry is the only bound.
    Disabled,
    /// Not scheduled (yet, or it was lost); try to schedule at `retry_at`.
    Unarmed { retry_at: Duration },
    /// Scheduled; restart it at `next_restart`.
    Armed { next_restart: Duration },
}

/// What the keeper must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Due {
    /// Schedule a (new) delayed leave.
    ArmLeave,
    /// Restart the scheduled delayed leave's timer.
    RestartLeave,
    /// Re-send the membership with this `expires` (relative to `created_ts`).
    Refresh { expires: Duration },
}

/// The refresh / heartbeat schedule, as a pure state machine over a monotonic
/// time offset. Every method takes `now` explicitly so it can be driven by a
/// fake clock in tests.
#[derive(Debug, Clone)]
pub(crate) struct Schedule {
    timing: RtcMemberTiming,
    /// When the current membership chain started (our join was acknowledged).
    /// `created_ts` on the server corresponds to this instant.
    joined_at: Duration,
    /// Until when the currently published membership is valid.
    valid_until: Duration,
    next_refresh: Duration,
    leave: LeaveState,
}

impl Schedule {
    /// A membership chain that was (re)joined at `now` with `expires = expiry`.
    pub(crate) fn joined(timing: RtcMemberTiming, now: Duration) -> Self {
        Self {
            timing,
            joined_at: now,
            valid_until: now + timing.expiry,
            next_refresh: now + timing.refresh_every,
            leave: LeaveState::Unarmed { retry_at: now },
        }
    }

    /// The earliest instant at which [`due`](Self::due) returns an action.
    pub(crate) fn next_wake(&self) -> Duration {
        let leave = match self.leave {
            LeaveState::Disabled => None,
            LeaveState::Unarmed { retry_at } => Some(retry_at),
            LeaveState::Armed { next_restart } => Some(next_restart),
        };
        leave.map_or(self.next_refresh, |l| l.min(self.next_refresh))
    }

    /// The action due at `now`, if any. The dead-man switch goes first: its
    /// deadline is seconds away, the membership's is hours away.
    pub(crate) fn due(&self, now: Duration) -> Option<Due> {
        match self.leave {
            LeaveState::Unarmed { retry_at } if retry_at <= now => return Some(Due::ArmLeave),
            LeaveState::Armed { next_restart } if next_restart <= now => {
                return Some(Due::RestartLeave)
            }
            _ => {}
        }
        (self.next_refresh <= now).then(|| Due::Refresh {
            expires: self.expires_at(now),
        })
    }

    /// `expires` for a membership re-sent at `now`: valid for a full `expiry`
    /// beyond `now`, measured from the chain's `created_ts`.
    pub(crate) fn expires_at(&self, now: Duration) -> Duration {
        now.saturating_sub(self.joined_at) + self.timing.expiry
    }

    /// Until when the currently published membership is valid.
    #[cfg(test)]
    pub(crate) fn valid_until(&self) -> Duration {
        self.valid_until
    }

    pub(crate) fn refreshed(&mut self, now: Duration, expires: Duration) {
        self.valid_until = self.joined_at + expires;
        self.next_refresh = now + self.timing.refresh_every;
    }

    pub(crate) fn refresh_failed(&mut self, now: Duration) {
        self.next_refresh = now + self.timing.retry_after_error;
    }

    /// A new chain (fresh `created_ts`) was started at `now`.
    pub(crate) fn rejoined(&mut self, now: Duration) {
        self.joined_at = now;
        self.valid_until = now + self.timing.expiry;
        self.next_refresh = now + self.timing.refresh_every;
    }

    pub(crate) fn leave_armed(&mut self, now: Duration) {
        self.leave = LeaveState::Armed {
            next_restart: now + self.timing.leave_restart_every,
        };
    }

    pub(crate) fn leave_arm_failed(&mut self, now: Duration) {
        self.leave = LeaveState::Unarmed {
            retry_at: now + self.timing.retry_after_error,
        };
    }

    pub(crate) fn leave_restart_failed(&mut self, now: Duration) {
        let wait = self
            .timing
            .retry_after_error
            .min(self.timing.leave_restart_every);
        self.leave = LeaveState::Armed {
            next_restart: now + wait,
        };
    }

    /// The delayed leave is gone; schedule a new one right away.
    pub(crate) fn leave_lost(&mut self, now: Duration) {
        self.leave = LeaveState::Unarmed { retry_at: now };
    }

    pub(crate) fn leave_disabled(&mut self) {
        self.leave = LeaveState::Disabled;
    }

    pub(crate) fn leave_enabled(&self) -> bool {
        self.leave != LeaveState::Disabled
    }
}

// ---------------------------------------------------------------------------
// Transport seam (live homeserver vs. test fake)
// ---------------------------------------------------------------------------

/// How a transport call failed, as far as the keeper cares.
#[derive(Debug)]
pub(crate) enum OpError {
    /// `M_NOT_FOUND`: e.g. the delayed event no longer exists.
    NotFound(anyhow::Error),
    /// The server does not support / allow this (MSC4140 off, delay too large).
    Unsupported(anyhow::Error),
    /// Anything else (network, 5xx, rate limit, token): retry later.
    Other(anyhow::Error),
}

impl OpError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            OpError::NotFound(e) | OpError::Unsupported(e) | OpError::Other(e) => e,
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::NotFound(e) => write!(f, "not found: {e:#}"),
            OpError::Unsupported(e) => write!(f, "unsupported: {e:#}"),
            OpError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

type OpResult<T> = std::result::Result<T, OpError>;

/// The homeserver operations the keeper needs. `LiveTransport` talks to the
/// real homeserver; tests use a recording fake.
pub(crate) trait MemberTransport: Send + 'static {
    /// Publish our membership (`created_ts: None` starts a new chain).
    fn send_member(
        &mut self,
        created_ts: Option<MilliSecondsSinceUnixEpoch>,
        expires: Duration,
    ) -> impl Future<Output = OpResult<OwnedEventId>> + Send;
    /// `origin_server_ts` of one of our events (the `created_ts` anchor).
    fn origin_server_ts(
        &mut self,
        event_id: &OwnedEventId,
    ) -> impl Future<Output = OpResult<MilliSecondsSinceUnixEpoch>> + Send;
    /// Whether our membership state event currently holds a membership
    /// (non-empty content).
    fn member_is_live(&mut self) -> impl Future<Output = OpResult<bool>> + Send;
    /// Schedule an empty `call.member` for our key after `delay` (MSC4140).
    fn schedule_leave(&mut self, delay: Duration) -> impl Future<Output = OpResult<String>> + Send;
    fn restart_leave(&mut self, delay_id: &str) -> impl Future<Output = OpResult<()>> + Send;
    fn send_leave_now(&mut self, delay_id: &str) -> impl Future<Output = OpResult<()>> + Send;
    /// Send the empty `call.member` ourselves (fallback leave).
    fn clear_member(&mut self) -> impl Future<Output = OpResult<()>> + Send;
}

// ---------------------------------------------------------------------------
// Keeper: drives a transport according to the schedule
// ---------------------------------------------------------------------------

pub(crate) struct Keeper<T: MemberTransport> {
    transport: T,
    sched: Schedule,
    origin: Instant,
    /// `origin_server_ts` of the event that started the current chain.
    created_ts: Option<MilliSecondsSinceUnixEpoch>,
    /// The event that started the current chain (to fetch `created_ts` late).
    chain_event: OwnedEventId,
    delay_id: Option<String>,
    room_id: String,
}

impl<T: MemberTransport> Keeper<T> {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    /// Join: publish the membership (hard error if that fails, as with
    /// `set_rtc_member`), anchor `created_ts`, then try to arm the dead-man
    /// switch (soft: the membership works without it).
    pub(crate) async fn start(
        mut transport: T,
        timing: RtcMemberTiming,
        room_id: String,
    ) -> Result<Self> {
        timing.validate()?;
        let origin = Instant::now();
        let chain_event = transport
            .send_member(None, timing.expiry)
            .await
            .map_err(OpError::into_anyhow)
            .context("failed to publish RTC membership")?;
        let now = origin.elapsed();
        let mut keeper = Self {
            transport,
            sched: Schedule::joined(timing, now),
            origin,
            created_ts: None,
            chain_event,
            delay_id: None,
            room_id,
        };
        keeper.anchor_created_ts().await;
        keeper.arm_leave().await;
        tracing::info!(
            room_id = %keeper.room_id,
            dead_man_switch = keeper.delay_id.is_some(),
            "RTC membership held (refresh before expiry, delayed leave on crash)"
        );
        Ok(keeper)
    }

    async fn anchor_created_ts(&mut self) {
        match self.transport.origin_server_ts(&self.chain_event).await {
            Ok(ts) => self.created_ts = Some(ts),
            Err(e) => tracing::warn!(
                room_id = %self.room_id,
                error = %e,
                "could not read our membership event's origin_server_ts; will retry before the refresh"
            ),
        }
    }

    async fn arm_leave(&mut self) {
        let delay = self.sched.timing.leave_delay;
        match self.transport.schedule_leave(delay).await {
            Ok(id) => {
                self.delay_id = Some(id);
                self.sched.leave_armed(self.now());
            }
            Err(OpError::Unsupported(e)) => {
                tracing::info!(
                    room_id = %self.room_id,
                    error = %format!("{e:#}"),
                    "homeserver offers no MSC4140 delayed leave; membership relies on its expiry alone"
                );
                self.sched.leave_disabled();
            }
            Err(e) => {
                tracing::warn!(room_id = %self.room_id, error = %e, "scheduling the delayed leave failed; retrying");
                self.sched.leave_arm_failed(self.now());
            }
        }
    }

    /// Start a new membership chain (fresh `created_ts`): used when our
    /// membership was removed while we are still in the call.
    async fn rejoin(&mut self) {
        let expiry = self.sched.timing.expiry;
        match self.transport.send_member(None, expiry).await {
            Ok(eid) => {
                self.chain_event = eid;
                self.created_ts = None;
                self.sched.rejoined(self.now());
                self.anchor_created_ts().await;
                tracing::info!(room_id = %self.room_id, "RTC membership re-published (new membership chain)");
            }
            Err(e) => {
                tracing::warn!(room_id = %self.room_id, error = %e, "re-publishing RTC membership failed; retrying");
                // Treat as an overdue refresh: retried after the back-off,
                // and without an anchor that retry takes this path again.
                self.created_ts = None;
                self.sched.refresh_failed(self.now());
            }
        }
    }

    async fn refresh(&mut self, expires: Duration) {
        if self.created_ts.is_none() {
            self.anchor_created_ts().await;
        }
        let Some(created_ts) = self.created_ts else {
            // Without the anchor a refresh would silently start a new chain;
            // do it explicitly so the schedule matches what the server holds.
            self.rejoin().await;
            return;
        };
        match self.transport.send_member(Some(created_ts), expires).await {
            Ok(_) => {
                self.sched.refreshed(self.now(), expires);
                tracing::debug!(
                    room_id = %self.room_id,
                    expires_s = expires.as_secs(),
                    "RTC membership refreshed"
                );
            }
            Err(e) => {
                tracing::warn!(room_id = %self.room_id, error = %e, "RTC membership refresh failed; retrying");
                self.sched.refresh_failed(self.now());
            }
        }
    }

    async fn restart_leave(&mut self) {
        let Some(id) = self.delay_id.clone() else {
            self.sched.leave_lost(self.now());
            return;
        };
        match self.transport.restart_leave(&id).await {
            Ok(()) => self.sched.leave_armed(self.now()),
            Err(OpError::NotFound(_)) => {
                // The delayed leave is gone: it fired (we were unreachable for
                // longer than the delay) or the server dropped it. Re-arm
                // first, then make sure we are still a member.
                self.delay_id = None;
                let live = self.transport.member_is_live().await;
                tracing::warn!(
                    room_id = %self.room_id,
                    member_still_live = ?live.as_ref().ok(),
                    "delayed leave no longer exists; re-arming"
                );
                self.sched.leave_lost(self.now());
                self.arm_leave().await;
                // Unknown (error) counts as gone: a rejoin is always safe
                // (Element re-shares keys), re-using the old created_ts after
                // a leave is not (Element would think keys were shared).
                if !matches!(live, Ok(true)) {
                    self.rejoin().await;
                }
            }
            Err(e) => {
                tracing::warn!(room_id = %self.room_id, error = %e, "restarting the delayed leave failed; retrying");
                self.sched.leave_restart_failed(self.now());
            }
        }
    }

    /// Perform the one action due now, if any. Returns whether one ran.
    pub(crate) async fn step(&mut self) -> bool {
        match self.sched.due(self.now()) {
            Some(Due::ArmLeave) => self.arm_leave().await,
            Some(Due::RestartLeave) => self.restart_leave().await,
            Some(Due::Refresh { expires }) => self.refresh(expires).await,
            None => return false,
        }
        true
    }

    /// Leave the call: have the server send the delayed leave now, or send
    /// the empty membership ourselves.
    pub(crate) async fn leave(&mut self) -> Result<()> {
        if let Some(id) = self.delay_id.take() {
            match self.transport.send_leave_now(&id).await {
                Ok(()) => {
                    tracing::info!(room_id = %self.room_id, "left the call (delayed leave sent now)");
                    return Ok(());
                }
                Err(e) => tracing::warn!(
                    room_id = %self.room_id,
                    error = %e,
                    "sending the delayed leave now failed; clearing membership directly"
                ),
            }
        }
        self.transport
            .clear_member()
            .await
            .map_err(OpError::into_anyhow)
            .context("failed to clear RTC membership")?;
        tracing::info!(room_id = %self.room_id, "left the call (membership cleared)");
        Ok(())
    }

    /// Run until `stop` fires (or its sender is dropped), then leave.
    pub(crate) async fn run(mut self, mut stop: oneshot::Receiver<()>) -> Result<()> {
        loop {
            while self.step().await {}
            let wake = self.origin + self.sched.next_wake();
            tokio::select! {
                _ = &mut stop => break,
                _ = tokio::time::sleep_until(wake) => {}
            }
        }
        self.leave().await
    }
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// How long [`RtcMembership::leave`] waits for the leave request(s).
const LEAVE_TIMEOUT: Duration = Duration::from_secs(30);

/// A held MatrixRTC membership, returned by
/// [`AgentClient::hold_rtc_member`]. While it lives, a background task keeps
/// the membership valid and the dead-man switch armed.
///
/// Call [`leave`](Self::leave) on hang-up. Dropping the handle without
/// `leave` also stops the refresh and leaves in the background (best effort,
/// needs the runtime to stay up); if the process dies instead, the delayed
/// leave removes the membership within
/// [`leave_delay`](RtcMemberTiming::leave_delay), or, on a homeserver without
/// MSC4140, the membership lapses at its current expiry (at most
/// [`expiry`](RtcMemberTiming::expiry)).
pub struct RtcMembership {
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    dead_man_switch: bool,
}

impl RtcMembership {
    fn spawn<T: MemberTransport>(keeper: Keeper<T>) -> Self {
        let dead_man_switch = keeper.sched.leave_enabled();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(keeper.run(rx));
        Self {
            stop: Some(tx),
            task: Some(task),
            dead_man_switch,
        }
    }

    /// Whether a delayed leave (MSC4140) protects this membership. `false`
    /// when the homeserver refused it at join time.
    pub fn has_dead_man_switch(&self) -> bool {
        self.dead_man_switch
    }

    /// Whether the keeper task is still running (it only stops on leave).
    pub fn is_active(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stop refreshing and leave the call. Bounded by 30 s.
    pub async fn leave(mut self) -> Result<()> {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(LEAVE_TIMEOUT, task).await {
            Ok(Ok(res)) => res,
            Ok(Err(join)) => Err(anyhow!("RTC membership task failed: {join}")),
            Err(_) => Err(anyhow!(
                "leaving the call timed out after {LEAVE_TIMEOUT:?}"
            )),
        }
    }
}

impl Drop for RtcMembership {
    fn drop(&mut self) {
        // Signal the keeper to leave; it finishes on its own (detached).
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

impl AgentClient {
    /// Join a MatrixRTC call as a member and **keep** that membership for as
    /// long as the returned [`RtcMembership`] lives: the long-call form of
    /// [`set_rtc_member`](Self::set_rtc_member) (same content, same state key,
    /// same MSC3757 owned-key fallback).
    ///
    /// Publishes the membership (error if that fails), then in the background
    /// re-sends it every [`refresh_every`](RtcMemberTiming::refresh_every) with
    /// the original `created_ts` and an `expires` reaching
    /// [`expiry`](RtcMemberTiming::expiry) past "now", and keeps an MSC4140
    /// delayed leave armed so a crash removes the member within
    /// [`leave_delay`](RtcMemberTiming::leave_delay). If our membership
    /// disappears mid-call (the delayed leave fired during an outage), it is
    /// re-published as a new membership. See the module docs for the
    /// matrix-js-sdk behaviour this mirrors.
    ///
    /// Runs on its own clone of this client and rotates that clone's token
    /// itself, so the caller's client may go stale meanwhile.
    pub async fn hold_rtc_member(
        &self,
        room_id: &str,
        livekit_alias: &str,
        livekit_service_url: &str,
        timing: RtcMemberTiming,
    ) -> Result<RtcMembership> {
        let transport =
            LiveTransport::new(self.clone(), room_id, livekit_alias, livekit_service_url)?;
        let keeper = Keeper::start(transport, timing, room_id.to_owned()).await?;
        Ok(RtcMembership::spawn(keeper))
    }
}

// ---------------------------------------------------------------------------
// Live transport
// ---------------------------------------------------------------------------

/// How the keeper should treat a homeserver error, decided from its errcode
/// and HTTP status only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrClass {
    NotFound,
    Unsupported,
    Other,
}

/// Classify a homeserver error for the keeper.
fn classify(kind: Option<&ErrorKind>, status: Option<u16>) -> ErrClass {
    match (kind, status) {
        (Some(ErrorKind::NotFound), _) => ErrClass::NotFound,
        // MSC4140 disabled (403 "Sending delayed events has been disallowed"),
        // endpoint unknown, or our delay above `max_event_delay_duration`
        // (400 with a non-standard errcode).
        (Some(ErrorKind::Unrecognized), _) | (Some(ErrorKind::Forbidden), _) => {
            ErrClass::Unsupported
        }
        (_, Some(400)) | (_, Some(405)) | (None, Some(404)) => ErrClass::Unsupported,
        _ => ErrClass::Other,
    }
}

fn is_token_rejection(kind: Option<&ErrorKind>, status: Option<u16>) -> bool {
    matches!(kind, Some(ErrorKind::UnknownToken(_))) || status == Some(401)
}

fn op_error(class: ErrClass, err: anyhow::Error) -> OpError {
    match class {
        ErrClass::NotFound => OpError::NotFound(err),
        ErrClass::Unsupported => OpError::Unsupported(err),
        ErrClass::Other => OpError::Other(err),
    }
}

fn sdk_err(e: matrix_sdk::Error) -> (bool, OpError) {
    let kind = e.client_api_error_kind();
    let status = e.as_client_api_error().map(|c| c.status_code.as_u16());
    let (token, class) = (is_token_rejection(kind, status), classify(kind, status));
    (token, op_error(class, anyhow::Error::new(e)))
}

fn http_err(e: matrix_sdk::HttpError) -> (bool, OpError) {
    let kind = e.client_api_error_kind();
    let status = e.as_client_api_error().map(|c| c.status_code.as_u16());
    let (token, class) = (is_token_rejection(kind, status), classify(kind, status));
    (token, op_error(class, anyhow::Error::new(e)))
}

/// Run `$op` (an expression using `$s: &mut LiveTransport` that yields
/// `Result<T, (bool /*token rejected*/, OpError)>`) with a proactive token
/// rotation before and one reauth-and-retry on a token rejection.
macro_rules! with_token {
    ($s:ident, $op:expr) => {{
        $s.ensure_fresh().await;
        match $op {
            Ok(v) => Ok(v),
            Err((true, first)) => {
                tracing::warn!(error = %first, "RTC membership request rejected (token); re-authenticating");
                match $s.agent.reauth_token_only().await {
                    Ok(()) => $op.map_err(|(_, e)| e),
                    Err(e) => Err(OpError::Other(e.context("token refresh failed"))),
                }
            }
            Err((false, e)) => Err(e),
        }
    }};
}

pub(crate) struct LiveTransport {
    agent: AgentClient,
    room_id: OwnedRoomId,
    device_id: String,
    alias: String,
    service_url: String,
    /// Owned (MSC3757) or plain key, whichever the first send got accepted.
    state_key: Option<CallMemberStateKey>,
}

impl LiveTransport {
    fn new(agent: AgentClient, room_id: &str, alias: &str, service_url: &str) -> Result<Self> {
        let room_id: OwnedRoomId = room_id
            .try_into()
            .map_err(|e| anyhow!("invalid room_id: {e}"))?;
        let device_id = agent
            .device_id()
            .ok_or_else(|| anyhow!("agent has no device_id; cannot set RTC membership"))?;
        Ok(Self {
            agent,
            room_id,
            device_id,
            alias: alias.to_owned(),
            service_url: service_url.to_owned(),
            state_key: None,
        })
    }

    async fn ensure_fresh(&mut self) {
        if self.agent.token_seconds_left() < TOKEN_REFRESH_MARGIN {
            if let Err(e) = self.agent.reauth_token_only().await {
                tracing::warn!(error = %format!("{e:#}"), "RTC membership keeper: proactive token rotation failed");
            }
        }
    }

    fn room(&self) -> std::result::Result<matrix_sdk::Room, (bool, OpError)> {
        self.agent
            .rtc_room(self.room_id.as_str())
            .map_err(|e| (false, OpError::Other(e)))
    }

    fn key(&self) -> std::result::Result<CallMemberStateKey, (bool, OpError)> {
        match &self.state_key {
            Some(k) => Ok(k.clone()),
            None => self
                .agent
                .rtc_member_state_key(true)
                .map_err(|e| (false, OpError::Other(e))),
        }
    }

    async fn put_member(
        &self,
        key: &CallMemberStateKey,
        content: CallMemberEventContent,
    ) -> std::result::Result<OwnedEventId, (bool, OpError)> {
        let room = self.room()?;
        room.send_state_event_for_key(key, content)
            .await
            .map(|r| r.event_id)
            .map_err(sdk_err)
    }

    async fn send_member_once(
        &mut self,
        created_ts: Option<MilliSecondsSinceUnixEpoch>,
        expires: Duration,
    ) -> std::result::Result<OwnedEventId, (bool, OpError)> {
        let content = rtc_member_content(
            &self.device_id,
            &self.alias,
            &self.service_url,
            created_ts,
            Some(expires),
        );
        if let Some(key) = self.state_key.clone() {
            return self.put_member(&key, content).await;
        }
        // First send: prefer the MSC3757 owned key, fall back to the plain key
        // (same policy as `set_rtc_member`), and remember which one worked.
        let owned = self.key()?;
        match self.put_member(&owned, content.clone()).await {
            Ok(eid) => {
                tracing::info!(
                    state_key = owned.as_ref(),
                    "RTC membership published (owned state key)"
                );
                self.state_key = Some(owned);
                Ok(eid)
            }
            Err((true, e)) => Err((true, e)),
            Err((false, e)) => {
                tracing::warn!(error = %e, "owned RTC member state key rejected; retrying unprefixed");
                let plain = self
                    .agent
                    .rtc_member_state_key(false)
                    .map_err(|e| (false, OpError::Other(e)))?;
                let eid = self.put_member(&plain, content).await?;
                tracing::info!(
                    state_key = plain.as_ref(),
                    "RTC membership published (unprefixed state key)"
                );
                self.state_key = Some(plain);
                Ok(eid)
            }
        }
    }

    async fn origin_ts_once(
        &self,
        event_id: &OwnedEventId,
    ) -> std::result::Result<MilliSecondsSinceUnixEpoch, (bool, OpError)> {
        let req = get_room_event::v3::Request::new(self.room_id.clone(), event_id.clone());
        let resp = self.agent.client().send(req).await.map_err(http_err)?;
        let ts: Option<UInt> = resp
            .event
            .get_field("origin_server_ts")
            .map_err(|e| (false, OpError::Other(anyhow!("event JSON: {e}"))))?;
        ts.map(MilliSecondsSinceUnixEpoch).ok_or((
            false,
            OpError::Other(anyhow!("event {event_id} has no origin_server_ts")),
        ))
    }

    async fn member_is_live_once(&self) -> std::result::Result<bool, (bool, OpError)> {
        let key = self.key()?;
        let req = get_state_event_for_key::v3::Request::new(
            self.room_id.clone(),
            StateEventType::CallMember,
            key.as_ref().to_owned(),
        );
        match self.agent.client().send(req).await {
            Ok(resp) => {
                let v: serde_json::Value = serde_json::from_str(resp.event_or_content.get())
                    .map_err(|e| (false, OpError::Other(anyhow!("state JSON: {e}"))))?;
                Ok(v.as_object().is_some_and(|o| !o.is_empty()))
            }
            Err(e) => match http_err(e) {
                (_, OpError::NotFound(_)) => Ok(false),
                other => Err(other),
            },
        }
    }

    async fn schedule_leave_once(
        &self,
        delay: Duration,
    ) -> std::result::Result<String, (bool, OpError)> {
        let key = self.key()?;
        let req = delayed_state_event::unstable::Request::new(
            self.room_id.clone(),
            key.as_ref().to_owned(),
            DelayParameters::Timeout { timeout: delay },
            &CallMemberEventContent::new_empty(None),
        )
        .map_err(|e| {
            (
                false,
                OpError::Other(anyhow!("serialize delayed leave: {e}")),
            )
        })?;
        self.agent
            .client()
            .send(req)
            .await
            .map(|r| r.delay_id)
            .map_err(http_err)
    }

    async fn update_delayed_once(
        &self,
        delay_id: &str,
        action: update_delayed_event::unstable::UpdateAction,
    ) -> std::result::Result<(), (bool, OpError)> {
        let req = update_delayed_event::unstable::Request::new(delay_id.to_owned(), action);
        self.agent
            .client()
            .send(req)
            .await
            .map(|_| ())
            .map_err(http_err)
    }

    async fn clear_once(&self) -> std::result::Result<(), (bool, OpError)> {
        let key = self.key()?;
        self.put_member(&key, CallMemberEventContent::new_empty(None))
            .await
            .map(|_| ())
    }
}

impl MemberTransport for LiveTransport {
    async fn send_member(
        &mut self,
        created_ts: Option<MilliSecondsSinceUnixEpoch>,
        expires: Duration,
    ) -> OpResult<OwnedEventId> {
        with_token!(self, self.send_member_once(created_ts, expires).await)
    }

    async fn origin_server_ts(
        &mut self,
        event_id: &OwnedEventId,
    ) -> OpResult<MilliSecondsSinceUnixEpoch> {
        with_token!(self, self.origin_ts_once(event_id).await)
    }

    async fn member_is_live(&mut self) -> OpResult<bool> {
        with_token!(self, self.member_is_live_once().await)
    }

    async fn schedule_leave(&mut self, delay: Duration) -> OpResult<String> {
        with_token!(self, self.schedule_leave_once(delay).await)
    }

    async fn restart_leave(&mut self, delay_id: &str) -> OpResult<()> {
        use update_delayed_event::unstable::UpdateAction;
        with_token!(
            self,
            self.update_delayed_once(delay_id, UpdateAction::Restart)
                .await
        )
    }

    async fn send_leave_now(&mut self, delay_id: &str) -> OpResult<()> {
        use update_delayed_event::unstable::UpdateAction;
        with_token!(
            self,
            self.update_delayed_once(delay_id, UpdateAction::Send).await
        )
    }

    async fn clear_member(&mut self) -> OpResult<()> {
        with_token!(self, self.clear_once().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const H: Duration = Duration::from_secs(3600);
    const SERVER_EPOCH_MS: u64 = 1_790_000_000_000;

    fn timing() -> RtcMemberTiming {
        RtcMemberTiming::default()
    }

    // ---- pure schedule, driven by an explicit (mocked) clock ---------------

    /// Drive the schedule with every action succeeding and return the
    /// (time, expires) of each refresh.
    fn simulate(sched: &mut Schedule, until: Duration) -> Vec<(Duration, Duration)> {
        let mut refreshes = Vec::new();
        loop {
            let now = sched.next_wake();
            if now > until {
                return refreshes;
            }
            match sched.due(now).expect("next_wake must be a due instant") {
                Due::ArmLeave => sched.leave_armed(now),
                Due::RestartLeave => sched.leave_armed(now),
                Due::Refresh { expires } => {
                    assert!(
                        now < sched.valid_until(),
                        "refresh at {now:?} came after the membership lapsed at {:?}",
                        sched.valid_until()
                    );
                    sched.refreshed(now, expires);
                    refreshes.push((now, expires));
                }
            }
        }
    }

    #[test]
    fn schedule_refreshes_hourly_and_never_lets_the_membership_lapse() {
        let mut sched = Schedule::joined(timing(), Duration::ZERO);
        let refreshes = simulate(&mut sched, 30 * H);
        assert_eq!(refreshes.len(), 30, "one refresh per hour over 30 h");
        for (k, (at, expires)) in refreshes.iter().enumerate() {
            let k = k as u32 + 1;
            assert_eq!(*at, H * k, "refresh k={k} at k hours");
            // expires is relative to created_ts (t=0): always 4 h past "now".
            assert_eq!(*expires, H * k + 4 * H);
        }
        // After the last refresh the membership is still good for 4 h.
        assert_eq!(sched.valid_until(), 30 * H + 4 * H);
    }

    #[test]
    fn schedule_arms_leave_immediately_then_restarts_every_15s() {
        let mut sched = Schedule::joined(timing(), Duration::from_secs(5));
        assert_eq!(sched.next_wake(), Duration::from_secs(5));
        assert_eq!(sched.due(Duration::from_secs(5)), Some(Due::ArmLeave));
        sched.leave_armed(Duration::from_secs(5));
        assert_eq!(sched.due(Duration::from_secs(19)), None);
        assert_eq!(sched.next_wake(), Duration::from_secs(20));
        assert_eq!(sched.due(Duration::from_secs(20)), Some(Due::RestartLeave));
        // A failed restart retries after min(retry, restart) = 10 s, well
        // inside the 60 s dead-man delay.
        sched.leave_restart_failed(Duration::from_secs(20));
        assert_eq!(sched.next_wake(), Duration::from_secs(30));
    }

    #[test]
    fn schedule_retries_a_failed_refresh_until_it_lands_before_expiry() {
        let mut sched = Schedule::joined(timing(), Duration::ZERO);
        sched.leave_disabled();
        // Refresh at 1 h fails for (almost) the whole 3 h of head-room.
        let mut now = sched.next_wake();
        assert_eq!(now, H);
        let mut attempts = 0;
        while now < 4 * H - Duration::from_secs(30) {
            assert!(matches!(sched.due(now), Some(Due::Refresh { .. })));
            sched.refresh_failed(now);
            attempts += 1;
            now = sched.next_wake();
        }
        assert!(attempts > 1000, "retried every 10 s: {attempts}");
        let Some(Due::Refresh { expires }) = sched.due(now) else {
            panic!("refresh must still be due");
        };
        assert!(now < sched.valid_until(), "the late retry is still in time");
        sched.refreshed(now, expires);
        assert_eq!(sched.valid_until(), now + 4 * H);
        assert_eq!(sched.next_wake(), now + H);
    }

    #[test]
    fn schedule_rejoin_starts_a_new_chain() {
        let mut sched = Schedule::joined(timing(), Duration::ZERO);
        sched.leave_disabled();
        sched.rejoined(2 * H + Duration::from_secs(7));
        assert_eq!(sched.valid_until(), 6 * H + Duration::from_secs(7));
        assert_eq!(sched.next_wake(), 3 * H + Duration::from_secs(7));
        // expires is relative to the NEW chain start.
        assert_eq!(sched.expires_at(3 * H + Duration::from_secs(7)), 5 * H);
    }

    #[test]
    fn timing_validation_rejects_unsafe_values() {
        assert!(timing().validate().is_ok());
        let bad_refresh = RtcMemberTiming {
            refresh_every: 4 * H,
            ..timing()
        };
        assert!(bad_refresh.validate().is_err());
        let bad_restart = RtcMemberTiming {
            leave_restart_every: Duration::from_secs(60),
            ..timing()
        };
        assert!(bad_restart.validate().is_err());
        let zero = RtcMemberTiming {
            retry_after_error: Duration::ZERO,
            ..timing()
        };
        assert!(zero.validate().is_err());
    }

    #[test]
    fn classify_maps_msc4140_failures() {
        assert_eq!(
            classify(Some(&ErrorKind::NotFound), Some(404)),
            ErrClass::NotFound
        );
        assert_eq!(
            classify(Some(&ErrorKind::Unrecognized), Some(404)),
            ErrClass::Unsupported
        );
        assert_eq!(
            classify(Some(&ErrorKind::Forbidden), Some(403)),
            ErrClass::Unsupported
        );
        assert_eq!(
            classify(Some(&ErrorKind::Unknown), Some(400)),
            ErrClass::Unsupported
        );
        assert_eq!(classify(None, Some(404)), ErrClass::Unsupported);
        assert_eq!(
            classify(Some(&ErrorKind::Unknown), Some(500)),
            ErrClass::Other
        );
        assert_eq!(classify(None, None), ErrClass::Other);
    }

    // ---- keeper against a recording fake homeserver (paused tokio clock) ----

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Member {
            at: Duration,
            created_ts: Option<u64>,
            expires: Duration,
        },
        Schedule {
            at: Duration,
        },
        Restart {
            at: Duration,
        },
        LeaveNow,
        Clear,
        IsLive,
    }

    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
        /// event id -> server ts
        events: Vec<u64>,
        msc4140: bool,
        fail_member_until: Duration,
        /// Restarts from this time on answer M_NOT_FOUND (once).
        lose_leave_at: Option<Duration>,
        member_live_after_loss: bool,
        fail_leave_now: bool,
        next_delay_id: u32,
        fail_join: bool,
    }

    #[derive(Clone)]
    struct FakeTransport {
        state: Arc<Mutex<Fake>>,
        t0: Instant,
    }

    impl FakeTransport {
        fn new(f: Fake) -> (Self, Arc<Mutex<Fake>>) {
            let state = Arc::new(Mutex::new(f));
            (
                Self {
                    state: state.clone(),
                    t0: Instant::now(),
                },
                state,
            )
        }
        fn at(&self) -> Duration {
            self.t0.elapsed()
        }
    }

    fn other(msg: &str) -> OpError {
        OpError::Other(anyhow!(msg.to_owned()))
    }

    impl MemberTransport for FakeTransport {
        async fn send_member(
            &mut self,
            created_ts: Option<MilliSecondsSinceUnixEpoch>,
            expires: Duration,
        ) -> OpResult<OwnedEventId> {
            let at = self.at();
            let mut f = self.state.lock().unwrap();
            f.calls.push(Call::Member {
                at,
                created_ts: created_ts.map(|t| u64::from(t.0)),
                expires,
            });
            if f.fail_join || at < f.fail_member_until {
                return Err(other("simulated 502"));
            }
            f.events.push(SERVER_EPOCH_MS + at.as_millis() as u64);
            let id = format!("$ev{}:example.org", f.events.len() - 1);
            Ok(OwnedEventId::try_from(id).unwrap())
        }

        async fn origin_server_ts(
            &mut self,
            event_id: &OwnedEventId,
        ) -> OpResult<MilliSecondsSinceUnixEpoch> {
            let f = self.state.lock().unwrap();
            let idx: usize = event_id.as_str()[3..]
                .split(':')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            Ok(MilliSecondsSinceUnixEpoch(
                UInt::new(f.events[idx]).unwrap(),
            ))
        }

        async fn member_is_live(&mut self) -> OpResult<bool> {
            let mut f = self.state.lock().unwrap();
            f.calls.push(Call::IsLive);
            Ok(f.member_live_after_loss)
        }

        async fn schedule_leave(&mut self, _delay: Duration) -> OpResult<String> {
            let at = self.at();
            let mut f = self.state.lock().unwrap();
            f.calls.push(Call::Schedule { at });
            if !f.msc4140 {
                return Err(OpError::Unsupported(anyhow!("M_UNRECOGNIZED")));
            }
            f.next_delay_id += 1;
            Ok(format!("syd_{}", f.next_delay_id))
        }

        async fn restart_leave(&mut self, _delay_id: &str) -> OpResult<()> {
            let at = self.at();
            let mut f = self.state.lock().unwrap();
            f.calls.push(Call::Restart { at });
            if f.lose_leave_at.is_some_and(|t| at >= t) {
                f.lose_leave_at = None;
                return Err(OpError::NotFound(anyhow!("M_NOT_FOUND")));
            }
            Ok(())
        }

        async fn send_leave_now(&mut self, _delay_id: &str) -> OpResult<()> {
            let mut f = self.state.lock().unwrap();
            f.calls.push(Call::LeaveNow);
            if f.fail_leave_now {
                return Err(other("simulated timeout"));
            }
            Ok(())
        }

        async fn clear_member(&mut self) -> OpResult<()> {
            self.state.lock().unwrap().calls.push(Call::Clear);
            Ok(())
        }
    }

    fn members(state: &Arc<Mutex<Fake>>) -> Vec<(Duration, Option<u64>, Duration)> {
        state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Member {
                    at,
                    created_ts,
                    expires,
                } => Some((*at, *created_ts, *expires)),
                _ => None,
            })
            .collect()
    }

    fn count(state: &Arc<Mutex<Fake>>, pred: impl Fn(&Call) -> bool) -> usize {
        state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| pred(c))
            .count()
    }

    async fn hold(fake: Fake) -> (RtcMembership, Arc<Mutex<Fake>>) {
        let (t, state) = FakeTransport::new(fake);
        let keeper = Keeper::start(t, timing(), "!room:example.org".into())
            .await
            .expect("join");
        (RtcMembership::spawn(keeper), state)
    }

    /// Every re-send must land while the previous one is still valid, with the
    /// original created_ts, and push the expiry 4 h past "now".
    #[tokio::test(start_paused = true)]
    async fn membership_is_resent_before_expiry_with_the_original_created_ts() {
        let (m, state) = hold(Fake {
            msc4140: true,
            ..Default::default()
        })
        .await;
        assert!(m.has_dead_man_switch());
        tokio::time::sleep(10 * H + Duration::from_secs(1)).await;
        assert!(m.is_active());
        m.leave().await.unwrap();

        let sends = members(&state);
        assert_eq!(sends.len(), 11, "join + 10 hourly refreshes: {sends:?}");
        let (join_at, join_created, join_expires) = sends[0];
        assert_eq!(join_created, None, "the join starts the chain");
        assert_eq!(join_expires, 4 * H);
        let anchor = SERVER_EPOCH_MS + join_at.as_millis() as u64;
        let mut valid_until = join_at + join_expires;
        for (k, (at, created, expires)) in sends.iter().enumerate().skip(1) {
            assert_eq!(*created, Some(anchor), "refresh {k} keeps created_ts");
            assert!(
                *at < valid_until,
                "refresh {k} at {at:?} after lapse {valid_until:?}"
            );
            assert_eq!(*at, join_at + H * k as u32, "hourly");
            assert_eq!(*expires, *at - join_at + 4 * H);
            valid_until = join_at + *expires;
        }
        // Dead-man switch: armed once, restarted every 15 s for 10 h.
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 1);
        let restarts = count(&state, |c| matches!(c, Call::Restart { .. }));
        assert_eq!(restarts, (10 * 3600) / 15);
        // Leave = the delayed leave sent now; no direct clear needed.
        let calls = state.lock().unwrap().calls.clone();
        assert_eq!(calls.last(), Some(&Call::LeaveNow));
        assert_eq!(count(&state, |c| *c == Call::Clear), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn without_msc4140_the_membership_is_still_refreshed() {
        let (m, state) = hold(Fake::default()).await;
        assert!(!m.has_dead_man_switch());
        tokio::time::sleep(5 * H + Duration::from_secs(1)).await;
        m.leave().await.unwrap();
        assert_eq!(members(&state).len(), 6);
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 1);
        assert_eq!(count(&state, |c| matches!(c, Call::Restart { .. })), 0);
        let calls = state.lock().unwrap().calls.clone();
        assert_eq!(calls.last(), Some(&Call::Clear), "fallback leave");
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_retries_through_an_outage_and_lands_before_expiry() {
        let (t, state) = FakeTransport::new(Fake {
            msc4140: true,
            ..Default::default()
        });
        let keeper = Keeper::start(t, timing(), "!r:example.org".into())
            .await
            .unwrap();
        // Outage from now until 3 h 50 (the join is done; refreshes fail).
        state.lock().unwrap().fail_member_until = 3 * H + Duration::from_secs(50 * 60);
        let m = RtcMembership::spawn(keeper);
        tokio::time::sleep(4 * H + Duration::from_secs(1)).await;
        m.leave().await.unwrap();

        let sends = members(&state);
        let ok: Vec<_> = sends
            .iter()
            .filter(|(at, _, _)| *at >= 3 * H + Duration::from_secs(50 * 60))
            .collect();
        let first_ok = ok.first().expect("a refresh landed after the outage");
        assert!(
            first_ok.0 < 4 * H,
            "landed at {:?}, before the 4 h expiry",
            first_ok.0
        );
        assert!(first_ok.0 - (3 * H + Duration::from_secs(50 * 60)) <= Duration::from_secs(10));
        assert_eq!(
            first_ok.2,
            first_ok.0 + 4 * H,
            "expires 4 h past the late refresh"
        );
        let failed = sends.len() - ok.len() - 1;
        assert!(
            failed > 1000,
            "retried every 10 s during the outage: {failed}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fired_delayed_leave_rearms_and_rejoins_with_a_new_created_ts() {
        let (m, state) = hold(Fake {
            msc4140: true,
            lose_leave_at: Some(Duration::from_secs(300)),
            member_live_after_loss: false,
            ..Default::default()
        })
        .await;
        tokio::time::sleep(H + Duration::from_secs(400)).await;
        m.leave().await.unwrap();

        assert_eq!(count(&state, |c| *c == Call::IsLive), 1);
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 2);
        let sends = members(&state);
        // join, rejoin at 300 s (new chain), refresh 1 h after the rejoin.
        assert_eq!(sends.len(), 3, "{sends:?}");
        assert_eq!(sends[1].0, Duration::from_secs(300));
        assert_eq!(sends[1].1, None, "rejoin starts a new chain");
        let new_anchor = SERVER_EPOCH_MS + 300_000;
        assert_eq!(sends[2].0, Duration::from_secs(300) + H);
        assert_eq!(sends[2].1, Some(new_anchor));
        assert_eq!(sends[2].2, 5 * H, "1 h into the new chain + 4 h");
        // The re-arm happened BEFORE the rejoin.
        let calls = state.lock().unwrap().calls.clone();
        let arm2 = calls
            .iter()
            .rposition(|c| matches!(c, Call::Schedule { .. }))
            .unwrap();
        let rejoin = calls
            .iter()
            .position(
                |c| matches!(c, Call::Member { created_ts: None, at, .. } if *at > Duration::ZERO),
            )
            .unwrap();
        assert!(arm2 < rejoin);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_delayed_leave_with_a_live_member_only_rearms() {
        let (m, state) = hold(Fake {
            msc4140: true,
            lose_leave_at: Some(Duration::from_secs(300)),
            member_live_after_loss: true,
            ..Default::default()
        })
        .await;
        tokio::time::sleep(H + Duration::from_secs(1)).await;
        m.leave().await.unwrap();
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 2);
        let sends = members(&state);
        assert_eq!(
            sends.len(),
            2,
            "join + hourly refresh, no rejoin: {sends:?}"
        );
        assert_eq!(sends[1].1, Some(SERVER_EPOCH_MS), "chain kept");
    }

    #[tokio::test(start_paused = true)]
    async fn leave_falls_back_to_clearing_and_stops_refreshing() {
        let (m, state) = hold(Fake {
            msc4140: true,
            fail_leave_now: true,
            ..Default::default()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        m.leave().await.unwrap();
        let before = state.lock().unwrap().calls.len();
        let calls = state.lock().unwrap().calls.clone();
        assert_eq!(&calls[before - 2..], &[Call::LeaveNow, Call::Clear]);
        // Nothing is sent after the leave, however long we wait.
        tokio::time::sleep(10 * H).await;
        assert_eq!(state.lock().unwrap().calls.len(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_leaves_in_the_background() {
        let (m, state) = hold(Fake {
            msc4140: true,
            ..Default::default()
        })
        .await;
        drop(m);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let calls = state.lock().unwrap().calls.clone();
        assert_eq!(calls.last(), Some(&Call::LeaveNow));
        tokio::time::sleep(5 * H).await;
        assert_eq!(members(&state).len(), 1, "no refresh after drop");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_join_is_an_error_and_spawns_nothing() {
        let (t, state) = FakeTransport::new(Fake {
            msc4140: true,
            fail_join: true,
            ..Default::default()
        });
        let res = Keeper::start(t, timing(), "!r:example.org".into()).await;
        assert!(res.is_err());
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 0);
    }
}
