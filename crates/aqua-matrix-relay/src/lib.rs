//! Generic Matrix agent daemon.
//!
//! This crate owns the *transport lifecycle* — authenticate via siwx-oidc,
//! connect to Matrix, stream-sync, rotate the client ahead of token expiry,
//! deduplicate inbound messages by a timestamp watermark that persists across
//! rotations (so a DM arriving in the rotation gap isn't lost), shut down
//! cleanly on SIGTERM/SIGINT, and exit cleanly after repeated connect failures
//! so systemd can self-heal. It knows nothing about *what an agent does* with a
//! message.
//!
//! To build a new agent you implement [`MessageHandler`] and call
//! [`run_daemon`]. That's the whole surface — no Matrix types appear in the
//! trait, so a handler author never touches matrix-sdk directly. See
//! `examples/echo_agent.rs` for a complete ~30-line agent.
//!
//! matrix-sdk 0.17's deeply-nested async types overflow rustc's default
//! Send-bound recursion budget when we spawn `client.sync(...)` in a tokio
//! task (the same reason the agent lib raises it).
#![recursion_limit = "256"]
//!
//! The transport (siwx-oidc + Matrix) is intentionally NOT abstracted: this
//! crate's identity is "DID-authenticated, E2EE Matrix agent template," and a
//! `Transport` trait would dilute that. The seam is at the message boundary,
//! not the wire.
//!
//! ## Lifecycle (per client cycle)
//!
//! The outer loop in [`run_daemon`] builds a fresh [`AgentClient`] (which hits
//! the refresh-grant path, preserving `device_id` and the crypto store), then:
//!   1. joins any pending invites,
//!   2. sends the handler's one-time `hello()` on the first cycle only,
//!   3. upserts the fleet-registry entry,
//!   4. runs a sync stream + optional periodic tick until the token nears
//!      expiry, then drops the client and loops to rotate.
//!
//! This mirrors the matrix-sdk reality that the `Client` has no public API to
//! swap an access token in place; rotating the whole client ~30 s before expiry
//! is what avoids the `M_UNKNOWN_TOKEN` sync wedge.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::{
            receipt::ReceiptThread,
            room::{
                member::{MembershipState, StrippedRoomMemberEvent},
                message::{MessageType, OriginalSyncRoomMessageEvent},
            },
        },
    },
};

// Re-export everything a handler crate needs so it can depend on ONLY this
// crate (plus tokio for its `main`). A new agent never imports matrix-sdk.
// ReplyStream / TypingGuard let handlers stream answers and show "typing…".
pub use aqua_matrix_agent::{load_dotenv, AgentClient, AgentConfig, ReplyStream, TypingGuard};
pub use async_trait::async_trait;

/// Rotate the client `REFRESH_GUARD_SECS` before the access token expires. 30 s
/// gives the refresh-grant round trip + a fresh initial sync ample headroom
/// inside the (typically 300 s) token TTL.
const REFRESH_GUARD_SECS: u64 = 30;
/// Minimum sleep between rotations, so a token that arrived near expiry still
/// gets one full cycle instead of hammering siwx-oidc.
const MIN_CYCLE_SECS: u64 = 15;
/// After this many consecutive [`AgentClient::connect`] failures, exit so
/// systemd's `Restart=always` brings up a fresh process. The connect path can
/// accumulate matrix-sdk / SQLite resources on partial failures; a fresh
/// process resets that. `StartLimitBurst` still guards against runaway
/// restarts.
const MAX_CONNECT_FAILURES: u32 = 3;

/// One inbound text message, decoded from Matrix for a [`MessageHandler`].
///
/// All borrowed fields point at the live sync event (or a local owned by
/// [`dispatch`]) for the duration of the `handle_message` call; nothing here
/// outlives it.
pub struct InboundMessage<'a> {
    pub sender_mxid: &'a str,
    // SEAM(aqua-security): user proxy key. `sender_did` is `None` this phase; it
    // is the DID allow/deny key and the anchor for a future user-minted
    // proxy/delegation key. That key may ride here as a new field OR arrive via
    // existing Matrix message artefacts (event signatures, `m.room.member`
    // state, custom content keys) — and Matrix artefacts may well suffice, so no
    // new field is added now. This lives where messages are parsed, not at the
    // container-auth layer.
    pub sender_did: Option<String>,
    pub event_id: &'a str,
    pub room_id: &'a str,
    pub timestamp_ms: u64,
    pub msgtype: &'a str, // "m.text", …
    pub body: &'a str,
}

