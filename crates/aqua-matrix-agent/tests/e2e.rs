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
