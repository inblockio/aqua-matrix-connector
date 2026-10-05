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
//! ## Rejoin in place (2026-10-05, KEYLOSS-1)
//!
//! Element Call re-sends its media key to a member only when that member's
//! `(user, device, createdTs())` is new to it, and Synapse drops a state event
//! whose content equals the current one (no new event, no new `createdTs`).
//! [`RtcMembership::rejoin`] asks the RUNNING keeper to re-publish the
//! membership with a fresh `created_ts`, so every Element Call peer reads a
//! new joiner and sends its key again. Same keeper, same state key, no empty
//! membership in between, and the MSC4140 delayed leave stays armed
//! throughout (Synapse cancels pending delayed state only for OTHER senders,
//! `synapse/handlers/delayed_events.py` `_handle_state_deltas`). The keeper
//! adopts the new `created_ts` for every later hourly refresh, so a refresh
//! never flips back to the old one. The new value is the server-anchored
//! estimate of "now" (`origin_server_ts` of the chain's first event plus the
//! monotonic time since), never below the old value plus 1 ms; the host
//! wall clock is used only when that anchor could not be read.
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
//! ## Tokens, and why there is no matrix-sdk `Client` here
//!
//! siwx-oidc access tokens live ~5 min and this keeper runs for hours. It
//! must NOT hold or build a matrix-sdk `Client`: every `Client` this crate
//! builds opens its own OlmMachine over the agent's shared SQLite crypto
//! store, and two OlmMachines on one store is the 2026-09-15/27 one-time-key
//! collision (the Scribe went one-way Olm-deaf). So the keeper copies the
//! caller's current access token into a token-only [`RestSession`] and sends
//! plain ruma requests over reqwest; nothing it sends needs E2EE. Before
//! every request it rotates that token when near expiry with
//! [`mint_session_token`](crate::mint_session_token) (siwx-oidc refresh grant,
//! persisted to `config.toml`, no `Client`), and it retries once after an
//! `M_UNKNOWN_TOKEN`. It never syncs and never touches the Olm account. A
//! rotation is bounded in time, never persists a session for another user or
//! device, and after such a mismatch the keeper stops instead of logging in
//! afresh.

use std::future::Future;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use std::borrow::Cow;

