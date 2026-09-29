//! A messenger **profile**: which identity the tools speak for, who may be
//! messaged by default, which tools are exposed, and the limits. The host
//! "Aqua System" bridge and an embedded agent differ only in their profile;
//! the policy code ([`crate::engine`]) and the tool surface
//! ([`crate::jsonrpc`]) are shared.
//!
//! `wait_for_reply` is OFF in the default profile and in
//! [`Profile::embedded_agent`] (Tim, 2026-09-29): a turn-based agent receives
//! its owner's answer as its next turn, and a blocking wait inside a turn
//! stalls it. Only the host bridge profile ([`Profile::host`]) turns it on,
//! explicitly, so the host behaviour is unchanged. A disabled tool is not
//! listed at all, and the engine refuses it if called anyway.

use crate::attachments::AttachmentPolicy;
use crate::jsonrpc::{
    TOOL_NAMES, T_FETCH_ATTACHMENT, T_LIST_RECIPIENTS, T_READ_INBOX, T_SEND_FILE, T_SEND_MESSAGE,
    T_WAIT_FOR_REPLY,
};

/// Size, rate and wait limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest Markdown body accepted by `send_message`, in bytes.
    pub max_message_bytes: usize,
    /// Largest file accepted by `send_file`, in bytes.
    pub max_file_bytes: u64,
    /// Per-target (person or room) send budget: at most this many sends ...
    pub rate_count: usize,
    /// ... per this many seconds (sliding window).
    pub rate_window_secs: u64,
    /// Longest `wait_for_reply` (only relevant where it is enabled).
    pub max_wait_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_bytes: 20_000,
            max_file_bytes: 10 * 1024 * 1024,
            rate_count: 20,
            rate_window_secs: 600,
            max_wait_secs: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Identity name used in texts and framing ("Aqua System").
    pub label: String,
    /// MCP `serverInfo.name`.
    pub server_name: String,
    /// Who must approve messaging anyone new ("Tim", "your owner").
    pub approver: String,
    /// Allow-list name used when `to` is omitted (the embedded agent's owner).
    /// `None` = `to` is required (host bridge).
    pub default_to: Option<String>,
    /// Append the `<sub>via cwd@host</sub>` origin tag to outgoing messages
    /// (host: many sessions share one identity; an agent speaks for itself).
    pub tag_origin: bool,
    /// Extra sentence on `read_inbox` about who shares the inbox.
    pub inbox_note: String,
    /// The tools exposed. Always listed in canonical [`TOOL_NAMES`] order.
    pub tools: Vec<&'static str>,
    pub limits: Limits,
    pub attachments: AttachmentPolicy,
}

impl Default for Profile {
    /// The shared default: [`Profile::embedded_agent`] for an unnamed agent
    /// whose owner is `owner`. No `wait_for_reply`.
    fn default() -> Self {
        Self::embedded_agent("this agent", "owner")
    }
}

impl Profile {
    /// The host-wide "Aqua System" bridge: all six tools (`wait_for_reply`
    /// enabled explicitly here and only here), origin tags, no default
    /// recipient.
    pub fn host() -> Self {
        Self {
            label: "Aqua System".into(),
            server_name: "aqua-system-bridge-mcp".into(),
            approver: "Tim".into(),
            default_to: None,
            tag_origin: true,
            inbox_note: "The inbox is shared by all sessions on this host.".into(),
            tools: vec![
                T_SEND_MESSAGE,
                T_SEND_FILE,
                T_LIST_RECIPIENTS,
                T_READ_INBOX,
                T_FETCH_ATTACHMENT,
            ],
            limits: Limits::default(),
            attachments: AttachmentPolicy::default(),
        }
        .with_wait_for_reply()
    }

    /// The default tooling of an agent embedding the connector: its own
    /// identity, `to` defaults to the owner, no origin tag. `wait_for_reply`
    /// is OFF (an embedded agent receives its owner's replies as its next
    /// turn); enable it explicitly with [`Profile::with_wait_for_reply`].
    pub fn embedded_agent(label: &str, owner_name: &str) -> Self {
        Self {
            label: label.into(),
            server_name: "aqua-messenger-mcp".into(),
            approver: "your owner".into(),
            default_to: Some(owner_name.into()),
            tag_origin: false,
            inbox_note:
                "The inbox holds messages to this agent from allow-listed people and rooms.".into(),
            tools: vec![
                T_SEND_MESSAGE,
                T_SEND_FILE,
                T_LIST_RECIPIENTS,
                T_READ_INBOX,
                T_FETCH_ATTACHMENT,
            ],
            limits: Limits::default(),
            attachments: AttachmentPolicy::default(),
        }
    }

    /// Opt in to `wait_for_reply` (explicit configuration only).
    pub fn with_wait_for_reply(mut self) -> Self {
        if !self.tools.contains(&T_WAIT_FOR_REPLY) {
            self.tools.push(T_WAIT_FOR_REPLY);
        }
        self.normalize();
        self
    }

    /// Drop `wait_for_reply`.
    pub fn without_wait_for_reply(mut self) -> Self {
        self.tools.retain(|t| *t != T_WAIT_FOR_REPLY);
        self
    }

    /// Keep `tools` deduplicated and in canonical order.
    fn normalize(&mut self) {
        let mut out: Vec<&'static str> = TOOL_NAMES
            .iter()
            .copied()
            .filter(|t| self.tools.contains(t))
            .collect();
        out.dedup();
        self.tools = out;
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_reply_is_host_only() {
        assert!(!Profile::default().has_tool(T_WAIT_FOR_REPLY));
        assert!(!Profile::embedded_agent("A", "owner").has_tool(T_WAIT_FOR_REPLY));
        let host = Profile::host();
        assert!(host.has_tool(T_WAIT_FOR_REPLY));
        // canonical order, wait_for_reply before fetch_attachment as before
        assert_eq!(host.tools, TOOL_NAMES.to_vec());
        let opted = Profile::embedded_agent("A", "o").with_wait_for_reply();
        assert_eq!(opted.tools, TOOL_NAMES.to_vec());
        assert!(!opted.without_wait_for_reply().has_tool(T_WAIT_FOR_REPLY));
    }
}
