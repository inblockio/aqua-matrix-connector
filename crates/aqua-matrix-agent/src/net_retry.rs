//! Network-outage resilience for the connect path.
//!
//! **Why this exists.** In three nightly ISP outages (2026-09-24..27, 1.5 to
//! 3.5 h each) every consultant container and the Scribe exited 3,537 times:
//! `connect failed (3/3): ... dns error: Temporary failure in name resolution`,
//! then `3 consecutive connect failures; exiting`, so podman restarted each
//! container every ~2.5 min. Exiting is also dangerous in itself: on
//! 2026-08-08 podman's `on-failure` policy gave up after one retry and 13
//! containers sat dead for 78 h. A process restart cannot bring the network
//! back, so a network failure now waits IN-PROCESS instead.
//!
//! **Policy.**
//! * [`classify_connect_error`] splits a failed connect into
//!   [`ConnectErrorClass::Transient`] (the network or an upstream proxy is
//!   down) and [`ConnectErrorClass::Fatal`] (everything else).
//! * Transient: retry forever with capped exponential backoff plus jitter
//!   ([`Backoff`]: 2 s, 4 s, 8 s, ... capped at 60 s).
//! * Fatal: the caller's existing exit behaviour, so a genuine crash loop
//!   (auth rejected, store/crypto fault, fd exhaustion) still surfaces to
//!   podman/systemd and the crash-loop watcher.
//! * Logging ([`OutageTracker`]): ONE `WARN network outage: waiting for
//!   connectivity` on entry, a `WARN network outage: still waiting ...`
//!   reminder at most every [`OUTAGE_REMINDER_EVERY`], and ONE
//!   `INFO network restored after <N>s (<M> attempts)` on recovery. The
//!   individual attempts log at DEBUG only.
//!
//! Every attempt is a full [`AgentClient::connect`], i.e. it goes through the
//! normal session logic (cached token -> refresh grant -> did:key auth). That
//! logic itself refuses to escalate on a network error (see
//! `acquire_session`), so an outage never burns or rotates the refresh token:
//! the grant is only presented once siwx-oidc is reachable again.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::{AgentClient, AgentConfig};

/// First backoff delay after a transient connect failure.
pub const BACKOFF_BASE: Duration = Duration::from_secs(2);
/// Upper bound for the backoff delay (before jitter, which only shortens it).
pub const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Jitter shaves up to this fraction off each delay, so 21 containers that lost
/// the network at the same instant do not hit siwx-oidc in lockstep on return.
/// Subtractive, so a delay never exceeds [`BACKOFF_CAP`].
pub const JITTER_FRACTION: f64 = 0.2;
/// Minimum spacing of the "still waiting" reminder while an outage lasts.
pub const OUTAGE_REMINDER_EVERY: Duration = Duration::from_secs(300);
/// Delay between attempts after a NON-transient failure (unchanged from the
/// relay's historical 10 s).
pub const FATAL_RETRY_DELAY: Duration = Duration::from_secs(10);

/// How a failed connect is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectErrorClass {
    /// The network, DNS, TLS path or an upstream proxy is down: wait in-process.
    Transient,
    /// Anything else: keep the caller's exit behaviour.
    Fatal,
}

/// Local resource exhaustion and definitive auth rejections. Checked FIRST, so
/// they stay fatal even when the chain also carries a transient-looking marker.
/// `EMFILE` in particular surfaces as `tcp connect error: Too many open files
/// (os error 24)` under hyper's `client error (Connect)`, which would otherwise
/// match the transient list; it is the fd-leak signature (2026-06-09) and must
/// keep crashing the process so the leak stays visible and a restart sheds it.
const FATAL_MARKERS: &[&str] = &[
    "too many open files",
    "os error 24", // EMFILE
    "os error 23", // ENFILE
    "no space left on device",
    "cannot allocate memory",
    "out of memory",
    "401 unauthorized",
    "403 forbidden",
    "m_unknown_token",
    "m_forbidden",
    "invalid_grant",
];

