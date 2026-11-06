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
//! `[[observers]]` (`name`, `mxid`, `room_id`, optional `note`) name a member
//! who sits in one room (a person's DM with the bridge) without being counted
//! as one of its members, so that DM stays a 1:1. An observer is neither a
//! recipient nor a room. A row that is also a recipient, whose room is a
//! `[[rooms]]` entry, or that is malformed rejects the whole file; a running
//! daemon then keeps its last good lists (see [`ParseFailure`]). Older
//! binaries ignore the section.
//!
//! The daemon re-reads the file whenever its modification time changes, so an
//! edit takes effect on the next request without a restart. A file that is
//! missing or does not parse yields an EMPTY list (fail closed): nobody can be
//! messaged and every inbound message is dropped until it is fixed. The one
//! exception is a rejected `[[observers]]` row once a good file has loaded:
//! the lists stay as they were and the reason is reported.

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

/// A read-only member of one room (`[[observers]]`): joined to a DM or room
/// the bridge uses, but not counted as one of its members.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Observer {
    /// Short handle used in the journal (`observer loaded name=...`).
    pub name: String,
    /// Full Matrix user id of the observer.
    pub mxid: String,
    /// The one room (exact room id) in which this member is not counted.
    pub room_id: String,
    /// Optional human-readable note (purpose).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Observer {
    /// True when `mxid` is this observer (ASCII case-insensitive, as Synapse
    /// lowercases localparts) and `room_id` is the room it is bound to.
    pub fn is(&self, mxid: &str, room_id: &str) -> bool {
        self.room_id == room_id && self.mxid.eq_ignore_ascii_case(mxid)
    }
}

/// How many of the `joined` members (MXIDs) of `room_id` count: all of them
/// except observers bound to this room. With no observer for the room this is
/// just the number of joined members.
pub fn count_joined<'a>(
    room_id: &str,
    joined: impl IntoIterator<Item = &'a str>,
    observers: &[Observer],
) -> u64 {
    joined
        .into_iter()
        .filter(|m| !observers.iter().any(|o| o.is(m, room_id)))
        .count() as u64
}

#[derive(Debug, Default, Deserialize)]
struct AllowListFile {
    #[serde(default)]
    recipients: Vec<Recipient>,
    #[serde(default)]
    rooms: Vec<RoomEntry>,
    /// Read as raw TOML so a bad row is an OBSERVER error (the running lists
    /// are kept), not a syntax error of the whole file (fail closed).
    #[serde(default)]
    observers: Option<toml::Value>,
}

/// A parsed allow-list file.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parsed {
    pub recipients: Vec<Recipient>,
    pub rooms: Vec<RoomEntry>,
    pub observers: Vec<Observer>,
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

/// Parse allow-list TOML (see [`parse_checked`]); the error is its message.
pub fn parse_all(text: &str) -> Result<Parsed, String> {
    parse_checked(text).map_err(String::from)
}

/// Why a file was refused. Either way the file is rejected as a whole.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseFailure {
    /// A syntax error or a bad recipient or room: the lists fail closed.
    File(String),
    /// A bad `[[observers]]` row (`observer <name>: <reason>`): a running
    /// daemon keeps its last good lists.
    Observer(String),
}

impl From<ParseFailure> for String {
    fn from(f: ParseFailure) -> String {
        match f {
            ParseFailure::File(m) | ParseFailure::Observer(m) => m,
        }
    }
}

/// Parse allow-list TOML. Invalid entries (bad MXID or room id, empty or
/// duplicate name, duplicate room id) are an error for the whole file, so a
/// typo can never silently widen or narrow the list. Observer rows are
/// checked last, so a file error elsewhere wins (fail closed).
pub fn parse_checked(text: &str) -> Result<Parsed, ParseFailure> {
    parse_with(text, &[], &[])
}

/// [`parse_checked`] for a list that has built-in recipients (`fixed`, an
/// embedded agent's owner) and MXIDs no observer may use (`reserved`, the
/// bridge's own account). The built-in name checks run with the recipient and
/// room checks, so they win over an observer error.
fn parse_with(
    text: &str,
    fixed: &[Recipient],
    reserved: &[String],
) -> Result<Parsed, ParseFailure> {
    let file: AllowListFile = toml::from_str(text)
        .map_err(|e| ParseFailure::File(format!("allow-list parse error: {e}")))?;
    let (recipients, rooms) =
        people_and_rooms(file.recipients, file.rooms).map_err(ParseFailure::File)?;
    check_fixed(fixed, &recipients, &rooms).map_err(ParseFailure::File)?;
    let observers = parse_observers(file.observers, &recipients, &rooms, fixed, reserved)
        .map_err(ParseFailure::Observer)?;
    Ok(Parsed {
        recipients,
        rooms,
        observers,
    })
}

