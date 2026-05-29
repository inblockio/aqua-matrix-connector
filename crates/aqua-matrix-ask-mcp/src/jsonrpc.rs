//! The minimal MCP JSON-RPC 2.0 surface `claude` exercises over stdio.
//!
//! Verified empirically against `claude` v2.1.157 (see the Phase B handover):
//! the client sends, in order,
//!   1. `initialize`            (id 0)  — `params.protocolVersion = "2025-11-25"`
//!   2. `notifications/initialized`     — a notification (no `id`, no reply)
//!   3. `tools/list`            (id 1)
//!   4. `tools/call`            (id N)  — `params.name`, `params.arguments`
//!
//! We keep the protocol handling **pure** here: [`handle`] maps one parsed
//! request `Value` to an `Option<Value>` response (None for notifications),
//! given a closure that resolves an `ask_human` call to its answer text. The
//! binary ([`crate::main`]) owns stdin/stdout and the unix-socket side effect;
//! this module owns the wire shapes and is fully unit-testable.

use serde_json::{json, Value};

use crate::TOOL_NAME;

/// Protocol version we advertise. We echo back whatever the client sent in
/// `initialize` (forward-compatible); this is only the fallback if absent.
pub const FALLBACK_PROTOCOL_VERSION: &str = "2025-11-25";

/// The `tools/list` entry for our single tool.
fn tool_descriptor() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Ask the human operator a free-form question over the chat \
channel and BLOCK until they answer. You MUST call this before running any \
destructive or irreversible command (e.g. `rm`, `rm -rf`, `git push --force`, \
`git reset --hard`, dropping data) and proceed only if the answer authorises \
it. Returns the operator's verbatim answer, or a denial if they decline or do \
not respond.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to put to the human, including \
enough context (the exact command / blast radius) for them to decide."
                }
            },
            "required": ["question"]
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

/// Outcome of resolving an `ask_human` tool call, as returned by the bridge
/// closure passed to [`handle`].
pub struct AskOutcome {
    /// Text to surface to the model as the tool result.
    pub text: String,
    /// Whether this should be flagged as an error result (`isError: true`) so
    /// the model treats a denial as a hard stop rather than a normal answer.
    pub is_error: bool,
}

/// Handle one parsed JSON-RPC request, returning the response to write (or
/// `None` for a notification, which gets no reply).
///
/// `ask` is invoked only for a `tools/call` of our tool; it receives the
/// `question` argument and returns the [`AskOutcome`]. Keeping it a closure lets
/// the binary plug in the unix-socket round-trip while tests plug in a stub.
pub fn handle<F>(req: &Value, mut ask: F) -> Option<Value>
where
    F: FnMut(&str) -> AskOutcome,
{
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();

    // Notifications have no `id` and never get a response.
    if id.is_none() {
        // e.g. notifications/initialized — nothing to do.
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
                    "serverInfo": {"name": "aqua-matrix-ask-mcp", "version": env!("CARGO_PKG_VERSION")}
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
            let question = params
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("question"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let outcome = ask(question);
            Some(ok(
                id,
                json!({
                    "content": [{"type": "text", "text": outcome.text}],
                    "isError": outcome.is_error
                }),
            ))
        }
        // ping or anything else we don't implement: reply with an empty result
        // for `ping` (MCP spec), method-not-found otherwise.
        "ping" => Some(ok(id, json!({}))),
        other => Some(err(id, -32601, &format!("method not found: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_ask(_q: &str) -> AskOutcome {
        panic!("ask must not be called for this request");
    }

    #[test]
    fn initialize_echoes_client_protocol_version() {
        let req = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}}
        });
        let resp = handle(&req, never_ask).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(resp["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(resp["id"], 0);
    }

    #[test]
    fn initialize_falls_back_when_version_absent() {
        let req = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}});
        let resp = handle(&req, never_ask).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], FALLBACK_PROTOCOL_VERSION);
    }

    #[test]
    fn notification_gets_no_response() {
        let req = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle(&req, never_ask).is_none());
    }

    #[test]
    fn tools_list_advertises_ask_human() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let resp = handle(&req, never_ask).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], TOOL_NAME);
        assert!(tools[0]["inputSchema"]["properties"]["question"].is_object());
        assert_eq!(tools[0]["inputSchema"]["required"][0], "question");
    }

    #[test]
    fn tools_call_invokes_ask_and_wraps_answer() {
        let req = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": {"question": "ok to rm?"}}
        });
        let mut seen = String::new();
        let resp = handle(&req, |q| {
            seen = q.to_string();
            AskOutcome { text: "yes, proceed".into(), is_error: false }
        })
        .unwrap();
        assert_eq!(seen, "ok to rm?");
        assert_eq!(resp["result"]["content"][0]["text"], "yes, proceed");
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(resp["id"], 7);
    }

    #[test]
    fn tools_call_denial_is_flagged_is_error() {
        let req = json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": {"question": "ok?"}}
        });
        let resp = handle(&req, |_| AskOutcome {
            text: "DENIED by operator".into(),
            is_error: true,
        })
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert_eq!(resp["result"]["content"][0]["text"], "DENIED by operator");
    }

    #[test]
    fn tools_call_unknown_tool_errors() {
        let req = json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "something_else", "arguments": {}}
        });
        let resp = handle(&req, never_ask).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn missing_question_arg_defaults_to_empty_and_still_asks() {
        let req = json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": {}}
        });
        let resp = handle(&req, |q| {
            assert_eq!(q, "");
            AskOutcome { text: "n/a".into(), is_error: false }
        })
        .unwrap();
        assert_eq!(resp["result"]["isError"], false);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"});
        let resp = handle(&req, never_ask).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