/// Positive evidence that the failure is the network path, not us. Matched
/// case-insensitively against the whole anyhow chain (the typed errors are
/// wrapped by `context` and by matrix-sdk/reqwest/hyper, so the rendered chain
/// is the stable seam, the same approach as `is_unknown_token`).
const TRANSIENT_MARKERS: &[&str] = &[
    // DNS (hyper-util GaiResolver -> getaddrinfo). EAI_AGAIN is the outage
    // signature; NXDOMAIN is included because an offline resolver can return it
    // too, and a genuine hostname typo would never have connected at deploy.
    "dns error",
    "failed to lookup address",
    "temporary failure in name resolution",
    "name or service not known",
    "no address associated with hostname",
    // TCP establishment and mid-request connection loss.
    "client error (connect)", // hyper-util: failure while ESTABLISHING the connection
    "tcp connect error",
    "error trying to connect",
    "connection refused",
    "connection reset",
    "connection aborted",
    "connection closed before message completed",
    "network is unreachable",
    "network is down",
    "no route to host",
    "host is unreachable",
    "broken pipe",
    "timed out", // "operation timed out", "Connection timed out (os error 110)"
    "unexpected eof",
    // TLS handshake. `peer misbehaved` covers rustls' IllegalHelloRetryRequest*
    // (~30/day against matrix.inblock.io). An invalid certificate is also
    // waited out: during an ISP failover it is typically a captive portal or
    // middlebox, and a genuinely expired server cert is not fixed by a restart
    // either (the reminder WARN keeps it visible).
    "peer misbehaved",
    "received fatal alert",
    "tls handshake",
    "peer closed connection",
    "invalid peer certificate",
    // Upstream proxy / overload. http::StatusCode renders "502 Bad Gateway",
    // both in reqwest-based bails ("whoami returned 502 Bad Gateway") and in
    // ruma's "[502 Bad Gateway] <non-json bytes>".
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
    "429 too many requests",
    "m_limit_exceeded",
];

/// Classify a failed connect/auth attempt. Conservative: an error is only
/// [`Transient`](ConnectErrorClass::Transient) on positive evidence that the
/// network path is at fault; anything unrecognised (500s, store/crypto errors,
/// `database is locked`, config errors, auth rejections) is
/// [`Fatal`](ConnectErrorClass::Fatal), i.e. keeps the pre-existing behaviour.
pub fn classify_connect_error(err: &anyhow::Error) -> ConnectErrorClass {
    let chain = err
        .chain()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
        .to_ascii_lowercase();

    // Typed io::Error kinds where the chain exposes them (best-effort: most
    // arrive rendered inside hyper/reqwest errors, hence the string markers).
    let mut typed_transient = false;
    for cause in err.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if matches!(io.raw_os_error(), Some(23) | Some(24)) {
                return ConnectErrorClass::Fatal;
            }
            use std::io::ErrorKind as K;
            if matches!(
                io.kind(),
                K::ConnectionRefused
                    | K::ConnectionReset
                    | K::ConnectionAborted
                    | K::NotConnected
                    | K::TimedOut
                    | K::BrokenPipe
                    | K::UnexpectedEof
                    | K::HostUnreachable
                    | K::NetworkUnreachable
                    | K::NetworkDown
            ) {
                typed_transient = true;
            }
        }
    }

    if FATAL_MARKERS.iter().any(|m| chain.contains(m)) {
        return ConnectErrorClass::Fatal;
    }
    if typed_transient || TRANSIENT_MARKERS.iter().any(|m| chain.contains(m)) {
        ConnectErrorClass::Transient
    } else {
        ConnectErrorClass::Fatal
    }
}

/// Shorthand for `classify_connect_error(err) == Transient`.
pub fn is_transient_network_error(err: &anyhow::Error) -> bool {
    classify_connect_error(err) == ConnectErrorClass::Transient
}

