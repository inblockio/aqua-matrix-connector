//! The send-side faults of the guaranteed-delivery trial (T2 and the lost
//! response), through the PRE-EXISTING public send API only, so the same file
//! runs against the base commit (red) and against the fix (green).

use super::fake_hs::{agent, SendMode, ROOM};

/// T2: the homeserver answers a send with 502, then accepts: the peer sees
/// exactly one message.
#[tokio::test]
async fn t2_502_then_success_is_exactly_one_event() {
    let (agent, hs, store) = agent("t2").await;
    hs.lock().unwrap().script.push_back(SendMode::Fail502);
    agent
        .send_to_room_chunked(ROOM, "the answer")
        .await
        .expect("a transient 502 must not fail a reliable send");
    let s = hs.lock().unwrap();
    assert_eq!(s.bodies(), vec!["the answer".to_string()], "puts: {}", s.put_calls);
    let _ = std::fs::remove_dir_all(store);
}

/// The duplicate case: the homeserver persists the event and the response is
/// lost. The retry must not post it again.
#[tokio::test]
async fn lost_send_response_is_still_exactly_one_event() {
    let (agent, hs, store) = agent("lost").await;
    hs.lock().unwrap().script.push_back(SendMode::PersistDropResponse);
    agent
        .send_to_room_chunked(ROOM, "the answer")
        .await
        .expect("reliable send retries after a lost response");
    let s = hs.lock().unwrap();
    assert_eq!(
        s.bodies(),
        vec!["the answer".to_string()],
        "a retried send must be the same event (puts: {})",
        s.put_calls
    );
    let _ = std::fs::remove_dir_all(store);
}
