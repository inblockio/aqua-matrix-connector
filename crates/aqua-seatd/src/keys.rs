//! Key minting. A seat's Ed25519 key is created once, with mode 0600 and
//! `create_new` (so two racing attaches cannot overwrite each other), and is
//! never read back by anything but the connector's own key loader.

use anyhow::{Context, Result};
use aqua_seat::paths::{create_private_file_new, SeatPaths};
use siwx_oidc_auth::SiwxKey;

/// What `ensure_key` found or made. Only the public DID leaves this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub did: String,
    /// True when this call created the key (a new identity).
    pub minted: bool,
}

/// Make sure `agent.pem` exists for the seat (creating the 0700 seat dir and
/// the 0600 key if new) and return its DID.
pub fn ensure_key(paths: &SeatPaths) -> Result<KeyInfo> {
    paths.ensure_dir()?;
    let key_path = paths.key();
    let mut minted = false;
    if !key_path.exists() {
        let key = SiwxKey::generate_ed25519();
        let pem = key.to_pem().context("encoding the new key")?;
        minted = create_private_file_new(&key_path, pem.as_bytes())?;
    }
    let key = SiwxKey::from_pem_file(&key_path).context("loading the seat key")?;
    Ok(KeyInfo {
        did: key.did(),
        minted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_seat::paths::mode_of;

    #[test]
    fn mints_once_with_private_modes_and_a_stable_did() {
        let t = tempfile::tempdir().unwrap();
        let p = SeatPaths::new(t.path(), "seat-test-x").unwrap();
        let a = ensure_key(&p).unwrap();
        assert!(a.minted);
        assert!(a.did.starts_with("did:key:z6Mk"), "{}", a.did);
        assert_eq!(mode_of(&p.key()).unwrap(), 0o600);
        assert_eq!(mode_of(&p.dir).unwrap(), 0o700);
        let b = ensure_key(&p).unwrap();
        assert!(!b.minted);
        assert_eq!(a.did, b.did, "a DID never changes");
        let other = SeatPaths::new(t.path(), "seat-test-y").unwrap();
        assert_ne!(ensure_key(&other).unwrap().did, a.did);
    }
}
