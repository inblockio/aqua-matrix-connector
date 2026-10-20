//! `hosts.toml`: the hub's allow-list of hosts (host DID -> host name).
//! Public data only. Re-read when the file changes, so adding a host needs no
//! restart. A missing file is an empty list: no remote host is accepted.
//!
//! ```toml
//! [[hosts]]
//! did = "did:key:z6Mk..."
//! name = "clawi"
//! ```

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HostEntry {
    pub did: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    hosts: Vec<HostEntry>,
}

pub fn valid_did(d: &str) -> bool {
    d.starts_with("did:key:z6Mk")
        && d.len() <= 100
        && d.chars().all(|c| c.is_ascii_alphanumeric() || c == ':')
}

pub fn valid_host_name(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 64
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Parse and validate the file text. A bad entry fails the whole file (closed).
pub fn parse(text: &str) -> Result<Vec<HostEntry>, String> {
    let f: File = toml::from_str(text).map_err(|e| format!("hosts.toml: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for h in &f.hosts {
        if !valid_did(&h.did) {
            return Err(format!("hosts.toml: {:?} is not an Ed25519 did:key", h.did));
        }
        if !valid_host_name(&h.name) {
            return Err(format!(
                "hosts.toml: {:?} is not a usable host name",
                h.name
            ));
        }
        if !seen.insert(h.did.clone()) {
            return Err(format!("hosts.toml: {} is listed twice", h.did));
        }
    }
    Ok(f.hosts)
}

pub struct HostList {
    path: PathBuf,
    entries: Vec<HostEntry>,
    mtime: Option<SystemTime>,
    error: Option<String>,
}

impl HostList {
    pub fn new(path: PathBuf) -> Self {
        let mut l = Self {
            path,
            entries: Vec::new(),
            mtime: None,
            error: None,
        };
        l.reload();
        l
    }

    /// Re-read the file if it changed. A file that fails to parse keeps the
    /// previous list out of service: the list becomes empty (fail closed).
    pub fn reload(&mut self) {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if mtime == self.mtime && (self.mtime.is_some() || self.entries.is_empty()) {
            return;
        }
        self.mtime = mtime;
        match std::fs::read_to_string(&self.path) {
            Ok(t) => match parse(&t) {
                Ok(e) => {
                    self.entries = e;
                    self.error = None;
                }
                Err(e) => {
                    tracing::warn!("{e}; no remote host is accepted until it is fixed");
                    self.entries.clear();
                    self.error = Some(e);
                }
            },
            Err(_) => {
                self.entries.clear();
                self.error = None;
            }
        }
    }

    pub fn lookup(&mut self, did: &str) -> Option<HostEntry> {
        self.reload();
        self.entries.iter().find(|h| h.did == did).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1: &str = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK";
    const D2: &str = "did:key:z6MktWjP95fMBwGi2L8qZ6nH8rbyWiYjVhHp6Bv4qFuaAJ8e";

    #[test]
    fn parses_entries() {
        let t = format!("[[hosts]]\ndid = \"{D1}\"\nname = \"clawi\"\n[[hosts]]\ndid = \"{D2}\"\nname = \"system-001\"\n");
        let h = parse(&t).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].name, "clawi");
        assert_eq!(parse("").unwrap().len(), 0);
    }

    #[test]
    fn a_bad_entry_fails_the_whole_file() {
        for bad in [
            "[[hosts]]\ndid = \"did:web:x\"\nname = \"a\"\n",
            &format!("[[hosts]]\ndid = \"{D1}\"\nname = \"a b\"\n"),
            &format!("[[hosts]]\ndid = \"{D1}\"\nname = \"a\"\n[[hosts]]\ndid = \"{D1}\"\nname = \"b\"\n"),
            "[[hosts]]\nname = \"a\"\n",
            "not toml [",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn list_is_hot_reloaded_and_fails_closed() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("hosts.toml");
        let mut l = HostList::new(p.clone());
        assert!(l.lookup(D1).is_none(), "missing file = nobody allowed");
        std::fs::write(&p, format!("[[hosts]]\ndid = \"{D1}\"\nname = \"clawi\"\n")).unwrap();
        assert_eq!(l.lookup(D1).unwrap().name, "clawi");
        assert!(l.lookup(D2).is_none());
        // a broken edit does not leave the old list in force
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, "[[hosts]]\ndid = \"nope\"\nname = \"x\"\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::open(&p)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(l.lookup(D1).is_none());
    }
}