use matrix_sdk::ruma::api::auth_scheme::{AuthScheme, SendAccessToken};
use matrix_sdk::ruma::api::client::delayed_events::{
    delayed_state_event, update_delayed_event, DelayParameters,
};
use matrix_sdk::ruma::api::client::room::get_room_event;
use matrix_sdk::ruma::api::client::state::{get_state_event_for_key, send_state_event};
use matrix_sdk::ruma::api::error::{Error as RumaApiError, ErrorKind, FromHttpResponseError};
use matrix_sdk::ruma::api::path_builder::VersionHistory;
use matrix_sdk::ruma::api::{IncomingResponse, MatrixVersion, OutgoingRequest, SupportedVersions};
use matrix_sdk::ruma::events::call::member::{CallMemberEventContent, CallMemberStateKey};
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::exports::http;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, UInt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::call::{rtc_member_content, rtc_member_state_key_for};
use crate::{
    mint_session_token, unix_now, AgentClient, AgentConfig, SessionIdentityMismatch,
    TOKEN_REFRESH_MARGIN,
};

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
    pub(crate) fn valid_until(&self) -> Duration {
        self.valid_until
    }

    pub(crate) fn refreshed(&mut self, now: Duration, expires: Duration) {
        self.valid_until = self.joined_at + expires;
        self.next_refresh = now + self.timing.refresh_every;
    }

    /// A refresh (or rejoin) failed; retry after the back-off, or after the
    /// server's `retry_after` if that is longer.
    pub(crate) fn refresh_failed(&mut self, now: Duration, retry_after: Option<Duration>) {
        self.next_refresh = now + back_off(self.timing.retry_after_error, retry_after);
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

    pub(crate) fn leave_arm_failed(&mut self, now: Duration, retry_after: Option<Duration>) {
        self.leave = LeaveState::Unarmed {
            retry_at: now + back_off(self.timing.retry_after_error, retry_after),
        };
    }

    pub(crate) fn leave_restart_failed(&mut self, now: Duration, retry_after: Option<Duration>) {
        let wait = self
            .timing
            .retry_after_error
            .min(self.timing.leave_restart_every);
        self.leave = LeaveState::Armed {
            next_restart: now + back_off(wait, retry_after),
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

/// The wait after a failure: our own back-off, or the homeserver's
/// `retry_after` (429 `M_LIMIT_EXCEEDED`) when that is longer. Honouring it
/// can push a dead-man restart past `leave_delay`; hammering a rate-limited
/// server would not get the restart through any sooner.
fn back_off(ours: Duration, retry_after: Option<Duration>) -> Duration {
    retry_after.map_or(ours, |r| ours.max(r))
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
    /// 429 / `M_LIMIT_EXCEEDED`: retry, but not before `retry_after`.
    RateLimited {
        err: anyhow::Error,
        retry_after: Option<Duration>,
    },
    /// Anything else (network, 5xx, token): retry later.
    Other(anyhow::Error),
}

impl OpError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            OpError::NotFound(e)
            | OpError::Unsupported(e)
            | OpError::Other(e)
            | OpError::RateLimited { err: e, .. } => e,
        }
    }

    fn inner(&self) -> &anyhow::Error {
        match self {
            OpError::NotFound(e)
            | OpError::Unsupported(e)
            | OpError::Other(e)
            | OpError::RateLimited { err: e, .. } => e,
        }
    }

    /// How long the server asked us to wait, if it did.
    fn retry_after(&self) -> Option<Duration> {
        match self {
            OpError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Whether the homeserver answered `M_FORBIDDEN`.
    fn is_forbidden(&self) -> bool {
        self.inner()
            .downcast_ref::<RumaApiError>()
            .and_then(RumaApiError::error_kind)
            .is_some_and(|k| matches!(k, ErrorKind::Forbidden))
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::NotFound(e) => write!(f, "not found: {e:#}"),
            OpError::Unsupported(e) => write!(f, "unsupported: {e:#}"),
            OpError::RateLimited { err, retry_after } => {
                write!(f, "rate limited (retry after {retry_after:?}): {err:#}")
            }
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
    /// Delete a scheduled delayed leave without sending it (hand-over).
    fn cancel_leave(&mut self, delay_id: &str) -> impl Future<Output = OpResult<()>> + Send;
    /// A condition under which the keeper must stop instead of retrying
    /// (the session can no longer be renewed for this identity).
    fn fatal(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// Keeper: drives a transport according to the schedule
// ---------------------------------------------------------------------------

/// After the first failure of a streak, warn only every this many failures
/// (debug in between): an hours-long outage retries every 10 s.
const WARN_EVERY: u32 = 30;

/// What the handle (or a newer hold) asks the keeper to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmd {
    /// Keep the membership alive.
    Hold,
    /// Leave the call (delayed leave sent now, else clear).
    Leave,
    /// A newer hold for the same room and device takes over: cancel our
    /// delayed leave and stop WITHOUT touching the membership, which the new
    /// hold is about to (re)publish.
    HandOver,
}

pub(crate) struct Keeper<T: MemberTransport> {
    transport: T,
    sched: Schedule,
    origin: Instant,
    /// `origin_server_ts` of the event that started the current chain.
    created_ts: Option<MilliSecondsSinceUnixEpoch>,
    /// The event that started the current chain (to fetch `created_ts` late).
    /// `None` = no chain on the server we may continue: the next refresh
    /// must be a rejoin (new `created_ts`), e.g. after a failed rejoin.
    chain_event: Option<OwnedEventId>,
    /// The scheduled delayed leave, shared with the hold registry so a newer
    /// hold can cancel it if this keeper cannot.
    delay_id: Arc<StdMutex<Option<String>>>,
    room_id: String,
    /// Consecutive failed operations (log rate limiting).
    failures: u32,
}

impl<T: MemberTransport> Keeper<T> {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn delay_id(&self) -> Option<String> {
        self.delay_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set_delay_id(&self, id: Option<String>) {
        *self.delay_id.lock().unwrap_or_else(|p| p.into_inner()) = id;
    }

    fn take_delay_id(&self) -> Option<String> {
        self.delay_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Log a failed operation: the first of a streak and every
    /// [`WARN_EVERY`]th at warn, the rest at debug.
    fn failed(&mut self, what: &str, e: &OpError) {
        self.failures += 1;
        if self.failures == 1 || self.failures.is_multiple_of(WARN_EVERY) {
            tracing::warn!(
                room_id = %self.room_id,
                error = %e,
                consecutive_failures = self.failures,
                "{what} failed; retrying"
            );
        } else {
            tracing::debug!(
                room_id = %self.room_id,
                error = %e,
                consecutive_failures = self.failures,
                "{what} failed; retrying"
            );
        }
    }

    fn succeeded(&mut self) {
        if self.failures > 1 {
            tracing::info!(
                room_id = %self.room_id,
                failures = self.failures,
                "RTC membership keeper recovered"
            );
        }
        self.failures = 0;
    }

    /// Join: publish the membership (hard error if that fails, as with
    /// `set_rtc_member`), anchor `created_ts`, then try to arm the dead-man
    /// switch (soft: the membership works without it).
    #[cfg(test)]
    pub(crate) async fn start(
        transport: T,
        timing: RtcMemberTiming,
        room_id: String,
    ) -> Result<Self> {
        Self::start_shared(transport, timing, room_id, Arc::default()).await
    }

    async fn start_shared(
        mut transport: T,
        timing: RtcMemberTiming,
        room_id: String,
        delay_id: Arc<StdMutex<Option<String>>>,
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
            chain_event: Some(chain_event),
            delay_id,
            room_id,
            failures: 0,
        };
        keeper.anchor_created_ts().await;
        keeper.arm_leave().await;
        tracing::info!(
            room_id = %keeper.room_id,
            dead_man_switch = keeper.delay_id().is_some(),
            "RTC membership held (refresh before expiry, delayed leave on crash)"
        );
        Ok(keeper)
    }

    async fn anchor_created_ts(&mut self) {
        let Some(event) = self.chain_event.clone() else {
            return;
        };
        match self.transport.origin_server_ts(&event).await {
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
                self.set_delay_id(Some(id));
                self.sched.leave_armed(self.now());
                self.succeeded();
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
                self.failed("scheduling the delayed leave", &e);
                self.sched.leave_arm_failed(self.now(), e.retry_after());
            }
        }
    }

    /// Start a new membership chain (fresh `created_ts`): used when our
    /// membership was removed, or lapsed, while we are still in the call.
    async fn rejoin(&mut self) {
        let expiry = self.sched.timing.expiry;
        // Until a rejoin succeeds there is no chain we may continue.
        self.chain_event = None;
        self.created_ts = None;
        match self.transport.send_member(None, expiry).await {
            Ok(eid) => {
                self.chain_event = Some(eid);
                self.sched.rejoined(self.now());
                self.succeeded();
                self.anchor_created_ts().await;
                tracing::info!(room_id = %self.room_id, "RTC membership re-published (new membership chain)");
            }
            Err(e) => {
                self.failed("re-publishing RTC membership", &e);
                // Retried after the back-off as a refresh, which sees no
                // chain and takes this path again.
                self.sched.refresh_failed(self.now(), e.retry_after());
            }
        }
    }

    async fn refresh(&mut self, expires: Duration) {
        if self.chain_event.is_none() {
            self.rejoin().await;
            return;
        }
        // A membership past its expiry is gone for Element, and one re-sent
        // with its old created_ts would read as "keys already shared".
        if self.now() >= self.sched.valid_until() {
            tracing::warn!(
                room_id = %self.room_id,
                "RTC membership lapsed before a refresh landed; re-publishing as a new chain"
            );
            self.rejoin().await;
            return;
        }
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
                self.succeeded();
                tracing::debug!(
                    room_id = %self.room_id,
                    expires_s = expires.as_secs(),
                    "RTC membership refreshed"
                );
            }
            Err(e) => {
                self.failed("RTC membership refresh", &e);
                self.sched.refresh_failed(self.now(), e.retry_after());
            }
        }
    }

    /// [`RtcMembership::rejoin`]: re-publish the membership IN PLACE as a new
    /// chain with a fresh `created_ts`, keeping this keeper, its state key
    /// and its armed delayed leave. On success the keeper adopts the new
    /// `created_ts` (and restarts its refresh schedule from now), so later
    /// refreshes continue the NEW chain. On failure nothing changes: the old
    /// chain is still what the server holds and keeps being refreshed.
    async fn rejoin_in_place(&mut self) -> Result<RtcRejoined> {
        if self.created_ts.is_none() {
            self.anchor_created_ts().await;
        }
        let at = self.now();
        let created_ts = self.next_created_ts(at);
        let expiry = self.sched.timing.expiry;
        match self.transport.send_member(Some(created_ts), expiry).await {
            Ok(event_id) => {
                self.chain_event = Some(event_id.clone());
                self.created_ts = Some(created_ts);
                self.sched.rejoined(at);
                self.succeeded();
                tracing::info!(
                    room_id = %self.room_id,
                    created_ts = u64::from(created_ts.0),
                    "RTC membership re-published in place (new created_ts, peers read a rejoin)"
                );
                Ok(RtcRejoined {
                    event_id: event_id.to_string(),
                    created_ts_ms: u64::from(created_ts.0),
                })
            }
            Err(e) => {
                self.failed("rejoining the call in place", &e);
                Err(e
                    .into_anyhow()
                    .context("failed to re-publish the RTC membership"))
            }
        }
    }

    /// The `created_ts` of a chain started at monotonic `at`: the current
    /// chain's anchor plus the monotonic time since its start (the server
    /// clock's "now", no host wall clock), at least 1 ms past the anchor so
    /// it always differs. Without an anchor (its `origin_server_ts` could not
    /// be read) the host wall clock is the only estimate left.
    fn next_created_ts(&self, at: Duration) -> MilliSecondsSinceUnixEpoch {
        match self.created_ts {
            Some(anchor) => {
                let elapsed = at.saturating_sub(self.sched.joined_at).as_millis();
                let elapsed = u64::try_from(elapsed).unwrap_or(u64::MAX).max(1);
                MilliSecondsSinceUnixEpoch(UInt::new_saturating(
                    u64::from(anchor.0).saturating_add(elapsed),
                ))
            }
            None => {
                tracing::warn!(
                    room_id = %self.room_id,
                    "rejoin without a created_ts anchor; using the host clock for the new created_ts"
                );
                MilliSecondsSinceUnixEpoch::now()
            }
        }
    }

    async fn restart_leave(&mut self) {
        let Some(id) = self.delay_id() else {
            self.sched.leave_lost(self.now());
            return;
        };
        match self.transport.restart_leave(&id).await {
            Ok(()) => {
                self.sched.leave_armed(self.now());
                self.succeeded();
            }
            Err(OpError::NotFound(_)) => {
                // The delayed leave is gone: it fired (we were unreachable for
                // longer than the delay) or the server dropped it. Re-arm
                // first, then make sure we are still a member.
                self.set_delay_id(None);
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
                self.failed("restarting the delayed leave", &e);
                self.sched.leave_restart_failed(self.now(), e.retry_after());
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

    /// Run every due action, then wait until the next one, serving a
    /// [`RtcMembership::rejoin`] request if one comes first. A rejoin never
    /// interrupts a request in flight (it waits for the due actions), so it
    /// cannot orphan a delayed leave the server already scheduled. Returns
    /// early with the reason when the transport reports a fatal condition.
    async fn tick(&mut self, rejoins: &mut mpsc::Receiver<RejoinRequest>) -> Option<String> {
        loop {
            if let Some(fatal) = self.transport.fatal() {
                return Some(fatal);
            }
            if !self.step().await {
                break;
            }
        }
        if let Some(fatal) = self.transport.fatal() {
            return Some(fatal);
        }
        tokio::select! {
            biased;
            Some(reply) = rejoins.recv() => {
                // A caller that already gave up (timeout, dropped) is not
                // served late: no surprise rejoin minutes after the ask.
                if !reply.is_closed() {
                    let res = self.rejoin_in_place().await;
                    let _ = reply.send(res);
                }
            }
            _ = tokio::time::sleep_until(self.origin + self.sched.next_wake()) => {}
        }
        None
    }

    /// Leave the call: have the server send the delayed leave now, or send
    /// the empty membership ourselves.
    pub(crate) async fn leave(&mut self) -> Result<()> {
        if let Some(id) = self.take_delay_id() {
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

    /// Hand the membership over to a newer hold: cancel our delayed leave so
    /// it cannot remove the new membership, and leave the state event alone.
    /// A delayed leave we could not cancel stays in the shared slot for the
    /// new hold to cancel.
    async fn hand_over(&mut self) {
        let Some(id) = self.take_delay_id() else {
            return;
        };
        match self.transport.cancel_leave(&id).await {
            Ok(()) | Err(OpError::NotFound(_)) => tracing::info!(
                room_id = %self.room_id,
                "RTC membership handed over to a new hold (delayed leave cancelled)"
            ),
            Err(e) => {
                tracing::warn!(
                    room_id = %self.room_id,
                    error = %e,
                    "cancelling our delayed leave for the hand-over failed; the new hold retries"
                );
                self.set_delay_id(Some(id));
            }
        }
    }

    /// Run until told to leave or hand over (a dropped sender counts as
    /// leave). The command preempts any in-flight request, so a hung
    /// homeserver or token endpoint can never keep a leave from starting.
    pub(crate) async fn run(
        mut self,
        mut cmd: watch::Receiver<Cmd>,
        mut rejoins: mpsc::Receiver<RejoinRequest>,
    ) -> Result<()> {
        let mut fatal = None;
        let why = loop {
            let current = *cmd.borrow_and_update();
            if current != Cmd::Hold {
                break current;
            }
            tokio::select! {
                biased;
                changed = cmd.changed() => {
                    if changed.is_err() {
                        break Cmd::Leave;
                    }
                }
                stop = self.tick(&mut rejoins) => {
                    if let Some(reason) = stop {
                        fatal = Some(reason);
                        break Cmd::Leave;
                    }
                }
            }
        };
        if why == Cmd::HandOver {
            self.hand_over().await;
            return Ok(());
        }
        let left = self.leave().await;
        match fatal {
            Some(reason) => {
                tracing::error!(
                    room_id = %self.room_id,
                    reason = %reason,
                    left = left.is_ok(),
                    "RTC membership keeper stopped: its session can no longer be renewed"
                );
                Err(anyhow!("RTC membership keeper stopped: {reason}"))
            }
            None => left,
        }
    }
}

// ---------------------------------------------------------------------------
// Public handle, and one keeper per (room, user, device)
// ---------------------------------------------------------------------------

/// How long [`RtcMembership::leave`] waits for the leave request(s), and how
/// long a new hold waits for the previous keeper of the same membership.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound of [`RtcMembership::rejoin`]: the keeper may first finish the
/// actions due (each request bounded by [`REQUEST_TIMEOUT`], a token rotation
/// and one retry included), then sends the rejoin.
const REJOIN_TIMEOUT: Duration = Duration::from_secs(60);

/// One [`RtcMembership::rejoin`] request: where the keeper sends the outcome.
type RejoinRequest = oneshot::Sender<Result<RtcRejoined>>;

/// What [`RtcMembership::rejoin`] published: the membership event that now
/// starts the chain, and its `created_ts` (ms since the Unix epoch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtcRejoined {
    pub event_id: String,
    pub created_ts_ms: u64,
}

/// Ask the keeper to leave or hand over, unless it was already told to.
fn request(cmd: &watch::Sender<Cmd>, to: Cmd) {
    cmd.send_if_modified(|c| {
        if *c == Cmd::Hold {
            *c = to;
            true
        } else {
            false
        }
    });
}

/// The registry's view of a running keeper.
struct Slot {
    cmd: Arc<watch::Sender<Cmd>>,
    /// Becomes `true` (or its sender drops) when the keeper task has ended.
    done: watch::Receiver<bool>,
    abort: tokio::task::AbortHandle,
    delay_id: Arc<StdMutex<Option<String>>>,
}

type SlotCell = Arc<tokio::sync::Mutex<Option<Slot>>>;

/// One slot per membership state key (room, user, device), process-wide. A
/// new hold for the same key takes the slot's lock for its whole start, so
/// two holds never run their joins concurrently.
fn slot_for(key: &str) -> SlotCell {
    static HOLDS: OnceLock<StdMutex<HashMap<String, SlotCell>>> = OnceLock::new();
    HOLDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(key.to_owned())
        .or_default()
        .clone()
}

/// Make sure a previous keeper of this membership can no longer clear it:
/// ask it to hand over (or let a leave it already started finish), wait for
/// it, abort it if it does not stop, and cancel any delayed leave it left
/// behind. Our own delayed leave is NOT cancelled by our own new state
/// event, so without this a detached old keeper (or its armed delayed
/// leave) would remove the new call's membership.
async fn supersede<T: MemberTransport>(prev: Slot, transport: &mut T, room_id: &str) {
    request(&prev.cmd, Cmd::HandOver);
    let mut done = prev.done;
    let stopped = tokio::time::timeout(LEAVE_TIMEOUT, done.wait_for(|d| *d))
        .await
        .is_ok();
    if !stopped {
        prev.abort.abort();
        tracing::warn!(
            room_id,
            "previous RTC membership keeper did not stop within {LEAVE_TIMEOUT:?}; aborted it"
        );
    }
    let stale = prev
        .delay_id
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(id) = stale {
        match transport.cancel_leave(&id).await {
            Ok(()) | Err(OpError::NotFound(_)) => {
                tracing::info!(room_id, "cancelled the previous hold's delayed leave")
            }
            Err(e) => tracing::warn!(
                room_id,
                error = %e,
                "could not cancel the previous hold's delayed leave; it may remove this membership once"
            ),
        }
    }
}

/// Start a keeper for membership `key`, first retiring any previous keeper
/// of the same key (see [`supersede`]).
pub(crate) async fn hold_keyed<T: MemberTransport>(
    mut transport: T,
    timing: RtcMemberTiming,
    room_id: String,
    key: String,
) -> Result<RtcMembership> {
    timing.validate()?;
    let cell = slot_for(&key);
    let mut slot = cell.lock().await;
    if let Some(prev) = slot.take() {
        supersede(prev, &mut transport, &room_id).await;
    }
    let delay_id: Arc<StdMutex<Option<String>>> = Arc::default();
    let keeper = Keeper::start_shared(transport, timing, room_id, delay_id.clone()).await?;
    let (membership, new_slot) = RtcMembership::spawn(keeper, delay_id);
    *slot = Some(new_slot);
    Ok(membership)
}

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
///
/// A newer hold of the same room by the same agent device takes the
/// membership over: this handle's keeper then stops without leaving, and a
/// later `leave` or drop of this handle does nothing to the new membership.
pub struct RtcMembership {
    cmd: Arc<watch::Sender<Cmd>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    dead_man_switch: bool,
    /// [`Self::rejoin`] requests to the keeper (one pending at a time).
    rejoin: mpsc::Sender<RejoinRequest>,
}

impl RtcMembership {
    fn spawn<T: MemberTransport>(
        keeper: Keeper<T>,
        delay_id: Arc<StdMutex<Option<String>>>,
    ) -> (Self, Slot) {
        let dead_man_switch = keeper.sched.leave_enabled();
        let (tx, rx) = watch::channel(Cmd::Hold);
        let cmd = Arc::new(tx);
        let (rejoin, rejoins) = mpsc::channel(1);
        let (done_tx, done_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let res = keeper.run(rx, rejoins).await;
            let _ = done_tx.send(true);
            res
        });
        let slot = Slot {
            cmd: cmd.clone(),
            done: done_rx,
            abort: task.abort_handle(),
            delay_id,
        };
        let membership = Self {
            cmd,
            task: Some(task),
            dead_man_switch,
            rejoin,
        };
        (membership, slot)
    }

    /// Rejoin the call IN PLACE: the running keeper re-publishes this
    /// membership with a fresh `created_ts` (see the module docs, "Rejoin in
    /// place"), so every Element Call peer reads a new joiner and re-sends
    /// its media key. Element Call shares keys per `(user, device,
    /// createdTs())` and Synapse drops identical state content, so neither a
    /// plain re-send nor a refresh can do this.
    ///
    /// The membership is never emptied, the delayed leave stays armed, and
    /// the keeper's later hourly refreshes carry the new `created_ts`. A
    /// failed rejoin changes nothing (the old chain keeps being refreshed).
    /// Inert unless called. The returned future owns what it needs (it can
    /// be spawned) and is bounded by 60 s; the keeper serves the request
    /// after the actions already due, and drops it unserved if this future
    /// was dropped first. Errors when a rejoin is already pending or the
    /// keeper has stopped (left, handed over, or its session expired).
    pub fn rejoin(&self) -> impl Future<Output = Result<RtcRejoined>> + Send + 'static {
        let tx = self.rejoin.clone();
        async move {
            let (reply, answer) = oneshot::channel();
            tx.try_send(reply).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    anyhow!("a rejoin of this RTC membership is already pending")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow!("the RTC membership keeper has stopped; nothing to rejoin")
                }
            })?;
            match tokio::time::timeout(REJOIN_TIMEOUT, answer).await {
                Ok(Ok(res)) => res,
                Ok(Err(_)) => Err(anyhow!(
                    "the RTC membership keeper stopped before the rejoin ran"
                )),
                Err(_) => Err(anyhow!("rejoining the call timed out after {REJOIN_TIMEOUT:?}")),
            }
        }
    }

    /// Whether a delayed leave (MSC4140) protects this membership. `false`
    /// when the homeserver refused it at join time.
    pub fn has_dead_man_switch(&self) -> bool {
        self.dead_man_switch
    }

    /// Whether the keeper task is still running. It stops on leave, on a
    /// hand-over to a newer hold, and when its session can no longer be
    /// renewed (then [`leave`](Self::leave) returns that error).
    pub fn is_active(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stop refreshing and leave the call. Bounded by 30 s.
    pub async fn leave(mut self) -> Result<()> {
        request(&self.cmd, Cmd::Leave);
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
        request(&self.cmd, Cmd::Leave);
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
    /// disappears mid-call (the delayed leave fired during an outage) or
    /// lapses, it is re-published as a new membership. See the module docs
    /// for the matrix-js-sdk behaviour this mirrors. [`RtcMembership::rejoin`]
    /// does the same on demand while the membership is live (peers re-send
    /// their media keys).
    ///
    /// A previous hold of the same room by this agent device is retired
    /// first (it hands over; its delayed leave is cancelled), so an old
    /// call's keeper cannot clear the new call's membership.
    ///
    /// The keeper does NOT use this client after the call returns: it copies
    /// the current access token and runs on its own token-only REST session
    /// ([`RestSession`]), never a matrix-sdk `Client`, so it cannot open a
    /// second OlmMachine on the shared crypto store. It rotates that token
    /// itself, so the caller's client may go stale meanwhile.
    pub async fn hold_rtc_member(
        &self,
        room_id: &str,
        livekit_alias: &str,
        livekit_service_url: &str,
        timing: RtcMemberTiming,
    ) -> Result<RtcMembership> {
        let session = RestSession::from_agent(self)?;
        hold_with_session(session, room_id, livekit_alias, livekit_service_url, timing).await
    }
}

/// [`AgentClient::hold_rtc_member`] on an explicit [`RestSession`] (the seam
/// the socket-level test drives).
pub(crate) async fn hold_with_session(
    session: RestSession,
    room_id: &str,
    livekit_alias: &str,
    livekit_service_url: &str,
    timing: RtcMemberTiming,
) -> Result<RtcMembership> {
    let key = format!("{room_id}|{}|{}", session.user_id, session.device_id);
    let transport = LiveTransport::new(session, room_id, livekit_alias, livekit_service_url)?;
    hold_keyed(transport, timing, room_id.to_owned(), key).await
}

// ---------------------------------------------------------------------------
// Token-only REST session (no matrix-sdk Client, no crypto store)
// ---------------------------------------------------------------------------

/// Per-request timeout of the keeper's HTTP client. Shorter than
/// [`LEAVE_TIMEOUT`] so a hung request cannot eat the whole hang-up budget,
/// and long enough for a slow homeserver.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The keeper's own homeserver session: a bearer token plus a plain HTTP
/// client.
///
/// Deliberately NOT a matrix-sdk `Client`. Every `Client` this crate builds
/// (`build_and_restore`) opens its own OlmMachine over the agent's shared
/// SQLite crypto store, and two live OlmMachines on one store is how the
/// Scribe went one-way Olm-deaf (2026-09-15, root-caused 2026-09-27: two
/// in-memory Olm accounts minted different one-time keys under one id).
/// `AgentClient::reauth_token_only` builds exactly such a `Client` on every
/// rotation, and this keeper rotates for the whole length of a call. Nothing
/// it sends needs E2EE (state events, `/event`, `/state`, and the MSC4140
/// delayed-event endpoints are plain authenticated REST), so it mints tokens
/// with [`mint_session_token`] (siwx-oidc only, no `Client`) and sends ruma
/// requests over reqwest. It never syncs, so it cannot consume to-device
/// messages either.
pub(crate) struct RestSession {
    http: reqwest::Client,
    /// Homeserver base URL without a trailing slash.
    homeserver: String,
    access_token: String,
    expires_at_unix: u64,
    user_id: OwnedUserId,
    device_id: String,
    /// What [`mint_session_token`] needs (key file, siwx-oidc, `config.toml`).
    config: AgentConfig,
    /// Upper bound of one token rotation (lock wait, grant, persist).
    rotate_timeout: Duration,
    /// Set when a rotation minted (or would have used) another identity:
    /// no further rotation is attempted, and the keeper stops.
    poisoned: Option<String>,
    /// Consecutive failed proactive rotations (log rate limiting).
    rotate_failures: u32,
}

impl RestSession {
    /// Snapshot `agent`'s current token and identity.
    fn from_agent(agent: &AgentClient) -> Result<Self> {
        let access_token = agent
            .client()
            .access_token()
            .ok_or_else(|| anyhow!("agent has no access token; cannot set RTC membership"))?;
        let device_id = agent
            .device_id()
            .ok_or_else(|| anyhow!("agent has no device_id; cannot set RTC membership"))?;
        Self::new(
            agent.config.clone(),
            access_token,
            agent.expires_at_unix,
            agent.user_id.clone(),
            device_id,
        )
    }

    pub(crate) fn new(
        config: AgentConfig,
        access_token: String,
        expires_at_unix: u64,
        user_id: OwnedUserId,
        device_id: String,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build the RTC membership HTTP client")?;
        Ok(Self {
            http,
            homeserver: config.matrix_url.trim_end_matches('/').to_owned(),
            access_token,
            expires_at_unix,
            user_id,
            device_id,
            config,
            rotate_timeout: REQUEST_TIMEOUT,
            poisoned: None,
            rotate_failures: 0,
        })
    }

    fn needs_rotation(&self) -> bool {
        self.expires_at_unix.saturating_sub(unix_now()) < TOKEN_REFRESH_MARGIN
    }

    /// Mint a fresh token for the SAME user and device. A session for
    /// another identity is refused before it is persisted
    /// ([`mint_session_token`] with the expected identity); after that the
    /// session is poisoned: no further rotation (and so no fresh login) is
    /// attempted and the keeper stops, because the membership names this
    /// device and switching devices mid-call is the caller's decision.
    ///
    /// The rotation runs in its own task, bounded by `rotate_timeout`: a
    /// keeper stopped mid-rotation (leave, drop) does not cancel a refresh
    /// grant between spending the refresh token and persisting the new one,
    /// and a hung siwx-oidc cannot stall the keeper or hold the store's
    /// session lock for longer than the bound.
    async fn rotate(&mut self) -> Result<()> {
        if let Some(why) = &self.poisoned {
            return Err(anyhow!("token rotation stopped: {why}"));
        }
        let config = self.config.clone();
        let (user, device) = (self.user_id.to_string(), self.device_id.clone());
        let limit = self.rotate_timeout;
        let minted = tokio::spawn(async move {
            let expect = Some((user.as_str(), device.as_str()));
            tokio::time::timeout(limit, mint_session_token(&config, expect)).await
        })
        .await;
        let minted = match minted {
            Ok(Ok(res)) => res,
            Ok(Err(_elapsed)) => Err(anyhow!("token rotation timed out after {limit:?}")),
            Err(join) => Err(anyhow!("token rotation task failed: {join}")),
        };
        match minted {
            Ok((token, user_id, device_id, expires_at_unix)) => {
                if user_id != self.user_id.as_str() || device_id != self.device_id {
                    // mint_session_token already refuses this; kept as a
                    // guard should that ever change.
                    let why = format!(
                        "token rotation returned {user_id}/{device_id}, expected {}/{}",
                        self.user_id, self.device_id
                    );
                    self.poisoned = Some(why.clone());
                    return Err(anyhow!(why));
                }
                self.access_token = token;
                self.expires_at_unix = expires_at_unix;
                self.rotate_failures = 0;
                tracing::debug!(
                    valid_for_s = expires_at_unix.saturating_sub(unix_now()),
                    "RTC membership keeper: access token rotated (no client rebuilt)"
                );
                Ok(())
            }
            Err(e) => {
                if e.chain()
                    .any(|c| c.downcast_ref::<SessionIdentityMismatch>().is_some())
                {
                    tracing::error!(
                        error = %format!("{e:#}"),
                        "RTC membership keeper: token rotation is for another identity; no further rotation"
                    );
                    self.poisoned = Some(format!("{e:#}"));
                }
                Err(e)
            }
        }
    }

    fn state_key(&self, underscore: bool) -> CallMemberStateKey {
        rtc_member_state_key_for(self.user_id.clone(), &self.device_id, underscore)
    }

    /// Send one client-server API request with the current token. The error
    /// carries whether the server rejected the token.
    async fn send<R>(&self, request: R) -> std::result::Result<R::IncomingResponse, (bool, OpError)>
    where
        R: OutgoingRequest<PathBuilder = VersionHistory, EndpointError = RumaApiError>,
        for<'a> R::Authentication: AuthScheme<Input<'a> = SendAccessToken<'a>>,
    {
        let other = |e: anyhow::Error| (false, OpError::Other(e));
        let versions = SupportedVersions {
            versions: [MatrixVersion::V1_1].into(),
            features: Default::default(),
        };
        let http_request: http::Request<Vec<u8>> = request
            .try_into_http_request(
                &self.homeserver,
                SendAccessToken::IfRequired(&self.access_token),
                Cow::Owned(versions),
            )
            .map_err(|e| other(anyhow!("build request: {e}")))?;
        let request = reqwest::Request::try_from(http_request)
            .map_err(|e| other(anyhow!("convert request: {e}")))?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| other(anyhow::Error::new(e).context("homeserver request failed")))?;
        // reqwest 0.12 and ruma share the `http` 1.x types.
        let mut builder = http::Response::builder().status(response.status());
        if let Some(headers) = builder.headers_mut() {
            *headers = response.headers().clone();
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| other(anyhow::Error::new(e).context("reading response body")))?;
        let http_response = builder
            .body(body)
            .map_err(|e| other(anyhow!("response: {e}")))?;
        R::IncomingResponse::try_from_http_response(http_response).map_err(|e| match e {
            FromHttpResponseError::Server(err) => {
                let kind = err.error_kind();
                let status = Some(err.status_code.as_u16());
                let (token, class) = (is_token_rejection(kind, status), classify(kind, status));
                let wait = retry_after_of(kind);
                (token, op_error(class, anyhow::Error::new(err), wait))
            }
            other_err => other(anyhow!("homeserver response: {other_err}")),
        })
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
    RateLimited,
    Unsupported,
    Other,
}

/// Classify a homeserver error for the keeper.
fn classify(kind: Option<&ErrorKind>, status: Option<u16>) -> ErrClass {
    match (kind, status) {
        (Some(ErrorKind::NotFound), _) => ErrClass::NotFound,
        (Some(ErrorKind::LimitExceeded(_)), _) | (_, Some(429)) => ErrClass::RateLimited,
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

/// The server's `retry_after` (`retry_after_ms` in the body, or the
/// `Retry-After` header, which ruma prefers) of an `M_LIMIT_EXCEEDED`.
fn retry_after_of(kind: Option<&ErrorKind>) -> Option<Duration> {
    use matrix_sdk::ruma::api::error::RetryAfter;
    match kind? {
        ErrorKind::LimitExceeded(data) => match data.retry_after? {
            RetryAfter::Delay(d) => Some(d),
            RetryAfter::DateTime(at) => Some(
                at.duration_since(std::time::SystemTime::now())
                    .unwrap_or_default(),
            ),
        },
        _ => None,
    }
}

fn op_error(class: ErrClass, err: anyhow::Error, retry_after: Option<Duration>) -> OpError {
    match class {
        ErrClass::NotFound => OpError::NotFound(err),
        ErrClass::RateLimited => OpError::RateLimited { err, retry_after },
        ErrClass::Unsupported => OpError::Unsupported(err),
        ErrClass::Other => OpError::Other(err),
    }
}

/// Run `$op` (an expression using `$s: &mut LiveTransport` that yields
/// `Result<T, (bool /*token rejected*/, OpError)>`) with a proactive token
/// rotation before and one rotate-and-retry on a token rejection.
macro_rules! with_token {
    ($s:ident, $op:expr) => {{
        $s.ensure_fresh().await;
        match $op {
            Ok(v) => Ok(v),
            Err((true, first)) => {
                tracing::warn!(error = %first, "RTC membership request rejected (token); rotating");
                match $s.session.rotate().await {
                    Ok(()) => $op.map_err(|(_, e)| e),
                    Err(e) => Err(OpError::Other(e.context("token refresh failed"))),
                }
            }
            Err((false, e)) => Err(e),
        }
    }};
}

pub(crate) struct LiveTransport {
    session: RestSession,
    room_id: OwnedRoomId,
    alias: String,
    service_url: String,
    /// Owned (MSC3757) or plain key, whichever the first send got accepted.
    state_key: Option<CallMemberStateKey>,
}

impl LiveTransport {
    fn new(session: RestSession, room_id: &str, alias: &str, service_url: &str) -> Result<Self> {
        let room_id: OwnedRoomId = room_id
            .try_into()
            .map_err(|e| anyhow!("invalid room_id: {e}"))?;
        Ok(Self {
            session,
            room_id,
            alias: alias.to_owned(),
            service_url: service_url.to_owned(),
            state_key: None,
        })
    }

    async fn ensure_fresh(&mut self) {
        if self.session.poisoned.is_some() || !self.session.needs_rotation() {
            return;
        }
        if let Err(e) = self.session.rotate().await {
            self.session.rotate_failures += 1;
            let n = self.session.rotate_failures;
            if n == 1 || n.is_multiple_of(WARN_EVERY) {
                tracing::warn!(error = %format!("{e:#}"), consecutive_failures = n, "RTC membership keeper: proactive token rotation failed");
            } else {
                tracing::debug!(error = %format!("{e:#}"), consecutive_failures = n, "RTC membership keeper: proactive token rotation failed");
            }
        }
    }

    fn key(&self) -> CallMemberStateKey {
        match &self.state_key {
            Some(k) => k.clone(),
            None => self.session.state_key(true),
        }
    }

    async fn put_member(
        &self,
        key: &CallMemberStateKey,
        content: CallMemberEventContent,
    ) -> std::result::Result<OwnedEventId, (bool, OpError)> {
        let req = send_state_event::v3::Request::new(self.room_id.clone(), key, &content)
            .map_err(|e| (false, OpError::Other(anyhow!("serialize call.member: {e}"))))?;
        self.session.send(req).await.map(|r| r.event_id)
    }

    async fn send_member_once(
        &mut self,
        created_ts: Option<MilliSecondsSinceUnixEpoch>,
        expires: Duration,
    ) -> std::result::Result<OwnedEventId, (bool, OpError)> {
        let content = rtc_member_content(
            &self.session.device_id,
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
        let owned = self.session.state_key(true);
        match self.put_member(&owned, content.clone()).await {
            Ok(eid) => {
                tracing::info!(
                    state_key = owned.as_ref(),
                    "RTC membership published (owned state key)"
                );
                self.state_key = Some(owned);
                Ok(eid)
            }
            // Only a policy refusal of the owned key (M_FORBIDDEN, no MSC3757)
            // means "try the plain key". An outage, a 5xx or a rate limit
            // must not flip the whole hold onto the legacy key.
            Err((false, e)) if e.is_forbidden() => {
                tracing::warn!(error = %e, "owned RTC member state key rejected; retrying unprefixed");
                let plain = self.session.state_key(false);
                let eid = self.put_member(&plain, content).await?;
                tracing::info!(
                    state_key = plain.as_ref(),
                    "RTC membership published (unprefixed state key)"
                );
                self.state_key = Some(plain);
                Ok(eid)
            }
            Err(other) => Err(other),
        }
    }

    async fn origin_ts_once(
        &self,
        event_id: &OwnedEventId,
    ) -> std::result::Result<MilliSecondsSinceUnixEpoch, (bool, OpError)> {
        let req = get_room_event::v3::Request::new(self.room_id.clone(), event_id.clone());
        let resp = self.session.send(req).await?;
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
        let req = get_state_event_for_key::v3::Request::new(
            self.room_id.clone(),
            StateEventType::CallMember,
            self.key().as_ref().to_owned(),
        );
        match self.session.send(req).await {
            Ok(resp) => {
                let v: serde_json::Value = serde_json::from_str(resp.event_or_content.get())
                    .map_err(|e| (false, OpError::Other(anyhow!("state JSON: {e}"))))?;
                Ok(v.as_object().is_some_and(|o| !o.is_empty()))
            }
            Err((_, OpError::NotFound(_))) => Ok(false),
            Err(other) => Err(other),
        }
    }

    async fn schedule_leave_once(
        &self,
        delay: Duration,
    ) -> std::result::Result<String, (bool, OpError)> {
        let req = delayed_state_event::unstable::Request::new(
            self.room_id.clone(),
            self.key().as_ref().to_owned(),
            DelayParameters::Timeout { timeout: delay },
            &CallMemberEventContent::new_empty(None),
        )
        .map_err(|e| {
            (
                false,
                OpError::Other(anyhow!("serialize delayed leave: {e}")),
            )
        })?;
        self.session.send(req).await.map(|r| r.delay_id)
    }

    async fn update_delayed_once(
        &self,
        delay_id: &str,
        action: update_delayed_event::unstable::UpdateAction,
    ) -> std::result::Result<(), (bool, OpError)> {
        let req = update_delayed_event::unstable::Request::new(delay_id.to_owned(), action);
        self.session.send(req).await.map(|_| ())
    }

    async fn clear_once(&self) -> std::result::Result<(), (bool, OpError)> {
        let key = self.key();
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

    async fn cancel_leave(&mut self, delay_id: &str) -> OpResult<()> {
        use update_delayed_event::unstable::UpdateAction;
        with_token!(
            self,
            self.update_delayed_once(delay_id, UpdateAction::Cancel)
                .await
        )
    }

    fn fatal(&self) -> Option<String> {
        self.session.poisoned.clone()
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
        sched.leave_restart_failed(Duration::from_secs(20), None);
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
            sched.refresh_failed(now, None);
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
        Cancel(String),
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
        /// Membership sends in `[from, until)` fail.
        fail_member_between: Option<(Duration, Duration)>,
        /// Membership sends from this time on never answer.
        hang_member_from: Option<Duration>,
        /// The first restart at or after `.0` answers 429 with retry_after `.1`.
        rate_limit_restart_at: Option<(Duration, Duration)>,
        /// From this time on the transport reports a fatal condition.
        fatal_from: Option<Duration>,
        /// `send_leave_now` takes this long; it is recorded when it completes.
        leave_now_delay: Duration,
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
            let hang = {
                let mut f = self.state.lock().unwrap();
                f.calls.push(Call::Member {
                    at,
                    created_ts: created_ts.map(|t| u64::from(t.0)),
                    expires,
                });
                let in_window = f
                    .fail_member_between
                    .is_some_and(|(from, until)| at >= from && at < until);
                if f.fail_join || at < f.fail_member_until || in_window {
                    return Err(other("simulated 502"));
                }
                f.hang_member_from.is_some_and(|t| at >= t)
            };
            if hang {
                return std::future::pending().await;
            }
            let mut f = self.state.lock().unwrap();
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
            if let Some((from, wait)) = f.rate_limit_restart_at {
                if at >= from {
                    f.rate_limit_restart_at = None;
                    return Err(OpError::RateLimited {
                        err: anyhow!("M_LIMIT_EXCEEDED"),
                        retry_after: Some(wait),
                    });
                }
            }
            if f.lose_leave_at.is_some_and(|t| at >= t) {
                f.lose_leave_at = None;
                return Err(OpError::NotFound(anyhow!("M_NOT_FOUND")));
            }
            Ok(())
        }

        async fn send_leave_now(&mut self, _delay_id: &str) -> OpResult<()> {
            let delay = self.state.lock().unwrap().leave_now_delay;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
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

        async fn cancel_leave(&mut self, delay_id: &str) -> OpResult<()> {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(Call::Cancel(delay_id.to_owned()));
            Ok(())
        }

        fn fatal(&self) -> Option<String> {
            let from = self.state.lock().unwrap().fatal_from?;
            (self.at() >= from).then(|| "simulated identity mismatch".to_owned())
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

    /// A registry key no other test uses (tests share the process-wide
    /// registry).
    fn unique_key() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "!room:example.org|test|{}",
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    async fn hold(fake: Fake) -> (RtcMembership, Arc<Mutex<Fake>>) {
        let (t, state) = FakeTransport::new(fake);
        let m = hold_keyed(t, timing(), "!room:example.org".into(), unique_key())
            .await
            .expect("join");
        (m, state)
    }

    /// KEYLOSS-1 F1: a rejoin in the FIRST hour (before any refresh, when
    /// the server still holds the join's content) is a real new event: the
    /// running keeper sends the membership with a fresh created_ts (anchor +
    /// time since the join), adopts it for every later hourly refresh (no
    /// flip back), and the delayed leave stays armed throughout: never
    /// cancelled, never re-scheduled, restarted on its 15 s cadence, no
    /// empty membership at any point.
    #[tokio::test(start_paused = true)]
    async fn rejoin_republishes_in_place_with_a_new_created_ts_that_refreshes_keep() {
        let (m, state) = hold(Fake {
            msc4140: true,
            ..Default::default()
        })
        .await;
        tokio::time::sleep(30 * 60 * Duration::from_secs(1)).await;
        let r = m.rejoin().await.expect("rejoin");
        tokio::time::sleep(2 * H + Duration::from_secs(1)).await;
        assert!(m.is_active(), "the same keeper still holds");
        m.leave().await.unwrap();

        let sends = members(&state);
        assert_eq!(sends.len(), 4, "join, rejoin, 2 hourly refreshes: {sends:?}");
        let (join_at, join_created, join_expires) = sends[0];
        assert_eq!((join_created, join_expires), (None, 4 * H), "the join");
        let anchor = SERVER_EPOCH_MS + join_at.as_millis() as u64;
        let (rejoin_at, rejoin_created, rejoin_expires) = sends[1];
        assert_eq!(rejoin_at, join_at + 30 * 60 * Duration::from_secs(1));
        let fresh = anchor + (rejoin_at - join_at).as_millis() as u64;
        assert_eq!(rejoin_created, Some(fresh), "server-anchored now, not the old anchor");
        assert!(fresh > anchor);
        assert_eq!(rejoin_expires, 4 * H, "a new chain: valid 4 h from its created_ts");
        assert_eq!(r.created_ts_ms, fresh);
        for (k, (at, created, expires)) in sends.iter().enumerate().skip(2) {
            let k = k as u32 - 1;
            assert_eq!(*created, Some(fresh), "refresh {k} keeps the NEW created_ts");
            assert_eq!(*at, rejoin_at + H * k, "hourly from the rejoin");
            assert_eq!(*expires, H * k + 4 * H, "relative to the new chain");
        }
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 1, "armed once");
        assert_eq!(count(&state, |c| matches!(c, Call::Cancel(_))), 0, "never cancelled");
        assert_eq!(count(&state, |c| *c == Call::Clear), 0, "never emptied");
        assert_eq!(count(&state, |c| *c == Call::IsLive), 0);
        let total = rejoin_at + 2 * H + Duration::from_secs(1);
        let restarts = count(&state, |c| matches!(c, Call::Restart { .. }));
        assert_eq!(restarts as u64, total.as_secs() / 15, "the dead-man switch never paused");
        let calls = state.lock().unwrap().calls.clone();
        assert_eq!(calls.last(), Some(&Call::LeaveNow), "the leave is the only removal");
    }

    /// Two rejoins: each takes a newer created_ts than the last.
    #[tokio::test(start_paused = true)]
    async fn consecutive_rejoins_each_take_a_newer_created_ts() {
        let (m, state) = hold(Fake {
            msc4140: true,
            ..Default::default()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let a = m.rejoin().await.expect("first");
        tokio::time::sleep(Duration::from_secs(15)).await;
        let b = m.rejoin().await.expect("second");
        m.leave().await.unwrap();
        assert!(b.created_ts_ms > a.created_ts_ms);
        assert_eq!(b.created_ts_ms - a.created_ts_ms, 15_000);
        let created: Vec<_> = members(&state).into_iter().map(|(_, c, _)| c).collect();
        assert_eq!(created, vec![None, Some(a.created_ts_ms), Some(b.created_ts_ms)]);
    }

    /// A failed rejoin changes nothing: the error comes back, the old chain
    /// (old created_ts) is what the next refresh continues, nothing emptied.
    #[tokio::test(start_paused = true)]
    async fn a_failed_rejoin_keeps_the_old_chain() {
        let (m, state) = hold(Fake {
            msc4140: true,
            fail_member_between: Some((Duration::from_secs(60), Duration::from_secs(61))),
            ..Default::default()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(m.rejoin().await.is_err(), "the 502 is reported");
        tokio::time::sleep(H).await;
        m.leave().await.unwrap();
        let sends = members(&state);
        let anchor = SERVER_EPOCH_MS + sends[0].0.as_millis() as u64;
        assert_eq!(sends.len(), 3, "join, failed rejoin, refresh: {sends:?}");
        assert_eq!(sends[2].1, Some(anchor), "the refresh continues the OLD chain");
        assert_eq!(sends[2].0, sends[0].0 + H, "on the original schedule");
        assert_eq!(count(&state, |c| *c == Call::Clear), 0);
        assert_eq!(count(&state, |c| matches!(c, Call::Schedule { .. })), 1);
    }

    /// A rejoin of a keeper that already stopped is refused and sends nothing.
    #[tokio::test(start_paused = true)]
    async fn a_rejoin_after_leave_is_refused_and_sends_nothing() {
        let (m, state) = hold(Fake {
            msc4140: true,
            ..Default::default()
        })
        .await;
        let late = m.rejoin();
        m.leave().await.unwrap();
        let err = late.await.expect_err("nothing to rejoin");
        assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        assert_eq!(members(&state).len(), 1, "only the join was ever sent");
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
        let (m, _slot) = RtcMembership::spawn(keeper, Arc::default());
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

    /// M1: after our delayed leave fired, a FAILED rejoin must be retried as a
    /// rejoin (new chain). Re-anchoring the old join's created_ts would read
    /// to Element as "keys already shared" after a leave.
    #[tokio::test(start_paused = true)]
    async fn a_failed_rejoin_is_retried_as_a_rejoin_not_a_refresh() {
        let (m, state) = hold(Fake {
            msc4140: true,
            lose_leave_at: Some(Duration::from_secs(300)),
            member_live_after_loss: false,
            fail_member_between: Some((Duration::from_secs(300), Duration::from_secs(305))),
            ..Default::default()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(400)).await;
        m.leave().await.unwrap();
        let sends = members(&state);
        assert_eq!(
            sends.len(),
            3,
            "join, failed rejoin, retried rejoin: {sends:?}"
        );
        assert_eq!(sends[1].0, Duration::from_secs(300));
        assert_eq!(
            sends[2].0,
            Duration::from_secs(310),
            "retried after the back-off"
        );
        assert_eq!(sends[2].1, None, "the retry starts a new chain: {sends:?}");
    }

    /// minor 3: a membership that lapsed (every refresh failed until past its
    /// expiry) is gone for Element; re-sending the old created_ts would read
    /// as the same chain. It must come back as a new chain.
    #[tokio::test(start_paused = true)]
    async fn a_lapsed_membership_rejoins_instead_of_refreshing() {
        let (m, state) = hold(Fake {
            fail_member_between: Some((Duration::from_secs(1), 4 * H + Duration::from_secs(30))),
            ..Default::default()
        })
        .await;
        assert!(!m.has_dead_man_switch());
        tokio::time::sleep(4 * H + Duration::from_secs(60)).await;
        m.leave().await.unwrap();
        let sends = members(&state);
        let first_ok = sends
            .iter()
            .find(|(at, _, _)| *at >= 4 * H + Duration::from_secs(30))
            .expect("a send after the outage");
        assert_eq!(first_ok.1, None, "lapsed chain restarts: {first_ok:?}");
        assert_eq!(first_ok.2, 4 * H, "a fresh chain's expiry");
    }

    /// M2: leave() must stop the keeper even while one of its requests hangs.
    #[tokio::test(start_paused = true)]
    async fn leave_preempts_a_hung_request() {
        let (m, state) = hold(Fake {
            msc4140: true,
            hang_member_from: Some(Duration::from_secs(1)),
            ..Default::default()
        })
        .await;
        // The hourly refresh at 1 h hangs forever.
        tokio::time::sleep(H + Duration::from_secs(5)).await;
        let started = Instant::now();
        m.leave()
            .await
            .expect("leave must not wait for the hung refresh");
        assert!(started.elapsed() < Duration::from_secs(1));
        let calls = state.lock().unwrap().calls.clone();
        assert_eq!(calls.last(), Some(&Call::LeaveNow));
    }

    /// minor 4: a 429 on a restart waits the server's retry_after (45 s),
    /// not our own 10 s back-off.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_restart_waits_for_retry_after() {
        let (m, state) = hold(Fake {
            msc4140: true,
            rate_limit_restart_at: Some((Duration::from_secs(90), Duration::from_secs(45))),
            ..Default::default()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(200)).await;
        m.leave().await.unwrap();
        let restarts: Vec<Duration> = state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Restart { at } => Some(*at),
                _ => None,
            })
            .collect();
        let limited = restarts
            .iter()
            .position(|t| *t == Duration::from_secs(90))
            .expect("restart at 90 s");
        assert_eq!(
            restarts[limited + 1],
            Duration::from_secs(135),
            "next restart honours retry_after: {restarts:?}"
        );
    }

    #[test]
    fn rate_limits_are_classified_with_their_retry_after() {
        use matrix_sdk::ruma::api::error::{LimitExceededErrorData, RetryAfter};
        let mut data = LimitExceededErrorData::new();
        data.retry_after = Some(RetryAfter::Delay(Duration::from_millis(2500)));
        let kind = ErrorKind::LimitExceeded(data);
        assert_eq!(classify(Some(&kind), Some(429)), ErrClass::RateLimited);
        assert_eq!(classify(None, Some(429)), ErrClass::RateLimited);
        assert_eq!(
            retry_after_of(Some(&kind)),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(retry_after_of(Some(&ErrorKind::Unknown)), None);
        assert_eq!(
            back_off(Duration::from_secs(10), Some(Duration::from_secs(3))),
            Duration::from_secs(10)
        );
        assert_eq!(
            back_off(Duration::from_secs(10), Some(Duration::from_secs(30))),
            Duration::from_secs(30)
        );
    }

    /// minor 1: once the session cannot be renewed for our identity, the
    /// keeper stops (no retry loop), and the handle surfaces why.
    #[tokio::test(start_paused = true)]
    async fn a_fatal_session_error_stops_the_keeper_and_is_surfaced() {
        let (m, state) = hold(Fake {
            msc4140: true,
            fatal_from: Some(Duration::from_secs(100)),
            ..Default::default()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(200)).await;
        assert!(!m.is_active(), "the keeper stopped");
        let calls = state.lock().unwrap().calls.clone();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::Restart { at } if *at > Duration::from_secs(100))),
            "no retries after the fatal condition"
        );
        assert_eq!(calls.last(), Some(&Call::LeaveNow), "best-effort leave");
        let err = m.leave().await.expect_err("the stop is surfaced");
        assert!(format!("{err:#}").contains("identity mismatch"), "{err:#}");
    }

    /// minor 2: a new hold of the same membership takes over. The old keeper
    /// cancels its delayed leave and stops without leaving, and dropping its
    /// handle afterwards does nothing to the new membership.
    #[tokio::test(start_paused = true)]
    async fn a_new_hold_takes_over_and_the_old_handle_cannot_clear_it() {
        let key = unique_key();
        let (t, state) = FakeTransport::new(Fake {
            msc4140: true,
            ..Default::default()
        });
        let old = hold_keyed(t.clone(), timing(), "!room:example.org".into(), key.clone())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        let new = hold_keyed(t, timing(), "!room:example.org".into(), key)
            .await
            .unwrap();
        assert!(!old.is_active(), "the old keeper handed over");
        drop(old);
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert!(new.is_active());
        let calls = state.lock().unwrap().calls.clone();
        let new_join = calls
            .iter()
            .position(|c| matches!(c, Call::Member { created_ts: None, at, .. } if *at == Duration::from_secs(60)))
            .expect("second join");
        let cancel = calls
            .iter()
            .position(|c| *c == Call::Cancel("syd_1".into()))
            .expect("old delayed leave cancelled");
        assert!(cancel < new_join, "cancelled before the new join");
        assert!(
            !calls[new_join..]
                .iter()
                .any(|c| matches!(c, Call::LeaveNow | Call::Clear)),
            "nothing cleared the new membership: {:?}",
            &calls[new_join..]
        );
        new.leave().await.unwrap();
        assert_eq!(
            count(&state, |c| *c == Call::LeaveNow),
            1,
            "only the new hold left"
        );
    }

    /// minor 2: an old keeper already leaving (handle dropped) finishes its
    /// leave BEFORE the new hold joins, so the leave cannot land on top of
    /// the new membership.
    #[tokio::test(start_paused = true)]
    async fn a_new_hold_waits_for_a_previous_leave_in_flight() {
        let key = unique_key();
        let (t, state) = FakeTransport::new(Fake {
            msc4140: true,
            leave_now_delay: Duration::from_secs(5),
            ..Default::default()
        });
        let old = hold_keyed(t.clone(), timing(), "!room:example.org".into(), key.clone())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(20)).await;
        drop(old);
        tokio::task::yield_now().await;
        let new = hold_keyed(t, timing(), "!room:example.org".into(), key)
            .await
            .unwrap();
        let calls = state.lock().unwrap().calls.clone();
        let left = calls.iter().position(|c| *c == Call::LeaveNow).unwrap();
        let joined = calls
            .iter()
            .rposition(|c| {
                matches!(
                    c,
                    Call::Member {
                        created_ts: None,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(
            left < joined,
            "old leave completed before the new join: {calls:?}"
        );
        drop(new);
    }

    // ---- live transport over a real socket: no Client, no crypto store -----

    /// One recorded HTTP request: (method, path with query, bearer, body).
    type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;

    /// Knobs of [`fake_homeserver_with`]. The same socket serves siwx-oidc.
    #[derive(Clone, Default)]
    struct Hs {
        /// While set, requests carrying `Bearer tok0` get 401 M_UNKNOWN_TOKEN.
        revoked: Arc<std::sync::atomic::AtomicBool>,
        /// Answer for a membership PUT on the owned (`_@...`) state key.
        owned_key_error: Option<(&'static str, &'static str)>,
        /// siwx-oidc (any non-`/_matrix/` path) accepts and never answers.
        siwx_hangs: bool,
        /// Body of a 200 from siwx-oidc `/token`.
        token_json: Option<String>,
    }

    /// A minimal homeserver on 127.0.0.1 that answers the keeper's endpoints
    /// and 404s everything else (so token rotation against it fails).
    async fn fake_homeserver() -> (String, Seen) {
        fake_homeserver_with(Hs::default()).await
    }

    async fn fake_homeserver_with(hs: Hs) -> (String, Seen) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let log = log.clone();
                let hs = hs.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let header = |name: &str| {
                        head.lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                            })
                            .unwrap_or_default()
                    };
                    let len: usize = header("content-length").parse().unwrap_or(0);
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let method = first.next().unwrap_or("").to_owned();
                    let path = first.next().unwrap_or("").to_owned();
                    let bearer = header("authorization");
                    log.lock().unwrap().push((
                        method.clone(),
                        path.clone(),
                        bearer.clone(),
                        body.clone(),
                    ));
                    let matrix = path.starts_with("/_matrix/");
                    if !matrix && hs.siwx_hangs {
                        std::future::pending::<()>().await;
                    }
                    let owned_put = method == "PUT"
                        && path.contains("/state/org.matrix.msc3401.call.member/_@")
                        && !path.contains("msc4140.delay=");
                    let (status, json) = if matrix
                        && bearer == "Bearer tok0"
                        && hs.revoked.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        (
                            "401 Unauthorized",
                            r#"{"errcode":"M_UNKNOWN_TOKEN","error":"revoked"}"#.to_owned(),
                        )
                    } else if let (true, Some((status, json))) = (owned_put, hs.owned_key_error) {
                        (status, json.to_owned())
                    } else if !matrix && path.starts_with("/token") && hs.token_json.is_some() {
                        ("200 OK", hs.token_json.clone().unwrap())
                    } else if method == "PUT"
                        && path.contains("/state/org.matrix.msc3401.call.member/")
                    {
                        if path.contains("org.matrix.msc4140.delay=") {
                            ("200 OK", r#"{"delay_id":"syd_1"}"#.to_owned())
                        } else {
                            ("200 OK", r#"{"event_id":"$join:example.org"}"#.to_owned())
                        }
                    } else if method == "GET" && path.contains("/event/") {
                        (
                            "200 OK",
                            format!(
                                r#"{{"event_id":"$join:example.org","type":"org.matrix.msc3401.call.member","origin_server_ts":{SERVER_EPOCH_MS},"sender":"@a:example.org","room_id":"!r:example.org","content":{{}}}}"#
                            ),
                        )
                    } else if method == "POST"
                        && path.contains("/org.matrix.msc4140/delayed_events/")
                    {
                        ("200 OK", "{}".to_owned())
                    } else {
                        (
                            "404 Not Found",
                            r#"{"errcode":"M_UNRECOGNIZED","error":"fake"}"#.to_owned(),
                        )
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                        json.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    /// The gate for the 2026-09-27 OTK incident class: the keeper talks to the
    /// homeserver over plain REST and, even while it keeps trying to rotate
    /// its token, never creates or opens the agent's crypto store.
    #[tokio::test]
    async fn live_keeper_uses_plain_rest_and_never_opens_the_crypto_store() {
        let (base, seen) = fake_homeserver().await;
        let store_dir = std::env::temp_dir().join(format!(
            "aqua-rtc-member-test-{}-{}",
            std::process::id(),
            unix_now()
        ));
        std::fs::create_dir_all(&store_dir).unwrap();
        let key_file = store_dir.join("agent.pem");
        std::fs::write(
            &key_file,
            siwx_oidc_auth::SiwxKey::generate_ed25519()
                .to_pem()
                .unwrap(),
        )
        .unwrap();
        let config = AgentConfig {
            key_file,
            siwx_url: base.clone(),
            matrix_url: format!("{base}/"),
            client_id: None,
            redirect_uri: None,
            store_dir: store_dir.clone(),
            device_id: Some("AQUA_test".into()),
            device_role: Default::default(),
        };
        // Near expiry: every request first tries a rotation (which goes to
        // the fake siwx-oidc and fails), exercising the token path too.
        let session = RestSession::new(
            config,
            "tok0".into(),
            unix_now() + 5,
            "@agent:example.org".try_into().unwrap(),
            "AQUA_test".into(),
        )
        .unwrap();
        let fast = RtcMemberTiming {
            expiry: Duration::from_secs(4),
            refresh_every: Duration::from_millis(900),
            leave_delay: Duration::from_secs(2),
            leave_restart_every: Duration::from_millis(250),
            retry_after_error: Duration::from_millis(200),
        };
        let m = hold_with_session(
            session,
            "!r:example.org",
            "!r:example.org",
            "https://lk.example.org",
            fast,
        )
        .await
        .expect("join over plain REST");
        assert!(m.has_dead_man_switch());
        tokio::time::sleep(Duration::from_millis(1300)).await;
        m.leave().await.expect("leave");

        let seen = seen.lock().unwrap().clone();
        let matrix: Vec<_> = seen
            .iter()
            .filter(|(_, p, _, _)| p.starts_with("/_matrix/"))
            .collect();
        assert!(
            matrix.iter().all(|(_, _, b, _)| b == "Bearer tok0"),
            "every homeserver request carries the session token: {matrix:?}"
        );
        let member_puts: Vec<_> = matrix
            .iter()
            .filter(|(m, p, _, _)| m == "PUT" && !p.contains("msc4140.delay"))
            .collect();
        assert!(member_puts.len() >= 2, "join + refresh: {member_puts:?}");
        assert!(
            member_puts[0].1.contains(
                "/_matrix/client/v3/rooms/!r:example.org/state/org.matrix.msc3401.call.member/_@agent:example.org_AQUA_test_m.call"
            ),
            "owned state key on the stable v3 path: {}",
            member_puts[0].1
        );
        assert!(
            !member_puts[0].3.contains("created_ts"),
            "join starts a chain"
        );
        assert!(
            member_puts[1]
                .3
                .contains(&format!("\"created_ts\":{SERVER_EPOCH_MS}")),
            "refresh keeps the server created_ts: {}",
            member_puts[1].3
        );
        assert!(matrix.iter().any(|(m, p, _, b)| m == "PUT"
            && p.contains("org.matrix.msc4140.delay=2000")
            && b == "{}"));
        let restarts = matrix
            .iter()
            .filter(|(_, p, _, b)| p.contains("/delayed_events/syd_1") && b.contains("restart"))
            .count();
        assert!(restarts >= 3, "restarts every 250 ms: {restarts}");
        let last = matrix.last().unwrap();
        assert!(
            last.1.contains("/delayed_events/syd_1") && last.3.contains("\"send\""),
            "leave sends the delayed leave now: {last:?}"
        );
        assert!(
            seen.iter().any(|(_, p, _, _)| !p.starts_with("/_matrix/")),
            "rotation was attempted against siwx-oidc"
        );

        let names: Vec<String> = std::fs::read_dir(&store_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&store_dir);
        assert!(
            names.iter().all(|n| !n.starts_with("matrix-sdk-")),
            "the membership keeper opened a matrix-sdk store: {names:?}"
        );
        assert_eq!(names, vec!["agent.pem".to_owned()], "nothing but the key");
    }

    fn live_store(tag: &str, base: &str, device: &str, cached_device: Option<&str>) -> AgentConfig {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let store_dir = std::env::temp_dir().join(format!(
            "aqua-rtc-member-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&store_dir).unwrap();
        let key_file = store_dir.join("agent.pem");
        std::fs::write(
            &key_file,
            siwx_oidc_auth::SiwxKey::generate_ed25519()
                .to_pem()
                .unwrap(),
        )
        .unwrap();
        if let Some(cached) = cached_device {
            let mut cfg = crate::ConfigFile::default();
            cfg.oidc.client_id = Some("client".into());
            cfg.oidc.redirect_uri = Some("http://localhost:0/callback".into());
            cfg.session = Some(crate::SessionCache {
                access_token: "tok0".into(),
                user_id: "@agent:example.org".into(),
                device_id: cached.into(),
                expires_at_unix: unix_now() + 3600,
                refresh_token: Some("rt0".into()),
                did: Some("did:key:z6Mktest".into()),
            });
            cfg.save(&store_dir.join("config.toml")).unwrap();
        }
        AgentConfig {
            key_file,
            siwx_url: base.to_owned(),
            matrix_url: format!("{base}/"),
            client_id: None,
            redirect_uri: None,
            store_dir,
            device_id: Some(device.into()),
            device_role: Default::default(),
        }
    }

    /// The keeper's session: `tok0` for `@agent:example.org` / `AQUA_test`.
    fn live_session(config: AgentConfig, valid_for_s: u64) -> RestSession {
        RestSession::new(
            config,
            "tok0".into(),
            unix_now() + valid_for_s,
            "@agent:example.org".try_into().unwrap(),
            "AQUA_test".into(),
        )
        .unwrap()
    }

    fn fast_timing() -> RtcMemberTiming {
        RtcMemberTiming {
            expiry: Duration::from_secs(4),
            refresh_every: Duration::from_millis(900),
            leave_delay: Duration::from_secs(2),
            leave_restart_every: Duration::from_millis(250),
            retry_after_error: Duration::from_millis(200),
        }
    }

    /// Each live test holds its own room: holds of one (room, user, device)
    /// hand over to each other process-wide, as they should.
    async fn live_hold(session: RestSession, room: &str) -> Result<RtcMembership> {
        hold_with_session(session, room, room, "https://lk.example.org", fast_timing()).await
    }

    /// A 401 on a keeper request rotates the token (refresh grant, persisted
    /// to config.toml) and retries the request once with the new token.
    #[tokio::test]
    async fn a_rejected_token_is_rotated_and_the_request_retried() {
        let hs = Hs {
            token_json: Some(
                r#"{"access_token":"tok1","token_type":"Bearer","expires_in":3600,"refresh_token":"rt1"}"#
                    .into(),
            ),
            ..Default::default()
        };
        hs.revoked.store(true, std::sync::atomic::Ordering::SeqCst);
        let (base, seen) = fake_homeserver_with(hs).await;
        let config = live_store("rotate", &base, "AQUA_test", Some("AQUA_test"));
        let store_dir = config.store_dir.clone();
        let m = live_hold(live_session(config, 3600), "!rotate:example.org")
            .await
            .expect("join succeeds after one rotation");
        tokio::time::sleep(Duration::from_millis(600)).await;
        m.leave().await.expect("leave");

        let seen = seen.lock().unwrap().clone();
        let matrix: Vec<_> = seen
            .iter()
            .filter(|r| r.1.starts_with("/_matrix/"))
            .collect();
        assert_eq!(
            matrix[0].2, "Bearer tok0",
            "the join first tried the old token"
        );
        assert!(
            matrix[1..].iter().all(|r| r.2 == "Bearer tok1"),
            "the retry and everything after it use the rotated token: {matrix:?}"
        );
        assert!(
            matrix[1].0 == "PUT" && matrix[1].1 == matrix[0].1,
            "the join was retried"
        );
        let grants: Vec<_> = seen.iter().filter(|r| r.1.starts_with("/token")).collect();
        assert_eq!(grants.len(), 1, "one refresh grant: {grants:?}");
        assert!(grants[0].3.contains("refresh_token=rt0"));
        let persisted = crate::ConfigFile::load(&store_dir.join("config.toml"))
            .unwrap()
            .session
            .unwrap();
        let _ = std::fs::remove_dir_all(&store_dir);
        assert_eq!(persisted.access_token, "tok1");
        assert_eq!(persisted.refresh_token.as_deref(), Some("rt1"));
        assert_eq!(persisted.device_id, "AQUA_test");
    }

    /// minor 1: a rotation that would yield another device's session is
    /// refused before anything is spent or persisted, is never retried as a
    /// fresh login, and stops the keeper with the error surfaced.
    #[tokio::test]
    async fn a_device_mismatch_is_refused_and_stops_the_keeper() {
        let hs = Hs::default();
        let revoked = hs.revoked.clone();
        let (base, seen) = fake_homeserver_with(hs).await;
        // config.toml holds the agent's session for ANOTHER device.
        let config = live_store("mismatch", &base, "AQUA_other", Some("AQUA_other"));
        let store_dir = config.store_dir.clone();
        let before = std::fs::read_to_string(store_dir.join("config.toml")).unwrap();
        let m = live_hold(live_session(config, 3600), "!mismatch:example.org")
            .await
            .expect("join with the still-valid token");
        revoked.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert!(!m.is_active(), "the keeper stopped");
        let n = seen.lock().unwrap().len();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(seen.lock().unwrap().len(), n, "no requests after the stop");
        let err = m.leave().await.expect_err("the mismatch is surfaced");
        let after = std::fs::read_to_string(store_dir.join("config.toml")).unwrap();
        let _ = std::fs::remove_dir_all(&store_dir);
        assert!(format!("{err:#}").contains("mismatch"), "{err:#}");
        let siwx: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.1.starts_with("/_matrix/"))
            .cloned()
            .collect();
        assert!(
            siwx.is_empty(),
            "no refresh grant, no fresh login: {siwx:?}"
        );
        assert_eq!(before, after, "config.toml untouched");
    }

    /// M2: a siwx-oidc that accepts connections and never answers must not
    /// stall the keeper: each rotation gives up after `rotate_timeout` and
    /// the request goes out with the current token; leave still completes.
    #[tokio::test]
    async fn a_hung_token_endpoint_cannot_stall_join_or_leave() {
        let (base, seen) = fake_homeserver_with(Hs {
            siwx_hangs: true,
            ..Default::default()
        })
        .await;
        let config = live_store("hang", &base, "AQUA_test", None);
        let store_dir = config.store_dir.clone();
        // Near expiry: every request first tries a rotation.
        let mut session = live_session(config, 5);
        session.rotate_timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let m = tokio::time::timeout(
            Duration::from_secs(5),
            live_hold(session, "!hang:example.org"),
        )
        .await
        .expect("join must not hang on siwx-oidc")
        .expect("join");
        tokio::time::timeout(Duration::from_secs(5), m.leave())
            .await
            .expect("leave must not hang on siwx-oidc")
            .expect("leave");
        let _ = std::fs::remove_dir_all(&store_dir);
        assert!(started.elapsed() < Duration::from_secs(5));
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|r| !r.1.starts_with("/_matrix/")),
            "a rotation was attempted"
        );
        assert!(seen
            .iter()
            .filter(|r| r.1.starts_with("/_matrix/"))
            .all(|r| r.2 == "Bearer tok0"));
    }

    /// minor 5: only M_FORBIDDEN on the owned (MSC3757) key falls back to the
    /// plain key; an outage or a rate limit fails the join instead.
    #[tokio::test]
    async fn the_plain_state_key_is_used_only_after_m_forbidden() {
        for (status, json, falls_back) in [
            (
                "403 Forbidden",
                r#"{"errcode":"M_FORBIDDEN","error":"no"}"#,
                true,
            ),
            (
                "500 Internal Server Error",
                r#"{"errcode":"M_UNKNOWN","error":"boom"}"#,
                false,
            ),
            (
                "429 Too Many Requests",
                r#"{"errcode":"M_LIMIT_EXCEEDED","error":"slow","retry_after_ms":10}"#,
                false,
            ),
        ] {
            let (base, seen) = fake_homeserver_with(Hs {
                owned_key_error: Some((status, json)),
                ..Default::default()
            })
            .await;
            let config = live_store("forbidden", &base, "AQUA_test", None);
            let store_dir = config.store_dir.clone();
            let res = live_hold(live_session(config, 3600), "!forbidden:example.org").await;
            let plain_puts = seen
                .lock()
                .unwrap()
                .iter()
                .filter(|r| {
                    r.0 == "PUT" && r.1.contains("/state/org.matrix.msc3401.call.member/@agent")
                })
                .count();
            let _ = std::fs::remove_dir_all(&store_dir);
            if falls_back {
                let m = res.expect("M_FORBIDDEN falls back to the plain key");
                assert!(plain_puts >= 1);
                m.leave().await.unwrap();
            } else {
                assert!(res.is_err(), "{status} must fail the join");
                assert_eq!(plain_puts, 0, "{status} must not switch to the plain key");
            }
        }
    }

    /// Source guard: the non-test part of this module must never build a
    /// matrix-sdk Client or reach the Client-building token rotation, the
    /// pattern that put two OlmMachines on one crypto store (2026-09-27).
    #[test]
    fn keeper_code_never_builds_a_matrix_client() {
        let src = include_str!("rtc_member.rs");
        let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap();
        let code: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            // The keeper's own plain HTTP client is the point, not a violation.
            .replace("reqwest::Client::builder", "");
        for forbidden in [
            "build_and_restore",
            "reauth_token_only",
            "reauth_inner",
            "reauth(",
            "sqlite_store",
            "Client::builder",
            "::connect(",
            "self.clone()",
            "matrix_sdk::Client",
        ] {
            assert!(
                !code.contains(forbidden),
                "rtc_member.rs production code uses `{forbidden}`"
            );
        }
        assert!(code.contains("mint_session_token("));
    }

    /// KEYLOSS-1 F1, on the wire (the bodies the keeper PUTs): the rejoin
    /// content differs from BOTH the join's (what the server holds in the
    /// first hour) and a refresh's (later), so Synapse's identical-content
    /// dedupe (`handlers/message.py` `deduplicate_state_event`, canonical JSON
    /// of the content) cannot swallow it; it carries the new created_ts,
    /// which the refresh after it keeps.
    #[tokio::test]
    async fn rejoin_wire_content_differs_from_join_and_refresh_and_carries_the_new_created_ts() {
        let (base, seen) = fake_homeserver().await;
        let config = live_store("rejoin-wire", &base, "AQUA_test", Some("AQUA_test"));
        let m = live_hold(live_session(config, 3600), "!rejoin-wire:example.org")
            .await
            .expect("join");
        tokio::time::sleep(Duration::from_millis(1300)).await;
        let r = m.rejoin().await.expect("rejoin");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        m.leave().await.expect("leave");

        let seen = seen.lock().unwrap().clone();
        let bodies: Vec<serde_json::Value> = seen
            .iter()
            .filter(|(m, p, _, _)| {
                m == "PUT"
                    && p.contains("/state/org.matrix.msc3401.call.member/")
                    && !p.contains("msc4140.delay")
            })
            .map(|(_, _, _, b)| serde_json::from_str(b).unwrap())
            .collect();
        assert!(bodies.len() >= 4, "join, refresh, rejoin, refresh: {bodies:?}");
        let (join, refresh, rejoin, after) = (&bodies[0], &bodies[1], &bodies[2], &bodies[3]);
        assert!(join.get("created_ts").is_none(), "{join}");
        assert_eq!(refresh["created_ts"], SERVER_EPOCH_MS, "{refresh}");
        assert_eq!(rejoin["created_ts"], r.created_ts_ms, "{rejoin}");
        assert!(r.created_ts_ms > SERVER_EPOCH_MS);
        assert_eq!(rejoin["expires"], 4000, "a new chain, 4 s fast timing");
        assert_ne!(rejoin, join, "differs from the first-hour state");
        assert_ne!(rejoin, refresh, "differs from a refreshed state");
        assert_eq!(after["created_ts"], r.created_ts_ms, "the refresh adopted it");
        for b in &bodies {
            assert!(b.as_object().is_some_and(|o| !o.is_empty()), "never emptied by a PUT: {b}");
            assert_eq!(b["device_id"], "AQUA_test");
        }
    }
}
