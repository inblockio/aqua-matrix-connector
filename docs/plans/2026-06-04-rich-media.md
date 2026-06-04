# Rich-media for the Matrix connector — plan & hypothesis register

**Branch:** `feat/rich-media`  **Date:** 2026-06-04  **Method:** logic-model → process-pipeline

## Goal (one sentence)
Give the agent first-class, programmatic Matrix rich-media capabilities — send/receive
files & images, send/receive voice messages, and detect/initiate calls — exposed as clean
`AgentClient` methods and surfaced through the `MessageHandler`/`InboundMessage` seam,
working inside E2E-encrypted DMs.

## Scope boundary (decided autonomously; flagged for review)
**"Video calls" = call signaling + detection, NOT live WebRTC media participation.**
matrix-sdk 0.17 ships no WebRTC/LiveKit media stack; an agent can detect a call and
ring/notify, but cannot stream webcam/mic without embedding `webrtc-rs` + Element Call SFU
(a separate, large follow-up). Files / images / voice are fully real.

## Source-verified feasibility (de-risked before coding)
- `Room::send_attachment(name, &Mime, Vec<u8>, AttachmentConfig)` — auto-encrypts in E2E rooms
  (room/mod.rs:2742). Receive via `client.media().get_media_content(&MediaRequestParameters{source,
  MediaFormat::File})` — auto-decrypts (media.rs:466).
- Voice: `AttachmentInfo::Voice(BaseAudioInfo{duration,size,waveform})` + an `audio/*` mime →
  SDK sets `content.voice = UnstableVoiceContentBlock` (room/mod.rs:347) and the MSC3245
  audio-details block (waveform `Vec<f32>` 0–1) when duration+waveform present (room/mod.rs:343).
- matrix-sdk default features already include `e2e-encryption`, `sqlite`,
  `unstable-msc3245-v1-compat`. Workspace adds only `markdown` (additive).

## Hypothesis register (immutable during execution)

| ID | If | Then | Assumptions | Verification |
|----|-----|------|-------------|--------------|
| H1 | Add `mime`/`mime_guess`/`imagesize` deps + `media.rs` with `send_file`/`send_image` over `Room::send_attachment` | Agent sends files/images to an E2E DM; homeserver returns event_id | DM-room resolver reusable; mime detectable | `cargo build`; unit test on mime/kind mapping; compile of example |
| H2 | `send_voice_message` passes `AttachmentInfo::Voice{duration,size,waveform 0–1}` with `audio/ogg` mime | Sent event carries `org.matrix.msc3245.voice` + `msc1767.audio` (Element X voice bubble) | waveform synthesizable when caller omits it | source proof (room/mod.rs:343-347) + unit test on waveform normalization |
| H3 | Widen `dispatch` to map Image/Audio/Video/File `MessageType` → `InboundMedia{kind,filename,mime,size,dims,duration,is_voice,waveform,handle}` | Handler receives media metadata + can fetch bytes | watermark/dedup/empty-body logic preserved for media | `cargo build`; unit test on inbound mapping |
| H4 | `download_media(&MediaHandle)` wraps `get_media_content(.., MediaFormat::File)` | Encrypted media auto-decrypts to plaintext bytes | handle carries the ruma `MediaSource` | source proof (media.rs auto-decrypt) + signature/compile |
| H5 | `InboundMessage` gains owned `media: Option<InboundMedia>` (additive) + `on_call` default-no-op trait method | Existing handlers (heartbeat, claude-p, echo) compile unchanged | only `dispatch` constructs `InboundMessage` | connector `cargo build` + `cd ../aqua-agents && cargo build` |
| H6 | Register `OriginalSyncCallInviteEvent`/`HangupEvent` handlers → `InboundCall` via `on_call`; add `ring_call`/`notify_call` | Legacy call signaling detected + a call can be announced | macro-generated sync aliases exist; msc3401 = best-effort | `cargo build`; compile of example `on_call` |
| H7 | All new code stays in connector crates, names no agents-side crate | `check-dep-direction.sh` passes | media backends live in aqua-agents | run `scripts/check-dep-direction.sh` |
| H8 | All changes land | `cargo build --release` + `cargo clippy` clean + `cargo test` (new unit tests) green | — | run all three (convergence criterion) |

## Activities (dependency-ordered)
1. **[core]** Cargo deps (agent crate): `mime`, `mime_guess`, `imagesize`. (H1)
2. **[core]** `agent/src/media.rs`: `MediaKind`, `MediaHandle`, `ensure_dm_room` refactor,
   `send_file/send_image/send_audio/send_video/send_voice_message`, `download_media[_to_temp]`. (H1,H2,H4)
3. **[core]** `relay`: `InboundMedia`, `InboundMessage.media`, widen `dispatch`, re-export media types. (H3,H5)
4. **[core]** Calls: `InboundCall`, `on_call` default method, register call handlers, `ring_call`/`notify_call`. (H6)
5. **[leaf]** Unit tests + `examples/media_agent.rs`. (H1,H2,H3)
6. **[leaf]** Docs: README/CLAUDE.md/REFERENCE + `/matrix-message` skill. 
7. **[leaf]** `../aqua-agents` `claude-p` backend wiring (consume inbound media; expose send). (H5)
8. **[gate]** `cargo build --release` + clippy + tests + dep-direction. (H7,H8)

## Boundary conditions
- Connector stays agent-name-free (H7). No WebRTC media. No audio transcoding in the connector
  (caller supplies duration/optional waveform). No `git push` (branch + local commits only).
- Do not break: text path, streaming edits, watermark/dedup, E2E, existing handler compile (H5).
