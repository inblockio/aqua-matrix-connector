#![cfg(feature = "e2e")]

use aqua_matrix_agent::{AgentClient, AgentConfig};
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

#[tokio::test]
async fn e2ee_bidirectional_messaging() {
    tracing_subscriber::fmt()
        .with_env_filter("warn,aqua_matrix_agent=info")
        .try_init()
        .ok();

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

    // Agent A sends a unique message to Agent B
    let tag = uuid::Uuid::new_v4().to_string();
    let msg_a_to_b = format!("e2e-test-a-to-b-{tag}");
    let event_id = agent_a
        .send_dm(agent_b.user_id(), &msg_a_to_b)
        .await
        .expect("Agent A failed to send");
    println!("Agent A sent: {msg_a_to_b} (event: {event_id})");

    // Agent B syncs and reads the message
    agent_b.sync_once().await.expect("Agent B sync failed");
    agent_b.join_invited_rooms().await.expect("Agent B join failed");
    agent_b.sync_once().await.expect("Agent B post-join sync failed");

    let room_id = agent_b
        .dm_room_id(agent_a.user_id())
        .await
        .expect("failed to get DM room")
        .expect("no DM room found between agents");

    let messages = agent_b
        .messages(&room_id, 10)
        .await
        .expect("Agent B failed to read messages");

    let found = messages.iter().find(|m| m.body == msg_a_to_b);
    assert!(found.is_some(), "Agent B did not find message from Agent A: {msg_a_to_b}");
    assert_ne!(
        found.unwrap().body, "[unable to decrypt]",
        "message from A was not decryptable by B (E2EE key exchange failed)"
    );
    println!("Agent B received and decrypted: {msg_a_to_b}");

    // Agent B replies to Agent A (bidirectional test)
    let msg_b_to_a = format!("e2e-test-b-to-a-{tag}");
    let event_id = agent_b
        .send_dm(agent_a.user_id(), &msg_b_to_a)
        .await
        .expect("Agent B failed to send reply");
    println!("Agent B sent: {msg_b_to_a} (event: {event_id})");

    // Agent A syncs and reads the reply
    agent_a.sync_once().await.expect("Agent A sync failed");
    agent_a.join_invited_rooms().await.expect("Agent A join failed");
    agent_a.sync_once().await.expect("Agent A post-join sync failed");

    let room_id = agent_a
        .dm_room_id(agent_b.user_id())
        .await
        .expect("failed to get DM room")
        .expect("no DM room found between agents (reverse direction)");

    let messages = agent_a
        .messages(&room_id, 10)
        .await
        .expect("Agent A failed to read messages");

    let found = messages.iter().find(|m| m.body == msg_b_to_a);
    assert!(found.is_some(), "Agent A did not find reply from Agent B: {msg_b_to_a}");
    assert_ne!(
        found.unwrap().body, "[unable to decrypt]",
        "reply from B was not decryptable by A (E2EE key exchange failed)"
    );
    println!("Agent A received and decrypted: {msg_b_to_a}");

    // Verify no UTD messages in the recent exchange
    let all_msgs = agent_a.messages(&room_id, 10).await.unwrap();
    let utd_count = all_msgs.iter().filter(|m| m.body == "[unable to decrypt]").count();
    println!("UTD messages in recent history: {utd_count}");
    assert_eq!(utd_count, 0, "found {utd_count} undecryptable messages in the exchange");

    println!("\nE2EE bidirectional test PASSED");
    println!("  Agent A: {} ({})", agent_a.user_id(), agent_a.did());
    println!("  Agent B: {} ({})", agent_b.user_id(), agent_b.did());
    println!("  Messages verified decryptable in both directions");
}