/// A concrete agent backend. The relay owns the Matrix lifecycle; the handler
/// owns "what to do with a message" and "what to do on each tick".
///
/// All methods are `&self`; the handler is shared (`Arc`) across the sync
/// stream and every spawned task, so keep interior state behind `Mutex`/atomics
/// if you need it. `handle_message` should return quickly — spawn a task for
/// slow work (an LLM call, a subprocess) so the sync stream keeps flowing.
#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    /// Logical role for the fleet registry, e.g. `"heartbeat"`, `"claude-channel"`.
    fn role(&self) -> &str;

    /// systemd unit supervising this agent, recorded in the registry entry.
    /// `None` for ad-hoc runs with no supervising unit.
    fn systemd_unit(&self) -> Option<&str> {
        None
    }

    /// Human-readable Matrix profile display name set on first connect, so
    /// people see an alias (e.g. "claude-channel") instead of the raw
    /// DID-derived MXID. Defaults to [`role`](Self::role); override for a
    /// friendlier label.
    fn display_name(&self) -> String {
        self.role().to_string()
    }

    /// One-time greeting DM'd to `target` on the first successful connect.
    /// `None` stays silent.
    fn hello(&self, _agent: &AgentClient) -> Option<String> {
        None
    }

    /// Periodic tick interval. `None` (the default) disables the timer; the
    /// daemon then only reacts to inbound messages.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// Called every [`tick_interval`](Self::tick_interval) (only when it is
    /// `Some`). The default is a no-op.
    async fn on_tick(&self, _agent: &AgentClient, _target: &str) {}

    /// DID/MXID allow-deny SEAM. Default == today's exact behavior (single
    /// target, case-insensitive). [`dispatch`] and `register_invite_autojoin`
    /// call THIS instead of the inline equality check. SEAM(aqua-security):
    /// this is the white/blacklist hook keyed on DIDs — a future signature will
    /// take `sender_did: Option<&str>` plus a per-template allow/deny policy
    /// object (BOTH an allow-list AND a deny-list); the bool default stays
    /// `{target}` this phase so behavior is unchanged.
    fn authorize(&self, sender_mxid: &str, target: &str) -> bool {
        sender_mxid.eq_ignore_ascii_case(target)
    }

    /// Handle one inbound text message from `target`. The relay has already
    /// confirmed the sender and deduplicated by timestamp watermark, so this
    /// fires at most once per message. Now takes a structured message and
    /// returns `Result` so the relay owns uniform error logging (the relay
    /// still never unwinds; it logs the `Err`).
    async fn handle_message(
        &self,
        agent: &AgentClient,
        target: &str,
        msg: &InboundMessage<'_>,
    ) -> anyhow::Result<()>;
}

