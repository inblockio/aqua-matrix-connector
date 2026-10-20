//! `seats/<seat>/meta.toml`: the non-secret facts about a seat.

use std::path::Path;

use anyhow::{Context, Result};
use aqua_seat::paths::write_private_atomic;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeatMeta {
    pub seat: String,
    pub did: String,
    /// Known after the first login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mxid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub host: String,
    pub created_ms: u64,
}

impl SeatMeta {
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing meta.toml")?;
        write_private_atomic(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_missing() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("meta.toml");
        assert!(SeatMeta::load(&p).is_none());
        let m = SeatMeta {
            seat: "a".into(),
            did: "did:key:z6MkX".into(),
            mxid: Some("@u:dev.matrix.inblock.io".into()),
            device_id: Some("AQUA_0123456789ab".into()),
            host: "h".into(),
            created_ms: 5,
        };
        m.save(&p).unwrap();
        assert_eq!(SeatMeta::load(&p), Some(m));
        std::fs::write(&p, "not = [toml").unwrap();
        assert!(SeatMeta::load(&p).is_none());
    }
}