fn people_and_rooms(
    recipients: Vec<Recipient>,
    rooms: Vec<RoomEntry>,
) -> Result<(Vec<Recipient>, Vec<RoomEntry>), String> {
    let mut seen = std::collections::HashSet::new();
    for r in &recipients {
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
    for r in &rooms {
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
    let rooms = rooms
        .into_iter()
        .map(|r| RoomEntry {
            name: r.name.trim().to_string(),
            room_id: r.room_id.trim().to_string(),
            note: r.note,
        })
        .collect();
    Ok((recipients, rooms))
}

/// Reject a file that reuses a built-in (owner) name for another MXID or
/// names a room like the owner.
fn check_fixed(
    fixed: &[Recipient],
    recipients: &[Recipient],
    rooms: &[RoomEntry],
) -> Result<(), String> {
    for f in fixed {
        if let Some(c) = recipients
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(&f.name) && !e.mxid.eq_ignore_ascii_case(&f.mxid))
        {
            return Err(format!(
                "allow-list entry {:?} reuses the built-in name {:?} for a different MXID",
                c.name, f.name
            ));
        }
        if let Some(r) = rooms.iter().find(|r| r.name.eq_ignore_ascii_case(&f.name)) {
            return Err(format!(
                "[[rooms]] entry {:?} reuses the built-in name {:?}",
                r.name, f.name
            ));
        }
    }
    Ok(())
}

/// Validate the `[[observers]]` rows. Errors read `observer <name>: <reason>`.
fn parse_observers(
    raw: Option<toml::Value>,
    recipients: &[Recipient],
    rooms: &[RoomEntry],
    fixed: &[Recipient],
    reserved: &[String],
) -> Result<Vec<Observer>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let toml::Value::Array(rows) = raw else {
        return Err(
            "observer section: `observers` must be an array of tables ([[observers]])".into(),
        );
    };
    let mut out: Vec<Observer> = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        let label = row
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("#{}", i + 1));
        let fail = |why: String| format!("observer {label}: {why}");
        let o: Observer = row.try_into().map_err(|e| fail(e.to_string()))?;
        let name = o.name.trim().to_string();
        if name.is_empty() {
            return Err(fail("the name is empty".into()));
        }
        if name.starts_with('@') || name.starts_with('!') {
            return Err(fail(
                "the name must be a short handle, not an MXID or room id".into(),
            ));
        }
        if !is_valid_mxid(&o.mxid) {
            return Err(fail(format!(
                "{:?} is not a valid MXID (@localpart:server)",
                o.mxid
            )));
        }
        let room_id = o.room_id.trim().to_string();
        if !is_valid_room_id(&room_id) {
            return Err(fail(format!(
                "{:?} is not a valid room id (!opaque:server)",
                o.room_id
            )));
        }
        if reserved.iter().any(|r| r.eq_ignore_ascii_case(&o.mxid)) {
            return Err(fail(format!(
                "{} is this bridge's own account; it can not observe its own rooms",
                o.mxid
            )));
        }
        if recipients
            .iter()
            .chain(fixed)
            .any(|r| r.mxid.eq_ignore_ascii_case(&o.mxid))
        {
            return Err(fail(format!(
                "{} is also a recipient; an observer is never a person the bridge messages",
                o.mxid
            )));
        }
        if rooms.iter().any(|r| r.room_id == room_id) {
            return Err(fail(format!("its room {room_id} is a [[rooms]] entry")));
        }
        if recipients
            .iter()
            .map(|r| r.name.as_str())
            .chain(fixed.iter().map(|r| r.name.as_str()))
            .chain(rooms.iter().map(|r| r.name.as_str()))
            .any(|n| n.trim().eq_ignore_ascii_case(&name))
        {
            return Err(fail(
                "the name is already used by a recipient or a room".into(),
            ));
        }
        if out.iter().any(|x| x.name.eq_ignore_ascii_case(&name)) {
            return Err(fail("the name is used by another observer".into()));
        }
        if let Some(x) = out.iter().find(|x| x.room_id == room_id) {
            return Err(fail(format!(
                "room {room_id} already has the observer {}; one room has at most one",
                x.name
            )));
        }
        out.push(Observer {
            name,
            mxid: o.mxid,
            room_id,
            note: o.note,
        });
    }
    Ok(out)
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
    observers: Vec<Observer>,
    /// MXIDs that may not be observers (the bridge's own account).
    reserved: Vec<String>,
    /// Rooms an observer row has named since this list was built, kept after
    /// the row is gone: their member snapshot may be stale either way.
    ever_observed: std::collections::BTreeSet<String>,
    load_error: Option<String>,
    /// Why the last reload was refused because of an `[[observers]]` row.
    /// With a good list loaded before, the lists were kept (`load_error`
    /// stays empty); otherwise they failed closed as well.
    observer_error: Option<String>,
    /// A file has loaded (or the optional file is absent): there are lists
    /// worth keeping when a later file is rejected for an observer row.
    loaded: bool,
    /// Events the owner of this list should audit (drained with
    /// [`AllowList::take_audit`]).
    pending_audit: Vec<serde_json::Value>,
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

    /// [`AllowList::new`] that also refuses these MXIDs as observers (the
    /// bridge's own account, read from its persisted session).
    pub fn new_with_reserved(path: PathBuf, reserved: Vec<String>) -> Self {
        let mut a = Self::unloaded(Some(path), Vec::new(), "Aqua System", "Tim");
        a.reserved = reserved;
        a.reload(true);
        a
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
        let mut a = Self::unloaded(path, fixed, label, approver);
        a.reload(true);
        a
    }

    fn unloaded(path: Option<PathBuf>, fixed: Vec<Recipient>, label: &str, approver: &str) -> Self {
        Self {
            path,
            mtime: None,
            recipients: fixed.clone(),
            fixed,
            rooms: Vec::new(),
            observers: Vec::new(),
            reserved: Vec::new(),
            ever_observed: Default::default(),
            load_error: None,
            observer_error: None,
            loaded: false,
            pending_audit: Vec::new(),
            label: label.to_string(),
            approver: approver.to_string(),
        }
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
        let mut observers = Vec::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_with(&text, &self.fixed, &self.reserved) {
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
                    observers = p.observers;
                    self.ever_observed
                        .extend(observers.iter().map(|o| o.room_id.clone()));
                    tracing::info!(count = recipients.len(), rooms = rooms.len(), path = %path.display(), "allow-list loaded");
                    for o in &observers {
                        tracing::info!(name = %o.name, room = %o.room_id, "observer loaded");
                    }
                    self.load_error = None;
                    self.observer_error = None;
                    self.loaded = true;
                }
                Err(ParseFailure::Observer(e)) => {
                    tracing::error!(path = %path.display(), "allow-list rejected: {e}");
                    self.observer_error = Some(e.clone());
                    // Keep what runs now: this file is refused, the daemon is
                    // not. Before any good load there is nothing to keep.
                    let keep = self.loaded && self.load_error.is_none();
                    self.pending_audit.push(serde_json::json!({
                        "event": "allowlist_rejected",
                        "reason": e,
                        "kept": if keep { "last_good" } else { "none" },
                    }));
                    if keep {
                        return true;
                    }
                    self.load_error = Some(e);
                }
                Err(ParseFailure::File(e)) => {
                    tracing::error!(path = %path.display(), "allow-list rejected, failing closed: {e}");
                    self.observer_error = None;
                    self.load_error = Some(e);
                }
            },
            // An embedded agent's optional extra file may simply not exist.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.fixed.is_empty() => {
                self.load_error = None;
                self.observer_error = None;
                self.loaded = true;
            }
            Err(e) => {
                tracing::error!(path = %path.display(), "allow-list unreadable, failing closed: {e}");
                self.observer_error = None;
                self.load_error = Some(format!("cannot read {}: {e}", path.display()));
            }
        }
        self.recipients = recipients;
        self.rooms = rooms;
        self.observers = observers;
        true
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

    /// The loaded `[[observers]]` rows.
    pub fn observers(&self) -> &[Observer] {
        &self.observers
    }

    /// True when an observer row has named `room_id` since this list was
    /// built, even if the row is gone now. Such a room's member snapshot can
    /// be stale in both directions, so its count is always taken live.
    pub fn was_observed(&self, room_id: &str) -> bool {
        self.ever_observed.contains(room_id)
    }

    /// The observers bound to `room_id` (owned, for use across an await).
    pub fn observers_in_room(&self, room_id: &str) -> Vec<Observer> {
        self.observers
            .iter()
            .filter(|o| o.room_id == room_id)
            .cloned()
            .collect()
    }

    /// Why the last reload was refused because of an observer row, if so.
    /// Set both when the running lists were kept and when nothing had loaded
    /// yet (a start error); cleared by the next good load.
    pub fn observer_error(&self) -> Option<&str> {
        self.observer_error.as_deref()
    }

    /// The error to show for this list: a load failure, else a refused
    /// observer row.
    pub fn error_text(&self) -> Option<&str> {
        self.load_error().or(self.observer_error())
    }

    /// Events to audit since the last call (a rejected file).
    pub fn take_audit(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.pending_audit)
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
            observers: p.observers,
            reserved: Vec::new(),
            ever_observed: Default::default(),
            load_error: None,
            observer_error: None,
            loaded: true,
            pending_audit: Vec::new(),
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

    // ---- [[observers]] (dev-connector D10 b) ------------------------------

    const TIM_DM: &str = "!timdm:matrix.inblock.io";
    const OBS: &str = "@reader:matrix.inblock.io";

    const WITH_OBSERVER: &str = r#"