/// Run the agent daemon forever: connect, serve, rotate, repeat. Only returns
/// by `std::process::exit` after [`MAX_CONNECT_FAILURES`] consecutive connect
/// failures (so systemd restarts a clean process).
pub async fn run_daemon<H: MessageHandler>(config: AgentConfig, target: &str, handler: H) {
    let handler = Arc::new(handler);
    let target = Arc::new(target.to_string());

    tracing::info!(
        "{} daemon starting (target: {}, sync: stream, refresh-guard: {}s)",
        handler.role(),
        target,
        REFRESH_GUARD_SECS,
    );

    // The inbound-message dedupe watermark lives ACROSS cycles, created once
    // here at "now": the FIRST cycle therefore ignores pre-startup backlog
    // (preserving prior behavior), while LATER cycles carry it forward. That
    // carry-forward is what stops the rotation gap from losing a message: when
    // the client rotates (~30 s before token expiry) the new cycle's initial
    // sync re-delivers recent timeline; with a per-cycle "now" watermark a DM
    // that landed during the gap (its ts is just older than the new cycle's
    // start) would look like backlog and be dropped. Carrying the watermark
    // means a gap message (ts > carried watermark) is dispatched exactly once,
    // while already-seen messages (ts <= watermark) stay skipped.
    //
    // Edge (acceptable): after MAX_CONNECT_FAILURES the process exits and a
    // fresh process starts with the watermark reset to "now", re-ignoring any
    // backlog — fine, that's a hard restart, not a rotation.
    let watermark = Arc::new(AtomicU64::new(now_epoch_ms()));

    // Trap SIGTERM/SIGINT once and fan it out via a Notify so every cycle can
    // unwind cleanly. `podman stop`/`systemctl stop` send SIGTERM and SIGKILL
    // after a grace period; handling it lets us abort the in-flight sync and
    // return from `run_daemon` (so `main` exits 0) well before the SIGKILL.
    // SIGINT (Ctrl-C) is treated identically for local runs.
    let shutdown = Arc::new(Notify::new());
    spawn_shutdown_listener(shutdown.clone());

    let mut first_cycle = true;
    let mut consecutive_failures: u32 = 0;
    loop {
        let agent = match AgentClient::connect(config.clone()).await {
            Ok(a) => {
                consecutive_failures = 0;
                a
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::error!(
                    "{}: AgentClient::connect failed ({consecutive_failures}/{MAX_CONNECT_FAILURES}): {e:#}",
                    handler.role(),
                );
                if consecutive_failures >= MAX_CONNECT_FAILURES {
                    tracing::error!(
                        "{}: {MAX_CONNECT_FAILURES} consecutive connect failures; exiting for systemd Restart=always (avoids in-process resource accumulation)",
                        handler.role(),
                    );
                    std::process::exit(1);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // Sync once first so a pending invite is actually visible: right after
        // connect the initial sync may not yet carry an invite the peer sent
        // moments earlier, and `join_invited_rooms` would then join nothing.
        if let Err(e) = agent.sync_once().await {
            tracing::warn!("{}: pre-join sync_once failed: {e:#}", handler.role());
        }

        // Join pending invites, then record each joined room as the DM with our
        // peer. A freshly connected client has empty `m.direct` and an
        // unpopulated member list, so without this the first outbound message
        // (the hello below) fails to resolve the shared room and `create_dm`
        // spawns a DUPLICATE — which, against a programmatic peer, splits the two
        // sides into separate rooms and breaks Megolm key exchange (in
        // production it leaves stray empty rooms). The peer is the only party
        // that DMs us, so any room it invited us to IS the DM room.
        match agent.join_invited_rooms().await {
            Ok(joined) => {
                for room_id in &joined {
                    if let Err(e) = agent.mark_dm(room_id, &target).await {
                        tracing::warn!("{}: mark_dm({room_id}) failed: {e:#}", handler.role());
                    } else {
                        tracing::info!("{}: marked joined room {room_id} as DM with peer", handler.role());
                    }
                }
            }
            Err(e) => tracing::warn!("{}: join_invited_rooms failed: {e:#}", handler.role()),
        }

        // One sync so the peer's device keys are known before we encrypt the
        // hello (otherwise the hello is undecryptable on their side until the
        // next round-trip). Best-effort.
        if let Err(e) = agent.sync_once().await {
            tracing::warn!("{}: settle sync_once failed: {e:#}", handler.role());
        }

        let now = unix_now_secs();
        let ttl = agent
            .expires_at_unix()
            .saturating_sub(now)
            .saturating_sub(REFRESH_GUARD_SECS)
            .max(MIN_CYCLE_SECS);
        let refresh_deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
        tracing::info!(
            "{}: client cycle starting (token ttl {}s, rotating in {}s)",
            handler.role(),
            agent.expires_at_unix().saturating_sub(now),
            ttl,
        );

        if first_cycle {
            // Publish a human-readable alias before anything else, so the hello
            // DM and registry entry surface under a readable name rather than
            // the DID-derived MXID. Best-effort: never block startup on it.
            let alias = handler.display_name();
            if let Err(e) = agent.set_display_name(&alias).await {
                tracing::warn!("{}: set display name failed: {e:#}", handler.role());
            } else {
                tracing::info!("{}: display name set to {:?}", handler.role(), alias);
            }
            if let Some(hello) = handler.hello(&agent) {
                // Only greet into an EXISTING DM room. If none resolves yet (the
                // peer's invite hasn't been synced/joined), sending would
                // `create_dm` a stray duplicate room and pollute m.direct — skip
                // it; the auto-join handler will pick up the real room and the
                // first real exchange establishes Megolm there anyway.
                match agent.dm_room_id(&target).await {
                    Ok(Some(_)) => {
                        if let Err(e) = agent.send_dm(&target, &hello).await {
                            tracing::warn!("{}: hello send failed: {e:#}", handler.role());
                        }
                    }
                    _ => tracing::info!(
                        "{}: no DM room yet; deferring hello (auto-join will converge)",
                        handler.role()
                    ),
                }
            }
            first_cycle = false;
        }

        // Upsert the fleet-registry entry on every cycle start so a freshly
        // rotated session re-announces itself promptly. Best-effort: never let a
        // registry failure perturb the daemon.
        if let Err(e) = agent
            .update_registry(handler.role(), handler.systemd_unit())
            .await
        {
            tracing::warn!("{}: registry update failed: {e:#}", handler.role());
        }

        let exit = run_cycle(
            &agent,
            &target,
            &handler,
            refresh_deadline,
            watermark.clone(),
            &shutdown,
        )
        .await;
        if exit == "shutdown" {
            // `run_cycle` already aborted the in-flight sync task before
            // returning the sentinel, so nothing is left dangling. Return so
            // `main` falls through and the process exits 0 before SIGKILL.
            tracing::info!("{}: received SIGTERM; shutting down", handler.role());
            return;
        }
        tracing::info!("{}: cycle ended ({exit}); reconnecting", handler.role());
    }
}

/// Spawn a detached task that awaits SIGTERM or SIGINT and then notifies
/// `shutdown`. Whichever signal fires first triggers the same clean shutdown
/// path; subsequent signals are irrelevant (we're already unwinding).
/// If installing a handler fails we log and leave that signal untrapped rather
/// than abort startup — the other signal (and systemd's SIGKILL) still apply.
fn spawn_shutdown_listener(shutdown: Arc<Notify>) {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e:#}");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to install SIGINT handler: {e:#}");
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("SIGTERM received"),
            _ = sigint.recv() => tracing::info!("SIGINT received"),
        }
        // `notify_one` both wakes a waiter that's already parked AND stores a
        // permit if none is, so a cycle that hasn't yet reached its
        // `notified()` await still observes shutdown on its next call. Exactly
        // one cycle runs at a time, so a single permit suffices.
        shutdown.notify_one();
    });
}

