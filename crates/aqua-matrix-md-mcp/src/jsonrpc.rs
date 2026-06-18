//! The minimal MCP JSON-RPC 2.0 surface `claude` exercises over stdio.
//!
//! Mirrors `aqua-matrix-ask-mcp`'s handler (the same `claude` client drives
//! both): `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.
//! [`handle`] maps one parsed request `Value` to an `Option<Value>` response
//! (None for notifications), given a closure that performs the
//! `send_markdown_file` delivery and returns its [`MdOutcome`]. The binary
//! ([`crate::main`]) owns stdin/stdout and the unix-socket side effect; this
//! module owns the wire shapes and is fully unit-testable.

use serde_json::{json, Value};

use crate::TOOL_NAME;

/// Protocol version we advertise. We echo back whatever the client sent in
/// `initialize` (forward-compatible); this is only the fallback if absent.
pub const FALLBACK_PROTOCOL_VERSION: &str = "2025-11-25";

/// The `tools/list` entry for our single tool.
fn tool_descriptor() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Deliver a Markdown document to the user as a real downloadable file attachment (an `.md` they can open and save). Call this when the user asks for the answer AS A FILE (for example \"give me the markdown file\", \"the markdown FILE\", \"send it as a file\", \"as a .md\", \"export this as a markdown file\"). Pass a short descriptive `filename` and the COMPLETE Markdown document as `markdown`. Do NOT write or save any file yourself with another tool, and never tell the user a local or container path: this tool performs the actual delivery over the chat channel. Returns whether the attachment was delivered.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "filename": {
                    "type": "string",
                    "description": "A short descriptive name for the document, for example \"aqua-partner-strategy\". The channel sanitizes it and ensures a .md extension; you do not need to add one."
                },
                "markdown": {
                    "type": "string",
                    "description": "The complete Markdown document to deliver, well-structured and ideally starting with a top-level `# ` heading."
                }
            },
            "required": ["filename", "markdown"]
        }
    })
}

/// A success response envelope.
fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error response envelope.
fn err(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Outcome of resolving a `send_markdown_file` tool call, as returned by the
/// bridge closure passed to [`handle`].
pub struct MdOutcome {
    /// Text to surface to the model as the tool result.
    pub text: String,
    /// Whether to flag this as an error result (`isError: true`). Set true only
    /// when nothing was delivered and the model should paste the document inline
    /// as a fallback; a successful attach (or an inline fallback the daemon
    /// already performed) is not an error.
    pub is_error: bool,
}

/// Handle one parsed JSON-RPC request, returning the response to write (or
/// `None` for a notification, which gets no reply).
///
/// `deliver` is invoked only for a `tools/call` of our tool; it receives the
/// `filename` and `markdown` arguments and returns the [`MdOutcome`]. Keeping it
/// a closure lets the binary plug in the unix-socket round-trip while tests plug
/// in a stub.
pub fn handle<F>(req: &Value, mut deliver: F) -> Option<Value>
where
    F: FnMut(&str, &str) -> MdOutcome,
{
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();

    // Notifications have no `id` and never get a response.
    if id.is_none() {
        return None;
    }
    let id = id.unwrap();

    match method {
        "initialize" => {
            let protocol = req
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(FALLBACK_PROTOCOL_VERSION);
            Some(ok(
                id,
                json!({
                    "protocolVersion": protocol,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "aqua-matrix-md-mcp", "version": env!("CARGO_PKG_VERSION")}
                }),
            ))
        }
        "tools/list" => Some(ok(id, json!({"tools": [tool_descriptor()]}))),
        "tools/call" => {
            let params = req.get("params");
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if name != TOOL_NAME {
                return Some(err(id, -32602, &format!("unknown tool: {name}")));
            }
            let args = params.and_then(|p| p.get("arguments"));
            let filename = args
                .and_then(|a| a.get("filename"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let markdown = args
                .and_then(|a| a.get("markdown"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let outcome = deliver(filename, markdown);
            Some(ok(
                id,
                json!({
                    "content": [{"type": "text", "text": outcome.text}],
                    "isError": outcome.is_error
                }),
            ))
        }
        "ping" => Some(ok(id, json!({}))),
        other => Some(err(id, -32601, &format!("method not found: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_deliver(_f: &str, _m: &str) -> MdOutcome {
        panic!("deliver must not be called for this request");
    }

    #[test]
    fn initialize_echoes_client_protocol_version() {
        let req = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}}
        });
        let resp = handle(&req, never_deliver).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(resp["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(resp["id"], 0);
    }

    #[test]
    fn notification_gets_no_response() {
        let req = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle(&req, never_deliver).is_none());
    }

    #[test]
    fn tools_list_advertises_send_markdown_file() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = handle(&req, never_deliver).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], TOOL_NAME);
        assert!(tools[0]["inputSchema"]["properties"]["filename"].is_object());
        assert!(tools[0]["inputSchema"]["properties"]["markdown"].is_object());
        let required = tools[0]["inputSchema"]["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r == "filename"));
        assert!(required.iter().any(|r| r == "markdown"));
    }

    #[test]
    fn tools_call_invokes_deliver_with_both_args() {
        let req = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": {"filename": "overview", "markdown": "# Overview"}}
        });
        let mut seen = (String::new(), String::new());
        let resp = handle(&req, |f, m| {
            seen = (f.to_string(), m.to_string());
            MdOutcome { text: "Sent it to you as a markdown file.".into(), is_error: false }
        })
        .unwrap();
        assert_eq!(seen.0, "overview");
        assert_eq!(seen.1, "# Overview");
        assert_eq!(resp["result"]["content"][0]["text"], "Sent it to you as a markdown file.");
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(resp["id"], 7);
    }

    #[test]
    fn tools_call_failure_is_flagged_is_error() {
        let req = json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": {"filename": "x", "markdown": "y"}}
        });
        let resp = handle(&req, |_, _| MdOutcome {
            text: "could not deliver; paste inline".into(),
            is_error: true,
        })
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[test]
    fn tools_call_unknown_tool_errors() {
        let req = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "something_else", "arguments": {}}
        });
        let resp = handle(&req, never_deliver).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn missing_args_default_to_empty_and_still_deliver() {
        let req = json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": {}}
        });
        let resp = handle(&req, |f, m| {
            assert_eq!(f, "");
            assert_eq!(m, "");
            MdOutcome { text: "n/a".into(), is_error: false }
        })
        .unwrap();
        assert_eq!(resp["result"]["isError"], false);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"});
        let resp = handle(&req, never_deliver).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
