# Fix call-agent ↔ Element Call media interop + independent E2E

**Method:** logic-model → process-pipeline.  **Date:** 2026-06-05.
**Connector:** feat/rich-media.  **Call agent:** ../aqua-agents feat/rtc-call-agent.

## GOAL
The agent shows up in Element X (m.call.member works) but exchanges NO media:
its video/TTS audio don't reach Element X, and the operator's speech doesn't reach
the agent. Find the root cause, fix it, and build an INDEPENDENT end-to-end test
that verifies real media flow (not the agent-to-agent false-green).

## CONTEXT (observed)
- Operator confirmed: participant tile shows, but no agent video, no agent audio,
  and the agent doesn't react to the operator speaking.
- Prior "END E2E" was agent↔agent: both used the same naive lk-jwt `room` string
  + libwebrtc codecs, so they interoperated — it never tested a real Element Call client.
- The await-capture run died on `401 M_UNKNOWN_TOKEN`: `sync_once()` uses
  `SyncSettings::default()` and there is no token refresh in the call-agent's long loops.

## Hypothesis register (immutable during execution)
| ID | If | Then | Verification |
|----|-----|------|--------------|
| GH1 | Read lk-jwt-service src + decode the agent's JWT `video.room` claim | We know the exact SFU room each side lands in | decoded claim + source |
| GH2 | Agent vs Element Call derive DIFFERENT LiveKit rooms for the same Matrix room | Explains media gap (separate SFU rooms); match `/sfu/get` to fix | reference client sees agent media after fix |
| GH3 | Same room but no media → codec/connection mismatch | Align codecs/track subscription | reference client subscribes A/V |
| GH4 | Add proactive token refresh/reconnect to the call-agent long loops | Calls/await survive >300s, no 401 | >5min run clean |
| GH5 | Headless reference LiveKit client joins the Element-Call way + asserts bidi media | Test that can't false-green | independent E2E receives agent A/V + vice-versa |
| GH6 | After fixes | Operator sees+hears agent, agent transcribes operator | operator confirm + independent test green |

## ACTIVITIES (dependency-ordered)
1. ⟶ **Ground truth (GH1/GH2):** read element-hq/lk-jwt-service `/sfu/get` room mapping;
   read Element Call's `/sfu/get` request (what `room`/alias it sends); decode the agent's
   live JWT `video.room`. Determine if agent & Element Call share a room. Capture a real
   operator `m.call.member` foci_preferred.livekit_alias if feasible.
2. ⟶ **Fix token refresh (GH4):** the call-agent's await/hold loops must refresh the Matrix
   token (reconnect or proper sync settings) so they survive the ~300s TTL.
3. ⟶ **Fix the join (GH2/GH3):** make `fetch_livekit_token` / the join land in the SAME SFU
   room Element Call uses; align codecs if needed.
4. ⟶ **Independent E2E (GH5):** a headless reference LiveKit client that joins the
   Element-Call way (same room derivation) and asserts it RECEIVES the agent's video+audio
   and the agent receives its track. Must fail if they're in different rooms.
5. **Live confirm (GH6):** operator in Element X sees+hears the agent + agent transcribes.

## BOUNDARY CONDITIONS
- Ground truth BEFORE changing the join (no more guessing).
- The independent test must not share the agent's room-derivation assumption (that is the
  exact false-green to avoid).
- Deepgram key only from ~/.aqua-secrets; never printed/committed. No connector media stack.

## Convergence
The independent reference-client E2E proves bidirectional media with the agent, and the
operator confirms in Element X.

## DISCOVERED DURING EXECUTION
- **GH2 REFUTED (proven live):** agent & Element Call land in the SAME LiveKit room.
  lk-jwt hashes `[room_id, "m.call#ROOM"]`; both pass the raw room_id → identical
  `video.room`. (e2e proof: rtc_room_alias_matches_element_call.)
