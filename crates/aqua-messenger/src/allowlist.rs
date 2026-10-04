//! The recipient allow-list: the only people the bridge will message, and the
//! only senders whose messages reach the inbox.
//!
//! File: `<state dir>/allowlist.toml`, for example:
//!
//! ```toml
//! [[recipients]]
//! name = "tim"
//! mxid = "@someone:matrix.inblock.io"
//!
//! [[rooms]]
//! name = "daily-updates"
//! room_id = "!abc:matrix.inblock.io"
//! note = "Daily Updates group"
//! ```
//!
//! `[[recipients]]` are people (DMs); `[[rooms]]` are group rooms the bridge
//! may post into and whose messages reach the inbox. A room the bridge joined
//! but that is not listed is never sendable and its messages are dropped.
//! Names are unique across both lists (case-insensitive).
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

/// A group room the bridge may post into (`[[rooms]]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoomEntry {
    /// Short handle sessions use in `to` / `from` (case-insensitive).
    pub name: String,
    /// Matrix room id, e.g. `!abc:matrix.inblock.io` (compared exactly).
    pub room_id: String,
    /// Optional human-readable note (purpose, audience).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AllowListFile {
    #[serde(default)]
    recipients: Vec<Recipient>,
    #[serde(default)]
    rooms: Vec<RoomEntry>,
}

/// A parsed allow-list file.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    pub recipients: Vec<Recipient>,
    pub rooms: Vec<RoomEntry>,
}

/// What a `to` / `from` argument resolved to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target<'a> {
    Person(&'a Recipient),
    Room(&'a RoomEntry),
}

/// Structural room-id check: `!opaque:server`, no whitespace.
pub fn is_valid_room_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('!') else {
        return false;
    };
    let Some((local, server)) = rest.split_once(':') else {
        return false;
    };
    !local.is_empty() && !server.is_empty() && !s.chars().any(char::is_whitespace)
}

/// What the daemon does with an invite (pure decision, see `handle_invite`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteAction {
    /// Inviter is not allow-listed: leave the room.
    Decline,
    /// A real 1:1 DM invite: join and record it in `m.direct` for the inviter.
    JoinAsDm,
    /// A group room (or a listed room): join, but NEVER record it as a DM, so
    /// a later `send_message(to: <person>)` cannot land in it.
    JoinAsGroup,
}

/// Decide an invite. Only an invite that the inviter flagged `is_direct` and
/// that is not a configured `[[rooms]]` entry is treated as a DM (2026-09-29
/// incident: two group-room invites were marked as Tim's DMs).
pub fn invite_action(inviter_allowed: bool, is_direct: bool, room_listed: bool) -> InviteAction {
    if !inviter_allowed {
        InviteAction::Decline
    } else if is_direct && !room_listed {
        InviteAction::JoinAsDm
    } else {
        InviteAction::JoinAsGroup
    }
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

/// Parse allow-list TOML (people only; see [`parse_all`]).
pub fn parse(text: &str) -> Result<Vec<Recipient>, String> {
    parse_all(text).map(|p| p.recipients)
}

/// Parse allow-list TOML. Invalid entries (bad MXID or room id, empty or
/// duplicate name, duplicate room id) are an error for the whole file, so a
/// typo can never silently widen or narrow the list.
pub fn parse_all(text: &str) -> Result<Parsed, String> {
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
        if !seen.insert(r.name.trim().to_ascii_lowercase()) {
            return Err(format!("allow-list has duplicate name {:?}", r.name));
        }
    }
    let mut seen_ids = std::collections::HashSet::new();
    for r in &file.rooms {
        let name = r.name.trim();
        if name.is_empty() {
            return Err(format!(
                "[[rooms]] entry for {} has an empty name",
                r.room_id
            ));
        }
        if name.starts_with('@') || name.starts_with('!') {
            return Err(format!(
                "[[rooms]] name {:?} must be a short handle, not an MXID or room id",
                r.name
            ));
        }
        if !is_valid_room_id(r.room_id.trim()) {
            return Err(format!(
                "[[rooms]] entry {:?}: {:?} is not a valid room id (!opaque:server)",
                r.name, r.room_id
            ));
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!("allow-list has duplicate name {:?}", r.name));
        }
        if !seen_ids.insert(r.room_id.trim().to_string()) {
            return Err(format!("[[rooms]] lists room {:?} twice", r.room_id));
        }
    }
    let rooms = file
        .rooms
        .into_iter()
        .map(|r| RoomEntry {
            name: r.name.trim().to_string(),
            room_id: r.room_id.trim().to_string(),
            note: r.note,
        })
        .collect();
    Ok(Parsed {
        recipients: file.recipients,
        rooms,
    })
}

