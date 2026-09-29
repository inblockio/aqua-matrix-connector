//! Engine policy through both backends (in-process `Engine` and the unix
//! socket), against a mock Matrix transport. State dirs live under
//! `~/.cache` (disk), never `/tmp` (RAM-backed tmpfs on the dev host).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aqua_messenger::allowlist::{AllowList, Recipient};
use aqua_messenger::inbox::{MediaRef, NewEntry};
use aqua_messenger::mcp::{call_tool, SocketBackend};
use aqua_messenger::proto::Request;
use aqua_messenger::{
    jsonrpc, server, Dest, Engine, FetchRequest, Fetched, Profile, ReplyRef, Sent, Transport,
};
use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Default)]
struct Mock {
    /// (destination, text, reply)
    sent: Mutex<Vec<(Dest, String, Option<ReplyRef>)>>,
    fetches: AtomicUsize,
    payload: Vec<u8>,
    fail_sends: bool,
}

#[async_trait]
impl Transport for Mock {
    async fn send_text(
        &self,
        to: &Dest,
        md: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        if self.fail_sends {
            return Err("homeserver said no".into());
        }
        let mut s = self.sent.lock().unwrap();
        s.push((to.clone(), md.into(), reply.cloned()));
        Ok(Sent {
            event_id: format!("$ev{}", s.len()),
            thread_root: reply.and_then(|r| r.thread_root.clone()),
        })
    }
    async fn send_file(
        &self,
        to: &Dest,
        path: &Path,
        caption: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        self.send_text(to, &format!("FILE {} {caption}", path.display()), reply)
            .await
    }
    async fn fetch(&self, req: FetchRequest) -> Result<Fetched, String> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        assert!(req.media.is_some());
        Ok(Fetched {
            bytes: self.payload.clone(),
            mimetype: Some("application/pdf".into()),
        })
    }
}

