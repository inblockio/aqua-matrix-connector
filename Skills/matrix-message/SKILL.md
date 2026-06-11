---
name: matrix-message
description: Send and receive E2E encrypted Matrix messages via aqua-matrix-agent
---

# Matrix Messaging

Send and receive end-to-end encrypted Matrix messages using `aqua-matrix-agent`.

## Prerequisites

Binary at `~/aqua-matrix-agent/target/debug/aqua-matrix-agent`. If missing:

```bash
cd ~/aqua-matrix-agent && cargo build
```

## Quick Reference

| Operation | Command |
|---|---|
| Send message | `~/aqua-matrix-agent/target/debug/aqua-matrix-agent --message "text" --target "@user:matrix.inblock.io"` |
| Read messages | `~/aqua-matrix-agent/target/debug/aqua-matrix-agent --read --read-limit 20 --target "@user:matrix.inblock.io"` |
| Send + read | `~/aqua-matrix-agent/target/debug/aqua-matrix-agent --message "text" --read --target "@user:matrix.inblock.io"` |
| Print agent DID | `~/aqua-matrix-agent/target/debug/aqua-matrix-agent --print-did` |
| Use different identity | Add `--key-file path/to/other.pem` |

## Setup

**Zero-config:** The agent auto-registers an OIDC client on first run and caches credentials in `~/.aqua-matrix-agent/config.toml`. No manual setup needed.

**Default servers:**
- Matrix: `https://matrix.inblock.io`
- SIWX-OIDC: `https://siwx-oidc.inblock.io`
- Override with `--matrix-url` / `--siwx-url` if using a different server

**Target:** set `AGENT_TARGET` in a `.env` file (copy `.env.example`); no target is hardcoded in the repo. On this host a `.env` in the repo dir already provides it, so the commands above work without `--target`. Override ad-hoc with `--target`.

**Agent identity:** Ed25519 key at `agent.pem` in CWD (auto-generated if missing). Each key file produces a unique DID and Matrix account.

## Encryption and Verification

- All DMs are E2E encrypted by default (Megolm via matrix-sdk)
- The agent bootstraps cross-signing on first connect, so its device appears as "verified" in Element
- Crypto state (identity keys, session keys) persists in SQLite at `~/.aqua-matrix-agent/`
- Messages that cannot be decrypted show as `[unable to decrypt]`

## Agent-to-Agent Communication

Two agents can communicate by using different key files:

```bash
# Agent A (default key)
aqua-matrix-agent --message "ping" --target "@agent-b-user:matrix.inblock.io"

# Agent B (separate key)
aqua-matrix-agent --key-file agent-b.pem --store-dir ~/.aqua-matrix-agent-b \
  --read --target "@agent-a-user:matrix.inblock.io"
```

Pre-existing keys: `~/aqua-matrix-agent/agent.pem` (Agent A) and `~/aqua-matrix-agent/agent-b.pem` (Agent B).

## Rich media (files, images, voice, calls)

Beyond text, the connector can send and receive rich media over E2E DMs. These are
**`AgentClient` library methods for agent backends** (a `MessageHandler` that calls
`run_daemon`), not flags on the one-shot CLI. matrix-sdk auto-encrypts attachments
on send and auto-decrypts them on download, so this works transparently in encrypted
DMs. The connector does **no** audio decoding — for a voice message you supply the
duration (and optionally a waveform).

**Send** (all async, return `anyhow::Result`; `target` = peer MXID, `caption` becomes the body):

- `agent.send_image(target, path, caption)` — `m.image` (dimensions read from header bytes, no full decode)
- `agent.send_file(target, path, caption)` — `m.file`
- `agent.send_audio(target, path)` / `agent.send_video(target, path, caption)`
- `agent.send_voice_message(target, path, duration_ms, waveform)` — `m.audio` with MSC3245 voice markers so Element X shows a waveform bubble; pass the known `duration_ms`, and `waveform: Option<Vec<f32>>` (synthesised if `None`)

**Receive:** inbound attachments arrive on `InboundMessage.media: Option<InboundMedia>`
(captioned attachments put the caption in `body`). `InboundMedia` carries `kind`
(`Image|Audio|Voice|Video|File`), `filename`, `mimetype`, `size`, `duration_ms`,
`width`/`height`, `is_voice`, `waveform`, and a `handle`. Fetch the bytes on demand:

- `agent.download_media(&media.handle)` → `Vec<u8>` (auto-decrypted)
- `agent.download_media_to_temp(&media.handle, dir)` → `PathBuf`

**Calls — signaling and detection only.** `agent.ring_call(target)` sends an
`m.call.notify` Ring (MSC4075) so a peer's Element X shows an incoming call, and the
default-no-op `MessageHandler::on_call` receives inbound `m.call.invite` /
`m.call.notify` / `m.call.hangup` events as an `InboundCall { signal, call_id,
sender_mxid, room_id }`. **The agent can ring and detect calls but cannot place or
carry an actual audio/video stream** — matrix-sdk 0.17 ships no WebRTC/LiveKit media
stack; live call-media participation would need a separate WebRTC engine plus the
Element Call SFU. Files, images and voice messages are fully functional.

Worked example: `crates/aqua-matrix-relay/examples/media_agent.rs`. Design notes:
`docs/plans/2026-06-04-rich-media.md`. Full method/type reference: `docs/REFERENCE.md`.

## Environment Variables

| Var | Purpose | Default |
|---|---|---|
| `AGENT_KEY_FILE` | Path to Ed25519 PEM key | `agent.pem` |
| `SIWX_URL` | siwx-oidc server URL | `https://siwx-oidc.inblock.io` |
| `MATRIX_URL` | Matrix homeserver URL | `https://matrix.inblock.io` |
| `OIDC_CLIENT_ID` | Override OIDC client (skips auto-registration) | auto |
| `OIDC_REDIRECT_URI` | Override redirect URI | `http://localhost:0/callback` |
| `AGENT_STORE_DIR` | SQLite + config directory | `~/.aqua-matrix-agent` |

## Troubleshooting

- **"siwx-oidc authentication failed"**: Server may be down. Check `curl https://siwx-oidc.inblock.io/.well-known/openid-configuration`
- **"[unable to decrypt]"**: Crypto store mismatch. The sender encrypted before this agent's device existed. New messages will decrypt fine.
- **"initial sync failed"**: Network issue or expired token. Retry.
- **Cross-signing bootstrap warning**: Non-fatal. Agent works without cross-signing; device just shows as "unverified" in Element.
