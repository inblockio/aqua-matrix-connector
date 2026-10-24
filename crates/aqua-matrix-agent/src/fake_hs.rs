//! Test-only fake homeserver for the delivery tests: one joined, unencrypted
//! room; `PUT /send` with scripted faults; the sent events come back in `/sync`
//! (the own-event echo, with `unsigned.transaction_id`) and in `GET /event`.
//! Like Synapse, a repeated transaction id answers with the event it already
//! has instead of persisting a second one.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const ROOM: &str = "!room:localhost";
pub const USER: &str = "@agent:localhost";

/// What the next `PUT /send` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendMode {
    /// Persist (or dedupe by transaction id) and answer.
    Ok,
    /// Answer 502 and persist nothing.
    Fail502,
    /// Persist the event, then drop the connection without answering: the
    /// client cannot tell whether the event landed (the duplicate case).
    PersistDropResponse,
}

#[derive(Clone, Debug)]
pub struct Stored {
    pub event_id: String,
    pub txn: String,
    pub body: String,
    pub ts: u64,
}

#[derive(Default)]
pub struct Hs {
    /// Modes consumed one per `PUT /send` (then `Ok` forever).
    pub script: VecDeque<SendMode>,
    pub timeline: Vec<Stored>,
    pub put_calls: usize,
    pub event_gets: usize,
    /// When false `GET /event` answers 404 (an event fetch that cannot prove).
    pub event_fetch_ok: bool,
    txns: HashMap<String, String>,
    echoed: usize,
    syncs: u64,
    counter: u64,
}

impl Hs {
    pub fn bodies(&self) -> Vec<String> {
        self.timeline.iter().map(|s| s.body.clone()).collect()
    }
}