/// Capped exponential backoff: `base * 2^n`, capped at `cap`, then reduced by
/// up to [`JITTER_FRACTION`].
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
        }
    }

    /// The un-jittered delay for the `attempt`-th retry (0-based).
    pub fn nominal(&self, attempt: u32) -> Duration {
        // 2^16 * any sane base is far beyond the cap; clamp the shift so the
        // multiplication cannot overflow however long the outage runs.
        let factor = 1u32 << attempt.min(16);
        self.base.saturating_mul(factor).min(self.cap)
    }

    /// The delay before the next attempt, jittered, advancing the schedule.
    pub fn next_delay(&mut self) -> Duration {
        let d = apply_jitter(self.nominal(self.attempt), unit_random());
        self.attempt = self.attempt.saturating_add(1);
        d
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BACKOFF_BASE, BACKOFF_CAP)
    }
}

/// Shave up to [`JITTER_FRACTION`] off `nominal`; `unit` is in `[0, 1)`.
pub fn apply_jitter(nominal: Duration, unit: f64) -> Duration {
    let unit = unit.clamp(0.0, 1.0);
    nominal.mul_f64(1.0 - JITTER_FRACTION * unit)
}

/// A dependency-free uniform-ish value in `[0, 1)`. Jitter only needs to
/// de-correlate processes, not be cryptographic: `RandomState` is seeded per
/// process, and the counter varies it per call.
fn unit_random() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// What an outage transition should log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutageLog {
    /// First transient failure: one WARN.
    Entered,
    /// Periodic reminder while the outage lasts.
    Reminder,
    /// Nothing above DEBUG.
    Quiet,
}

/// Tracks one outage: when it started, how many attempts it took, when the
/// last WARN went out, and the backoff schedule. Time is passed in so the
/// logging cadence is unit-testable without sleeping.
#[derive(Debug)]
pub struct OutageTracker {
    started: Option<Instant>,
    failures: u32,
    last_warn: Option<Instant>,
    reminder_every: Duration,
    backoff: Backoff,
}

impl Default for OutageTracker {
    fn default() -> Self {
        Self::new(Backoff::default(), OUTAGE_REMINDER_EVERY)
    }
}

impl OutageTracker {
    pub fn new(backoff: Backoff, reminder_every: Duration) -> Self {
        Self {
            started: None,
            failures: 0,
            last_warn: None,
            reminder_every,
            backoff,
        }
    }

    pub fn in_outage(&self) -> bool {
        self.started.is_some()
    }

    /// Failed attempts so far in the current outage.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Seconds since the outage started (0 when not in one).
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.started
            .map(|s| now.saturating_duration_since(s))
            .unwrap_or_default()
    }

    /// Record a transient failure observed at `now`. Returns what to log and
    /// how long to wait before the next attempt.
    pub fn on_transient_failure(&mut self, now: Instant) -> (OutageLog, Duration) {
        self.failures = self.failures.saturating_add(1);
        let log = match (self.started, self.last_warn) {
            (None, _) => {
                self.started = Some(now);
                self.last_warn = Some(now);
                OutageLog::Entered
            }
            (Some(_), Some(last)) if now.saturating_duration_since(last) >= self.reminder_every => {
                self.last_warn = Some(now);
                OutageLog::Reminder
            }
            _ => OutageLog::Quiet,
        };
        (log, self.backoff.next_delay())
    }

    /// Record a successful connect at `now`. If an outage was in progress,
    /// returns `(duration, attempts)` where `attempts` counts every connect
    /// attempt of the outage INCLUDING the successful one, and resets.
    pub fn on_success(&mut self, now: Instant) -> Option<(Duration, u32)> {
        let started = self.started.take()?;
        let out = (
            now.saturating_duration_since(started),
            self.failures.saturating_add(1),
        );
        self.failures = 0;
        self.last_warn = None;
        self.backoff.reset();
        Some(out)
    }
}

/// Result of [`connect_with_outage_retry`]. Returned once per connect and
/// matched immediately, so the large `Connected` variant is not boxed.
#[allow(clippy::large_enum_variant)]
pub enum ConnectOutcome {
    Connected(AgentClient),
    /// The `shutdown` future resolved (SIGTERM/SIGINT) before a connect succeeded.
    Shutdown,
    /// `max_fatal` consecutive NON-transient failures; the caller decides how
    /// to exit (the relay exits 1 for podman/systemd).
    Fatal(anyhow::Error),
}

