//! Daemon ⇄ MCP-server IPC over a per-run unix domain socket.
//!
//! Trivial, newline-delimited JSON: the MCP server opens the socket, writes one
//! [`AskRequest`] line, reads back one [`AskReply`] line, closes. The daemon's
//! listener accepts one connection per `ask_human` call, runs the Phase A
//! `pending.ask(...)`, and writes the [`AskReply`].
//!
//! Newline framing (not length-prefix) keeps it greppable and easy to test; the
//! payloads are single-line JSON so an embedded `\n` can't occur (serde emits
//! compact JSON, and any newline inside a string is escaped as `\n`).

use serde::{Deserialize, Serialize};

/// Server → daemon: "the model wants to ask the human this".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskRequest {
    pub question: String,
}

/// Daemon → server: the human's answer, or an explicit denial.
///
/// `granted = false` covers every fail-closed case (deny / timeout / send
/// failure on the daemon side). The server turns that into a tool result that
/// tells the model it was **not** granted, so it must not proceed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskReply {
    pub granted: bool,
    /// The human's free-form answer when `granted`; a short reason otherwise.
    pub answer: String,
}

impl AskReply {
    pub fn granted(answer: impl Into<String>) -> Self {
        Self { granted: true, answer: answer.into() }
    }
    pub fn denied(reason: impl Into<String>) -> Self {
        Self { granted: false, answer: reason.into() }
    }
}

/// Encode a request as a single newline-terminated JSON line.
pub fn encode_request(req: &AskRequest) -> String {
    let mut s = serde_json::to_string(req).expect("AskRequest serializes");
    s.push('\n');
    s
}

/// Encode a reply as a single newline-terminated JSON line.
pub fn encode_reply(reply: &AskReply) -> String {
    let mut s = serde_json::to_string(reply).expect("AskReply serializes");
    s.push('\n');
    s
}

/// Parse a request line (the trailing newline may already be stripped).
pub fn decode_request(line: &str) -> anyhow::Result<AskRequest> {
    Ok(serde_json::from_str(line.trim_end_matches('\n'))?)
}

/// Parse a reply line (the trailing newline may already be stripped).
pub fn decode_reply(line: &str) -> anyhow::Result<AskReply> {
    Ok(serde_json::from_str(line.trim_end_matches('\n'))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let req = AskRequest { question: "rm -rf /tmp/x — ok?".into() };
        let line = encode_request(&req);
        assert!(line.ends_with('\n'), "must be newline-framed");
        assert_eq!(line.matches('\n').count(), 1, "exactly one frame");
        assert_eq!(decode_request(&line).unwrap(), req);
    }

    #[test]
    fn reply_roundtrips_both_variants() {
        for reply in [AskReply::granted("yes go"), AskReply::denied("timeout")] {
            let line = encode_reply(&reply);
            assert!(line.ends_with('\n'));
            assert_eq!(decode_reply(&line).unwrap(), reply);
        }
    }

    #[test]
    fn embedded_newline_in_question_is_escaped_to_one_frame() {
        // A multi-line question must not break the one-line-per-frame invariant.
        let req = AskRequest { question: "line1\nline2".into() };
        let line = encode_request(&req);
        assert_eq!(line.matches('\n').count(), 1, "embedded \\n must be escaped");
        assert_eq!(decode_request(&line).unwrap().question, "line1\nline2");
    }

    #[test]
    fn decode_tolerates_missing_trailing_newline() {
        let req = AskRequest { question: "q".into() };
        let raw = serde_json::to_string(&req).unwrap(); // no newline
        assert_eq!(decode_request(&raw).unwrap(), req);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_request("not json").is_err());
        assert!(decode_reply("{").is_err());
    }
}
