//! A rich-media aqua-matrix agent: receive attachments, save them, and send some
//! back. Same contract as `echo_agent.rs` — implement `MessageHandler`, build an
//! `AgentConfig`, call `run_daemon` — but this one exercises the media/call API.
//!
//! What it does:
//!   * Inbound attachment (image/audio/voice/video/file) → log its kind/size/
//!     duration, download the bytes to `/tmp/aqua-media`, and DM a description.
//!   * The text command `send image` → create a tiny 1x1 PNG in /tmp and
//!     `send_image` it back; `send voice` → `send_voice_message` a tiny clip.
//!   * An inbound call signal (invite / ring / hangup) → log it and DM that we
//!     only do signaling, no media.
//!
//! Run with:  cargo run -p aqua-matrix-relay --example media_agent
//! (generates `media.pem` on first run; talks to the default homeserver.)

use std::path::PathBuf;

use aqua_matrix_relay::{
    async_trait, run_daemon, AgentClient, AgentConfig, InboundCall, InboundMessage, MessageHandler,
};

struct MediaHandler;

#[async_trait]
impl MessageHandler for MediaHandler {
    fn role(&self) -> &str {
        "media"
    }

    fn hello(&self, agent: &AgentClient) -> Option<String> {
        Some(format!(
            "[media] online as {}. Send me a file and I'll save it; \
             type `send image` or `send voice` and I'll send one back.",
            agent.user_id()
        ))
    }

    async fn handle_message(
        &self,
        agent: &AgentClient,
        target: &str,
        msg: &InboundMessage<'_>,
    ) -> anyhow::Result<()> {
        // Attachment? Log its metadata, pull the (decrypted) bytes to a temp
        // dir, and describe what we got. The `handle` carries the MediaSource;
        // we never name a Matrix type.
        if let Some(media) = &msg.media {
            tracing::info!(
                "got {} {:?} ({} bytes, {:?} ms)",
                media.kind.as_str(),
                media.filename,
                media.size.unwrap_or(0),
                media.duration_ms,
            );
            let saved = agent
                .download_media_to_temp(&media.handle, "/tmp/aqua-media")
                .await?;
            let secs = media.duration_ms.map(|ms| ms as f64 / 1000.0);
            let dur = secs.map(|s| format!(", {s:.1}s")).unwrap_or_default();
            agent
                .send_dm(
                    target,
                    &format!(
                        "got a {} ({}{}), saved to {}",
                        media.kind.as_str(),
                        media.filename,
                        dur,
                        saved.display(),
                    ),
                )
                .await?;
            return Ok(());
        }

        // Plain text: a couple of demo commands that send media back. Anything
        // else gets the help text.
        match msg.body {
            "send image" => {
                // A 1x1 transparent PNG, enough for the SDK to send as `m.image`.
                let png: &[u8] = &[
                    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49,
                    0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06,
                    0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44,
                    0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d,
                    0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42,
                    0x60, 0x82,
                ];
                tokio::fs::write("/tmp/aqua-pixel.png", png).await?;
                agent
                    .send_image(target, "/tmp/aqua-pixel.png", Some("a 1x1 pixel"))
                    .await?;
            }
            "send voice" => {
                // Reuse the same blob as a stand-in clip; a real voice note would
                // be opus. We pass the duration (we'd know it from encoding) and
                // let the connector synthesise a waveform.
                tokio::fs::write("/tmp/aqua-clip.ogg", b"not really opus, just a demo").await?;
                agent
                    .send_voice_message(target, "/tmp/aqua-clip.ogg", 3_200, None)
                    .await?;
            }
            other => {
                agent
                    .send_dm(
                        target,
                        &format!(
                            "you said {other:?}. Try `send image`, `send voice`, \
                             or send me a file and I'll save it."
                        ),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Signaling only — we can observe a call but matrix-sdk carries no media
    /// stream, so we just log it and say so.
    async fn on_call(&self, agent: &AgentClient, target: &str, call: &InboundCall) {
        tracing::info!(
            "call signal {:?} (id {}) from {} in {}",
            call.signal,
            call.call_id,
            call.sender_mxid,
            call.room_id,
        );
        let _ = agent
            .send_dm(
                target,
                &format!(
                    "got a {:?} call signal — I do signaling only, no media.",
                    call.signal
                ),
            )
            .await;
    }
}

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let config = AgentConfig {
        key_file: PathBuf::from("media.pem"),
        siwx_url: "https://siwx-oidc.inblock.io".into(),
        matrix_url: "https://matrix.inblock.io".into(),
        client_id: None,
        redirect_uri: None,
        store_dir: PathBuf::from(home).join(".aqua-matrix-media"),
        // None → connect() derives a stable device_id from the DID.
        device_id: None,
    };

    // Whoever is allowed to talk to the agent. Set AGENT_TARGET (e.g. in a
    // `.env` — see `.env.example`) to your own Matrix user ID.
    let target = std::env::var("AGENT_TARGET")
        .expect("set AGENT_TARGET to the Matrix user ID this media agent should serve");
    run_daemon(config, &target, MediaHandler).await;
}