/// Run one client cycle: register handlers, sync until the rotation deadline
/// (or sync end, or shutdown), then abort the sync task and report why it ended.
///
/// `watermark` is the cross-cycle inbound-message dedupe key, owned by
/// [`run_daemon`] and passed in (not created here). It is seeded once at daemon
/// start to "now", so the FIRST cycle ignores pre-startup backlog; LATER cycles
/// reuse the SAME watermark, so when this cycle's initial sync re-delivers
/// recent timeline after a client rotation, a message that arrived in the
/// rotation gap (ts > watermark) is dispatched exactly once while already-seen
/// messages (ts <= watermark) stay skipped. [`dispatch`] still advances it
/// monotonically.
///
/// `shutdown` is fired by the daemon's SIGTERM/SIGINT listener; observing it
/// returns the `"shutdown"` sentinel so [`run_daemon`] can exit cleanly.
async fn run_cycle<H: MessageHandler>(
    agent: &AgentClient,
    target: &Arc<String>,
    handler: &Arc<H>,
    refresh_deadline: tokio::time::Instant,
    watermark: Arc<AtomicU64>,
    shutdown: &Notify,
) -> &'static str {
    register_handler(agent.clone(), target.clone(), handler.clone(), watermark);
    // Auto-join invites as they arrive on the sync stream (not only at cycle
    // start), so an invite the peer sends mid-cycle — or just after connect,
    // before the startup join sees it — is still joined and recorded as the DM.
    register_invite_autojoin(agent.clone(), target.clone(), handler.clone());

    let sync_client = agent.client().clone();
    let mut sync_task = tokio::spawn(async move { sync_client.sync(SyncSettings::default()).await });

    let exit = match handler.tick_interval() {
        Some(interval) => {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick — the hello (or the previous cycle's
            // last tick) just went out; let the operator see the real cadence.
            tick.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.notified() => break "shutdown",
                    _ = tokio::time::sleep_until(refresh_deadline) => break "refresh-deadline",
                    res = &mut sync_task => {
                        log_sync_end(res);
                        break "sync-ended";
                    }
                    _ = tick.tick() => {
                        handler.on_tick(agent, target).await;
                    }
                }
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = shutdown.notified() => "shutdown",
                _ = tokio::time::sleep_until(refresh_deadline) => "refresh-deadline",
                res = &mut sync_task => {
                    log_sync_end(res);
                    "sync-ended"
                }
            }
        }
    };

    // If the sync task already completed (its `res = &mut sync_task` arm fired —
    // e.g. sync() returned on a 401), it MUST NOT be awaited again: polling a
    // finished JoinHandle panics ("JoinHandle polled after completion"). Guard on
    // is_finished() so we only abort+await while it's still running (the
    // shutdown / refresh-deadline / tick exits). A finished handle is just
    // dropped — its result was already logged by the sync-ended arm.
    if !sync_task.is_finished() {
        sync_task.abort();
        let _ = sync_task.await;
    }
    exit
}

fn log_sync_end(res: Result<matrix_sdk::Result<()>, tokio::task::JoinError>) {
    match res {
        Ok(Ok(_)) => tracing::warn!("matrix sync returned Ok (unexpected)"),
        Ok(Err(e)) => tracing::warn!("matrix sync error: {e:#}"),
        Err(e) => tracing::warn!("matrix sync task join error: {e:#}"),
    }
}

