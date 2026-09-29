//! The recipient allow-list: the only people the bridge will message, and the
//! only senders whose messages reach the inbox.
//!
//! File: `<state dir>/allowlist.toml`, for example:
//!
//! ```toml
//! [[recipients]]
//! name = "tim"
//! mxid = "@someone:matrix.inblock.io"
//! ```
//!
//! The daemon re-reads the file whenever its modification time changes, so an
//! edit takes effect on the next request without a restart. A file that is
//! missing or does not parse yields an EMPTY list (fail closed): nobody can be
//! messaged and every inbound message is dropped until it is fixed.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recipient {
    /// Short handle sessions use in `to` / `from` (case-insensitive).
    pub name: String,
    /// Full Matrix user id, e.g. `@abc:matrix.inblock.io`.
    pub mxid: String,
    /// Optional human-readable note (full name, role).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AllowListFile {
    #[serde(default)]
    recipients: Vec<Recipient>,
}

/// Structural MXID check: `@localpart:server`, no whitespace.
pub fn is_valid_mxid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('@') else {
        return false;
    };
    let Some((local, server)) = rest.split_once(':') else {
        return false;
    };
    !local.is_empty() && !server.is_empty() && !s.chars().any(char::is_whitespace)
}

/// Parse allow-list TOML. Invalid entries (bad MXID, empty or duplicate name)
/// are an error for the whole file, so a typo can never silently widen or
/// narrow the list.
pub fn parse(text: &str) -> Result<Vec<Recipient>, String> {
    let file: AllowListFile =
        toml::from_str(text).map_err(|e| format!("allow-list parse error: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for r in &file.recipients {
        if r.name.trim().is_empty() {
            return Err(format!("allow-list entry for {} has an empty name", r.mxid));
        }
        if !is_valid_mxid(&r.mxid) {
            return Err(format!(
                "allow-list entry {:?}: {:?} is not a valid MXID (@localpart:server)",
                r.name, r.mxid
            ));
        }
        if !seen.insert(r.name.to_ascii_lowercase()) {
            return Err(format!("allow-list has duplicate name {:?}", r.name));
        }
    }
    Ok(file.recipients)
}

/// The allow-list plus the file it came from, reloaded on mtime change.
pub struct AllowList {
    path: PathBuf,
    mtime: Option<SystemTime>,
    recipients: Vec<Recipient>,
    load_error: Option<String>,
}

impl AllowList {
    pub fn new(path: PathBuf) -> Self {
        let mut a = Self {
            path,
            mtime: None,
            recipients: Vec::new(),
            load_error: None,
        };
        a.reload(true);
        a
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file if its mtime changed (or `force`). Returns true when a
    /// reload happened.
    pub fn reload(&mut self, force: bool) -> bool {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if !force && mtime == self.mtime && self.mtime.is_some() {
            return false;
        }
        self.mtime = mtime;
        match std::fs::read_to_string(&self.path) {
            Ok(text) => match parse(&text) {
                Ok(list) => {
                    tracing::info!(count = list.len(), path = %self.path.display(), "allow-list loaded");
                    self.recipients = list;
                    self.load_error = None;
                }
                Err(e) => {
                    tracing::error!(path = %self.path.display(), "allow-list rejected, failing closed: {e}");
                    self.recipients.clear();
                    self.load_error = Some(e);
                }
            },
            Err(e) => {
                tracing::error!(path = %self.path.display(), "allow-list unreadable, failing closed: {e}");
                self.recipients.clear();
                self.load_error = Some(format!("cannot read {}: {e}", self.path.display()));
            }
        }
        true
    }

    pub fn recipients(&self) -> &[Recipient] {
        &self.recipients
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Resolve a `to`/`from` argument (name or MXID) to an allow-listed
    /// recipient. Names compare case-insensitively; MXIDs compare ASCII
    /// case-insensitively (Synapse lowercases localparts).
    pub fn resolve(&self, who: &str) -> Option<&Recipient> {
        let who = who.trim();
        self.recipients
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(who) || r.mxid.eq_ignore_ascii_case(who))
    }

    /// The allow-listed recipient whose MXID is `sender`, if any.
    pub fn by_mxid(&self, sender: &str) -> Option<&Recipient> {
        self.recipients
            .iter()
            .find(|r| r.mxid.eq_ignore_ascii_case(sender))
    }

    /// The error a session sees when `who` is not on the list.
    pub fn refusal(&self, who: &str) -> String {
        if let Some(e) = &self.load_error {
            return format!("REFUSED: the allow-list failed to load ({e}); no one can be messaged until {} is fixed", self.path.display());
        }
        let names: Vec<&str> = self.recipients.iter().map(|r| r.name.as_str()).collect();
        format!(
            "REFUSED: {who:?} is not on the Aqua System allow-list. Allowed recipients: [{}]. \
             To add someone, append a [[recipients]] entry to {} (no restart needed), and only \
             with Tim's approval.",
            names.join(", "),
            self.path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[[recipients]]
name = "tim"
mxid = "@abc123:matrix.inblock.io"
note = "Tim Bansemer"

[[recipients]]
name = "Kenn"
mxid = "@def456:matrix.inblock.io"
"#;

    fn list(text: &str) -> AllowList {
        AllowList {
            path: PathBuf::from("/nonexistent"),
            mtime: None,
            recipients: parse(text).unwrap(),
            load_error: None,
        }
    }

    #[test]
    fn parses_and_resolves_by_name_or_mxid() {
        let a = list(GOOD);
        assert_eq!(a.resolve("tim").unwrap().mxid, "@abc123:matrix.inblock.io");
        assert_eq!(a.resolve("TIM").unwrap().name, "tim");
        assert_eq!(a.resolve("kenn").unwrap().name, "Kenn");
        assert_eq!(a.resolve("@ABC123:matrix.inblock.io").unwrap().name, "tim");
        assert!(a.resolve("mallory").is_none());
        assert!(a.resolve("@abc123:evil.example").is_none());
    }

    #[test]
    fn by_mxid_only_matches_mxids() {
        let a = list(GOOD);
        assert!(a.by_mxid("@abc123:matrix.inblock.io").is_some());
        assert!(a.by_mxid("tim").is_none());
    }

    #[test]
    fn rejects_bad_entries() {
        assert!(parse("[[recipients]]\nname = \"x\"\nmxid = \"abc:server\"\n").is_err());
        assert!(parse("[[recipients]]\nname = \"x\"\nmxid = \"@abc\"\n").is_err());
        assert!(parse("[[recipients]]\nname = \" \"\nmxid = \"@a:b\"\n").is_err());
        let dup = "[[recipients]]\nname = \"x\"\nmxid = \"@a:b\"\n[[recipients]]\nname = \"X\"\nmxid = \"@c:d\"\n";
        assert!(parse(dup).is_err());
        assert!(parse("not toml [[").is_err());
    }

    #[test]
    fn empty_file_is_empty_list() {
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn missing_file_fails_closed() {
        let a = AllowList::new(PathBuf::from("/nonexistent/allowlist.toml"));
        assert!(a.recipients().is_empty());
        assert!(a.load_error().is_some());
        assert!(a.refusal("tim").contains("failed to load"));
    }

    #[test]
    fn refusal_names_the_allowed() {
        let a = list(GOOD);
        let r = a.refusal("mallory");
        assert!(r.starts_with("REFUSED"));
        assert!(r.contains("tim, Kenn"));
    }
}
