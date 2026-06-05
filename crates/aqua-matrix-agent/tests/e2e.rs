#![cfg(feature = "e2e")]

use aqua_matrix_agent::{AgentClient, AgentConfig, MediaKind};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn agent_config(key_file: &str, store_dir: &str) -> AgentConfig {
    AgentConfig {
        key_file: repo_root().join(key_file),
        siwx_url: "https://siwx-oidc.inblock.io".into(),
        matrix_url: "https://matrix.inblock.io".into(),
        client_id: None,
        redirect_uri: None,
        store_dir: PathBuf::from(store_dir),
    }
}

async fn sync_n(agent: &AgentClient, n: usize) {
    for _ in 0..n {
        agent.sync_once().await.expect("sync failed");
    }
}

fn clean_store(path: &str) {
    let _ = std::fs::remove_dir_all(path);
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn e2ee_bidirectional_messaging() {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .try_init()
        .ok();

    // Wipe stores to avoid stale crypto keys from prior runs or CLI usage.
    // The device_id is deterministic per DID, so any other crypto store that
    // used the same key file will have uploaded conflicting identity keys.
    clean_store("/tmp/aqua-e2e-agent-a");
    clean_store("/tmp/aqua-e2e-agent-b");

    // Connect both agents via CAIP-122 auth
    let agent_a = AgentClient::connect(agent_config("agent.pem", "/tmp/aqua-e2e-agent-a"))
        .await
        .expect("Agent A failed to connect");
    println!("Agent A connected: {} ({})", agent_a.user_id(), agent_a.did());

    let agent_b = AgentClient::connect(agent_config("agent-b.pem", "/tmp/aqua-e2e-agent-b"))
        .await
        .expect("Agent B failed to connect");
    println!("Agent B connected: {} ({})", agent_b.user_id(), agent_b.did());

    assert_ne!(agent_a.user_id(), agent_b.user_id(), "agents must have different identities");

    // Phase 1: Establish DM room and get both agents into it.
    // The setup message creates the room and invites Agent B.
    agent_a
        .send_dm(agent_b.user_id(), "e2e-room-setup")
        .await
        .expect("Agent A failed to create DM room");
    println!("DM room created by Agent A");

    // Agent B joins the room
    sync_n(&agent_b, 2).await;
    agent_b.join_invited_rooms().await.expect("Agent B join failed");
    sync_n(&agent_b, 2).await;
    println!("Agent B joined the room");

    // Agent A learns about Agent B's device keys
    sync_n(&agent_a, 2).await;

    // Phase 2: Agent B sends first (B created its outbound session AFTER joining,
    // so B's session key is shared with A from the start).
    let tag = uuid::Uuid::new_v4().to_string();

    let msg_b_to_a = format!("e2e-test-b-to-a-{tag}");
    let event_id = agent_b
        .send_dm(agent_a.user_id(), &msg_b_to_a)
        .await
        .expect("Agent B failed to send");
    println!("Agent B sent: {msg_b_to_a} (event: {event_id})");

    // Agent A syncs to receive the message and key-sharing events
    sync_n(&agent_a, 2).await;

    let room_id = agent_a
        .dm_room_id(agent_b.user_id())
        .await
        .expect("failed to get DM room")
        .expect("no DM room found between agents");

    let messages = agent_a
        .messages(&room_id, 10)
        .await
        .expect("Agent A failed to read messages");

    let found = messages.iter().find(|m| m.body == msg_b_to_a);
    assert!(
        found.is_some(),
        "Agent A did not find message from Agent B: {msg_b_to_a}\nMessages: {:?}",
        messages.iter().map(|m| &m.body).collect::<Vec<_>>()
    );
    assert_ne!(
        found.unwrap().body, "[unable to decrypt]",
        "message from B was not decryptable by A (E2EE key exchange failed)"
    );
    println!("Agent A received and decrypted: {msg_b_to_a}");

    // Phase 3: Agent A replies (bidirectional test)
    let msg_a_to_b = format!("e2e-test-a-to-b-{tag}");
    let event_id = agent_a
        .send_dm(agent_b.user_id(), &msg_a_to_b)
        .await
        .expect("Agent A failed to send reply");
    println!("Agent A sent: {msg_a_to_b} (event: {event_id})");

    // Agent B syncs to receive
    sync_n(&agent_b, 2).await;

    let room_id_b = agent_b
        .dm_room_id(agent_a.user_id())
        .await
        .expect("failed to get DM room")
        .expect("no DM room found between agents (reverse direction)");

    let messages = agent_b
        .messages(&room_id_b, 10)
        .await
        .expect("Agent B failed to read messages");

    let found = messages.iter().find(|m| m.body == msg_a_to_b);
    assert!(
        found.is_some(),
        "Agent B did not find reply from Agent A: {msg_a_to_b}\nMessages: {:?}",
        messages.iter().map(|m| &m.body).collect::<Vec<_>>()
    );
    assert_ne!(
        found.unwrap().body, "[unable to decrypt]",
        "reply from A was not decryptable by B (E2EE key exchange failed)"
    );
    println!("Agent B received and decrypted: {msg_a_to_b}");

    println!("\nE2EE bidirectional test PASSED");
    println!("  Agent A: {} ({})", agent_a.user_id(), agent_a.did());
    println!("  Agent B: {} ({})", agent_b.user_id(), agent_b.did());
    println!("  Messages verified decryptable in both directions");
}

/// Decode a base64url (no padding) string — the encoding used by JWT segments.
/// Returns `None` on any invalid byte so a malformed claim can't panic the test.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in bytes {
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Verify LIVE that an aqua agent can complete the MatrixRTC join handshake
/// (HW2/HW3): obtain a Matrix OpenID token, present it to the Element Call
/// `lk-jwt-service` behind `matrix.inblock.io`, and receive a scoped LiveKit
/// access token in return. This proves the openid_token shape the connector
/// emits is exactly what lk-jwt-service expects.
#[tokio::test]
async fn rtc_jwt_handshake() {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .try_init()
        .ok();

    clean_store("/tmp/aqua-e2e-rtc");

    // 1. Connect agent A.
    let agent = AgentClient::connect(agent_config("agent.pem", "/tmp/aqua-e2e-rtc"))
        .await
        .expect("Agent A failed to connect");
    println!("Agent connected: {} ({})", agent.user_id(), agent.did());

    // 2. device_id + openid token.
    let device_id = agent.device_id().expect("agent has no device_id");
    println!("device_id: {device_id}");

    let openid_token = agent
        .request_openid_token()
        .await
        .expect("request_openid_token failed");
    // Show the shape (but NOT the secret access_token) so the body is auditable.
    {
        let redacted = {
            let mut t = openid_token.clone();
            if let Some(at) = t.get_mut("access_token") {
                let len = at.as_str().map(|s| s.len()).unwrap_or(0);
                *at = serde_json::json!(format!("<{len} chars redacted>"));
            }
            t
        };
        println!("openid_token (access_token redacted): {redacted}");
    }
    // Sanity: the four fields lk-jwt-service reads must be present.
    for field in ["access_token", "token_type", "matrix_server_name", "expires_in"] {
        assert!(
            openid_token.get(field).is_some(),
            "openid_token missing field `{field}`: {openid_token}"
        );
    }

    // 3. Synthetic-but-plausible LiveKit room name. lk-jwt-service mints a token
    //    for the requested room and verifies the USER via the openid token, not
    //    room membership — so any room string works for the handshake proof.
    let room = format!("rtc-probe-{}", uuid::Uuid::new_v4());
    println!("requesting LiveKit token for room: {room}");

    // 4. Build the lk-jwt request body and POST to the focus's /sfu/get.
    let body = serde_json::json!({
        "room": room,
        "openid_token": openid_token,
        "device_id": device_id,
    });
    let endpoint = "https://matrix.inblock.io/livekit/jwt/sfu/get";
    let http = reqwest::Client::new();
    let resp = http
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .expect("POST to lk-jwt-service failed (transport)");

    let status = resp.status();
    let resp_text = resp.text().await.expect("read lk-jwt response body");
    println!("lk-jwt-service HTTP {status}");

    // 5. Assert success + inspect the minted token.
    assert!(
        status.is_success(),
        "lk-jwt-service did not return 2xx — HTTP {status}, body: {resp_text}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&resp_text).expect("lk-jwt response was not JSON");

    let jwt = json
        .get("jwt")
        .or_else(|| json.get("token"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("no non-empty `jwt`/`token` field in response: {json}"));
    let url = json
        .get("url")
        .or_else(|| json.get("sfu_url"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("no `url` field in response: {json}"));

    let preview: String = jwt.chars().take(12).collect();
    println!("LiveKit SFU url: {url}");
    println!("JWT length: {} chars, prefix: {preview}…", jwt.len());

    // Decode the JWT payload (segment 2 of header.payload.signature) and surface
    // the LiveKit claims so we can confirm it's scoped to our room + identity.
    let segments: Vec<&str> = jwt.split('.').collect();
    assert!(
        segments.len() >= 2,
        "JWT is not in header.payload.signature form: {} segments",
        segments.len()
    );
    if let Some(hdr) = b64url_decode(segments[0]).and_then(|b| String::from_utf8(b).ok()) {
        println!("JWT header: {hdr}");
    }
    let payload = b64url_decode(segments[1])
        .and_then(|b| String::from_utf8(b).ok())
        .expect("JWT payload was not valid base64url/UTF-8");
    let claims: serde_json::Value =
        serde_json::from_str(&payload).expect("JWT payload was not JSON");

    // Pretty-print full claims (a LiveKit access token has no user secret in it —
    // it's the room grant), then call out the room + identity specifically.
    println!(
        "JWT claims:\n{}",
        serde_json::to_string_pretty(&claims).unwrap_or_else(|_| payload.clone())
    );
    let identity = claims
        .get("sub")
        .or_else(|| claims.get("identity"))
        .and_then(|v| v.as_str());
    let claim_room = claims.pointer("/video/room").and_then(|v| v.as_str());
    let room_join = claims
        .pointer("/video/roomJoin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!("LiveKit identity (sub): {identity:?}");
    println!("LiveKit room claim:     {claim_room:?}");
    println!("LiveKit roomJoin:       {room_join}");

    // The identity must encode THIS session: lk-jwt-service builds it as
    // `<user_id>:<device_id>`, so confirm both halves are present — that's what
    // ties the LiveKit participant back to our Matrix device.
    if let Some(id) = identity {
        assert!(
            id.contains(agent.user_id()),
            "JWT identity {id:?} does not contain our user_id {}",
            agent.user_id()
        );
        assert!(
            id.contains(&device_id),
            "JWT identity {id:?} does not contain our device_id {device_id}"
        );
    }

    // The token MUST be room-scoped. lk-jwt-service does NOT echo the room name
    // verbatim — it grants on `unpaddedBase64(sha256(json([room, slotId])))`, so
    // the claim is an opaque, non-empty, room-derived alias rather than our raw
    // string. Assert presence + join grant, not literal equality.
    let claim_room = claim_room.expect("minted JWT has no /video/room claim — not room-scoped");
    assert!(!claim_room.is_empty(), "JWT /video/room claim is empty");
    assert!(room_join, "JWT does not grant roomJoin");

    println!("\nMatrixRTC JWT handshake PASSED");
    println!("  HTTP status : {status}");
    println!("  SFU url     : {url}");
    println!("  JWT length  : {} chars", jwt.len());
    println!("  Identity    : {identity:?}");
    println!("  Room alias  : {claim_room} (room-scoped grant for requested {room})");
}

/// A minimal but structurally-valid 2x2 PNG (signature + IHDR + IDAT + IEND).
/// Its IHDR declares 2x2 so `imagesize::blob_size` reads dimensions from the
/// header, and the pixel data is a real (if tiny) zlib stream so it's a genuine
/// image, not just bytes named `.png`.
fn tiny_png() -> Vec<u8> {
    // Generated once with a real PNG encoder (2x2 RGBA). Bytes are stable.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR length + type
        0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, // width=2 height=2
        0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xB6, 0x0D, 0x24, // bit depth/color/etc + CRC
        0x00, 0x00, 0x00, 0x16, 0x49, 0x44, 0x41, 0x54, // IDAT length + type
        0x78, 0x9C, 0x62, 0xF8, 0xCF, 0xC0, 0xF0, 0x9F, // zlib data
        0x81, 0x81, 0x01, 0x00, 0x00, 0x00, 0xFF, 0xFF,
        0x03, 0x00, 0x0E, 0xFD, 0x03, 0xFD, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
    ];
    PNG.to_vec()
}

/// Round-trip image, file and voice attachments through a live encrypted DM and
/// prove each downloads + decrypts with bytes intact between two real instances.
#[tokio::test]
async fn e2ee_media_exchange() {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .try_init()
        .ok();

    clean_store("/tmp/aqua-e2e-media-a");
    clean_store("/tmp/aqua-e2e-media-b");

    let agent_a = AgentClient::connect(agent_config("agent.pem", "/tmp/aqua-e2e-media-a"))
        .await
        .expect("Agent A failed to connect");
    println!(
        "Agent A connected: {} ({}) token ttl≈{}s",
        agent_a.user_id(),
        agent_a.did(),
        agent_a.expires_at_unix().saturating_sub(now_unix())
    );

    let agent_b = AgentClient::connect(agent_config("agent-b.pem", "/tmp/aqua-e2e-media-b"))
        .await
        .expect("Agent B failed to connect");
    println!("Agent B connected: {} ({})", agent_b.user_id(), agent_b.did());

    assert_ne!(
        agent_a.user_id(),
        agent_b.user_id(),
        "agents must have different identities"
    );

    // Phase 1: A creates the DM room and invites B; B joins. Same economy as the
    // text test (sync_n=2) — the siwx-oidc access token lives only ~300s, so we
    // keep the sync budget tight to finish all three channels inside one window.
    agent_a
        .send_dm(agent_b.user_id(), "e2e-media-room-setup")
        .await
        .expect("Agent A failed to create DM room");
    println!("DM room created by Agent A");

    sync_n(&agent_b, 2).await;
    agent_b.join_invited_rooms().await.expect("Agent B join failed");
    sync_n(&agent_b, 2).await;
    println!("Agent B joined the room");

    // A learns B's device keys.
    sync_n(&agent_a, 2).await;

    let tag = uuid::Uuid::new_v4().to_string();

    let room_a = agent_a
        .dm_room_id(agent_b.user_id())
        .await
        .expect("failed to get DM room (A)")
        .expect("no DM room found (A)");
    let room_b = agent_b
        .dm_room_id(agent_a.user_id())
        .await
        .expect("failed to get DM room (B)")
        .expect("no DM room found (B)");
    println!("DM room A={room_a} B={room_b}");

    // ---- Channel 1: B → A IMAGE -------------------------------------------
    // B sends first: B created its outbound Megolm session AFTER joining, so its
    // session key is shared with A from the start (the key-exchange ordering the
    // text test relies on). A is the receiver here.
    let png_bytes = tiny_png();
    let png_path = std::env::temp_dir().join(format!("aqua-e2e-{tag}.png"));
    std::fs::write(&png_path, &png_bytes).expect("write png");
    let img_event = agent_b
        .send_image(agent_a.user_id(), &png_path, Some(&format!("img-{tag}")))
        .await
        .expect("Agent B failed to send image");
    println!("Agent B sent IMAGE ({} bytes, event {img_event})", png_bytes.len());

    let got_png = recv_media(&agent_a, &room_a, MediaKind::Image, "IMAGE").await;
    assert_eq!(
        got_png, png_bytes,
        "IMAGE round-trip mismatch: A's decrypted bytes != B's sent bytes"
    );
    println!("Agent A downloaded + decrypted IMAGE; {} bytes match exactly", got_png.len());

    // ---- Channel 2: A → B FILE --------------------------------------------
    // Now A sends. A's outbound session is shared with B as part of receiving B's
    // image above (key-share to-device), so A→B decrypts too.
    let file_bytes = format!("hello from agent A — file payload {tag}\n").into_bytes();
    let file_path = std::env::temp_dir().join(format!("aqua-e2e-{tag}.txt"));
    std::fs::write(&file_path, &file_bytes).expect("write file");
    let file_event = agent_a
        .send_file(agent_b.user_id(), &file_path, Some(&format!("file-{tag}")))
        .await
        .expect("Agent A failed to send file");
    println!("Agent A sent FILE ({} bytes, event {file_event})", file_bytes.len());

    let got_file = recv_media(&agent_b, &room_b, MediaKind::File, "FILE").await;
    assert_eq!(
        got_file, file_bytes,
        "FILE round-trip mismatch: B's decrypted bytes != A's sent bytes"
    );
    println!("Agent B downloaded + decrypted FILE; {} bytes match exactly", got_file.len());

    // ---- Channel 3: B → A VOICE -------------------------------------------
    // A tiny "voice" payload — the connector does not decode audio, so any bytes
    // work; we assert the kind is Voice (MSC3245 marker) and the bytes survive.
    let voice_bytes: Vec<u8> = b"OggS\x00\x02fake-opus-voice-clip-for-e2e".to_vec();
    let voice_path = std::env::temp_dir().join(format!("aqua-e2e-{tag}.ogg"));
    std::fs::write(&voice_path, &voice_bytes).expect("write voice");
    let voice_event = agent_b
        .send_voice_message(agent_a.user_id(), &voice_path, 1500, None)
        .await
        .expect("Agent B failed to send voice message");
    println!(
        "Agent B sent VOICE ({} bytes, 1500ms, event {voice_event})",
        voice_bytes.len()
    );

    let got_voice = recv_media(&agent_a, &room_a, MediaKind::Voice, "VOICE").await;
    assert!(!got_voice.is_empty(), "VOICE download returned empty bytes");
    assert_eq!(
        got_voice, voice_bytes,
        "VOICE round-trip mismatch: A's decrypted bytes != B's sent bytes"
    );
    println!("Agent A downloaded + decrypted VOICE; {} bytes (kind=voice)", got_voice.len());

    println!("\nE2EE media exchange test PASSED");
    println!("  IMAGE B→A: round-trip + decrypt OK ({} bytes)", got_png.len());
    println!("  FILE  A→B: round-trip + decrypt OK ({} bytes)", got_file.len());
    println!("  VOICE B→A: round-trip + decrypt OK ({} bytes, kind=voice)", got_voice.len());
}

/// Sync `receiver` a few times, look for an inbound attachment of `want` via
/// `recent_media`, download and return its (decrypted) bytes. Retries the
/// sync/scan a handful of times because the media event + its Megolm key can
/// land on different sync rounds. Panics with a clear message if it never
/// appears (a real round-trip failure, not green-washed away).
async fn recv_media(
    receiver: &AgentClient,
    room_id: &str,
    want: MediaKind,
    label: &str,
) -> Vec<u8> {
    for attempt in 1..=8 {
        receiver.sync_once().await.expect("sync failed");
        let media = receiver
            .recent_media(room_id, 30)
            .await
            .expect("recent_media failed");
        let kinds: Vec<&str> = media.iter().map(|(k, _)| k.as_str()).collect();
        println!("  [{label}] attempt {attempt}: receiver sees media kinds {kinds:?}");
        if let Some((_, handle)) = media.iter().find(|(k, _)| *k == want) {
            // Found the event; downloading also decrypts. If the Megolm key
            // hasn't arrived yet the download errors — sync once more and retry.
            match receiver.download_media(handle).await {
                Ok(bytes) => return bytes,
                Err(e) => {
                    println!("  [{label}] attempt {attempt}: found event but download/decrypt not ready: {e:#}");
                }
            }
        }
    }
    panic!("{label}: receiver never received a decryptable {want:?} attachment after 8 sync rounds");
}

/// Verify LIVE that an aqua agent can advertise a **MatrixRTC membership**
/// (`org.matrix.msc3401.call.member`) so Element X / Element Call discovers it as
/// a call participant — the Matrix-signaling half of the LiveKit media join.
///
/// Proves four things against the real homeserver:
///  1. Agent A's `set_rtc_member` is ACCEPTED by the server (returns Ok / an
///     event id). Whether the MSC3757 owned (leading-underscore) state key is
///     allowed is reported by `set_rtc_member`'s own logging; this test asserts
///     the *effective* stored state regardless of which key form won.
///  2. A reads its own membership back and it deserializes to SessionContent with
///     the LiveKit focus + correct device_id.
///  3. B — a DIFFERENT user in the same room — reads the SAME state event,
///     proving other Matrix clients DISCOVER the agent's membership.
///  4. `clear_rtc_member` returns the membership to the Empty ("left") state.
///
/// The raw stored JSON is printed (from both A and B) so the wire shape can be
/// eyeballed against the Element Call schema.
#[tokio::test]
async fn rtc_member_advertise() {
    use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
    use matrix_sdk::ruma::events::call::member::{
        CallMemberEventContent, CallMemberStateKey,
    };
    use matrix_sdk::ruma::events::StateEventType;
    use matrix_sdk::ruma::{OwnedUserId, RoomId};

    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .try_init()
        .ok();

    clean_store("/tmp/aqua-e2e-rtcmem-a");
    clean_store("/tmp/aqua-e2e-rtcmem-b");

    let agent_a = AgentClient::connect(agent_config("agent.pem", "/tmp/aqua-e2e-rtcmem-a"))
        .await
        .expect("Agent A failed to connect");
    let agent_b = AgentClient::connect(agent_config("agent-b.pem", "/tmp/aqua-e2e-rtcmem-b"))
        .await
        .expect("Agent B failed to connect");
    println!("A = {} ({:?})", agent_a.user_id(), agent_a.device_id());
    println!("B = {} ({:?})", agent_b.user_id(), agent_b.device_id());

    // --- Establish the shared DM room (the existing e2e pattern) -----------
    agent_a
        .send_dm(agent_b.user_id(), "e2e-rtcmem-room-setup")
        .await
        .expect("A failed to create DM room");
    sync_n(&agent_b, 2).await;
    agent_b.join_invited_rooms().await.expect("B join failed");
    sync_n(&agent_b, 2).await;
    sync_n(&agent_a, 2).await;

    let room_id = agent_a
        .dm_room_id(agent_b.user_id())
        .await
        .expect("dm_room_id failed")
        .expect("no DM room between A and B");
    println!("shared room: {room_id}");

    // --- A advertises its MatrixRTC membership (alias = room_id) -----------
    let focus_url = "https://matrix.inblock.io/livekit/jwt";
    agent_a
        .set_rtc_member(&room_id, &room_id, focus_url)
        .await
        .expect("set_rtc_member was REJECTED by the homeserver");
    println!("A set_rtc_member ACCEPTED by homeserver");

    // Helper: read the call.member state for A's (user, device) from a given
    // reader's view, trying the owned (underscore) key then the plain key, and
    // returning (state_key_used, deserialized_content, raw_json).
    async fn read_member(
        reader: &AgentClient,
        room_id: &str,
        member_user: &str,
        member_device: &str,
    ) -> Option<(String, CallMemberEventContent, serde_json::Value)> {
        let rid: &RoomId = room_id.try_into().unwrap();
        let room = reader.client().get_room(rid)?;
        let user: OwnedUserId = member_user.try_into().unwrap();
        for underscore in [true, false] {
            let key = CallMemberStateKey::new(
                user.clone(),
                Some(member_device.to_owned()),
                underscore,
            );
            // Typed read: proves the stored event deserializes to the strongly
            // typed CallMemberEventContent via matrix-sdk's static path.
            let typed = room
                .get_state_event_static_for_key::<CallMemberEventContent, _>(&key)
                .await
                .ok()
                .flatten();
            // Raw read (for JSON eyeballing) by the same key string. The store
            // returns either a Sync or Stripped wrapper; both hold a `Raw` whose
            // `.json()` is the verbatim stored event. We pull the `content`
            // sub-object out so the printed JSON is exactly the membership shape.
            let raw = room
                .get_state_event(StateEventType::CallMember, key.as_ref())
                .await
                .ok()
                .flatten();
            let full_json = raw.as_ref().map(|r| {
                let rjv = match r {
                    RawAnySyncOrStrippedState::Sync(ev) => ev.json(),
                    RawAnySyncOrStrippedState::Stripped(ev) => ev.json(),
                };
                serde_json::from_str::<serde_json::Value>(rjv.get()).unwrap()
            });
            // Only treat the slot as present when BOTH the typed read succeeds
            // (so we return a real CallMemberEventContent) and we have raw JSON.
            if let (Some(full), Some(typed)) = (full_json, &typed) {
                // Deserialize the Raw wrapper into the event enum, then pull the
                // typed content out.
                if let Some(content) = typed
                    .deserialize()
                    .ok()
                    .and_then(|ev| ev.original_content().cloned())
                {
                    // Prefer the `content` sub-object for printing; fall back to
                    // the whole event if the shape is unexpected.
                    let content_json =
                        full.get("content").cloned().unwrap_or_else(|| full.clone());
                    return Some((key.as_ref().to_owned(), content, content_json));
                }
            }
        }
        None
    }

    let a_user = agent_a.user_id().to_owned();
    let a_device = agent_a.device_id().expect("A has no device_id");

    // --- (2) A reads its own membership back -------------------------------
    sync_n(&agent_a, 2).await;
    let (key_a, content_a, raw_a) = read_member(&agent_a, &room_id, &a_user, &a_device)
        .await
        .expect("A could not read back its own RTC membership");
    println!("\n=== A read-back: state_key = {key_a} ===");
    println!("{}", serde_json::to_string_pretty(&raw_a).unwrap());
    assert_session_focus(&content_a, &a_device, &room_id, focus_url, "A self read-back");

    // --- (3) B (different user) discovers A's membership -------------------
    sync_n(&agent_b, 3).await;
    let (key_b, content_b, raw_b) = read_member(&agent_b, &room_id, &a_user, &a_device)
        .await
        .expect("B could not DISCOVER A's RTC membership (cross-user read failed)");
    println!("\n=== B cross-user read of A's membership: state_key = {key_b} ===");
    println!("{}", serde_json::to_string_pretty(&raw_b).unwrap());
    assert_eq!(key_a, key_b, "A and B disagree on the membership state key");
    assert_session_focus(&content_b, &a_device, &room_id, focus_url, "B cross-user read");
    println!("\nCROSS-USER DISCOVERY PROVEN: B sees A's MatrixRTC membership.");

    // --- (4) A clears membership; read-back is Empty ----------------------
    agent_a
        .clear_rtc_member(&room_id)
        .await
        .expect("clear_rtc_member failed");
    sync_n(&agent_a, 2).await;
    let after = read_member(&agent_a, &room_id, &a_user, &a_device).await;
    match after {
        Some((_, CallMemberEventContent::Empty(_), raw)) => {
            println!("\n=== after clear: Empty membership (left call) ===");
            println!("{}", serde_json::to_string_pretty(&raw).unwrap());
        }
        Some((_, other, raw)) => panic!(
            "after clear_rtc_member the membership was NOT Empty: {other:?}\nraw: {raw}"
        ),
        None => panic!("after clear_rtc_member the state event vanished entirely (expected Empty)"),
    }
    println!("\nrtc_member_advertise: ALL CHECKS PASSED");
}

/// Assert a read-back membership is a SessionContent carrying the LiveKit focus,
/// the expected device_id, and the room-scoped call shape Element Call expects.
fn assert_session_focus(
    content: &matrix_sdk::ruma::events::call::member::CallMemberEventContent,
    expect_device: &str,
    expect_alias: &str,
    expect_focus_url: &str,
    label: &str,
) {
    use matrix_sdk::ruma::events::call::member::{
        ActiveFocus, CallMemberEventContent, Focus,
    };
    let session = match content {
        CallMemberEventContent::SessionContent(s) => s,
        other => panic!("{label}: expected SessionContent, got {other:?}"),
    };
    assert_eq!(
        session.device_id.as_str(),
        expect_device,
        "{label}: device_id mismatch"
    );
    assert!(
        matches!(session.focus_active, ActiveFocus::Livekit(_)),
        "{label}: focus_active is not livekit"
    );
    let lk = session
        .foci_preferred
        .iter()
        .find_map(|f| match f {
            Focus::Livekit(l) => Some(l),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{label}: no livekit focus in foci_preferred"));
    assert_eq!(lk.alias, expect_alias, "{label}: livekit_alias mismatch");
    assert_eq!(
        lk.service_url, expect_focus_url,
        "{label}: livekit_service_url mismatch"
    );
    // application=m.call, scope=m.room
    assert!(
        session.application.application_session_is_room_call(),
        "{label}: not a room-scoped m.call"
    );
}

/// Local extension so the assertion above stays readable.
trait RoomCallCheck {
    fn application_session_is_room_call(&self) -> bool;
}
impl RoomCallCheck for matrix_sdk::ruma::events::call::member::Application {
    fn application_session_is_room_call(&self) -> bool {
        use matrix_sdk::ruma::events::call::member::{Application, CallScope};
        match self {
            Application::Call(c) => c.scope == CallScope::Room && c.call_id.is_empty(),
            _ => false,
        }
    }
}