/// Continuously auto-join rooms we are invited to (and record them as the DM
/// with our peer), so the daemon converges on the SAME room the peer created
/// rather than `create_dm` later spawning a duplicate. Registered on the sync
/// stream so it fires whenever an invite arrives, not just at cycle start.
fn register_invite_autojoin<H: MessageHandler>(
    agent: AgentClient,
    target: Arc<String>,
    handler: Arc<H>,
) {
    let own = agent.user_id().to_string();
    let role = handler.role().to_string();
    agent.client().add_event_handler({
        let agent = agent.clone();
        move |ev: StrippedRoomMemberEvent, room: Room| {
            let agent = agent.clone();
            let target = target.clone();
            let role = role.clone();
            let handler = handler.clone();
            let own = own.clone();
            async move {
                // Only act on an invite addressed to us.
                if ev.state_key.as_str() != own
                    || ev.content.membership != MembershipState::Invite
                {
                    return;
                }
                // Invite auto-join is intentionally narrowed to the target only
                // (was: any inviter) — the default `authorize` keeps the
                // case-insensitive single-peer check. Safe under the strict
                // single-peer DM design; messaging/dispatch behavior unchanged.
                if !handler.authorize(ev.sender.as_str(), &target) {
                    return;
                }
                match room.join().await {
                    Ok(()) => {
                        let room_id = room.room_id().to_string();
                        tracing::info!("{role}: auto-joined invited room {room_id}");
                        if let Err(e) = agent.mark_dm(&room_id, &target).await {
                            tracing::warn!("{role}: mark_dm({room_id}) after auto-join failed: {e:#}");
                        }
                    }
                    Err(e) => tracing::warn!("{role}: auto-join failed: {e:#}"),
                }
            }
        }
    });
}

fn register_handler<H: MessageHandler>(
    agent: AgentClient,
    target: Arc<String>,
    handler: Arc<H>,
    watermark: Arc<AtomicU64>,
) {
    agent.client().add_event_handler({
        let agent = agent.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let agent = agent.clone();
            let target = target.clone();
            let handler = handler.clone();
            let watermark = watermark.clone();
            async move {
                dispatch(ev, room, &agent, &target, &handler, &watermark).await;
            }
        }
    });
}

async fn dispatch<H: MessageHandler>(
    ev: OriginalSyncRoomMessageEvent,
    room: Room,
    agent: &AgentClient,
    target: &str,
    handler: &Arc<H>,
    watermark: &AtomicU64,
) {
    // Only messages from the configured peer, and only ones newer than anything
    // we've already seen this cycle. The default `authorize` compares
    // case-insensitively: Synapse canonicalises MXIDs to lowercase, so
    // `ev.sender` is lowercased, while a `--target` derived from a mixed-case
    // `did:key` is not — an exact compare would silently drop every inbound
    // message from such a peer.
    if !handler.authorize(ev.sender.as_str(), target) {
        return;
    }
    let ts_ms = u64::from(ev.origin_server_ts.0);
    if ts_ms <= watermark.load(Ordering::Relaxed) {
        return;
    }

    // Instant "seen" acknowledgement (fire-and-forget): the user gets feedback
    // that the message landed before any handler latency. Best-effort.
    {
        let room = room.clone();
        let event_id = ev.event_id.clone();
        tokio::spawn(async move {
            let _ = room
                .send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
                .await;
        });
    }

    let body = match &ev.content.msgtype {
        MessageType::Text(t) => t.body.trim().to_string(),
        // Non-text content carries nothing for a handler; ignore without
        // advancing the watermark (the ts check still prevents reprocessing).
        _ => return,
    };

    // Advance the watermark BEFORE dispatching: if the handler (or the process)
    // dies mid-work we must not re-trigger on the same message after restart.
    watermark
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if ts_ms > v {
                Some(ts_ms)
            } else {
                None
            }
        })
        .ok();

    if body.is_empty() {
        return;
    }

    let msg = InboundMessage {
        sender_mxid: ev.sender.as_str(),
        sender_did: None,
        event_id: ev.event_id.as_str(),
        room_id: room.room_id().as_str(),
        timestamp_ms: ts_ms,
        msgtype: ev.content.msgtype.msgtype(),
        body: &body,
    };

    // The relay never unwinds on a handler error: log it at `warn` and keep
    // serving. (Errors are the handler's domain; the transport stays up.)
    if let Err(e) = handler.handle_message(agent, target, &msg).await {
        tracing::warn!("{}: handle_message failed: {e:#}", handler.role());
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
