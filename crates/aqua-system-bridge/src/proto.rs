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