/// The allow-list plus the file it came from, reloaded on mtime change.
pub struct AllowList {
    /// The allow-list file, if any (hot-reloaded on mtime change).
    path: Option<PathBuf>,
    mtime: Option<SystemTime>,
    /// Entries that are always present (an embedded agent's owner),
    /// independent of the file; a file can never re-point or remove them.
    fixed: Vec<Recipient>,
    recipients: Vec<Recipient>,
    rooms: Vec<RoomEntry>,
    load_error: Option<String>,
    /// Identity named in refusals ("the Aqua System allow-list").
    label: String,
    /// Who must approve additions ("Tim", "the agent's owner").
    approver: String,
}

impl AllowList {
    /// The host bridge's list: the file only (a missing or bad file is an
    /// EMPTY list, fail closed).
    pub fn new(path: PathBuf) -> Self {
        Self::build(Some(path), Vec::new(), "Aqua System", "Tim")
    }

    /// The embedded-agent default: ONLY the owner. More people or rooms need
    /// an explicit `extra_file` (same TOML: `[[recipients]]` + `[[rooms]]`),
    /// merged on top and hot-reloaded. A missing extra file is simply
    /// owner-only; a broken one fails closed back to owner-only (never to
    /// nobody, never to more). The owner's name cannot be re-pointed by the
    /// file.
    pub fn owner_only(owner: Recipient, extra_file: Option<PathBuf>, label: &str) -> Self {
        Self::build(extra_file, vec![owner], label, "the agent's owner")
    }

    fn build(path: Option<PathBuf>, fixed: Vec<Recipient>, label: &str, approver: &str) -> Self {
        let mut a = Self {
            path,
            mtime: None,
            recipients: fixed.clone(),
            fixed,
            rooms: Vec::new(),
            load_error: None,
            label: label.to_string(),
            approver: approver.to_string(),
        };
        a.reload(true);
        a
    }