/// Connect, waiting out network outages in-process.
///
/// * Transient failures ([`classify_connect_error`]) retry forever with
///   [`Backoff`], logging per [`OutageTracker`] (`label` prefixes every line).
/// * Non-transient failures are retried every [`FATAL_RETRY_DELAY`] up to
///   `max_fatal` consecutive times, then returned as
///   [`ConnectOutcome::Fatal`]. Transient failures in between neither count
///   nor reset that counter.
/// * `shutdown` is raced against every attempt AND every sleep, so SIGTERM
///   ends the wait immediately instead of after a 60 s backoff or a 30 s TCP
///   timeout.
pub async fn connect_with_outage_retry<S>(
    config: &AgentConfig,
    label: &str,
    max_fatal: u32,
    shutdown: S,
) -> ConnectOutcome
where
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut tracker = OutageTracker::default();
    let mut fatal: u32 = 0;
    loop {
        let res = tokio::select! {
            biased;
            _ = &mut shutdown => return ConnectOutcome::Shutdown,
            r = AgentClient::connect(config.clone()) => r,
        };
        let delay = match res {
            Ok(agent) => {
                if let Some((outage, attempts)) = tracker.on_success(Instant::now()) {
                    tracing::info!(
                        "{label}: network restored after {}s ({attempts} attempts)",
                        outage.as_secs()
                    );
                }
                return ConnectOutcome::Connected(agent);
            }
            Err(e) => match classify_connect_error(&e) {
                ConnectErrorClass::Transient => {
                    let now = Instant::now();
                    let (log, delay) = tracker.on_transient_failure(now);
                    match log {
                        OutageLog::Entered => tracing::warn!(
                            "{label}: network outage: waiting for connectivity (retrying in-process, \
                             backoff {}s..{}s; cause: {e:#})",
                            BACKOFF_BASE.as_secs(),
                            BACKOFF_CAP.as_secs(),
                        ),
                        OutageLog::Reminder => tracing::warn!(
                            "{label}: network outage: still waiting for connectivity after {}s \
                             ({} attempts; last error: {e:#})",
                            tracker.elapsed(now).as_secs(),
                            tracker.failures(),
                        ),
                        OutageLog::Quiet => tracing::debug!(
                            "{label}: connect attempt {} failed (network); next in {}ms: {e:#}",
                            tracker.failures(),
                            delay.as_millis(),
                        ),
                    }
                    delay
                }
                ConnectErrorClass::Fatal => {
                    fatal += 1;
                    tracing::error!(
                        "{label}: AgentClient::connect failed ({fatal}/{max_fatal}, non-transient): {e:#}"
                    );
                    if fatal >= max_fatal {
                        return ConnectOutcome::Fatal(e);
                    }
                    FATAL_RETRY_DELAY
                }
            },
        };
        tokio::select! {
            biased;
            _ = &mut shutdown => return ConnectOutcome::Shutdown,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn class(msg: &str) -> ConnectErrorClass {
        classify_connect_error(&anyhow!(msg.to_string()))
    }

    /// The exact shapes seen in the journal 2026-09-20..27 (URLs elided).
    #[test]
    fn observed_outage_errors_are_transient() {
        for msg in [
            "siwx-oidc authentication failed: GET /authorize failed: error sending request for url (https://siwx-oidc.inblock.io/authorize): client error (Connect): dns error: failed to lookup address information: Temporary failure in name resolution",
            "siwx-oidc authentication failed: GET /authorize failed: error sending request for url (x): client error (Connect): tcp connect error: Connection timed out (os error 110)",
            "siwx-oidc authentication failed: GET /authorize failed: error sending request for url (x): client error (Connect): tcp connect error: Connection refused (os error 111)",
            "initial sync failed: error sending request for url (x): client error (Connect): peer misbehaved: IllegalHelloRetryRequestWithWrongSessionId",
            "initial sync failed: error sending request for url (x): operation timed out",
            "POST /token (refresh) failed: error sending request for url (x): client error (Connect): tcp connect error: Connection timed out (os error 110)",
        ] {
            assert_eq!(class(msg), ConnectErrorClass::Transient, "{msg}");
        }
    }

    #[test]
    fn gateway_and_rate_limit_statuses_are_transient() {
        for msg in [
            "whoami returned 502 Bad Gateway: ",
            "/token refresh returned 503 Service Unavailable: upstream down",
            "siwx-oidc authentication failed: /sign_in returned 504 Gateway Timeout: ",
            "initial sync failed: the server returned an error: [502 Bad Gateway] <non-json bytes>",
            "initial sync failed: [429 Too Many Requests / M_LIMIT_EXCEEDED] slow down",
            "initial sync failed: error sending request: connection reset by peer",
            "initial sync failed: connection closed before message completed",
            "initial sync failed: client error (Connect): received fatal alert: HandshakeFailure",
            "client error (Connect): invalid peer certificate: UnknownIssuer",
            "Network is unreachable (os error 101)",
            "No route to host (os error 113)",
        ] {
            assert_eq!(class(msg), ConnectErrorClass::Transient, "{msg}");
        }
    }

    #[test]
    fn auth_store_and_config_errors_are_fatal() {
        for msg in [
            // The fresh-login bug another fix is chasing: must stay loud.
            "siwx-oidc authentication failed: /authorize returned 401 Unauthorized instead of 303",
            "whoami returned 401 Unauthorized: {\"errcode\":\"M_UNKNOWN_TOKEN\"}",
            "/token refresh returned 400 Bad Request: {\"error\":\"invalid_grant\"}",
            "failed to build Matrix client: Failed to load database version: database is locked: Error code 5: The database file is locked",
            "restore_session failed even after store wipe: the account in the store doesn't match",
            "failed to load key: PEM does not contain a recognized Ed25519 or P-256 PKCS#8 private key",
            "explicit device_id \"a b\" contains whitespace",
            "initial sync failed: [500 Internal Server Error / M_UNKNOWN] Internal server error",
            "something nobody has seen before",
        ] {
            assert_eq!(class(msg), ConnectErrorClass::Fatal, "{msg}");
        }
    }

    /// EMFILE arrives INSIDE a Connect error; the fd-leak signature must win.
    #[test]
    fn fd_exhaustion_beats_the_transient_markers() {
        for msg in [
            "error sending request: client error (Connect): tcp open error: Too many open files (os error 24)",
            "client error (Connect): dns error: Too many open files (os error 24)",
            "unable to open database file: Error code 14 (Too many open files)",
            "client error (Connect): No space left on device (os error 28)",
        ] {
            assert_eq!(class(msg), ConnectErrorClass::Fatal, "{msg}");
        }
    }

    #[test]
    fn typed_io_errors_are_classified_through_context() {
        let refused =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                .context("whoami request failed");
        assert_eq!(
            classify_connect_error(&refused),
            ConnectErrorClass::Transient
        );
        let emfile = anyhow::Error::new(std::io::Error::from_raw_os_error(24)).context("connect");
        assert_eq!(classify_connect_error(&emfile), ConnectErrorClass::Fatal);
        let perm = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("failed to write key");
        assert_eq!(classify_connect_error(&perm), ConnectErrorClass::Fatal);
    }

    #[test]
    fn backoff_schedule_doubles_from_2s_and_caps_at_60s() {
        let b = Backoff::default();
        let secs: Vec<u64> = (0..10).map(|n| b.nominal(n).as_secs()).collect();
        assert_eq!(secs, vec![2, 4, 8, 16, 32, 60, 60, 60, 60, 60]);
        // Never overflows, however long the outage.
        assert_eq!(b.nominal(u32::MAX), BACKOFF_CAP);
    }

    #[test]
    fn jitter_only_shortens_and_stays_within_20_percent() {
        let cap = BACKOFF_CAP;
        assert_eq!(apply_jitter(cap, 0.0), cap);
        assert_eq!(apply_jitter(cap, 1.0), Duration::from_secs(48));
        let mut b = Backoff::default();
        for n in 0..50 {
            let nominal = b.nominal(n);
            let d = b.next_delay();
            assert!(
                d <= nominal && d >= nominal.mul_f64(0.8),
                "attempt {n}: {d:?} vs {nominal:?}"
            );
            assert!(d <= BACKOFF_CAP);
        }
        b.reset();
        assert!(b.next_delay() <= BACKOFF_BASE);
    }

    #[test]
    fn outage_logs_once_then_reminds_every_5_min_then_restores() {
        let t0 = Instant::now();
        let mut tr = OutageTracker::default();
        assert!(!tr.in_outage());
        assert_eq!(tr.on_success(t0), None, "no outage, nothing to report");

        let (log, d) = tr.on_transient_failure(t0);
        assert_eq!(log, OutageLog::Entered);
        assert!(d <= BACKOFF_BASE);
        // Attempts every ~60 s for 12 minutes: reminders at +5 and +10 min only.
        let mut reminders = Vec::new();
        for s in (60..=720).step_by(60) {
            let (log, _) = tr.on_transient_failure(t0 + Duration::from_secs(s));
            assert_ne!(log, OutageLog::Entered);
            if log == OutageLog::Reminder {
                reminders.push(s);
            }
        }
        assert_eq!(reminders, vec![300, 600]);
        assert_eq!(tr.failures(), 13);

        let (dur, attempts) = tr.on_success(t0 + Duration::from_secs(750)).unwrap();
        assert_eq!(dur, Duration::from_secs(750));
        assert_eq!(attempts, 14, "13 failures + the successful attempt");
        assert!(!tr.in_outage());

        // A later outage starts from scratch: fresh WARN, backoff back at 2 s.
        let (log, d) = tr.on_transient_failure(t0 + Duration::from_secs(2000));
        assert_eq!(log, OutageLog::Entered);
        assert!(d <= BACKOFF_BASE);
    }

    fn test_config(tag: &str, key_file: Option<std::path::PathBuf>) -> AgentConfig {
        let dir = std::env::temp_dir().join(format!("aqua-net-retry-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        AgentConfig {
            key_file: key_file.unwrap_or_else(|| dir.join("agent.pem")),
            // Port 9 (discard) on loopback: nothing listens, so every attempt
            // is an immediate `Connection refused`, the stack-down signature.
            siwx_url: "http://127.0.0.1:9".into(),
            matrix_url: "http://127.0.0.1:9".into(),
            client_id: Some("test-client".into()),
            redirect_uri: Some("http://localhost/cb".into()),
            store_dir: dir,
            device_id: None,
            device_role: Default::default(),
        }
    }

    /// End to end through the real `AgentClient::connect`: an unreachable
    /// server never yields `Fatal` (max_fatal = 1 would return it on the first
    /// fatal classification) and SIGTERM-equivalent shutdown ends the wait
    /// promptly, mid-backoff.
    #[tokio::test]
    async fn refused_connection_waits_in_process_until_shutdown() {
        let config = test_config("refused", None);
        let started = std::time::Instant::now();
        let out = connect_with_outage_retry(
            &config,
            "test",
            1,
            tokio::time::sleep(Duration::from_secs(5)),
        )
        .await;
        let took = started.elapsed();
        assert!(
            matches!(out, ConnectOutcome::Shutdown),
            "outage must not end as Fatal/Connected"
        );
        assert!(
            took < Duration::from_secs(6),
            "shutdown must interrupt the backoff, took {took:?}"
        );
        let _ = std::fs::remove_dir_all(&config.store_dir);
    }

    /// A local, non-network failure keeps the exit path.
    #[tokio::test]
    async fn local_failure_is_fatal_after_max_attempts() {
        let config = test_config("fatal", Some("/nonexistent-dir/agent.pem".into()));
        let out = connect_with_outage_retry(&config, "test", 1, std::future::pending::<()>()).await;
        match out {
            ConnectOutcome::Fatal(e) => assert!(format!("{e:#}").contains("failed to write key")),
            _ => panic!("expected Fatal"),
        }
        let _ = std::fs::remove_dir_all(&config.store_dir);
    }
}
