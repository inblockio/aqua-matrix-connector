//! `operators.toml`: humans (by public DID) whom the hub invites into the
//! roster room so they can read it in Element. Public data only. Re-read when
//! the file changes. A missing file is an empty list. An operator is a reader
//! and writer of the room, not a seat: the roster is built from signed seat
//! claims, so an operator never appears in it.
//!
//! ```toml
//! [[operators]]
//! did = "did:pkh:eip155:1:0x..."
//! name = "tim"
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OperatorEntry {
    pub did: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    operators: Vec<OperatorEntry>,
}

pub fn valid_did(d: &str) -> bool {
    (d.starts_with("did:pkh:") || d.starts_with("did:key:"))
        && d.len() <= 200
        && d.chars().all(|c| c.is_ascii_alphanumeric() || c == ':')
}

/// Parse and validate the file text. A bad entry fails the whole file (closed).
pub fn parse(text: &str) -> Result<Vec<OperatorEntry>, String> {
    let f: File = toml::from_str(text).map_err(|e| format!("operators.toml: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for o in &f.operators {
        if !valid_did(&o.did) {
            return Err(format!("operators.toml: {:?} is not a usable DID", o.did));
        }
        if o.name.is_empty() || o.name.len() > 64 {
            return Err(format!("operators.toml: {:?} is not a usable name", o.name));
        }
        if !seen.insert(o.did.to_ascii_lowercase()) {
            return Err(format!("operators.toml: {} is listed twice", o.did));
        }
    }
    Ok(f.operators)
}

/// Retry delay after a failed invite or resolve.
const RETRY: Duration = Duration::from_secs(60);
/// "Done for this process": an operator who is in the room, or was invited, is
/// not invited again until the hub restarts (a deliberate leave stays left).
const DONE: Duration = Duration::from_secs(365 * 24 * 3600);

pub struct OperatorList {
    path: PathBuf,
    entries: Vec<OperatorEntry>,
    mtime: Option<SystemTime>,
    next_try: HashMap<String, Instant>,
}

impl OperatorList {
    pub fn new(path: PathBuf) -> Self {
        let mut l = Self {
            path,
            entries: Vec::new(),
            mtime: None,
            next_try: HashMap::new(),
        };
        l.reload();
        l
    }

    /// Re-read the file if it changed. A file that fails to parse empties the
    /// list (fail closed: nobody is invited).
    pub fn reload(&mut self) {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if mtime == self.mtime && (self.mtime.is_some() || self.entries.is_empty()) {
            return;
        }
        self.mtime = mtime;
        self.entries = match std::fs::read_to_string(&self.path) {
            Ok(t) => parse(&t).unwrap_or_else(|e| {
                tracing::warn!("{e}; no operator is invited until it is fixed");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
    }

    /// Operators whose invite is due now.
    pub fn due(&mut self) -> Vec<OperatorEntry> {
        self.reload();
        let now = Instant::now();
        self.entries
            .iter()
            .filter(|o| self.next_try.get(&o.did).is_none_or(|t| *t <= now))
            .cloned()
            .collect()
    }

    /// Record the outcome: `done` settles the operator for this process, else
    /// retry after a minute.
    pub fn settle(&mut self, did: &str, done: bool) {
        let after = if done { DONE } else { RETRY };
        self.next_try.insert(did.to_string(), Instant::now() + after);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIM: &str = "did:pkh:eip155:1:0x4B23da593596D94035c57Adf6C2454216449B1B2";

    #[test]
    fn parses_entries_and_refuses_bad_ones() {
        let t = format!("[[operators]]\ndid = \"{TIM}\"\nname = \"tim\"\n");
        let o = parse(&t).unwrap();
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].name, "tim");
        assert!(parse("").unwrap().is_empty());
        assert!(parse("[[operators]]\ndid = \"mailto:x\"\nname = \"x\"\n").is_err());
        assert!(parse(&format!("[[operators]]\ndid = \"{TIM}\"\nname = \"\"\n")).is_err());
        let twice = format!(
            "[[operators]]\ndid = \"{TIM}\"\nname = \"a\"\n[[operators]]\ndid = \"{}\"\nname = \"b\"\n",
            TIM.to_ascii_lowercase()
        );
        assert!(parse(&twice).is_err());
    }

    #[test]
    fn due_settle_and_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("operators.toml");
        let mut l = OperatorList::new(p.clone());
        assert!(l.due().is_empty(), "missing file: nobody");
        std::fs::write(&p, format!("[[operators]]\ndid = \"{TIM}\"\nname = \"tim\"\n")).unwrap();
        assert_eq!(l.due().len(), 1);
        l.settle(TIM, false);
        assert!(l.due().is_empty(), "a failed try waits");
        l.next_try.insert(TIM.to_string(), Instant::now());
        assert_eq!(l.due().len(), 1, "due again after the retry delay");
        l.settle(TIM, true);
        assert!(l.due().is_empty(), "settled for this process");
        std::fs::write(&p, "[[operators]]\ndid = 1\n").unwrap();
        // force the mtime to differ even on a coarse clock
        l.mtime = None;
        assert!(l.due().is_empty(), "a broken file invites nobody");
    }
}