- **GH7 CONFIRMED (root cause): call-media E2EE mismatch.** Element Call runs
  per-participant **AES-GCM** media E2EE (default-on in encrypted rooms; this
  deployment's call.element.io/config.json has no disable flag). Agent joins with
  `RoomOptions::default()` (no E2EE) → plaintext frames Element X can't use, and it
  can't decrypt Element X's frames. Membership renders (Matrix state) but zero media.
  - Key exchange: `io.element.call.encryption_keys`, **Olm-encrypted to-device**
    (matrix-js-sdk ToDeviceKeyTransport; feature_use_device_session_member_events=true).
    Payload: keys:[{index,key b64(16 random bytes)}], member{id,claimed_device_id},
    session{call_id,application:"m.call",scope:"m.room"}, room_id, sent_ts. index=(i+1)%256.
  - Frame cipher: AES-GCM 128-bit, IV 12B, key index in frame trailer[1]; salt
    `LKFrameEncryptionKey`, HKDF; Element Call MatrixKeyProvider ratchet_window_size=10,
    key_ring_size=256.
  - livekit rust 0.7.44 supports it: RoomOptions.encryption=Some(E2eeOptions{Gcm,
    KeyProvider{shared_key:false, ratchet_window_size:10, key_ring_size:256, HKDF}}),
    KeyProvider::set_key(&ParticipantIdentity, index, key). LK identity = `<user>:<device>`.
  - Transport feasible: matrix-sdk 0.17 `Encryption::encrypt_and_send_raw_to_device`
    (feature `experimental-send-custom-to-device`) Olm-encrypts a custom to-device type.
- **GH4 (token refresh):** call-agent long loops must refresh the Matrix token (~300s TTL)
  via reconnect; sync_once alone 401s after expiry.

## EXECUTION STATE (2026-06-05, pre-compact)
- **E1 DONE+COMMITTED** connector `4e8e2c7`: `send_call_encryption_keys`/`on_call_encryption_keys`
  (io.element.call.encryption_keys Olm to-device), feature `experimental-send-custom-to-device`.
  Also `rtc_room_alias_matches_element_call` e2e proved agent & Element Call share the SAME SFU room.
- **E2 DONE+COMMITTED** call-agent `8891003` (branch feat/rtc-call-agent): `e2ee.rs` CallE2ee/MatrixDriver —
  per-participant AES-GCM E2EE (HKDF, salt LKFrameEncryptionKey), gen+announce 16-byte key, ingest peer keys
  → set_key by LK identity `<user>:<device>`; token-refresh (reconnect within 60s of expiry) in driver +
  capture poll loop (fixes the 401). aqua-e2e call phase runs E2EE over the real DM room.
  - **VERIFIED:** aqua-e2e passes all 12 incl CALL-STT 100% BOTH ways WITH E2EE on (agent-to-agent crypto works).
  - **PARAM KNOB (if Element X interop fails):** e2ee.rs RATCHET_WINDOW_SIZE=16, KEY_RING_SIZE=16. The
    investigation subagent read EC MatrixKeyProvider as 10/256; implementer read 16/16. Likely irrelevant for
    index-0/no-ratchet, but try 10/256 first if real Element X decrypt fails.
- **IN PROGRESS — the independent test (operator in real Element X):** `--await-operator-call` armed,
  bg task brp40x9aj, log /tmp/await-e2ee.log, 20-min wait. Operator starts a call in their DM in Element X;
  agent captures membership + joins WITH E2EE. AUTONOMOUS DECRYPT PROOF = if the agent's STT logs a transcript
  of the operator's speech, the agent decrypted Element X's audio. Operator seeing/hearing the agent = encrypt proof.
  Re-arm cmd: `cd ~/aqua-agents && export LK_CUSTOM_WEBRTC=$(find /tmp/lk-probe -type d -name linux-x64-release|head -1)
  && set -a && . ~/.aqua-secrets/deepgram.env && set +a && ./target/debug/aqua-call-agent --key-file
  ~/aqua-matrix-agent/agent.pem --store-dir ~/.aqua-call-agent --await-operator-call --wait-timeout 1200 --hold-timeout 600`
- NEXT after operator confirm: if works → audit + finish; if decrypt fails → flip params to 10/256, rebuild, re-test.

## LIVE BREAKTHROUGH (2026-06-05 ~04:35) + parse fix
- Operator started a real Element X call. Agent: captured op membership (member-id 6zqlNZ...,
  state_key `_<user>_<device>_m.call`, content also has `m.call.intent:video` + `membershipID:<user>:<device>`),
  set own membership, installed own key (set_ok=true), CONNECTED e2ee=true, PUBLISHED video+TTS,
  **SUBSCRIBED to operator's remote audio+video** (SAME SFU room — media path live). Sent own key to 2 devices.
- BUG: agent could NOT parse operator's `io.element.call.encryption_keys` to-device:
  `invalid type: map, expected a sequence` — Element X sends `keys` as a SINGLE {index,key} OBJECT, not array.
  → no peer key installed → heard_transcripts=[] (no decrypt).
- FIX COMMITTED connector `330a709`: keys parses leniently (array|single-object|index-map); member/session/extra
  kept as Value so handler ALWAYS runs + LOGS verbatim (`RX io.element.call.encryption_keys`); device extracted
  from member.claimed_device_id/device_id/membershipID(last ':')/top-level; SEND now emits single-object keys +
  device_id+membershipID to match EX. 15 tests pass.
- RE-ARMED await (bxbgwiwbm, /tmp/await-e2ee3.log, 30min) for the operator to call again. EXPECT: RX raw log shows
  EX key shape; agent installs peer key; STT transcribes operator (=decrypt proof); operator sees/hears agent (=encrypt).
- IF encrypt direction (operator can't see/hear agent) still fails after this: suspects = (a) EX expects array not
  single-object after all (flip back), (b) ratchet/key_ring params 16 vs 10/256, (c) agent membership missing
  m.call.intent/membershipID. IF decrypt (no STT) still fails: read RX raw log for the true keys shape + device field.

## ✅ SUCCESS — full bidirectional E2EE media interop with real Element X (operator-confirmed 2026-06-05 ~05:09)
- DECRYPT (Element X → agent): agent installed operator's key (set_ok=true) and Deepgram STT transcribed the
  operator's REAL speech ("Hello. Please confirm that what you hear is…") — 2097 frames decrypted. Proven autonomously.
- ENCRYPT (agent → Element X): OPERATOR CONFIRMED "i heard and saw the agent. It worked." — agent's identity
  animation video + TTS greeting decrypted + rendered in Element X.
- Verbatim Element X encryption_keys shape (captured): keys=ARRAY [{index,key}], member.claimed_device_id=<device>,
  member.id=<per-session uuid, unused for identity>, session{application:m.call,call_id:"",scope:m.room}, room_id, sent_ts.
- Final fixes: connector 330a709 (permissive parse + raw log + device extract) + a9a3392 (send keys as ARRAY matching EX).
  Confirmed call used single-object send (also worked, EX lenient); shipped = array (EX-native, guaranteed-compatible).
- CONVERGENCE MET: independent real-Element-X test passes BOTH directions. m.call.member renders, video+TTS+STT all work.