fn event_json(s: &Stored, with_txn: bool) -> String {
    let unsigned = if with_txn {
        format!(r#","unsigned":{{"transaction_id":"{}"}}"#, s.txn)
    } else {
        String::new()
    };
    format!(
        r#"{{"type":"m.room.message","event_id":"{}","sender":"{USER}","origin_server_ts":{},"room_id":"{ROOM}","content":{{"msgtype":"m.text","body":{}}}{unsigned}}}"#,
        s.event_id,
        s.ts,
        serde_json::to_string(&s.body).unwrap()
    )
}

pub async fn spawn() -> (String, Arc<Mutex<Hs>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Hs {
        event_fetch_ok: true,
        ..Hs::default()
    }));
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let st = st.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
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
                    let req_body = buf[head_end..head_end + body_len].to_vec();
                    buf.drain(..head_end + body_len);

                    let request_line = head.lines().next().unwrap_or("").to_string();
                    let method = request_line.split_whitespace().next().unwrap_or("");
                    let path = request_line.split_whitespace().nth(1).unwrap_or("");
                    let path_only = path.split('?').next().unwrap_or("");
                    let (status, body) = if path.starts_with("/_matrix/client/versions") {
                        (
                            "200 OK",
                            r#"{"versions":["v1.1","v1.11"],"unstable_features":{}}"#.to_string(),
                        )
                    } else if path.starts_with("/_matrix/client/v3/sync") {
                        let mut s = st.lock().unwrap();
                        s.syncs += 1;
                        let first = s.syncs == 1;
                        let new: Vec<String> = s.timeline[s.echoed..]
                            .iter()
                            .map(|e| event_json(e, true))
                            .collect();
                        s.echoed = s.timeline.len();
                        let state_events = if first {
                            format!(
                                r#"{{"type":"m.room.create","event_id":"$create","sender":"{USER}","origin_server_ts":1,"state_key":"","content":{{"creator":"{USER}","room_version":"10"}}}},{{"type":"m.room.member","event_id":"$member","sender":"{USER}","origin_server_ts":2,"state_key":"{USER}","content":{{"membership":"join"}}}}"#
                            )
                        } else {
                            String::new()
                        };
                        (
                            "200 OK",
                            format!(
                                r#"{{"next_batch":"s_{}","rooms":{{"join":{{"{ROOM}":{{"state":{{"events":[{state_events}]}},"timeline":{{"events":[{}],"limited":false}}}}}}}}}}"#,
                                s.syncs,
                                new.join(",")
                            ),
                        )
                    } else if method == "PUT" && path_only.contains("/send/") {
                        let txn = path_only.rsplit('/').next().unwrap_or("").to_string();
                        let content: serde_json::Value =
                            serde_json::from_slice(&req_body).unwrap_or_default();
                        let text = content["body"].as_str().unwrap_or("").to_string();
                        let mut s = st.lock().unwrap();
                        s.put_calls += 1;
                        let mode = s.script.pop_front().unwrap_or(SendMode::Ok);
                        match mode {
                            SendMode::Fail502 => (
                                "502 Bad Gateway",
                                r#"{"errcode":"M_UNKNOWN","error":"bad gateway"}"#.to_string(),
                            ),
                            SendMode::Ok | SendMode::PersistDropResponse => {
                                let id = if let Some(id) = s.txns.get(&txn) {
                                    id.clone()
                                } else {
                                    s.counter += 1;
                                    let id = format!("$ev{}", s.counter);
                                    let ts = 1_000 + s.counter;
                                    s.txns.insert(txn.clone(), id.clone());
                                    s.timeline.push(Stored {
                                        event_id: id.clone(),
                                        txn: txn.clone(),
                                        body: text,
                                        ts,
                                    });
                                    id
                                };
                                if mode == SendMode::PersistDropResponse {
                                    drop(s);
                                    return; // closes the socket, no response
                                }
                                ("200 OK", format!(r#"{{"event_id":"{id}"}}"#))
                            }
                        }
                    } else if method == "GET" && path_only.contains("/event/") {
                        let id = path_only.rsplit('/').next().unwrap_or("");
                        let id = id.replace("%24", "$");
                        let mut s = st.lock().unwrap();
                        s.event_gets += 1;
                        let found = s.timeline.iter().find(|e| e.event_id == id).cloned();
                        match found {
                            Some(e) if s.event_fetch_ok => ("200 OK", event_json(&e, false)),
                            _ => (
                                "404 Not Found",
                                r#"{"errcode":"M_NOT_FOUND","error":"no such event"}"#.to_string(),
                            ),
                        }
                    } else if path_only.contains("/state/m.room.encryption") {
                        (
                            "404 Not Found",
                            r#"{"errcode":"M_NOT_FOUND","error":"no encryption"}"#.to_string(),
                        )
                    } else if path.starts_with("/_matrix/client/v3/keys/upload") {
                        ("200 OK", r#"{"one_time_key_counts":{}}"#.to_string())
                    } else if path.starts_with("/_matrix/client/v3/keys/query") {
                        ("200 OK", r#"{"device_keys":{},"failures":{}}"#.to_string())
                    } else {
                        ("200 OK", "{}".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, state)
}

/// A client against the fake homeserver, already synced into the room.
pub async fn agent(tag: &str) -> (crate::AgentClient, Arc<Mutex<Hs>>, std::path::PathBuf) {
    use matrix_sdk::ruma::{OwnedDeviceId, OwnedUserId};
    let (url, state) = spawn().await;
    let store = std::env::temp_dir().join(format!(
        "aqua-gd-{tag}-{}-{}",
        std::process::id(),
        crate::unix_now()
    ));
    let _ = std::fs::remove_dir_all(&store);
    std::fs::create_dir_all(&store).unwrap();
    let user: OwnedUserId = USER.try_into().unwrap();
    let device: OwnedDeviceId = "GDDEV".into();
    let client = crate::build_and_restore(&url, &store, &user, &device, "tok")
        .await
        .expect("client against the fake homeserver");
    client
        .sync_once(crate::catch_up_sync_settings())
        .await
        .expect("first sync");
    let agent = crate::AgentClient {
        client,
        did: "did:key:test".into(),
        user_id: user,
        expires_at_unix: u64::MAX,
        config: crate::AgentConfig {
            key_file: store.join("agent.pem"),
            siwx_url: url.clone(),
            matrix_url: url,
            client_id: None,
            redirect_uri: None,
            store_dir: store.clone(),
            device_id: None,
            device_role: crate::device_role::DeviceRole::Primary,
        },
        journal: None,
        recycle: None,
    };
    (agent, state, store)
}