[[recipients]]
name = "tim"
mxid = "@abc123:matrix.inblock.io"

[[rooms]]
name = "daily-updates"
room_id = "!HmZvPSoRvkqikWEvwD:matrix.inblock.io"

[[observers]]
name = "voice-inbox"
mxid = "@reader:matrix.inblock.io"
room_id = "!timdm:matrix.inblock.io"
note = "dev-connector inbox_read (D10): read-only member, not counted"
"#;

    fn obs(name: &str, mxid: &str, room_id: &str) -> Observer {
        Observer {
            name: name.into(),
            mxid: mxid.into(),
            room_id: room_id.into(),
            note: None,
        }
    }

    fn bad_observer_row(row: &str) -> String {
        format!("{}\n[[observers]]\n{row}\n", WITH_ROOMS)
    }

    #[test]
    fn observers_section_parses_with_room_and_note() {
        let p = parse_all(WITH_OBSERVER).unwrap();
        assert_eq!(p.recipients.len(), 1);
        assert_eq!(p.rooms.len(), 1);
        assert_eq!(p.observers.len(), 1);
        let o = &p.observers[0];
        assert_eq!(o.name, "voice-inbox");
        assert_eq!(o.mxid, OBS);
        assert_eq!(o.room_id, TIM_DM);
        assert!(o.note.as_deref().unwrap().contains("not counted"));
    }

    #[test]
    fn invalid_observer_rows_reject_the_whole_file() {
        let rows = [
            // malformed MXID
            "name = \"o\"\nmxid = \"reader:x\"\nroom_id = \"!r:x\"",
            "name = \"o\"\nmxid = \"@reader\"\nroom_id = \"!r:x\"",
            // malformed room id
            "name = \"o\"\nmxid = \"@reader:x\"\nroom_id = \"#alias:x\"",
            "name = \"o\"\nmxid = \"@reader:x\"\nroom_id = \"!r\"",
            // empty name, name that is an MXID or room id
            "name = \" \"\nmxid = \"@reader:x\"\nroom_id = \"!r:x\"",
            "name = \"@o:x\"\nmxid = \"@reader:x\"\nroom_id = \"!r:x\"",
            // also a recipient (the listed person), in any letter case
            "name = \"o\"\nmxid = \"@abc123:matrix.inblock.io\"\nroom_id = \"!r:x\"",
            "name = \"o\"\nmxid = \"@ABC123:matrix.inblock.io\"\nroom_id = \"!r:x\"",
            // its room is a [[rooms]] entry
            "name = \"o\"\nmxid = \"@reader:x\"\nroom_id = \"!HmZvPSoRvkqikWEvwD:matrix.inblock.io\"",
            // a required field is missing
            "name = \"o\"\nmxid = \"@reader:x\"",
            "name = \"o\"\nroom_id = \"!r:x\"",
        ];
        for row in rows {
            let text = bad_observer_row(row);
            let err = parse_all(&text).expect_err(row);
            assert!(err.contains("observer"), "{row}: {err}");
        }
        // Two rows with one name, and one (mxid, room) bound twice.
        let dup_name = format!(
            "{WITH_OBSERVER}\n[[observers]]\nname = \"VOICE-inbox\"\nmxid = \"@other:x\"\nroom_id = \"!o:x\"\n"
        );
        assert!(parse_all(&dup_name).unwrap_err().contains("observer"));
        let dup_binding = format!(
            "{WITH_OBSERVER}\n[[observers]]\nname = \"again\"\nmxid = \"@READER:matrix.inblock.io\"\nroom_id = \"{TIM_DM}\"\n"
        );
        assert!(parse_all(&dup_binding).unwrap_err().contains("observer"));
    }

    #[test]
    fn one_observer_may_watch_two_rooms() {
        let two = format!(
            "{WITH_OBSERVER}\n[[observers]]\nname = \"second\"\nmxid = \"{OBS}\"\nroom_id = \"!other:matrix.inblock.io\"\n"
        );
        assert_eq!(parse_all(&two).unwrap().observers.len(), 2);
    }

    #[test]
    fn an_observer_is_not_a_recipient_or_a_room() {
        let a = list(WITH_OBSERVER);
        assert!(a.resolve(OBS).is_none());
        assert!(a.resolve("voice-inbox").is_none());
        assert!(a.by_mxid(OBS).is_none());
        assert!(a.resolve_target(OBS).is_none());
        assert!(a.resolve_target(TIM_DM).is_none());
        assert!(!a.is_listed_room(TIM_DM));
        assert!(!a.refusal("x").contains("voice-inbox"));
    }

    #[test]
    fn old_binaries_ignore_the_observers_section() {
        // The shape of AllowListFile before [[observers]] existed: no
        // deny_unknown_fields, so an old binary reads a new file as before.
        #[derive(Debug, Default, Deserialize)]
        struct LegacyFile {
            #[serde(default)]
            recipients: Vec<Recipient>,
            #[serde(default)]
            rooms: Vec<RoomEntry>,
        }
        let legacy: LegacyFile = toml::from_str(WITH_OBSERVER).unwrap();
        assert_eq!(legacy.recipients.len(), 1);
        assert_eq!(legacy.rooms.len(), 1);
        // And today's parser tolerates a section it does not know.
        let future = format!("{GOOD}\n[[something_later]]\nname = \"x\"\n");
        assert_eq!(parse_all(&future).unwrap().recipients.len(), 2);
    }

    #[test]
    fn zero_observers_changes_nothing() {
        let p = parse_all(WITH_ROOMS).unwrap();
        assert!(p.observers.is_empty());
        let a = list(WITH_ROOMS);
        assert!(a.observers().is_empty());
        assert!(a.observer_error().is_none());
        // the refusal text and the lists are exactly what they were
        assert_eq!(p.recipients.len(), 1);
        assert_eq!(p.rooms.len(), 2);
    }

    #[test]
    fn counting_leaves_out_only_the_observers_of_that_room() {
        let tim = "@abc123:matrix.inblock.io";
        let bridge = "@bridge:matrix.inblock.io";
        let observers = [obs("voice-inbox", OBS, TIM_DM)];
        // no observers: every joined member counts
        assert_eq!(count_joined(TIM_DM, [tim, bridge, OBS], &[]), 3);
        // Tim + bridge + the observer is a DM (2)
        assert_eq!(count_joined(TIM_DM, [tim, bridge, OBS], &observers), 2);
        // the observer, letter case aside
        assert_eq!(
            count_joined(
                TIM_DM,
                [tim, bridge, "@READER:matrix.inblock.io"],
                &observers
            ),
            2
        );
        // the observer plus two others is a group (3)
        assert_eq!(
            count_joined(TIM_DM, [tim, bridge, "@third:x", OBS], &observers),
            3
        );
        // an observer bound to another room still counts here
        assert_eq!(
            count_joined("!elsewhere:x", [tim, bridge, OBS], &observers),
            3
        );
        // an observer that is not joined (an invite) changes nothing
        assert_eq!(count_joined(TIM_DM, [tim, bridge], &observers), 2);
        // one MXID bound to two rooms is left out of each, only there
        let two = [
            obs("a", OBS, TIM_DM),
            obs("b", OBS, "!second:matrix.inblock.io"),
        ];
        assert_eq!(
            count_joined("!second:matrix.inblock.io", [tim, bridge, OBS], &two),
            2
        );
        assert_eq!(count_joined("!third:x", [tim, bridge, OBS], &two), 3);
    }

    fn at(p: &Path, offset_secs: u64) {
        // Keep the mtime moving: two writes inside one clock tick would look unchanged.
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(offset_secs))
            .unwrap();
    }

    const GOOD_OBSERVER_FILE: &str = WITH_OBSERVER;
    /// `GOOD_OBSERVER_FILE` plus a new person (who must NOT appear while the
    /// file is rejected) and an observer row that cannot be accepted.
    fn rejected_file() -> String {
        format!(
            "{GOOD_OBSERVER_FILE}\n[[recipients]]\nname = \"mallory\"\nmxid = \"@mallory:matrix.inblock.io\"\n\n[[observers]]\nname = \"broken\"\nmxid = \"not-an-mxid\"\nroom_id = \"{TIM_DM}\"\n"
        )
    }

    #[test]
    fn an_invalid_observer_keeps_the_last_good_lists() {
        let p = cache_file("obs-keep", GOOD_OBSERVER_FILE);
        let mut a = AllowList::new(p.clone());
        assert_eq!(a.recipients().len(), 1);
        assert_eq!(a.rooms().len(), 1);
        assert_eq!(a.observers().len(), 1);
        assert!(a.observer_error().is_none());

        std::fs::write(&p, rejected_file()).unwrap();
        at(&p, 10);
        assert!(a.reload(false));
        // the lists did NOT change: the file is rejected as a whole
        assert_eq!(a.recipients().len(), 1, "mallory must not be added");
        assert!(a.resolve("mallory").is_none());
        assert_eq!(a.rooms().len(), 1);
        assert_eq!(a.observers().len(), 1);
        assert_eq!(a.observers()[0].name, "voice-inbox");
        // messaging still works: this is not a fail-closed state
        assert!(a.load_error().is_none());
        assert!(a.resolve("tim").is_some());
        assert!(a.refusal("x").starts_with("REFUSED: \"x\" is not on"));
        // and the reason is visible
        let why = a.observer_error().expect("observer error kept");
        assert!(why.contains("observer broken"), "{why}");

        // a fixed file is picked up and the error clears
        std::fs::write(&p, GOOD_OBSERVER_FILE.replace("daily-updates", "renamed")).unwrap();
        at(&p, 20);
        assert!(a.reload(false));
        assert!(a.observer_error().is_none());
        assert_eq!(a.rooms()[0].name, "renamed");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_syntax_error_still_fails_closed() {
        let p = cache_file("obs-syntax", GOOD_OBSERVER_FILE);
        let mut a = AllowList::new(p.clone());
        assert_eq!(a.recipients().len(), 1);
        std::fs::write(&p, "not toml [[").unwrap();
        at(&p, 10);
        assert!(a.reload(false));
        assert!(a.recipients().is_empty());
        assert!(a.rooms().is_empty());
        assert!(a.observers().is_empty());
        assert!(a.load_error().is_some());
        assert!(a.observer_error().is_none());
        // a bad recipient together with a bad observer is today's fail-closed
        std::fs::write(&p, GOOD_OBSERVER_FILE).unwrap();
        at(&p, 20);
        a.reload(false);
        assert_eq!(a.recipients().len(), 1);
        std::fs::write(
            &p,
            format!(
                "{}\n[[recipients]]\nname = \"x\"\nmxid = \"bad\"\n",
                rejected_file()
            ),
        )
        .unwrap();
        at(&p, 30);
        a.reload(false);
        assert!(a.recipients().is_empty());
        assert!(a.load_error().is_some());
        assert!(a.observer_error().is_none());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_invalid_observer_at_start_is_named_and_fails_closed() {
        let p = cache_file("obs-start", &rejected_file());
        let a = AllowList::new(p.clone());
        assert!(a.observer_error().unwrap().contains("observer broken"));
        // nothing was loaded: no last good list exists to keep
        assert!(a.recipients().is_empty());
        assert!(a.observers().is_empty());
        assert!(a.load_error().is_some());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_embedded_owner_cannot_be_made_an_observer() {
        let own = format!(
            "[[observers]]\nname = \"o\"\nmxid = \"{}\"\nroom_id = \"!r:x\"\n",
            owner().mxid
        );
        let p = cache_file("obs-owner", &own);
        let a = AllowList::owner_only(owner(), Some(p.clone()), "M");
        assert!(a.observer_error().unwrap().contains("observer o"));
        assert!(a.observers().is_empty());
        assert!(a.resolve("owner").is_some());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    // ---- fix pass: review findings B3, B4, B8 and the stale snapshot -------

    #[test]
    fn two_observers_for_one_room_are_refused() {
        let two = format!(
            "{WITH_OBSERVER}\n[[observers]]\nname = \"second\"\nmxid = \"@other:matrix.inblock.io\"\nroom_id = \"{TIM_DM}\"\n"
        );
        let err = parse_all(&two).unwrap_err();
        assert!(
            err.starts_with("observer second:")
                && err.contains(TIM_DM)
                && err.contains("voice-inbox"),
            "{err}"
        );
    }

    #[test]
    fn an_observer_name_may_not_reuse_a_person_or_room_name() {
        for name in ["tim", "TIM", "daily-updates"] {
            let text = format!(
                "{WITH_ROOMS}\n[[observers]]\nname = \"{name}\"\nmxid = \"@reader:x\"\nroom_id = \"!dm:x\"\n"
            );
            let err = parse_all(&text).unwrap_err();
            assert!(
                err.starts_with(&format!("observer {name}:")),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn the_bridges_own_account_cannot_be_an_observer() {
        let own = "@bridge:matrix.inblock.io";
        let text = format!(
            "[[recipients]]\nname = \"tim\"\nmxid = \"@abc123:matrix.inblock.io\"\n\n[[observers]]\nname = \"me\"\nmxid = \"@BRIDGE:matrix.inblock.io\"\nroom_id = \"{TIM_DM}\"\n"
        );
        let p = cache_file("obs-reserved", &text);
        let a = AllowList::new_with_reserved(p.clone(), vec![own.to_string()]);
        let why = a.observer_error().expect("the row is refused");
        assert!(
            why.starts_with("observer me:") && why.contains("own account"),
            "{why}"
        );
        assert!(a.observers().is_empty());
        // the same file is fine for a list that reserves nothing
        let open = AllowList::new(p.clone());
        assert_eq!(open.observers().len(), 1);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_owner_only_file_error_wins_over_an_observer_error() {
        // The extra file re-points the owner's name AND carries a bad observer.
        let text = "[[recipients]]\nname = \"owner\"\nmxid = \"@evil:x\"\n\n[[observers]]\nname = \"o\"\nmxid = \"bad\"\nroom_id = \"!r:x\"\n";
        let p = cache_file("obs-owner-order", text);
        let a = AllowList::owner_only(owner(), Some(p.clone()), "M");
        let why = a.load_error().expect("the recipient error closes the list");
        assert!(
            why.contains("built-in name") && why.contains("owner"),
            "{why}"
        );
        assert!(
            a.observer_error().is_none(),
            "the observer row is not blamed"
        );
        assert_eq!(a.resolve("owner").unwrap().mxid, "@own:x");
        assert!(a.by_mxid("@evil:x").is_none());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_room_stays_observed_after_its_row_is_gone() {
        let p = cache_file("obs-ever", GOOD_OBSERVER_FILE);
        let mut a = AllowList::new(p.clone());
        assert!(a.was_observed(TIM_DM));
        assert!(!a.was_observed("!elsewhere:x"));
        // the row is removed: the room still needs a live member count
        std::fs::write(&p, WITH_ROOMS).unwrap();
        at(&p, 10);
        assert!(a.reload(false));
        assert!(a.observers().is_empty());
        assert!(a.was_observed(TIM_DM), "its snapshot may be stale");
        assert!(!a.was_observed("!elsewhere:x"));
        // a rejected file changes nothing about it either
        std::fs::write(&p, rejected_file()).unwrap();
        at(&p, 20);
        a.reload(false);
        assert!(a.was_observed(TIM_DM));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
