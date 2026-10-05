//! Primary and secondary devices of one identity.
//!
//! A DID maps to one Matrix account, and every client of that account is a
//! device. The PRIMARY device owns the account-wide crypto state: it
//! bootstraps cross-signing, enables (and, without a persisted key, rotates)
//! secret storage, and after a store wipe deletes the account's other devices.
//!
//! A SECONDARY device runs the same identity on another host, next to a primary
//! that stays in charge. It must never create or replace account-wide state,
//! because the primary and every recipient rely on it:
//! - a cross-signing bootstrap mints a new master key, an identity reset that
//!   every recipient sees;
//! - enabling recovery rotates the secret-storage key and orphans the
//!   primary's recovery key;
//! - a prune deletes the primary's device;
//! - the derived device_id IS the primary's device, and two crypto stores on
//!   one device break both (one-time-key collision).
//!
//! So a secondary needs an explicit device_id of its own plus the identity's
//! key and recovery key, takes its cross-signing keys only from that recovery
//! key, and fails to connect instead of falling back to any of the above.

use std::path::Path;

use anyhow::{bail, Result};

/// How a client treats the account-wide crypto state of its identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeviceRole {
    /// The identity's own device (the default): bootstraps cross-signing and
    /// secret storage when they are missing, wipes a crypto store that belongs
    /// to another device and then prunes the account's other devices.
    #[default]
    Primary,
    /// An additional device of an identity whose primary runs elsewhere: never
    /// creates, rotates or deletes account-wide state (see the module docs).
    Secondary,
}

impl DeviceRole {
    /// Whether this device may wipe a crypto store bound to another device_id
    /// (and then prune the account's other devices).
    pub(crate) fn may_wipe_store(self) -> bool {
        self == DeviceRole::Primary
    }

    /// Whether this device may enable or rotate server-side secret storage.
    pub(crate) fn may_manage_secret_storage(self) -> bool {
        self == DeviceRole::Primary
    }
}

/// What `connect` does about cross-signing after the recovery-key restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrossSigning {
    /// The private keys are present: nothing to do.
    Present,
    /// Primary without keys: bootstrap them.
    Bootstrap,
    /// Secondary without keys: refuse to connect.
    Refuse,
}

pub(crate) fn cross_signing_action(role: DeviceRole, complete: bool) -> CrossSigning {
    match (complete, role) {
        (true, _) => CrossSigning::Present,
        (false, DeviceRole::Primary) => CrossSigning::Bootstrap,
        (false, DeviceRole::Secondary) => CrossSigning::Refuse,
    }
}

/// Preconditions of a secondary device, checked before it logs in: an explicit
/// device_id that is not the primary's derived one, and the identity's
/// recovery key in the store.
pub(crate) fn check_secondary(
    explicit_device_id: Option<&str>,
    primary_device_id: &str,
    recovery_key: &Path,
) -> Result<()> {
    let Some(id) = explicit_device_id.map(str::trim).filter(|s| !s.is_empty()) else {
        bail!(
            "a secondary device needs an explicit device_id; the derived one \
             ({primary_device_id}) is the primary's device"
        );
    };
    if id == primary_device_id {
        bail!("device_id {id} is the primary's device; a secondary device needs its own");
    }
    if !recovery_key.is_file() {
        bail!(
            "a secondary device needs the identity's recovery key at {} (copied from the \
             primary); without it, it could neither restore cross-signing nor leave secret \
             storage alone",
            recovery_key.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_is_the_default() {
        assert_eq!(DeviceRole::default(), DeviceRole::Primary);
    }

    #[test]
    fn only_the_primary_wipes_or_manages_secret_storage() {
        assert!(DeviceRole::Primary.may_wipe_store());
        assert!(DeviceRole::Primary.may_manage_secret_storage());
        assert!(!DeviceRole::Secondary.may_wipe_store());
        assert!(!DeviceRole::Secondary.may_manage_secret_storage());
    }

    #[test]
    fn cross_signing_is_bootstrapped_only_by_a_primary() {
        use CrossSigning::*;
        assert_eq!(cross_signing_action(DeviceRole::Primary, true), Present);
        assert_eq!(cross_signing_action(DeviceRole::Secondary, true), Present);
        assert_eq!(cross_signing_action(DeviceRole::Primary, false), Bootstrap);
        assert_eq!(cross_signing_action(DeviceRole::Secondary, false), Refuse);
    }

    /// A fresh store dir; with `key`, it holds a recovery key file.
    fn store_dir(tag: &str, key: bool) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aqua-device-role-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if key {
            std::fs::write(dir.join("recovery.key"), "EsT0 test key\n").unwrap();
        }
        dir
    }

    #[test]
    fn secondary_with_own_device_and_recovery_key_passes() {
        let dir = store_dir("ok", true);
        check_secondary(
            Some("AQUA_SYSTEM_CLAWI"),
            "AQUA_7fb6d868093d",
            &dir.join("recovery.key"),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secondary_without_explicit_device_is_refused() {
        let dir = store_dir("no-device", true);
        for explicit in [None, Some(""), Some("   ")] {
            let err = check_secondary(explicit, "AQUA_7fb6d868093d", &dir.join("recovery.key"))
                .unwrap_err();
            assert!(err.to_string().contains("explicit device_id"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secondary_on_the_primary_device_is_refused() {
        let dir = store_dir("primary-device", true);
        let err = check_secondary(
            Some(" AQUA_7fb6d868093d "),
            "AQUA_7fb6d868093d",
            &dir.join("recovery.key"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("primary's device"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secondary_without_recovery_key_is_refused() {
        let dir = store_dir("no-key", false);
        let err = check_secondary(
            Some("AQUA_SYSTEM_CLAWI"),
            "AQUA_7fb6d868093d",
            &dir.join("recovery.key"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("recovery key"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
