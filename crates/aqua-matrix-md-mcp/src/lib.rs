//! `aqua-matrix-md-mcp`: a tiny stdio MCP server exposing one tool,
//! `send_markdown_file(filename, markdown) -> delivered`.
//!
//! It speaks MCP JSON-RPC 2.0 over stdin/stdout (see [`jsonrpc`]). It owns
//! **no** Matrix session. When `claude` calls `send_markdown_file`, the server
//! opens the per-run unix socket named in `$MD_MCP_SOCK` (set by the daemon),
//! writes the `{filename, markdown}` pair as one JSON line, reads one JSON reply
//! line, and returns the delivery result to `claude` as the tool result. The
//! daemon side (`aqua_matrix_gating::MdBridge`) owns the Matrix session and
//! performs the actual `AgentClient::send_file`.
//!
//! Fail-closed: any IPC error (no socket, connect refused, malformed/empty
//! reply) becomes a tool result that tells the model the file was NOT delivered
//! and to paste the document inline instead, so content is never silently lost.

pub mod ipc;
pub mod jsonrpc;

/// The name of the single tool this server exposes. The fully-qualified name
/// `claude` sees is `mcp__<server>__send_markdown_file`; the daemon wires the
/// server in under the key `md`, so `--allowedTools mcp__md__send_markdown_file`.
pub const TOOL_NAME: &str = "send_markdown_file";

/// Env var the daemon sets on the spawned MCP subprocess: the path of the
/// per-run unix socket the server connects to for each `send_markdown_file` call.
pub const SOCK_ENV: &str = "MD_MCP_SOCK";

/// Lowercase ASCII slug: alphanumerics kept, every other run collapsed to a
/// single `-`, trimmed, capped to a sane length for a filename.
///
/// SINGLE SOURCE OF TRUTH for markdown-filename slugging across the codebase:
/// the daemon-side bridge (naming the attachment from the model-supplied
/// filename) and the agents-side backstop (naming it from the document's H1)
/// both call this, so the two can never drift.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars().take(60) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Turn a model-supplied `filename` into a safe `.md` filename for a temp file.
///
/// SECURITY: the model controls `filename`, so it must never be used as a path
/// verbatim. We take only the final path component (no directories, no `..`
/// traversal into the writable memory volume), slugify the stem, and force a
/// canonical `.md` extension. An empty or all-punctuation name falls back to a
/// generic one.
pub fn safe_md_filename(raw: &str) -> String {
    use std::path::Path;
    // Only ever the final component: never honour a directory or a `..` segment.
    let name = Path::new(raw).file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Drop any extension the model supplied so a ".md"/".markdown" is not slugged
    // into the stem; we re-add a canonical ".md" below.
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let slug = slugify(stem);
    if slug.is_empty() {
        "aqua-answer.md".to_string()
    } else {
        format!("{slug}.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  multiple   spaces  "), "multiple-spaces");
        assert_eq!(slugify("***"), "");
    }

    #[test]
    fn safe_filename_forces_md_extension() {
        assert_eq!(safe_md_filename("Aqua Partner Strategy"), "aqua-partner-strategy.md");
        assert_eq!(safe_md_filename("report.md"), "report.md");
        assert_eq!(safe_md_filename("notes.markdown"), "notes.md");
    }

    #[test]
    fn safe_filename_blocks_path_traversal() {
        // Only a bare slug + .md may ever survive: no directory, no `..`.
        assert_eq!(safe_md_filename("../../etc/passwd"), "passwd.md");
        assert_eq!(safe_md_filename("/etc/shadow"), "shadow.md");
        assert_eq!(safe_md_filename("a/b/c/deep"), "deep.md");
        let traversal = safe_md_filename("../../x");
        assert_eq!(traversal, "x.md");
        assert!(!traversal.contains('/') && !traversal.contains(".."));
    }

    #[test]
    fn safe_filename_empty_falls_back() {
        assert_eq!(safe_md_filename(""), "aqua-answer.md");
        assert_eq!(safe_md_filename("***"), "aqua-answer.md");
        assert_eq!(safe_md_filename(".."), "aqua-answer.md");
    }
}
