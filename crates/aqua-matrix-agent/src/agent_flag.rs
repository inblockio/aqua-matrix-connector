//! Self-declared agent flag: the custom profile field `io.inblock.agent` = `true`.
//!
//! At every connect an agent checks its own profile and sets the field when it
//! is missing (best effort, never fatal). A field that is already present is
//! left alone whatever its value, so an operator's deliberate value survives.
//! Which entry points do this by default, and the one switch, are decided by
//! [`flag_enabled`] (see `AGENT_SELF_FLAG`).

use anyhow::Result;
use std::future::Future;
use std::time::Duration;

/// The profile field an agent sets on itself.
pub const AGENT_FLAG_FIELD: &str = "io.inblock.agent";

/// The one switch. `0`/`false`/`off`/`no` never sets the flag; `1`/`true`/`on`/`yes` always
/// sets it; unset or anything else keeps the entry point's default.
pub const SELF_FLAG_ENV: &str = "AGENT_SELF_FLAG";

/// Upper bound for the whole check-and-set, so a slow homeserver cannot stall a connect.
pub(crate) const FLAG_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`ensure_agent_flag`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlagOutcome {
    /// The field was missing and is now set.
    Set,
    /// The field was already present (any value); nothing written.
    AlreadyPresent,
    /// Reading or writing failed or timed out; logged, never an error.
    Failed,
}

/// Which kind of caller is connecting; it sets the default of the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectKind {
    /// `AgentClient::connect`: person drivers (e2e harnesses), the one-shot CLI, probes.
    Plain,
    /// `connect_with_outage_retry`: the long-running agents (Scribe, consultants, bridge).
    LongRunning,
}

/// Read and write of the own profile field. A seam so the logic is testable without a server.
pub(crate) trait ProfileFlagStore {
    /// `Ok(true)` when the field is present (any value), `Ok(false)` when missing.
    fn is_present(&self) -> impl Future<Output = Result<bool>>;
    /// Set the field to `true`.
    fn set(&self) -> impl Future<Output = Result<()>>;
}

/// Whether to set the flag for this connect: the switch value `env` over the kind's default.
pub(crate) fn flag_enabled(_env: Option<&str>, _kind: ConnectKind) -> bool {
    false
}

/// Set the flag when missing. Never fails: every problem is logged and reported as [`FlagOutcome::Failed`].
pub(crate) async fn ensure_agent_flag<S: ProfileFlagStore>(_store: &S) -> FlagOutcome {
    FlagOutcome::Failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        present: Option<Result<bool, &'static str>>,
        set_result: Result<(), &'static str>,
        hang: bool,
        sets: AtomicUsize,
    }

    impl Fake {
        fn new(present: Result<bool, &'static str>) -> Self {
            Fake {
                present: Some(present),
                set_result: Ok(()),
                hang: false,
                sets: AtomicUsize::new(0),
            }
        }
    }

    impl ProfileFlagStore for Fake {
        async fn is_present(&self) -> Result<bool> {
            if self.hang {
                std::future::pending::<()>().await;
            }
            self.present.unwrap().map_err(|e| anyhow::anyhow!(e))
        }
        async fn set(&self) -> Result<()> {
            self.sets.fetch_add(1, Ordering::SeqCst);
            self.set_result.map_err(|e| anyhow::anyhow!(e))
        }
    }

    #[tokio::test]
    async fn ensure_when_missing_sets_the_flag_once() {
        let s = Fake::new(Ok(false));
        assert_eq!(ensure_agent_flag(&s).await, FlagOutcome::Set);
        assert_eq!(s.sets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn leave_alone_when_present() {
        let s = Fake::new(Ok(true));
        assert_eq!(ensure_agent_flag(&s).await, FlagOutcome::AlreadyPresent);
        assert_eq!(s.sets.load(Ordering::SeqCst), 0, "a present field is never rewritten");
    }

    #[tokio::test]
    async fn read_failure_does_not_fail_and_writes_nothing() {
        let s = Fake::new(Err("homeserver said 500"));
        assert_eq!(ensure_agent_flag(&s).await, FlagOutcome::Failed);
        assert_eq!(s.sets.load(Ordering::SeqCst), 0, "unknown state: do not write");
    }

    #[tokio::test]
    async fn write_failure_does_not_fail() {
        let mut s = Fake::new(Ok(false));
        s.set_result = Err("M_FORBIDDEN");
        assert_eq!(ensure_agent_flag(&s).await, FlagOutcome::Failed);
        assert_eq!(s.sets.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_homeserver_times_out_instead_of_stalling_connect() {
        let mut s = Fake::new(Ok(false));
        s.hang = true;
        assert_eq!(ensure_agent_flag(&s).await, FlagOutcome::Failed);
        assert_eq!(s.sets.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn switch_off_wins_over_every_default() {
        for v in ["0", "false", "off", "no", "OFF", " False "] {
            assert!(!flag_enabled(Some(v), ConnectKind::LongRunning), "{v}");
            assert!(!flag_enabled(Some(v), ConnectKind::Plain), "{v}");
        }
    }

    #[test]
    fn switch_on_forces_the_flag_for_plain_connect() {
        for v in ["1", "true", "on", "yes", "ON"] {
            assert!(flag_enabled(Some(v), ConnectKind::Plain), "{v}");
            assert!(flag_enabled(Some(v), ConnectKind::LongRunning), "{v}");
        }
    }

    /// DD8: with no switch set, people (plain connect: e2e drivers, one-shot CLI) end
    /// unflagged and the long-running agents end flagged.
    #[test]
    fn default_follows_the_entry_point() {
        for v in [None, Some(""), Some("maybe")] {
            assert!(!flag_enabled(v, ConnectKind::Plain), "{v:?}");
            assert!(flag_enabled(v, ConnectKind::LongRunning), "{v:?}");
        }
    }
}
