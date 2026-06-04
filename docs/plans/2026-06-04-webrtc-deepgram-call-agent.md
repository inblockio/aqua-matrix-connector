# Phase 2 — WebRTC call agent + Deepgram voice (transcript agents)

**Method:** logic-model → process-pipeline (fresh pass).  **Date:** 2026-06-04.
**Connector branch:** `feat/rich-media` (this repo).  **Agents branch:** `feat/rtc-call-agent` (../aqua-agents).

## GOAL (one sentence)
A "transcript/call agent" can **join a Matrix (Element Call / MatrixRTC) video call, publish a video track that is an animation representing itself and an audio track of Deepgram TTS speech, and transcribe the other party's audio via Deepgram STT** — proven by a live E2E where **two instances call each other and each sees/hears/transcribes the other, on top of the already-working text/file/image/voice channels, from both sides.**

## CONTEXT (verified by feasibility scout, 2026-06-04)
- **SFU is LIVE.** `https://matrix.inblock.io/.well-known/matrix/client` →
  `org.matrix.msc4143.rtc_foci: [{type: livekit, livekit_service_url: "https://matrix.inblock.io/livekit/jwt"}]`.
  lk-jwt-service healthz=200; LiveKit SFU at `wss://matrix.inblock.io/livekit/sfu/`.
  Join path: Matrix OpenID token → `POST /livekit/jwt/sfu/get {room, openid_token, device_id}` → `{jwt, url}` → LiveKit room.
- **Deepgram VALID** (key at `~/.aqua-secrets/deepgram.env`, var `DEEPGRAM_API_KEY`, 600/off-repo).
  TTS `POST /v1/speak?model=aura-2-thalia-en&encoding=linear16&sample_rate=48000&container=none` → raw PCM
  (or `encoding=opus&container=ogg`). STT `wss://api.deepgram.com/v1/listen?model=nova-3&encoding=linear16&sample_rate=48000`.
- **Crates:** `livekit` 0.7.44 + `livekit-api` 0.5.1. `webrtc-sys/build.rs` downloads a prebuilt libwebrtc
  from GitHub releases (`LK_CUSTOM_WEBRTC` to vendor/offline). Host rust 1.95, image 1.93.
- **Already done (Phase 1, confirmed live):** text/file/image/voice send+receive+decrypt between two
  instances; call *signaling* (`ring_call`/`on_call`). `AgentClient::recent_media` helper exists.

## ARCHITECTURE (where code lives)
- **Connector (`feat/rich-media`)** gains only thin Matrix-session primitives (no media stack):
  - `request_openid_token()` → the `{access_token, server_name, ...}` for lk-jwt.
  - MatrixRTC membership: set/clear the `m.call.member` (msc3401) state so Element X shows the agent as a
    participant; read `rtc_foci` from `.well-known`.
- **New crate in `../aqua-agents`: `aqua-call-agent`** (heavy media stays agents-side, per the repo boundary):
  deps `livekit`, the connector, a small Deepgram client (reqwest + tokio-tungstenite). Owns: jwt exchange,
  LiveKit connect, generated video frames (animation), TTS→audio-track, remote-audio→STT→transcript, and the
  two-instance call orchestration/binary.

## Hypothesis register (immutable during execution)
| ID | If | Then | Verification |
|----|-----|------|--------------|
| HW1 | Add `livekit` 0.7 to a crate and build in this env | libwebrtc downloads + links; crate builds | `/tmp/lk-probe` `cargo build` succeeds |
| HW2 | Add `AgentClient::request_openid_token()` (matrix-sdk openid API) | Agent gets a valid Matrix OpenID token | live call returns a token struct |
| HW3 | POST that token to `/livekit/jwt/sfu/get` with room+device | lk-jwt returns `{jwt, url}` | live HTTP 200 with a JWT |
| HW4 | `livekit::Room::connect(url, jwt)` then publish a generated video+audio track | Tracks publish; a second participant/SFU sees them | livekit room state / 2nd instance subscribes |
| HW5 | Connector sets the `m.call.member` state for the room | Element X / the peer agent sees the agent as a call participant | state event present; peer observes membership |
| HW6 | Deepgram TTS PCM pushed into the LiveKit audio source | Remote party receives intelligible audio | peer STT transcribes it correctly |
| HW7 | Subscribe to remote audio track → stream to Deepgram STT | Agent gets a correct transcript of what the peer said | transcript matches spoken text |
| HW8 | Two instances each join + publish animation+TTS + transcribe peer | Full bidirectional call works, both sides | live two-instance E2E asserts both transcripts |
| HW9 | All of text/file/image/voice/call run in one two-instance harness | The END E2E passes every channel, both directions | one test/binary green end-to-end |

## ACTIVITIES (dependency-ordered; ⟶ = critical path)
1. ⟶ **HW1** retire libwebrtc build risk (probe build, running). Decide `LK_CUSTOM_WEBRTC` vendoring if needed.
2. ⟶ **Connector primitives** (HW2, HW5): `request_openid_token`; `set_rtc_member`/`leave_rtc_member`;
   `.well-known` rtc_foci reader. Keep additive; dep-guard stays green.
3. ⟶ **`aqua-call-agent` crate** scaffold (HW3, HW4): jwt exchange + LiveKit connect + publish a static
   video+audio track (smoke: connect two clients to one room, see each other).
4. **Animation video source** (HW4): generate I420 frames (identity animation — pulsing disc + label).
5. **Deepgram client** (HW6, HW7): TTS (text→PCM) into the audio source; STT (remote PCM→transcript) via WS.
6. ⟶ **Two-instance call orchestration** (HW8): both agents ring → join the same room/call → publish →
   transcribe peer; assert transcripts both ways.
7. ⟶ **END E2E harness** (HW9): one runnable that exercises text+file+image+voice+call(video+TTS+STT) between
   two instances and reports per-channel pass/fail, both directions.
8. Docs update: lift the "no live media" caveat; document the call agent + Deepgram wiring + the END E2E.

## BOUNDARY CONDITIONS
- **Invariant:** livekit/libwebrtc/Deepgram stay OUT of the connector substrate — agents-side only. Connector
  gains only thin Matrix-session primitives. dep-direction guard must stay green.
- **Invariant:** do not break Phase-1 channels or existing handlers.
- **Secret:** Deepgram key only from `~/.aqua-secrets/deepgram.env`; never commit it; never print its value.
- **Risk:** libwebrtc download needs GitHub-releases network at build (mitigate `LK_CUSTOM_WEBRTC`); large
  image growth — keep the call agent a separate binary, not baked into the base agent image unless wanted.
- **Token-budget gotcha:** `sync_once()` uses `SyncSettings::default()` (timeout:None → long-poll); the
  ~300s siwx token TTL means call setup must be economical or refresh proactively.
- **Exclusion (for now):** no recording/persistence of call media beyond transcripts; no multi-party (>2) calls.

## Convergence criterion
The END E2E (HW9) passes every channel — text, file, image, voice, and a video call where each instance
publishes an identity animation + TTS speech and transcribes the other — from both sides, against the live
homeserver. Connector `cargo build`/dep-guard green; `aqua-call-agent` builds.
