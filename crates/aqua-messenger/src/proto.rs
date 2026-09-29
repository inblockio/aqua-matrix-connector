//! The socket protocol between a stdio MCP server (`aqua-system-bridge-mcp`,
//! `aqua-messenger-mcp`) and the process that owns the Matrix Client.
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
        /// Allow-list name, MXID, `[[rooms]]` name or room id; empty = the
        /// profile's default recipient (an embedded agent's owner).
        #[serde(default)]
        to: String,
        markdown: String,
        /// Short origin tag appended to the message (cwd basename + host, or a
        /// caller-supplied label).
        #[serde(default)]
        origin: String,
        /// Send as a Matrix reply to this message: an inbox `seq` (as a
        /// decimal string) or a Matrix event id (`$...`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    /// Upload a local file (absolute path, read by the daemon) as an attachment.
    SendFile {
        #[serde(default)]
        to: String,
        path: String,
        #[serde(default)]
        caption: Option<String>,
        #[serde(default)]
        origin: String,
        /// See [`Request::SendMessage::reply_to`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
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
        /// Only entries not yet marked read.
        #[serde(default)]
        unread_only: bool,
        /// Mark the returned entries read.
        #[serde(default)]
        mark_read: bool,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Block until an unread message from `from` arrives (or `timeout_s`
    /// passes, capped at [`crate::MAX_WAIT_SECS`]). Returned entries are marked
    /// read.
    WaitForReply {
        from: String,
        timeout_s: u64,
        /// Only count entries with `seq > after_seq` (e.g. the `inbox_seq`
        /// returned by the send this is a reply to). `None` = any unread.
        #[serde(default)]
        after_seq: Option<u64>,
    },
    /// Download (on demand), decrypt and store the attachment of one inbox
    /// entry; a second call returns the cached file.
    FetchAttachment { inbox_seq: u64 },
    /// The tool surface of this backend (`{server_name, instructions, tools,
    /// ...}`, see `jsonrpc::describe`), so a stdio server advertises exactly
    /// the tools, limits and annotations the backend serves.
    Describe,
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
            reply_to: None,
        };
        let line = encode_line(&r);
        assert!(line.contains("\"op\":\"send_message\""));
        // no reply_to key on the wire when absent (an older daemon sees the
        // exact pre-reply request)
        assert!(!line.contains("reply_to"));
        let back: Request = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn reply_to_and_optional_to_parse() {
        let r: Request =
            serde_json::from_str(r#"{"op":"send_message","markdown":"x","reply_to":"12"}"#)
                .unwrap();
        let Request::SendMessage {
            to,
            reply_to,
            origin,
            ..
        } = r
        else {
            panic!()
        };
        assert!(to.is_empty() && origin.is_empty());
        assert_eq!(reply_to.as_deref(), Some("12"));
        let d: Request = serde_json::from_str(r#"{"op":"describe"}"#).unwrap();
        assert_eq!(d, Request::Describe);
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
                limit: None
            }
        );
    }
}