fn dir(tag: &str) -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let d = base
        .join(".cache")
        .join(format!("msgr-engine-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const OWNER: &str = "@owner:localhost";

fn embedded(tag: &str, mock: Mock) -> (Arc<Engine<Mock>>, PathBuf) {
    let d = dir(tag);
    let owner = Recipient {
        name: "owner".into(),
        mxid: OWNER.into(),
        note: None,
    };
    let allow = AllowList::owner_only(owner, None, "Marina");
    (
        Arc::new(Engine::new(
            Profile::embedded_agent("Marina", "owner"),
            &d,
            allow,
            mock,
        )),
        d,
    )
}

async fn tool(e: &Engine<Mock>, name: &str, args: Value) -> (String, bool) {
    let desc = jsonrpc::describe(e.profile());
    call_tool(e, &desc, name, &args).await
}

fn attachment_entry(id: &str, size: Option<u64>) -> NewEntry {
    NewEntry {
        event_id: id.into(),
        room_id: "!r:localhost".into(),
        sender: OWNER.into(),
        sender_name: Some("owner".into()),
        room: None,
        ts_ms: 1_790_000_000_000,
        kind: "file".into(),
        body: "../../report.pdf".into(),
        filename: Some("../../report.pdf".into()),
        media: Some(MediaRef {
            source: json!({"file": {"url": "mxc://x/y", "key": "SECRET-KEY-MATERIAL"}}),
            mimetype: Some("application/pdf".into()),
            size,
        }),
        in_reply_to: None,
        thread_root: None,
    }
}

fn text_entry(
    id: &str,
    room_id: &str,
    sender: &str,
    room: Option<&str>,
    thread_root: Option<&str>,
) -> NewEntry {
    NewEntry {
        event_id: id.into(),
        room_id: room_id.into(),
        sender: sender.into(),
        sender_name: None,
        room: room.map(Into::into),
        ts_ms: 1_790_000_000_000,
        kind: "text".into(),
        body: format!("body of {id}"),
        filename: None,
        media: None,
        in_reply_to: None,
        thread_root: thread_root.map(Into::into),
    }
}

#[tokio::test]
async fn embedded_defaults_to_owner_and_refuses_strangers() {
    let (e, d) = embedded("owner", Mock::default());
    let (t, err) = tool(&e, "send_message", json!({"markdown": "hi"})).await;
    assert!(!err, "{t}");
    assert!(t.contains("Delivered to owner (@owner:localhost)"));
    let sent = e.transport().sent.lock().unwrap().clone();
    assert_eq!(
        sent,
        vec![(Dest::Person(OWNER.to_string()), "hi".to_string(), None)],
        "no origin tag for an embedded agent"
    );
    // the send result points at read_inbox, never at the disabled wait_for_reply
    assert!(
        t.contains("read_inbox") && !t.contains("wait_for_reply"),
        "{t}"
    );
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"to": "@stranger:localhost", "markdown": "hi"}),
    )
    .await;
    assert!(err && t.starts_with("REFUSED"), "{t}");
    let (t, err) = tool(&e, "wait_for_reply", json!({"from": "owner"})).await;
    assert!(
        err && t.contains("not enabled"),
        "wait_for_reply is off by default: {t}"
    );
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn host_profile_requires_to_and_tags_origin() {
    let d = dir("host");
    std::fs::write(
        d.join("allowlist.toml"),
        "[[recipients]]\nname = \"tim\"\nmxid = \"@tim:localhost\"\n",
    )
    .unwrap();
    let allow = AllowList::new(d.join("allowlist.toml"));
    let e = Engine::new(Profile::host(), &d, allow, Mock::default());
    let (t, err) = tool(&e, "send_message", json!({"markdown": "x"})).await;
    assert!(err && t.contains("send_message needs `to`"), "{t}");
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"to": "tim", "markdown": "x", "from_label": "ci"}),
    )
    .await;
    assert!(!err, "{t}");
    let body = e.transport().sent.lock().unwrap()[0].1.clone();
    assert!(body.starts_with("x\n\n<sub>via `ci ("), "{body}");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn caps_rate_limit_and_sensitive_paths() {
    let (e, d) = embedded("caps", Mock::default());
    let (t, err) = tool(&e, "send_message", json!({"markdown": "x".repeat(20_001)})).await;
    assert!(err && t.contains("the cap is 20000"), "{t}");
    for i in 0..20 {
        let (t, err) = tool(&e, "send_message", json!({"markdown": format!("m{i}")})).await;
        assert!(!err, "{t}");
    }
    let (t, err) = tool(&e, "send_message", json!({"markdown": "one too many"})).await;
    assert!(err && t.starts_with("RATE LIMITED"), "{t}");
    let env = d.join("prod.env");
    std::fs::write(&env, "K=V").unwrap();
    let (t, err) = tool(&e, "send_file", json!({"path": env})).await;
    assert!(err && t.contains("REFUSED"), "{t}");
    let own = d.join("attachments-note.txt");
    std::fs::write(&own, "state").unwrap();
    let (t, err) = tool(&e, "send_file", json!({"path": own})).await;
    assert!(err && t.contains("own state directory"), "{t}");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn failed_send_releases_the_rate_slot() {
    let (e, d) = embedded(
        "fail",
        Mock {
            fail_sends: true,
            ..Default::default()
        },
    );
    for _ in 0..25 {
        let (t, err) = tool(&e, "send_message", json!({"markdown": "x"})).await;
        assert!(err && t.starts_with("NOT delivered"), "{t}");
    }
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn inbox_is_untrusted_and_hides_the_media_key() {
    let (e, d) = embedded("inbox", Mock::default());
    assert!(e
        .classify_inbound(
            "@me:localhost",
            "@stranger:localhost",
            "!dm:localhost",
            "$x",
            false
        )
        .is_none());
    assert!(e
        .classify_inbound("@me:localhost", OWNER, "!dm:localhost", "$y", false)
        .is_some());
    assert_eq!(e.ingest(attachment_entry("$a", Some(5))), Some(1));
    assert_eq!(
        e.ingest(attachment_entry("$a", Some(5))),
        None,
        "event-id dedupe"
    );
    let raw = e
        .handle(Request::ReadInbox {
            from: None,
            since_seq: None,
            since_ts_ms: None,
            unread_only: false,
            mark_read: false,
            limit: None,
        })
        .await;
    assert!(
        !raw.data.to_string().contains("SECRET-KEY-MATERIAL"),
        "media source must not leave the engine"
    );
    let (t, err) = tool(&e, "read_inbox", json!({})).await;
    assert!(!err);
    assert!(
        t.starts_with("UNTRUSTED USER-AUTHORED DATA from Matrix (Marina inbox)"),
        "{t}"
    );
    assert!(t.contains("fetch_attachment with inbox_seq=1"), "{t}");
    assert!(!t.contains("SECRET-KEY-MATERIAL"));
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn fetch_attachment_stores_once_and_caches() {
    let (e, d) = embedded(
        "fetch",
        Mock {
            payload: b"%PDF-1.7 hello".to_vec(),
            ..Default::default()
        },
    );
    e.ingest(attachment_entry("$a", Some(14)));
    e.ingest(NewEntry {
        kind: "text".into(),
        filename: None,
        media: None,
        ..attachment_entry("$t", None)
    });
    e.ingest(attachment_entry("$big", Some(60 * 1024 * 1024)));
    let (t, err) = tool(&e, "fetch_attachment", json!({"inbox_seq": 1})).await;
    assert!(!err, "{t}");
    assert!(t.starts_with("UNTRUSTED USER-SUPPLIED FILE"), "{t}");
    let stored = d.join("attachments").join("1-report.pdf");
    assert!(
        t.contains(&stored.display().to_string()) && t.contains("\"cached\": false"),
        "{t}"
    );
    assert_eq!(std::fs::read(&stored).unwrap(), b"%PDF-1.7 hello");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (t, err) = tool(&e, "fetch_attachment", json!({"inbox_seq": "1"})).await;
    assert!(!err && t.contains("\"cached\": true"), "{t}");
    assert_eq!(
        e.transport().fetches.load(Ordering::SeqCst),
        1,
        "second call served from cache"
    );
    let (t, err) = tool(&e, "fetch_attachment", json!({"inbox_seq": 2})).await;
    assert!(err && t.contains("not an attachment"), "{t}");
    let (t, err) = tool(&e, "fetch_attachment", json!({"inbox_seq": 3})).await;
    assert!(err && t.contains("cap is"), "{t}");
    assert_eq!(
        e.transport().fetches.load(Ordering::SeqCst),
        1,
        "oversize refused before download"
    );
    let (t, err) = tool(&e, "fetch_attachment", json!({"inbox_seq": 99})).await;
    assert!(err && t.contains("no inbox entry"), "{t}");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn wait_for_reply_when_enabled() {
    let d = dir("wait");
    let owner = Recipient {
        name: "owner".into(),
        mxid: OWNER.into(),
        note: None,
    };
    let allow = AllowList::owner_only(owner, None, "M");
    let e = Arc::new(Engine::new(
        Profile::embedded_agent("M", "owner").with_wait_for_reply(),
        &d,
        allow,
        Mock::default(),
    ));
    let e2 = e.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        e2.ingest(NewEntry {
            kind: "text".into(),
            body: "yes, ship it".into(),
            filename: None,
            media: None,
            ..attachment_entry("$r", None)
        });
    });
    let (t, err) = tool(
        &e,
        "wait_for_reply",
        json!({"from": "owner", "timeout_s": 5}),
    )
    .await;
    assert!(
        !err && t.contains("yes, ship it") && t.contains("Reply received"),
        "{t}"
    );
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn socket_backend_serves_the_engine_and_describe() {
    let (e, d) = embedded("sock", Mock::default());
    let sock = d.join("m.sock");
    let listener = server::bind_socket(&sock).unwrap();
    assert!(
        server::bind_socket(&sock).is_err(),
        "a second owner is refused"
    );
    let task = tokio::spawn(server::serve(listener, e.clone()));
    let backend = SocketBackend::new(sock.clone(), "test");
    let desc = aqua_messenger::mcp::describe_or(&backend, json!({})).await;
    assert_eq!(desc["label"], "Marina");
    let names: Vec<&str> = desc["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "send_message",
            "send_file",
            "list_recipients",
            "read_inbox",
            "fetch_attachment"
        ]
    );
    let (t, err) = call_tool(&backend, &desc, "list_recipients", &json!({})).await;
    assert!(
        !err && t.contains("@owner:localhost") && t.contains("defaults to owner"),
        "{t}"
    );
    let (t, err) = call_tool(
        &backend,
        &desc,
        "send_message",
        &json!({"markdown": "via socket"}),
    )
    .await;
    assert!(!err, "{t}");
    assert_eq!(e.transport().sent.lock().unwrap().len(), 1);
    task.abort();
    let _ = std::fs::remove_dir_all(d);
}

const EXTRA: &str = r#"
[[rooms]]
name = "team"
room_id = "!team:localhost"
"#;

fn embedded_with_rooms(tag: &str) -> (Arc<Engine<Mock>>, PathBuf) {
    let d = dir(tag);
    std::fs::write(d.join("extra.toml"), EXTRA).unwrap();
    let owner = Recipient {
        name: "owner".into(),
        mxid: OWNER.into(),
        note: None,
    };
    let allow = AllowList::owner_only(owner, Some(d.join("extra.toml")), "Marina");
    (
        Arc::new(Engine::new(
            Profile::embedded_agent("Marina", "owner"),
            &d,
            allow,
            Mock::default(),
        )),
        d,
    )
}

#[tokio::test]
async fn rooms_are_first_class_targets_and_sources() {
    let (e, d) = embedded_with_rooms("rooms");
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"to": "team", "markdown": "hello team"}),
    )
    .await;
    assert!(
        !err && t.starts_with("Delivered to room team (!team:localhost)"),
        "{t}"
    );
    assert_eq!(
        e.transport().sent.lock().unwrap()[0].0,
        Dest::Room("!team:localhost".into())
    );
    // inbound: any member of a listed room; an unlisted group room is dropped
    let own = "@me:localhost";
    let acc = e
        .classify_inbound(own, "@stranger:localhost", "!team:localhost", "$1", true)
        .unwrap();
    assert_eq!(acc.room_name.as_deref(), Some("team"));
    assert!(
        e.classify_inbound(own, OWNER, "!other:localhost", "$2", true)
            .is_none(),
        "owner in an unlisted group room"
    );
    assert!(
        e.classify_inbound(own, own, "!team:localhost", "$3", true)
            .is_none(),
        "own messages"
    );
    // the room filter on read_inbox
    e.ingest(text_entry(
        "$r1",
        "!team:localhost",
        "@stranger:localhost",
        Some("team"),
        None,
    ));
    e.ingest(text_entry("$d1", "!dm:localhost", OWNER, None, None));
    let (t, err) = tool(&e, "read_inbox", json!({"from": "team", "since": "0"})).await;
    assert!(
        !err && t.contains("body of $r1") && !t.contains("body of $d1"),
        "{t}"
    );
    assert!(
        t.contains("\"event_id\": \"$r1\""),
        "entries expose the event id: {t}"
    );
    let (t, _) = tool(&e, "list_recipients", json!({})).await;
    assert!(t.contains("!team:localhost") && t.contains(OWNER), "{t}");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn reply_to_resolves_within_the_conversation() {
    let (e, d) = embedded_with_rooms("reply");
    let dm = e
        .ingest(text_entry("$q", "!dm:localhost", OWNER, None, None))
        .unwrap();
    let threaded = e
        .ingest(text_entry(
            "$tq",
            "!team:localhost",
            "@stranger:localhost",
            Some("team"),
            Some("$root"),
        ))
        .unwrap();
    // DM reply by seq (number or string): the transport gets room + sender
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"markdown": "answer", "reply_to": dm}),
    )
    .await;
    assert!(!err && t.contains("as a reply to $q"), "{t}");
    let r = e.transport().sent.lock().unwrap()[0].2.clone().unwrap();
    assert_eq!(
        r,
        ReplyRef {
            event_id: "$q".into(),
            sender: Some(OWNER.into()),
            room_id: Some("!dm:localhost".into()),
            thread_root: None
        }
    );
    // threaded room message: the reply carries the thread root
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"to": "team", "markdown": "in thread", "reply_to": threaded.to_string()}),
    )
    .await;
    assert!(
        !err && t.contains("as a reply to $tq (in thread $root)"),
        "{t}"
    );
    // by event id of an inbox entry
    let file = d.with_extension("report.md");
    std::fs::write(&file, "# report").unwrap();
    let (t, err) = tool(
        &e,
        "send_file",
        json!({"to": "team", "path": file.display().to_string(), "reply_to": "$tq"}),
    )
    .await;
    assert!(!err, "{t}");
    let _ = std::fs::remove_file(&file);
    // a message from another conversation is refused, before any send
    let n = e.transport().sent.lock().unwrap().len();
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"markdown": "wrong room", "reply_to": threaded}),
    )
    .await;
    assert!(
        err && t.starts_with("REFUSED") && t.contains("room team"),
        "{t}"
    );
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"to": "team", "markdown": "x", "reply_to": dm}),
    )
    .await;
    assert!(err && t.starts_with("REFUSED"), "{t}");
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"markdown": "x", "reply_to": 999}),
    )
    .await;
    assert!(err && t.contains("no inbox entry with seq 999"), "{t}");
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"markdown": "x", "reply_to": "not-an-id"}),
    )
    .await;
    assert!(err && t.contains("neither an inbox seq"), "{t}");
    assert_eq!(
        e.transport().sent.lock().unwrap().len(),
        n,
        "refusals never reach the transport"
    );
    // a bare event id unknown to the inbox is left to the transport to verify
    let (t, err) = tool(
        &e,
        "send_message",
        json!({"markdown": "x", "reply_to": "$elsewhere:localhost"}),
    )
    .await;
    assert!(!err, "{t}");
    let r = e
        .transport()
        .sent
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .2
        .clone()
        .unwrap();
    assert_eq!(
        r,
        ReplyRef {
            event_id: "$elsewhere:localhost".into(),
            ..Default::default()
        }
    );
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn wait_for_reply_is_gated_by_profile() {
    // default profile: not listed, refused by the engine even over the raw protocol
    let (e, d) = embedded("gate", Mock::default());
    assert!(!jsonrpc::describe(e.profile())
        .to_string()
        .contains("wait_for_reply"));
    let r = e
        .handle(Request::WaitForReply {
            from: "owner".into(),
            timeout_s: 1,
            after_seq: None,
        })
        .await;
    assert!(!r.ok && r.error.unwrap().contains("not enabled"));
    let _ = std::fs::remove_dir_all(d);
    // the host profile keeps it
    let d = dir("gate-host");
    std::fs::write(
        d.join("allowlist.toml"),
        "[[recipients]]\nname = \"tim\"\nmxid = \"@tim:localhost\"\n",
    )
    .unwrap();
    let e = Engine::new(
        Profile::host(),
        &d,
        AllowList::new(d.join("allowlist.toml")),
        Mock::default(),
    );
    let r = e
        .handle(Request::WaitForReply {
            from: "tim".into(),
            timeout_s: 1,
            after_seq: None,
        })
        .await;
    assert!(r.ok && r.data["timed_out"] == true, "{r:?}");
    let (t, err) = tool(&e, "send_message", json!({"to": "tim", "markdown": "x"})).await;
    assert!(
        !err && t.contains("pass as after_seq to wait_for_reply"),
        "host text unchanged: {t}"
    );
    let _ = std::fs::remove_dir_all(d);
}
