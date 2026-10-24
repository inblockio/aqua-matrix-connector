//! Obligation delivery against the fake homeserver: T6 (normal traffic), the
//! lost response, a token rotation, the proof rules and the watchdog's posts.

use std::sync::Arc;

use super::fake_hs::{agent, SendMode, ROOM};
use crate::{
    txn_id, Dest, ManualClock, ObligationPolicy, Phase, PostKind, SweepAction, WorkItem,
    WorkJournal, WorkState,
};

const E: &str = "$peer-message";

fn peer_item() -> WorkItem {
    WorkItem {
        event_id: E.into(),
        room_id: ROOM.into(),
        ts_ms: 1,
        msgtype: "m.text".into(),
        body: "hello agent".into(),
        state: WorkState::Pending,
        sender: None,
        obligation: None,
    }
}

fn journal(store: &std::path::Path, clock: &Arc<ManualClock>) -> Arc<WorkJournal> {
    let j = Arc::new(WorkJournal::load_with(
        store,
        clock.clone(),
        ObligationPolicy::default(),
    ));
    j.enqueue(peer_item());
    j
}

/// T6: normal traffic. One accepted message, one answer: exactly one post, no
/// liveness noise, the obligation closed (open count 0).
#[tokio::test]
async fn t6_normal_traffic_one_post_obligation_closed() {
    let (mut agent, hs, store) = agent("t6").await;
    let clock = Arc::new(ManualClock::new(10_000));
    let j = journal(&store, &clock);
    agent.attach_journal(j.clone());
    assert!(j.begin_turn(E));
    assert_eq!(j.phase(E), Some(Phase::Answering));

    agent
        .deliver_obligation(E, Dest::Room(ROOM), PostKind::Answer, "the answer")
        .await
        .unwrap();

    let s = hs.lock().unwrap();
    assert_eq!(s.bodies(), vec!["the answer".to_string()]);
    assert_eq!(s.put_calls, 1, "no extra post");
    assert_eq!(s.timeline[0].txn, txn_id(E, 0), "the deterministic id was used");
    drop(s);
    assert_eq!(j.open_stats().0, 0, "every obligation closed");
    clock.advance(20 * 60_000);
    assert!(j.sweep().is_empty(), "nothing left for the watchdog");
    let _ = std::fs::remove_dir_all(store);
}

/// The duplicate case through the obligation path: the response is lost, the
/// retry is the same event; with no fetch available the echo (carrying the
/// transaction id) is what closes the obligation.
#[tokio::test]
async fn lost_response_is_one_event_and_the_echo_closes_it() {
    let (mut agent, hs, store) = agent("lost-ob").await;
    {
        let mut s = hs.lock().unwrap();
        s.script.push_back(SendMode::PersistDropResponse);
        s.event_fetch_ok = false;
    }
    let clock = Arc::new(ManualClock::new(10_000));
    let j = journal(&store, &clock);
    agent.attach_journal(j.clone());

    let posted = agent
        .deliver_obligation(E, Dest::Room(ROOM), PostKind::Answer, "the answer")
        .await
        .unwrap();
    assert_eq!(hs.lock().unwrap().bodies(), vec!["the answer".to_string()]);
    assert_eq!(
        j.phase(E),
        Some(Phase::Answered { event_id: posted.clone() }),
        "sent but not proven: still open"
    );

    // The agent's own sync timeline delivers the echo, with its txn id.
    let txn = hs.lock().unwrap().timeline[0].txn.clone();
    assert_eq!(agent.observe_own_event(&posted, Some(&txn)), vec![E.to_string()]);
    assert_eq!(j.open_stats().0, 0);
    let _ = std::fs::remove_dir_all(store);
}

/// A token rotation (a new client over the same journal) after a send that
/// landed but could not be proven: nothing is posted again, the event fetch
/// closes the obligation.
#[tokio::test]
async fn rotation_after_unproven_post_never_reposts() {
    let (mut a1, hs, store) = agent("rot").await;
    hs.lock().unwrap().event_fetch_ok = false;
    let clock = Arc::new(ManualClock::new(10_000));
    let j = journal(&store, &clock);
    a1.attach_journal(j.clone());
    a1.deliver_obligation(E, Dest::Room(ROOM), PostKind::Answer, "the answer")
        .await
        .unwrap();
    assert_eq!(j.open_stats().0, 1, "unproven: open");

    // New cycle, new client (fresh token), same journal; the fetch works now.
    hs.lock().unwrap().event_fetch_ok = true;
    let mut a2 = a1.clone();
    a2.attach_journal(j.clone());
    a2.deliver_obligation(E, Dest::Room(ROOM), PostKind::Answer, "the answer")
        .await
        .unwrap();
    let s = hs.lock().unwrap();
    assert_eq!(s.put_calls, 1, "the journal's record is the first duplicate check");
    assert_eq!(s.bodies().len(), 1);
    drop(s);
    assert_eq!(j.open_stats().0, 0);
    let _ = std::fs::remove_dir_all(store);
}

/// The same obligation delivered from a fresh journal object (restart) after
/// the post landed: the homeserver dedupes on the deterministic id.
#[tokio::test]
async fn restart_without_a_record_is_still_the_same_event() {
    let (mut agent, hs, store) = agent("restart-txn").await;
    let clock = Arc::new(ManualClock::new(10_000));
    let j = journal(&store, &clock);
    agent.attach_journal(j.clone());
    agent
        .deliver_obligation(E, Dest::Room(ROOM), PostKind::Answer, "the answer")
        .await
        .unwrap();
    // The journal write was lost: a journal that never saw the post.
    let j2 = journal(&std::env::temp_dir().join(format!("gd-empty-{}", std::process::id())), &clock);
    agent.attach_journal(j2);
    agent
        .deliver_obligation(E, Dest::Room(ROOM), PostKind::Answer, "the answer")
        .await
        .unwrap();
    assert_eq!(hs.lock().unwrap().bodies().len(), 1, "txn id dedupes");
    let _ = std::fs::remove_dir_all(store);
}

/// The watchdog's posts: a turn running past the answer deadline gets one
/// liveness post (it closes nothing); past the fallback deadline the fallback is
/// posted and closes the obligation.
#[tokio::test]
async fn watchdog_posts_liveness_then_fallback_and_closes() {
    let (mut agent, hs, store) = agent("wd").await;
    let clock = Arc::new(ManualClock::new(0));
    let j = journal(&store, &clock);
    agent.attach_journal(j.clone());
    assert!(j.begin_turn(E));

    clock.set(5 * 60_000);
    let left = agent.execute_sweep(j.sweep(), None).await;
    assert!(left.is_empty());
    assert_eq!(hs.lock().unwrap().bodies(), vec![j.policy().liveness_text.clone()]);
    assert_eq!(j.open_stats().0, 1, "liveness does not close");

    clock.set(6 * 60_000);
    assert!(agent.execute_sweep(j.sweep(), None).await.is_empty());
    assert_eq!(hs.lock().unwrap().bodies().len(), 1, "liveness only once");

    clock.set(10 * 60_000);
    let actions = j.sweep();
    assert!(matches!(actions[0], SweepAction::Fallback { .. }), "{actions:?}");
    agent.execute_sweep(actions, None).await;
    let bodies = hs.lock().unwrap().bodies();
    assert_eq!(bodies, vec![j.policy().liveness_text.clone(), j.policy().fallback_text.clone()]);
    assert_eq!(j.open_stats().0, 0, "the proven fallback closed it");
    let _ = std::fs::remove_dir_all(store);
}
