//! Daemon and MCP-server IPC over a per-run unix domain socket.
//!
//! Trivial, newline-delimited JSON: the MCP server opens the socket, writes one
//! [`MdRequest`] line, reads back one [`MdReply`] line, closes. The daemon's
//! listener accepts one connection per `send_markdown_file` call, performs the
//! actual `AgentClient::send_file`, and writes the [`MdReply`].
//!
//! Newline framing (not length-prefix) keeps it greppable and easy to test. The
//! Markdown body is multi-line, but serde emits COMPACT JSON, so every embedded
//! newline is escaped to `\n` inside the string: one request is still exactly
//! one line. A round-trip test pins that invariant.

use serde::{Deserialize, Serialize};

/// Server to daemon: "the model wants this Markdown delivered as a file".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdRequest {
    /// The model-supplied filename. UNTRUSTED: the daemon sanitizes it before
    /// touching the filesystem (see `aqua_matrix_md_mcp::safe_md_filename`).
    pub filename: String,
    /// The complete Markdown document to attach.
    pub markdown: String,
}

/// Daemon to server: whether the file was delivered, plus a human-readable
/// detail surfaced to the model as the tool result.
///
/// `delivered = false` means the attachment could not be sent; in the common
/// case the daemon has already delivered the content to the user inline as a
/// fallback (content is never lost), and `detail` says so.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdReply {
    pub delivered: bool,
    /// How it was delivered, or why it could not be attached and how it fell
    /// back. Shown to the model so it confirms accurately and never invents a
    /// local or container path.
    pub detail: String,
}

impl MdReply {
    pub fn delivered(detail: impl Into<String>) -> Self {
        Self { delivered: true, detail: detail.into() }
    }
    pub fn failed(detail: impl Into<String>) -> Self {
        Self { delivered: false, detail: detail.into() }
    }
}

/// Encode a request as a single newline-terminated JSON line.
pub fn encode_request(req: &MdRequest) -> String {
    let mut s = serde_json::to_string(req).expect("MdRequest serializes");
    s.push('\n');
    s
}

/// Encode a reply as a single newline-terminated JSON line.
pub fn encode_reply(reply: &MdReply) -> String {
    let mut s = serde_json::to_string(reply).expect("MdReply serializes");
    s.push('\n');
    s
}

/// Parse a request line (the trailing newline may already be stripped).
pub fn decode_request(line: &str) -> anyhow::Result<MdRequest> {
    Ok(serde_json::from_str(line.trim_end_matches('\n'))?)
}

/// Parse a reply line (the trailing newline may already be stripped).
pub fn decode_reply(line: &str) -> anyhow::Result<MdReply> {
    Ok(serde_json::from_str(line.trim_end_matches('\n'))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let req = MdRequest { filename: "overview".into(), markdown: "# Overview\n\nbody".into() };
        let line = encode_request(&req);
        assert!(line.ends_with('\n'), "must be newline-framed");
        assert_eq!(decode_request(&line).unwrap(), req);
    }

    #[test]
    fn reply_roundtrips_both_variants() {
        for reply in [MdReply::delivered("attached as x.md"), MdReply::failed("sent inline instead")] {
            let line = encode_reply(&reply);
            assert!(line.ends_with('\n'));
            assert_eq!(decode_reply(&line).unwrap(), reply);
        }
    }

    #[test]
    fn multiline_markdown_stays_one_frame() {
        // The whole point of newline framing for a multi-line document: a
        // Markdown body with many newlines must still serialize to exactly one
        // line (embedded newlines escaped to `\n`), or framing would break.
        let md = "# Title\n\n## Section\n\n- a\n- b\n\nparagraph\n";
        let req = MdRequest { filename: "doc".into(), markdown: md.into() };
        let line = encode_request(&req);
        assert_eq!(line.matches('\n').count(), 1, "exactly one frame for a multi-line doc");
        assert_eq!(decode_request(&line).unwrap().markdown, md);
    }

    #[test]
    fn decode_tolerates_missing_trailing_newline() {
        let req = MdRequest { filename: "f".into(), markdown: "x".into() };
        let raw = serde_json::to_string(&req).unwrap(); // no newline
        assert_eq!(decode_request(&raw).unwrap(), req);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_request("not json").is_err());
        assert!(decode_reply("{").is_err());
    }
}
