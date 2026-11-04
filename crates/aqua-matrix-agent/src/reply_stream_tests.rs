//! The streaming reply must not hammer the homeserver when sends are refused
//! (2026-11-03: about 450 first-send attempts in 20 s against a rejected token)
//! and must survive its access token being refused mid-reply. A tiny local
//! homeserver stands in for the real one: it serves a sync with one joined,
//! unencrypted room, accepts `PUT .../send/...` only with the tokens the test
//! allows (anything else is `401 M_UNKNOWN_TOKEN`, or `403` when the test says
//! so), and records every send it saw.

use super::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ROOM: &str = "!stream:localhost";

/// One `PUT .../send/...` the server saw.
#[derive(Debug, Clone)]
struct Sent {
    token: String,
    status: u16,
    is_edit: bool,
    text: String,
}

#[derive(Clone, Default)]
struct Hs {
    /// Tokens whose sends succeed. Everything else gets `401 M_UNKNOWN_TOKEN`.
    accept: Arc<Mutex<HashSet<String>>>,
    /// Answer every send `403 M_FORBIDDEN` (a refusal that is not a token one).
    forbid: Arc<AtomicBool>,
    sent: Arc<Mutex<Vec<Sent>>>,
}

impl Hs {
    fn accepting(tokens: &[&str]) -> Self {
        let hs = Self::default();
        hs.accept
            .lock()
            .unwrap()
            .extend(tokens.iter().map(|t| t.to_string()));
        hs
    }
    fn sends(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
    fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

async fn serve(hs: Hs) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let hs = hs.clone();
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
                    let header = |name: &str| {
                        head.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                        })
                    };
                    let body_len = header("content-length")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + body_len {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body =
                        String::from_utf8_lossy(&buf[head_end..head_end + body_len]).to_string();
                    buf.drain(..head_end + body_len);

                    let request_line = head.lines().next().unwrap_or("");
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    let (status, reply) = if path.starts_with("/_matrix/client/versions") {
                        (
                            200,
                            r#"{"versions":["v1.1","v1.11"],"unstable_features":{}}"#.to_string(),
                        )
                    } else if path.starts_with("/_matrix/client/v3/sync") {
                        (
                            200,
                            format!(
                                r#"{{"next_batch":"s_1","rooms":{{"join":{{"{ROOM}":{{}}}}}}}}"#
                            ),
                        )
                    } else if path.starts_with("/_matrix/client/v3/keys/upload") {
                        (200, r#"{"one_time_key_counts":{}}"#.to_string())
                    } else if path.starts_with("/_matrix/client/v3/keys/query") {
                        (200, r#"{"device_keys":{},"failures":{}}"#.to_string())
                    } else if path.contains("/state/m.room.encryption") {
                        // Not an encrypted room: a plain `send` follows.
                        (
                            404,
                            r#"{"errcode":"M_NOT_FOUND","error":"Event not found."}"#.to_string(),
                        )
                    } else if method == "PUT" && path.contains("/send/") {
                        let token = header("authorization")
                            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
                            .unwrap_or_default();
                        let event: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        let is_edit = event.get("m.new_content").is_some();
                        let text = event
                            .pointer("/m.new_content/body")
                            .or_else(|| event.get("body"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let ok = hs.accept.lock().unwrap().contains(&token);
                        let (status, reply) = if hs.forbid.load(Ordering::SeqCst) {
                            (
                                403,
                                r#"{"errcode":"M_FORBIDDEN","error":"refused"}"#.to_string(),
                            )
                        } else if ok {
                            let n = hs.sent.lock().unwrap().len();
                            (200, format!(r#"{{"event_id":"$ev{n}:localhost"}}"#))
                        } else {
                            (
                                401,
                                r#"{"errcode":"M_UNKNOWN_TOKEN","error":"Token is not active","soft_logout":false}"#
                                    .to_string(),
                            )
                        };
                        hs.sent.lock().unwrap().push(Sent {
                            token,
                            status,
                            is_edit,
                            text,
                        });
                        (status, reply)
                    } else {
                        (200, "{}".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}",
                        reply.len()
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    url
}

fn temp_store(tag: &str) -> PathBuf {
    // Unique per call: tests run in parallel and a store directory is not shared.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "aqua-reply-stream-{tag}-{}-{}-{}",
        std::process::id(),
        unix_now(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The room handle of a client holding `token`, in its own store.
async fn room_with(url: &str, tag: &str, token: &str) -> matrix_sdk::Room {
    let store = temp_store(tag);
    let user: OwnedUserId = "@streamer:localhost".try_into().unwrap();
    let device: OwnedDeviceId = "STREAMDEV".into();
    let client = build_and_restore(url, &store, &user, &device, token)
        .await
        .expect("client against the fake homeserver");
    client
        .sync_once(catch_up_sync_settings())
        .await
        .expect("sync against the fake homeserver");
    let room_id: &RoomId = ROOM.try_into().unwrap();
    client.get_room(room_id).expect("the joined room")
}

/// A refresher that counts its calls and hands out a room on `token`, or fails
/// when `token` is `None`.
fn refresher(url: &str, token: Option<&'static str>) -> (RoomRefresher, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let url = url.to_string();
    let r: RoomRefresher = Arc::new(move || {
        let n = counted.fetch_add(1, Ordering::SeqCst);
        let url = url.clone();
        Box::pin(async move {
            match token {
                Some(t) => Ok(room_with(&url, &format!("fresh{n}"), t).await),
                None => Err(anyhow!("siwx-oidc unreachable")),
            }
        })
    });
    (r, calls)
}

/// Push `n` chunks of `len` bytes, the shape of a model streaming tokens.
async fn push_chunks(stream: &mut ReplyStream, n: usize, len: usize) {
    let chunk = "x".repeat(len);
    for _ in 0..n {
        stream
            .push(&chunk)
            .await
            .expect("push never fails the caller");
    }
}

/// THE incident: a first send the homeserver refuses was retried on every
/// chunk (about 450 requests in 20 s). One chunk, one attempt; the reply is
/// still delivered by `finish`, which does not wait for the backoff.
#[tokio::test]
async fn a_failing_first_send_is_not_retried_per_chunk() {
    let hs = Hs::accepting(&["t"]);
    hs.forbid.store(true, Ordering::SeqCst);
    let url = serve(hs.clone()).await;
    let mut stream = ReplyStream::start(room_with(&url, "a", "t").await, None)
        .await
        .unwrap();

    push_chunks(&mut stream, 300, 130).await;

    assert_eq!(
        hs.count(),
        1,
        "300 chunks must cost one open attempt, saw {:?}",
        hs.sends()
    );
    assert!(!stream.has_started());

    // The server recovers: finish delivers the whole reply at once.
    hs.forbid.store(false, Ordering::SeqCst);
    stream
        .finish(Some("the final answer"))
        .await
        .expect("finish delivers");
    let sends = hs.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!(sends[1].status, 200);
    assert!(!sends[1].is_edit, "no live message: a fresh one is sent");
    assert!(sends[1].text.contains("the final answer"));
}

/// A rejected token on the first send: the stream mints a fresh session once,
/// repeats the send on it, and carries on there (later edits and finish use the
/// fresh token and never refresh again).
#[tokio::test]
async fn a_rejected_token_is_refreshed_once_and_the_reply_carries_on() {
    let hs = Hs::accepting(&["fresh"]);
    let url = serve(hs.clone()).await;
    let (r, calls) = refresher(&url, Some("fresh"));
    let mut stream = ReplyStream::start(room_with(&url, "b", "stale").await, Some(r))
        .await
        .unwrap();

    push_chunks(&mut stream, 5, 130).await;
    assert!(stream.has_started(), "the first message landed");
    let sends = hs.sends();
    assert_eq!(sends.len(), 2, "{sends:?}");
    assert_eq!((sends[0].token.as_str(), sends[0].status), ("stale", 401));
    assert_eq!((sends[1].token.as_str(), sends[1].status), ("fresh", 200));

    stream.set_now("a progress line").await.unwrap();
    stream.finish(Some("done")).await.expect("finish");
    let sends = hs.sends();
    assert_eq!(sends.len(), 4, "{sends:?}");
    assert!(sends[2..]
        .iter()
        .all(|s| s.token == "fresh" && s.status == 200));
    assert!(sends[2].is_edit && sends[2].text.contains("a progress line"));
    assert!(sends[3].is_edit && sends[3].text.contains("done"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one refresh");
}

/// When the refresh itself fails, streaming stops after the one rejected
/// request (no per-chunk storm), and `finish` reports the rejected token after
/// ONE more request, not five, so the caller's fallback (refresh the agent's
/// token, send a plain DM) starts at once.
#[tokio::test]
async fn a_failed_refresh_stops_streaming_and_finish_reports_the_token() {
    let hs = Hs::accepting(&[]);
    let url = serve(hs.clone()).await;
    let (r, calls) = refresher(&url, None);
    let mut stream = ReplyStream::start(room_with(&url, "c", "stale").await, Some(r))
        .await
        .unwrap();

    push_chunks(&mut stream, 300, 130).await;
    assert_eq!(hs.count(), 1, "{:?}", hs.sends());
    assert!(!stream.has_started());

    let err = stream
        .finish(Some("the final answer"))
        .await
        .expect_err("nothing can be delivered");
    assert!(
        is_unknown_token(&err),
        "the caller's fallback keys on this: {err:#}"
    );
    assert_eq!(
        hs.count(),
        2,
        "finish stops at the rejected token: {:?}",
        hs.sends()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the failed refresh is not repeated"
    );
}

/// A token that is refused again right after a refresh is a signed-out
/// account: no second refresh, no more streaming.
#[tokio::test]
async fn a_token_refused_right_after_a_refresh_is_not_refreshed_in_a_loop() {
    let hs = Hs::accepting(&[]);
    let url = serve(hs.clone()).await;
    let (r, calls) = refresher(&url, Some("fresh"));
    let mut stream = ReplyStream::start(room_with(&url, "d", "stale").await, Some(r))
        .await
        .unwrap();

    push_chunks(&mut stream, 300, 130).await;
    assert_eq!(hs.count(), 2, "stale, then the fresh one: {:?}", hs.sends());

    let err = stream.finish(Some("x")).await.expect_err("still refused");
    assert!(is_unknown_token(&err));
    assert_eq!(hs.count(), 3, "{:?}", hs.sends());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// The backoff schedule itself, on synthetic instants.
#[test]
fn the_open_gate_backs_off_exponentially_to_its_cap_and_resets() {
    let t0 = Instant::now();
    let mut gate = OpenGate::default();
    assert!(
        gate.ready(t0),
        "a fresh gate lets the first attempt through"
    );

    let mut now = t0;
    let mut waits = Vec::new();
    for _ in 0..8 {
        assert!(gate.ready(now));
        let wait = gate.failed(now);
        waits.push(wait.as_secs());
        assert!(!gate.ready(now), "closed right after a failure");
        assert!(!gate.ready(now + wait - Duration::from_millis(1)));
        now += wait;
        assert!(gate.ready(now), "open again once the wait is over");
    }
    assert_eq!(waits, vec![1, 2, 4, 8, 16, 30, 30, 30]);

    gate.succeeded();
    assert!(gate.ready(now));
    assert_eq!(gate.failed(now), OPEN_RETRY_BASE, "a success starts over");
}

/// A rejected token ends the finalize loop at once; other errors still get
/// every attempt (the contract of `retry_finalize` otherwise unchanged).
#[tokio::test(start_paused = true)]
async fn finalize_stops_at_a_rejected_token_and_retries_anything_else() {
    let calls = AtomicUsize::new(0);
    let err = retry_finalize("t", || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(anyhow!(
            "the server returned an error: [401 / M_UNKNOWN_TOKEN] Token is not active"
        ))
    })
    .await
    .unwrap_err();
    assert!(is_unknown_token(&err));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let calls = AtomicUsize::new(0);
    let _ = retry_finalize("t", || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(anyhow!("connection reset"))
    })
    .await
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), FINALIZE_MAX_ATTEMPTS);
}
