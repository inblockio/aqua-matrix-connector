//! Claude-channel daemon: separate Matrix identity that forwards inbound DMs
//! from `--target` to `claude -p <prompt>` and DMs back the stdout.
//!
//! Each incoming Matrix message becomes a fresh `claude -p` invocation —
//! stateless per message. No conversation continuity for now (could be added
//! later via `claude -c <session-id>` keyed by room or user).
//!
//! See docs/ARCHITECTURE.md for the full design.
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::AgentClient;

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_REPLY_BYTES: usize = 16_000; // Matrix can take more, but be polite

pub async fn run(agent: &AgentClient, target: &str) {
    let watermark = Arc::new(AtomicU64::new(now_epoch_ms()));
    let target = Arc::new(target.to_string());

    tracing::info!(
        "claude-channel daemon starting (target: {}, sync: stream)",
        target
    );

    // Register the message handler before sync starts.
    register_handler(agent.clone(), target.clone(), watermark.clone());

    // Background sync — runs forever.
    let sync_client = agent.client().clone();
    tokio::spawn(async move {
        loop {
            tracing::info!("starting matrix sync stream");
            match sync_client.sync(SyncSettings::default()).await {
                Ok(_) => tracing::warn!("matrix sync returned Ok (unexpected); reconnecting in 5s"),
                Err(e) => tracing::warn!("matrix sync error: {e:#}; reconnecting in 5s"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Park forever — sync drives all work via event handler.
    std::future::pending::<()>().await;
}

fn register_handler(agent: AgentClient, target: Arc<String>, watermark: Arc<AtomicU64>) {
    agent.client().add_event_handler({
        let agent = agent.clone();
        move |ev: OriginalSyncRoomMessageEvent, _room: Room| {
            let agent = agent.clone();
            let target = target.clone();
            let watermark = watermark.clone();
            async move {
                if let Err(e) = handle_event(ev, &agent, &target, &watermark).await {
                    tracing::warn!("claude-channel handler error: {e:#}");
                }
            }
        }
    });
}

async fn handle_event(
    ev: OriginalSyncRoomMessageEvent,
    agent: &AgentClient,
    target: &str,
    watermark: &AtomicU64,
) -> anyhow::Result<()> {
    if ev.sender.as_str() != target {
        return Ok(());
    }
    let ts_ms = u64::from(ev.origin_server_ts.0);
    if ts_ms <= watermark.load(Ordering::Relaxed) {
        return Ok(());
    }

    let body = match &ev.content.msgtype {
        MessageType::Text(t) => t.body.trim().to_string(),
        _ => return Ok(()),
    };

    if body.is_empty() {
        return Ok(());
    }

    // Skip #shell — that belongs to the heartbeat channel, not the LLM channel.
    if body.to_lowercase().starts_with("#shell") {
        watermark
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                if ts_ms > v {
                    Some(ts_ms)
                } else {
                    None
                }
            })
            .ok();
        return Ok(());
    }

    // Advance watermark BEFORE spawning claude — if we crash mid-claude we don't
    // want to re-trigger on restart.
    watermark
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if ts_ms > v {
                Some(ts_ms)
            } else {
                None
            }
        })
        .ok();

    tracing::info!("claude-channel prompt from {}: {} chars", target, body.len());

    // Spawn claude in its own task so sync keeps flowing.
    let agent = agent.clone();
    let target_owned = target.to_string();
    tokio::spawn(async move {
        let reply = match invoke_claude(&body).await {
            Ok(out) => out,
            Err(e) => format!("[claude-channel error] {e:#}"),
        };
        let reply = if reply.trim().is_empty() {
            "[claude-channel] (no output)".to_string()
        } else {
            truncate(&reply, MAX_REPLY_BYTES)
        };
        if let Err(e) = agent.send_dm(&target_owned, &reply).await {
            tracing::warn!("claude-channel reply send failed: {e:#}");
        }
    });

    Ok(())
}

/// Run `claude -p <prompt>` with stdin closed, capturing stdout. Bounded by
/// CLAUDE_TIMEOUT. Spawns headlessly — uses whatever `claude` is on PATH plus
/// the absolute fallback that matches the systemd unit's `Environment=PATH`.
async fn invoke_claude(prompt: &str) -> anyhow::Result<String> {
    let claude_bin = find_claude_bin();
    tracing::debug!("invoking {} -p", claude_bin);

    let mut child = Command::new(&claude_bin)
        .arg("-p")
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {claude_bin} -p"))?;

    // No stdin needed; close it explicitly anyway.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }

    let with_timeout = tokio::time::timeout(CLAUDE_TIMEOUT, child.wait_with_output()).await;
    let output = match with_timeout {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => anyhow::bail!("claude wait failed: {e}"),
        Err(_) => anyhow::bail!("claude -p timed out after {}s", CLAUDE_TIMEOUT.as_secs()),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        anyhow::bail!(
            "claude -p exited with status {}: {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(stdout)
}

fn find_claude_bin() -> String {
    // Try absolute path first (matches the systemd unit Environment).
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = format!("{home}/.local/bin/claude");
    if std::path::Path::new(&candidate).exists() {
        return candidate;
    }
    // Fall back to PATH lookup.
    "claude".to_string()
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Cut on a char boundary to avoid splitting UTF-8.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n[...truncated]");
    out
}

// Re-export anyhow::Context locally so the file is self-contained.
use anyhow::Context;
