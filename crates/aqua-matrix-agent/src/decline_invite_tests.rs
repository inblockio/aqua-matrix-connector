//! `decline_invite` leaves the one invited room and nothing else.
//!
//! matrix-sdk's `Room::leave()` follows the room's `predecessor` links and leaves every
//! predecessor the client is joined to. The predecessor of an invite comes from the invite's
//! stripped `m.room.create`, which the INVITER writes: a stranger's room naming one of our joined
//! rooms (a DM with the owner) as its predecessor made `decline_invite` leave that room too, and a
//! room naming itself as predecessor looped the walk for ever. A tiny local homeserver stands in
//! for the real one: it serves one sync with a joined room and some invites, and records every
//! room request it saw.

use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MINE: &str = "!mine:localhost";
const ME: &str = "@declines:localhost";

type Log = Arc<Mutex<Vec<(String, String)>>>;

fn create_content(sender: &str, predecessor: Option<&str>) -> serde_json::Value {
    let mut content = serde_json::json!({"creator": sender, "room_version": "10"});
    if let Some(p) = predecessor {
        content["predecessor"] = serde_json::json!({"room_id": p, "event_id": "$forged"});
    }
    content
}

fn invite(sender: &str, predecessor: Option<&str>) -> serde_json::Value {
    serde_json::json!({"invite_state": {"events": [
        {"type": "m.room.create", "state_key": "", "sender": sender, "content": create_content(sender, predecessor)},
        {"type": "m.room.member", "state_key": ME, "sender": sender, "content": {"membership": "invite"}},
    ]}})
}

/// The sync the fake homeserver serves: our joined room, the given invites, the given left rooms.
fn sync_body(invites: &[(&str, Option<&str>)], left: &[&str]) -> String {
    let invites: serde_json::Map<String, serde_json::Value> = invites
        .iter()
        .map(|(room, pre)| ((*room).to_string(), invite("@stranger:localhost", *pre)))
        .collect();
    let left: serde_json::Map<String, serde_json::Value> = left
        .iter()
        .map(|r| ((*r).to_string(), serde_json::json!({})))
        .collect();
    serde_json::json!({
        "next_batch": "s_1",
        "rooms": {
            "join": {MINE: {
                "state": {"events": [
                    {"type": "m.room.create", "state_key": "", "sender": "@owner:localhost", "event_id": "$c", "origin_server_ts": 1,
                     "content": create_content("@owner:localhost", None)},
                    {"type": "m.room.member", "state_key": ME, "sender": ME, "event_id": "$m", "origin_server_ts": 2,
                     "content": {"membership": "join"}},
                ]},
                "timeline": {"events": [], "prev_batch": "p0"},
            }},
            "invite": invites,
            "leave": left,
        },
    })
    .to_string()
}

async fn fake_homeserver(sync: String) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::default();
    let seen = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (seen, sync) = (seen.clone(), sync.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let body_len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + body_len {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    buf.drain(..head_end + body_len);

                    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
                    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let path = target.split('?').next().unwrap_or("");
                    seen.lock()
                        .unwrap()
                        .push((method.to_string(), path.to_string()));
                    let body = if path.starts_with("/_matrix/client/versions") {
                        r#"{"versions":["v1.1","v1.11"],"unstable_features":{}}"#.to_string()
                    } else if path.ends_with("/sync") {
                        sync.clone()
                    } else if path.ends_with("/keys/upload") {
                        r#"{"one_time_key_counts":{}}"#.to_string()
                    } else if path.ends_with("/keys/query") {
                        r#"{"device_keys":{},"failures":{}}"#.to_string()
                    } else {
                        "{}".to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, log)
}

fn temp_store(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aqua-decline-invite-{tag}-{}-{}",
        std::process::id(),
        unix_now()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A client of the account `ME` against the fake homeserver, synced once.
async fn client_for(tag: &str, sync: String) -> (Client, Log) {
    let (url, log) = fake_homeserver(sync).await;
    let user: OwnedUserId = ME.try_into().unwrap();
    let device: OwnedDeviceId = "DECLINEDEV".into();
    let client = build_and_restore(&url, &temp_store(tag), &user, &device, "fake-token")
        .await
        .expect("client against the fake homeserver");
    client
        .sync_once(catch_up_sync_settings())
        .await
        .expect("sync against the fake homeserver");
    (client, log)
}

/// Room ids of every `POST .../<suffix>` the server saw, percent-decoded.
fn requests_ending(log: &Log, suffix: &str) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(method, path)| method == "POST" && path.ends_with(suffix))
        .map(|(_, path)| {
            let room = path
                .trim_end_matches(suffix)
                .rsplit('/')
                .next()
                .unwrap_or("");
            room.replace("%21", "!")
                .replace("%3A", ":")
                .replace("%3a", ":")
        })
        .collect()
}