    /// The allow-list file, if this list has one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The file as shown in messages (or "the messenger configuration").
    pub fn path_display(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "the messenger configuration".into())
    }

    /// Re-read the file if its mtime changed (or `force`). Returns true when a
    /// reload happened. A list without a file never reloads.
    pub fn reload(&mut self, force: bool) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if !force && mtime == self.mtime && self.mtime.is_some() {
            return false;
        }
        self.mtime = mtime;
        let mut recipients = self.fixed.clone();
        let mut rooms = Vec::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_all(&text).and_then(|p| self.merge_checked(p)) {
                Ok(p) => {
                    for e in p.recipients {
                        if !recipients
                            .iter()
                            .any(|r| r.mxid.eq_ignore_ascii_case(&e.mxid))
                        {
                            recipients.push(e);
                        }
                    }
                    rooms = p.rooms;
                    tracing::info!(count = recipients.len(), rooms = rooms.len(), path = %path.display(), "allow-list loaded");
                    self.load_error = None;
                }
                Err(e) => {
                    tracing::error!(path = %path.display(), "allow-list rejected, failing closed: {e}");
                    self.load_error = Some(e);
                }
            },
            // An embedded agent's optional extra file may simply not exist.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.fixed.is_empty() => {
                self.load_error = None;
            }
            Err(e) => {
                tracing::error!(path = %path.display(), "allow-list unreadable, failing closed: {e}");
                self.load_error = Some(format!("cannot read {}: {e}", path.display()));
            }
        }
        self.recipients = recipients;
        self.rooms = rooms;
        true
    }

    /// Reject a file that reuses a built-in (owner) name for another MXID or
    /// names a room like the owner.
    fn merge_checked(&self, p: Parsed) -> Result<Parsed, String> {
        for f in &self.fixed {
            if let Some(c) = p.recipients.iter().find(|e| {
                e.name.eq_ignore_ascii_case(&f.name) && !e.mxid.eq_ignore_ascii_case(&f.mxid)
            }) {
                return Err(format!(
                    "allow-list entry {:?} reuses the built-in name {:?} for a different MXID",
                    c.name, f.name
                ));
            }
            if let Some(r) = p
                .rooms
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case(&f.name))
            {
                return Err(format!(
                    "[[rooms]] entry {:?} reuses the built-in name {:?}",
                    r.name, f.name
                ));
            }
        }
        Ok(p)
    }

    pub fn recipients(&self) -> &[Recipient] {
        &self.recipients
    }

    pub fn rooms(&self) -> &[RoomEntry] {
        &self.rooms
    }

    /// The listed room with this exact room id, if any.
    pub fn room_by_id(&self, room_id: &str) -> Option<&RoomEntry> {
        self.rooms.iter().find(|r| r.room_id == room_id.trim())
    }

    /// True when `room_id` is a `[[rooms]]` entry (never a DM).
    pub fn is_listed_room(&self, room_id: &str) -> bool {
        self.room_by_id(room_id).is_some()
    }

    /// Resolve a `to`/`from` argument to a listed room: its name
    /// (case-insensitive) or its exact room id.
    pub fn resolve_room(&self, who: &str) -> Option<&RoomEntry> {
        let who = who.trim();
        self.rooms
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(who) || r.room_id == who)
    }

    /// Resolve a `to`/`from` argument to a person or a listed room.
    pub fn resolve_target(&self, who: &str) -> Option<Target<'_>> {
        if let Some(r) = self.resolve(who) {
            return Some(Target::Person(r));
        }
        self.resolve_room(who).map(Target::Room)
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
        let file = self.path_display();
        if let Some(e) = &self.load_error {
            if self.fixed.is_empty() {
                return format!("REFUSED: the allow-list failed to load ({e}); no one can be messaged until {file} is fixed");
            }
        }
        let names: Vec<&str> = self.recipients.iter().map(|r| r.name.as_str()).collect();
        let rooms: Vec<&str> = self.rooms.iter().map(|r| r.name.as_str()).collect();
        format!(
            "REFUSED: {who:?} is not on the {} allow-list. Allowed recipients: [{}]; \
             allowed rooms: [{}]. To add someone, append a [[recipients]] entry (a group room: a \
             [[rooms]] entry) to {file} (no restart needed), and only with {}'s approval.",
            self.label,
            names.join(", "),
            rooms.join(", "),
            self.approver
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

    const WITH_ROOMS: &str = r#"
[[recipients]]
name = "tim"
mxid = "@abc123:matrix.inblock.io"

[[rooms]]
name = "daily-updates"
room_id = "!HmZvPSoRvkqikWEvwD:matrix.inblock.io"
note = "Daily Updates"

[[rooms]]
name = "aqua-internal"
room_id = " !rvKzMvUBBaewXApthx:matrix.inblock.io "
"#;

    fn list(text: &str) -> AllowList {
        let p = parse_all(text).unwrap();
        AllowList {
            path: Some(PathBuf::from("/nonexistent")),
            mtime: None,
            fixed: Vec::new(),
            recipients: p.recipients,
            rooms: p.rooms,
            load_error: None,
            label: "Aqua System".into(),
            approver: "Tim".into(),
        }
    }

    #[test]
    fn parses_rooms_and_stays_backward_compatible() {
        let p = parse_all(WITH_ROOMS).unwrap();
        assert_eq!(p.recipients.len(), 1);
        assert_eq!(p.rooms.len(), 2);
        assert_eq!(p.rooms[0].note.as_deref(), Some("Daily Updates"));
        // room ids are trimmed
        assert_eq!(p.rooms[1].room_id, "!rvKzMvUBBaewXApthx:matrix.inblock.io");
        // a people-only file (the pre-rooms format) still parses, with no rooms
        let old = parse_all(GOOD).unwrap();
        assert_eq!(old.recipients.len(), 2);
        assert!(old.rooms.is_empty());
        assert_eq!(parse(GOOD).unwrap(), old.recipients);
    }

    #[test]
    fn rejects_bad_room_entries() {
        let bad_id = "[[rooms]]\nname = \"x\"\nroom_id = \"#alias:server\"\n";
        assert!(parse_all(bad_id).is_err());
        let no_server = "[[rooms]]\nname = \"x\"\nroom_id = \"!abc\"\n";
        assert!(parse_all(no_server).is_err());
        let empty = "[[rooms]]\nname = \" \"\nroom_id = \"!a:b\"\n";
        assert!(parse_all(empty).is_err());
        let mxid_name = "[[rooms]]\nname = \"@x:y\"\nroom_id = \"!a:b\"\n";
        assert!(parse_all(mxid_name).is_err());
        let dup_id = "[[rooms]]\nname = \"x\"\nroom_id = \"!a:b\"\n[[rooms]]\nname = \"y\"\nroom_id = \"!a:b\"\n";
        assert!(parse_all(dup_id).is_err());
        // a room may not share a name with a person
        let clash = "[[recipients]]\nname = \"tim\"\nmxid = \"@a:b\"\n[[rooms]]\nname = \"TIM\"\nroom_id = \"!a:b\"\n";
        assert!(parse_all(clash).is_err());
        // a bad room fails the whole file closed, people included
        let mixed = "[[recipients]]\nname = \"tim\"\nmxid = \"@a:b\"\n[[rooms]]\nname = \"r\"\nroom_id = \"nope\"\n";
        assert!(parse(mixed).is_err());
    }

    #[test]
    fn resolves_person_room_or_unknown() {
        let a = list(WITH_ROOMS);
        assert!(matches!(a.resolve_target("tim"), Some(Target::Person(r)) if r.name == "tim"));
        assert!(matches!(
            a.resolve_target("@ABC123:matrix.inblock.io"),
            Some(Target::Person(_))
        ));
        assert!(
            matches!(a.resolve_target("Daily-Updates"), Some(Target::Room(r)) if r.room_id == "!HmZvPSoRvkqikWEvwD:matrix.inblock.io")
        );
        assert!(matches!(
            a.resolve_target("!rvKzMvUBBaewXApthx:matrix.inblock.io"),
            Some(Target::Room(r)) if r.name == "aqua-internal"
        ));
        // room ids are case-sensitive; unknown ids and names resolve to nothing
        assert!(a
            .resolve_target("!rvkzmvubbaewxapthx:matrix.inblock.io")
            .is_none());
        assert!(a.resolve_target("!unlisted:matrix.inblock.io").is_none());
        assert!(a.resolve_target("mallory").is_none());
        // people resolution never yields a room
        assert!(a.resolve("daily-updates").is_none());
        assert!(a.is_listed_room("!HmZvPSoRvkqikWEvwD:matrix.inblock.io"));
        assert!(!a.is_listed_room("!other:matrix.inblock.io"));
        assert!(a.refusal("x").contains("daily-updates, aqua-internal"));
    }

    #[test]
    fn invite_decisions() {
        use InviteAction::*;
        assert_eq!(invite_action(false, true, false), Decline);
        assert_eq!(invite_action(false, false, true), Decline);
        assert_eq!(invite_action(true, true, false), JoinAsDm);
        // group invite from an allowed person: joined, never marked DM
        assert_eq!(invite_action(true, false, false), JoinAsGroup);
        // a listed room is never a DM, even if the invite claims is_direct
        assert_eq!(invite_action(true, true, true), JoinAsGroup);
        assert_eq!(invite_action(true, false, true), JoinAsGroup);
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

    fn owner() -> Recipient {
        Recipient {
            name: "owner".into(),
            mxid: "@own:x".into(),
            note: None,
        }
    }

    fn cache_file(tag: &str, text: &str) -> PathBuf {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let d = base
            .join(".cache")
            .join(format!("msgr-allow-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("extra.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn owner_only_default_allows_just_the_owner() {
        let a = AllowList::owner_only(owner(), None, "Marina");
        assert_eq!(a.recipients().len(), 1);
        assert!(a.rooms().is_empty());
        assert!(a.resolve("owner").is_some());
        assert!(a.by_mxid("@OWN:x").is_some());
        assert!(a.resolve_target("@someone:x").is_none());
        assert!(a.resolve_target("!room:x").is_none());
        let r = a.refusal("@someone:x");
        assert!(
            r.contains("not on the Marina allow-list") && r.contains("[owner]"),
            "{r}"
        );
        assert!(r.contains("the agent's owner's approval"), "{r}");
        // A missing optional extra file is fine: still owner-only, no error.
        let missing =
            AllowList::owner_only(owner(), Some(PathBuf::from("/nonexistent/extra.toml")), "M");
        assert_eq!(missing.recipients().len(), 1);
        assert!(missing.load_error().is_none());
    }

    #[test]
    fn owner_only_extra_file_is_explicit_and_fails_closed() {
        let p = cache_file("ok", WITH_ROOMS);
        let a = AllowList::owner_only(owner(), Some(p.clone()), "M");
        assert_eq!(a.recipients().len(), 2);
        assert_eq!(a.rooms().len(), 2);
        assert!(matches!(
            a.resolve_target("daily-updates"),
            Some(Target::Room(_))
        ));
        // Broken extra file: back to owner-only (no rooms), owner still reachable.
        std::fs::write(&p, "not = [valid").unwrap();
        let b = AllowList::owner_only(owner(), Some(p.clone()), "M");
        assert_eq!(b.recipients().len(), 1);
        assert!(b.rooms().is_empty());
        assert!(b.load_error().is_some());
        assert!(b.resolve("owner").is_some());
        assert!(!b.refusal("x").contains("failed to load"));
        // The extra file cannot re-point the owner's name.
        std::fs::write(&p, "[[recipients]]\nname = \"owner\"\nmxid = \"@evil:x\"\n").unwrap();
        let c = AllowList::owner_only(owner(), Some(p.clone()), "M");
        assert_eq!(c.resolve("owner").unwrap().mxid, "@own:x");
        assert!(c.by_mxid("@evil:x").is_none());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn host_refusal_text_is_unchanged() {
        let a = list(WITH_ROOMS);
        assert_eq!(
            a.refusal("mallory"),
            "REFUSED: \"mallory\" is not on the Aqua System allow-list. Allowed recipients: [tim]; \
             allowed rooms: [daily-updates, aqua-internal]. To add someone, append a [[recipients]] entry \
             (a group room: a [[rooms]] entry) to /nonexistent (no restart needed), and only with Tim's approval."
        );
    }
}
