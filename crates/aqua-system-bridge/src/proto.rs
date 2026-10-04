//! The socket protocol between `aqua-system-bridge-mcp` and the daemon.
//!
//! One request per connection: the client writes one JSON line, the daemon
//! writes one JSON line back and closes. Newline-delimited JSON keeps it
//! debuggable with `socat`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request from an MCP server (or any local client) to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Send a Markdown message to an allow-listed recipient.
    SendMessage {
        /// Allow-list name or MXID.
        to: String,
        markdown: String,
        /// Short origin tag appended to the message (cwd basename + host, or a
        /// caller-supplied label).
        origin: String,
    },
    /// Replace (`m.replace`) the text of a message the bridge itself sent
    /// earlier in the room `to` resolves to. `markdown` is the full new body;
    /// the origin tag is appended as for `send_message`.
    EditMessage {
        /// Allow-list name or MXID, or a `[[rooms]]` name or room id.
        to: String,
        /// Event id of the original message (not of an earlier edit).
        event_id: String,
        markdown: String,
        origin: String,
    },
    /// Upload a local file (absolute path, read by the daemon) as an attachment.
    SendFile {
        to: String,
        path: String,
        #[serde(default)]
        caption: Option<String>,
        origin: String,
    },
    /// List the allow-listed recipients.
    ListRecipients,
    /// Read inbox entries.
    ReadInbox {
        /// Allow-list name or MXID; `None` = everyone.
        #[serde(default)]
        from: Option<String>,
        /// Only entries with `seq > since_seq`.
        #[serde(default)]
        since_seq: Option<u64>,
        /// Only entries whose server timestamp is `> since_ts_ms`.
        #[serde(default)]
        since_ts_ms: Option<u64>,
        /// Only entries still open under the instance's inbox policy: `new`
        /// ones, or `new` + `seen` on an instance that tracks processing.
        #[serde(default)]
        unread_only: bool,
        /// Mark the returned `new` entries read (state `seen`).
        #[serde(default)]
        mark_read: bool,
        #[serde(default)]
        limit: Option<usize>,
        /// Origin label of the reading session, recorded on entries it marks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        /// Claude Code session id of the reading session, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Block until a `new` message from `from` arrives (or `timeout_s`
    /// passes, capped at [`crate::MAX_WAIT_SECS`]). Returned entries are marked
    /// `seen`.
    WaitForReply {
        from: String,
        timeout_s: u64,
        /// Only count entries with `seq > after_seq` (e.g. the `inbox_seq`
        /// returned by the send this is a reply to). `None` = any new one.
        #[serde(default)]
        after_seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Mark entries `processed`: a session acted on them, or decided nothing
    /// needs doing. Final; an entry already processed keeps its first mark.
    /// Refused unless the instance tracks processing.
    MarkProcessed {
        /// Explicit inbox seqs.
        #[serde(default)]
        seqs: Vec<u64>,
        /// Also every open entry with `seq <= up_to_seq` (restricted to
        /// `from`, if given).
        #[serde(default)]
        up_to_seq: Option<u64>,
        /// Person (DMs only) or room, as for `read_inbox`; only with `up_to_seq`.
        #[serde(default)]
        from: Option<String>,
        /// What was done, or why nothing needs doing.
        note: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Download (on demand), decrypt and store the attachment of one inbox
    /// entry; a second call returns the cached file.
    FetchAttachment { inbox_seq: u64 },
    /// Daemon health: identity, connection state, inbox counts.
    Status,
}

/// The daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: Value,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            error: None,
            data,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            data: Value::Null,
        }
    }
}

/// Encode as one newline-terminated JSON line.
pub fn encode_line<T: Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string(v).expect("protocol types always serialize");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_is_tagged() {
        let r = Request::SendMessage {
            to: "tim".into(),
            markdown: "# hi".into(),
            origin: "x@y".into(),
        };
        let line = encode_line(&r);
        assert!(line.contains("\"op\":\"send_message\""));
        let back: Request = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn edit_message_roundtrip_matches_the_wire_contract() {
        let wire = r##"{"op":"edit_message","to":"daily-updates","event_id":"$orig:x","markdown":"# Train Report","origin":"trains@nuc10"}"##;
        let r: Request = serde_json::from_str(wire).unwrap();
        assert_eq!(
            r,
            Request::EditMessage {
                to: "daily-updates".into(),
                event_id: "$orig:x".into(),
                markdown: "# Train Report".into(),
                origin: "trains@nuc10".into(),
            }
        );
        let line = encode_line(&r);
        assert!(line.contains("\"op\":\"edit_message\""));
        let back: Request = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn read_inbox_defaults() {
        let r: Request = serde_json::from_str(r#"{"op":"read_inbox"}"#).unwrap();
        assert_eq!(
            r,
            Request::ReadInbox {
                from: None,
                since_seq: None,
                since_ts_ms: None,
                unread_only: false,
                mark_read: false,
                limit: None,
                by: None,
                session: None,
            }
        );
    }

    #[test]
    fn pre_states_clients_still_parse() {
        // exactly what an MCP server from before the states sends
        let old = r#"{"op":"read_inbox","from":null,"since_seq":null,"since_ts_ms":null,"unread_only":true,"mark_read":true,"limit":50}"#;
        let Request::ReadInbox {
            unread_only,
            mark_read,
            by,
            ..
        } = serde_json::from_str(old).unwrap()
        else {
            panic!("not a read_inbox")
        };
        assert!(unread_only && mark_read && by.is_none());
        let wait = r#"{"op":"wait_for_reply","from":"tim","timeout_s":5,"after_seq":3}"#;
        assert!(matches!(
            serde_json::from_str(wait).unwrap(),
            Request::WaitForReply {
                after_seq: Some(3),
                by: None,
                ..
            }
        ));
    }

    #[test]
    fn mark_processed_wire() {
        let wire = r#"{"op":"mark_processed","seqs":[3,4],"note":"answered","by":"repo@host"}"#;
        let r: Request = serde_json::from_str(wire).unwrap();
        assert_eq!(
            r,
            Request::MarkProcessed {
                seqs: vec![3, 4],
                up_to_seq: None,
                from: None,
                note: "answered".into(),
                by: Some("repo@host".into()),
                session: None,
            }
        );
    }
}
