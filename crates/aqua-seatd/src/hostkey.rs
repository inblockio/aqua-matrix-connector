//! D8: the host identity. Each `aqua-seatd` has a host key `host.pem` (an
//! Ed25519 did:key, 0600) in its state root. It signs the endorsements that
//! let a seat on this host knock at the hub's roster room (D10). Only the
//! public DID ever leaves this module.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use aqua_seat::paths::create_private_file_new;
use siwx_oidc_auth::SiwxKey;

pub fn host_key_path(root: &Path) -> PathBuf {
    root.join("host.pem")
}

/// The host's public identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostId {
    pub did: String,
    /// True when this call created the key.
    pub minted: bool,
}

/// Make sure `host.pem` exists (0600, never overwritten) and return its DID.
pub fn ensure_host_key(root: &Path) -> Result<HostId> {
    aqua_seat::paths::ensure_private_dir(root)?;
    let path = host_key_path(root);
    let mut minted = false;
    if !path.exists() {
        let key = SiwxKey::generate_ed25519();
        let pem = key.to_pem().context("encoding the host key")?;
        minted = create_private_file_new(&path, pem.as_bytes())?;
    }
    let key = SiwxKey::from_pem_file(&path).context("loading the host key")?;
    Ok(HostId {
        did: key.did(),
        minted,
    })
}

/// The host's Ed25519 signing key (for endorsements).
pub fn load_signing_key(root: &Path) -> Result<ed25519_dalek::SigningKey> {
    match SiwxKey::from_pem_file(&host_key_path(root)).context("loading the host key")? {
        SiwxKey::Ed25519(k) => Ok(k),
        _ => bail!("the host key must be Ed25519"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_seat::paths::mode_of;

    #[test]
    fn mints_once_0600_and_keeps_its_did() {
        let t = tempfile::tempdir().unwrap();
        let a = ensure_host_key(t.path()).unwrap();
        assert!(a.minted && a.did.starts_with("did:key:z6Mk"), "{}", a.did);
        assert_eq!(mode_of(&host_key_path(t.path())).unwrap(), 0o600);
        let b = ensure_host_key(t.path()).unwrap();
        assert!(!b.minted);
        assert_eq!(a.did, b.did);
        let other = tempfile::tempdir().unwrap();
        assert_ne!(ensure_host_key(other.path()).unwrap().did, a.did);
        assert!(load_signing_key(t.path()).is_ok());
    }
}