fn leaves(log: &Log) -> Vec<String> {
    requests_ending(log, "/leave")
}

fn forgets(log: &Log) -> Vec<String> {
    requests_ending(log, "/forget")
}

#[tokio::test]
async fn declining_leaves_only_the_invited_room_not_the_joined_predecessor_it_names() {
    let (client, log) =
        client_for("pred", sync_body(&[("!trap:localhost", Some(MINE))], &[])).await;
    decline_invite_on(&client, "!trap:localhost").await.unwrap();
    assert_eq!(
        leaves(&log),
        ["!trap:localhost"],
        "one leave request, for the declined room only"
    );
    let mine = client.get_room(<&RoomId>::try_from(MINE).unwrap()).unwrap();
    assert_eq!(
        mine.state(),
        RoomState::Joined,
        "our joined room is untouched"
    );
}

#[tokio::test]
async fn a_joined_room_is_not_an_invite_and_is_never_left_by_declining() {
    let (client, log) = client_for("joined", sync_body(&[], &[])).await;
    let err = decline_invite_on(&client, MINE)
        .await
        .expect_err("a joined room is not a pending invite");
    assert!(err.to_string().contains("not a pending invite"), "{err:#}");
    assert!(leaves(&log).is_empty());
}

#[tokio::test]
async fn an_unknown_or_malformed_room_id_sends_nothing() {
    let (client, log) = client_for("unknown", sync_body(&[], &[])).await;
    assert!(decline_invite_on(&client, "!never-heard-of:localhost")
        .await
        .is_err());
    assert!(decline_invite_on(&client, "not a room id").await.is_err());
    assert!(leaves(&log).is_empty());
}

#[tokio::test]
async fn a_room_already_left_is_a_no_op() {
    let (client, log) = client_for("left", sync_body(&[], &["!old:localhost"])).await;
    decline_invite_on(&client, "!old:localhost").await.unwrap();
    assert!(leaves(&log).is_empty(), "{:?}", leaves(&log));
}

#[tokio::test]
async fn declining_an_invite_also_forgets_that_room_and_only_that_room() {
    // Room::leave() forgot a declined invite; the single request keeps that (best effort).
    let (client, log) =
        client_for("forget", sync_body(&[("!trap:localhost", Some(MINE))], &[])).await;
    decline_invite_on(&client, "!trap:localhost").await.unwrap();
    assert_eq!(forgets(&log), ["!trap:localhost"]);
}

#[tokio::test]
async fn a_room_that_names_itself_as_its_predecessor_is_declined_once_and_ends() {
    // Room::leave() has no cycle guard: this looped for ever (and grew without bound).
    let (client, log) = client_for(
        "self",
        sync_body(&[("!loop:localhost", Some("!loop:localhost"))], &[]),
    )
    .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        decline_invite_on(&client, "!loop:localhost"),
    )
    .await
    .expect("the decline ends")
    .unwrap();
    assert_eq!(leaves(&log), ["!loop:localhost"]);
}

#[tokio::test]
async fn two_invites_naming_each_other_are_declined_one_at_a_time() {
    let (client, log) = client_for(
        "cycle",
        sync_body(
            &[
                ("!a:localhost", Some("!b:localhost")),
                ("!b:localhost", Some("!a:localhost")),
            ],
            &[],
        ),
    )
    .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        decline_invite_on(&client, "!a:localhost"),
    )
    .await
    .expect("the decline ends")
    .unwrap();
    assert_eq!(leaves(&log), ["!a:localhost"], "declining a leaves b alone");
}
