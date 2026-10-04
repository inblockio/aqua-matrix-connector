//! Engine policy through both backends (in-process `Engine` and the unix
//! socket), against a mock Matrix transport. State dirs live under
//! `~/.cache` (disk), never `/tmp` (RAM-backed tmpfs on the dev host).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aqua_messenger::allowlist::{AllowList, Recipient};
use aqua_messenger::inbox::{InboxPolicy, MediaRef, NewEntry};
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
    async fn edit_text(&self, to: &Dest, event_id: &str, md: &str) -> Result<Sent, String> {
        if event_id.starts_with("$foreign") {
            return Err(format!(
                "REFUSED: event {event_id} was sent by someone else"
            ));
        }
        self.send_text(to, &format!("EDIT {event_id} {md}"), None)
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
            by: None,
            session: None,
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
            by: None,
            session: None,
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
            by: None,
            session: None,
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

// ---- inbox policy, handling states and edits (ported from the host daemon) --

const HOST_LIST: &str = "[[recipients]]\nname = \"tim\"\nmxid = \"@tim:localhost\"\n\n[[rooms]]\nname = \"daily-updates\"\nroom_id = \"!daily:localhost\"\n";

fn host_with(tag: &str, policy: InboxPolicy) -> (Engine<Mock>, PathBuf) {
    let d = dir(tag);
    std::fs::write(d.join("allowlist.toml"), HOST_LIST).unwrap();
    let allow = AllowList::new(d.join("allowlist.toml"));
    let profile = Profile::host().with_inbox(policy);
    (Engine::new(profile, &d, allow, Mock::default()), d)
}

fn tracking() -> InboxPolicy {
    InboxPolicy {
        track_processed: true,
        ..InboxPolicy::default()
    }
}

fn now_ms() -> u64 {
    aqua_messenger::inbox::now_ms()
}

fn read(open: bool, by: Option<&str>) -> Request {
    Request::ReadInbox {
        from: None,
        since_seq: None,
        since_ts_ms: None,
        unread_only: open,
        mark_read: true,
        limit: None,
        by: by.map(Into::into),
        session: by.map(|_| "ABC-123".into()),
    }
}

fn seqs_of(r: &aqua_messenger::proto::Response) -> Vec<u64> {
    assert!(r.ok, "{r:?}");
    r.data["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn default_host_keeps_unread_semantics_and_refuses_processing() {
    let (e, d) = host_with("untracked", InboxPolicy::default());
    let dm = e
        .ingest(text_entry(
            "$d1",
            "!dm:localhost",
            "@tim:localhost",
            None,
            None,
        ))
        .unwrap();
    let r = e.handle(read(true, Some("repo@host"))).await;
    assert_eq!(seqs_of(&r), vec![dm]);
    assert_eq!(r.data["track_processed"], false);
    // read = gone from the open view, exactly as before the states
    assert!(seqs_of(&e.handle(read(true, None)).await).is_empty());
    let r = e
        .handle(Request::MarkProcessed {
            seqs: vec![dm],
            up_to_seq: None,
            from: None,
            note: "done".into(),
            by: None,
            session: None,
        })
        .await;
    assert!(
        r.error
            .as_deref()
            .unwrap()
            .contains("does not track processed"),
        "{r:?}"
    );
    // who read it is recorded state, shown in a history read
    let all = e.handle(read(false, None)).await;
    assert_eq!(all.data["entries"][0]["seen"]["by"], "repo@host");
    assert_eq!(all.data["entries"][0]["seen"]["session"], "abc-123");
    let st = e.handle(Request::Status).await.data;
    assert_eq!(st["inbox_track_processed"], false);
    assert_eq!(st["inbox_processed"], 0);
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn processed_tracking_through_the_tools() {
    let (e, d) = host_with("tracked", tracking());
    let dm1 = e
        .ingest(text_entry(
            "$d1",
            "!dm:localhost",
            "@tim:localhost",
            None,
            None,
        ))
        .unwrap();
    let r1 = e
        .ingest(text_entry(
            "$r1",
            "!daily:localhost",
            "@bob:localhost",
            Some("daily-updates"),
            None,
        ))
        .unwrap();
    let r2 = e
        .ingest(text_entry(
            "$r2",
            "!daily:localhost",
            "@bob:localhost",
            Some("daily-updates"),
            None,
        ))
        .unwrap();
    let dm2 = e
        .ingest(text_entry(
            "$d2",
            "!dm:localhost",
            "@tim:localhost",
            None,
            None,
        ))
        .unwrap();
    let desc = jsonrpc::describe(e.profile());
    assert!(jsonrpc::desc_has_tool(&desc, "mark_processed"));

    // a pre-states MCP server (no by/session): its unread read gets the open
    // view, and a message only looked at stays open
    assert_eq!(
        seqs_of(&e.handle(read(true, None)).await),
        vec![dm1, r1, r2, dm2]
    );
    let (t, err) = tool(&e, "read_inbox", json!({})).await;
    assert!(
        !err && t.contains("4 open in total") && t.contains("mark_processed"),
        "{t}"
    );

    let (t, err) = tool(
        &e,
        "mark_processed",
        json!({"up_to_seq": r2, "from": "daily-updates", "note": "room chatter, no action"}),
    )
    .await;
    assert!(!err, "{t}");
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["marked"], json!([r1, r2]));
    assert_eq!(v["open_remaining"], 2);
    let (t, _) = tool(
        &e,
        "mark_processed",
        json!({"seqs": [dm1], "note": "answered in DM"}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&t).unwrap()["marked"],
        json!([dm1])
    );

    // a second session cannot redo or overwrite it
    let (t, _) = tool(
        &e,
        "mark_processed",
        json!({"seqs": [dm1, 99], "note": "again", "from_label": "other"}),
    )
    .await;
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["marked"], json!([]));
    assert_eq!(v["missing"], json!([99]));
    assert_eq!(v["already_processed"][0]["seq"], dm1);
    assert_eq!(
        v["already_processed"][0]["processed"]["note"],
        "answered in DM"
    );
    assert!(!v["already_processed"][0]["processed"]["by"]
        .as_str()
        .unwrap()
        .contains("other"));

    // open reads no longer return processed messages; history still does
    assert_eq!(seqs_of(&e.handle(read(true, Some("x@h"))).await), vec![dm2]);
    let (t, err) = tool(&e, "read_inbox", json!({"since": "0"})).await;
    assert!(
        !err && t.contains("\"state\": \"processed\"") && t.contains("answered in DM"),
        "{t}"
    );

    // refusals
    let (t, err) = tool(&e, "mark_processed", json!({"seqs": [dm2]})).await;
    assert!(err && t.contains("non-empty `note`"), "{t}");
    let (t, err) = tool(
        &e,
        "mark_processed",
        json!({"seqs": [dm2], "from": "tim", "note": "x"}),
    )
    .await;
    assert!(err && t.contains("up_to_seq"), "{t}");
    let (_, err) = tool(
        &e,
        "mark_processed",
        json!({"up_to_seq": 9, "from": "nobody here", "note": "x"}),
    )
    .await;
    assert!(err);

    let st = e.handle(Request::Status).await.data;
    assert_eq!(
        (
            st["inbox_new"].as_u64(),
            st["inbox_seen"].as_u64(),
            st["inbox_processed"].as_u64()
        ),
        (Some(0), Some(1), Some(3))
    );
    let audit = std::fs::read_to_string(d.join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"inbox_processed\""), "{audit}");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test]
async fn inbox_policy_bounds_age_and_refuses_media() {
    let policy = InboxPolicy {
        max_age: Some(Duration::from_secs(24 * 3600)),
        accept_media: false,
        ..InboxPolicy::default()
    };
    let (e, d) = host_with("policy", policy);
    let mut fresh = text_entry("$fresh", "!dm:localhost", "@tim:localhost", None, None);
    fresh.ts_ms = now_ms();
    assert!(e.ingest(fresh).is_some());
    // older than the age bound: never ingested (a backfill cannot resurrect it)
    let mut old = text_entry("$old", "!dm:localhost", "@tim:localhost", None, None);
    old.ts_ms = now_ms() - 25 * 3_600_000;
    assert!(e.ingest(old).is_none());
    // media: nothing recorded, logged once
    let mut media = attachment_entry("$img", Some(10));
    media.ts_ms = now_ms();
    assert!(e.ingest(media.clone()).is_none());
    assert!(e.ingest(media).is_none());
    let audit = std::fs::read_to_string(d.join("audit.jsonl")).unwrap();
    assert_eq!(audit.matches("inbound_dropped_media").count(), 1, "{audit}");
    assert!(!audit.contains("SECRET-KEY-MATERIAL"));
    let (t, err) = tool(&e, "fetch_attachment", json!({"inbox_seq": 1})).await;
    assert!(err && t.contains("not to accept inbound media"), "{t}");
    let st = e.handle(Request::Status).await.data;
    assert_eq!(st["inbox_max_age_hours"], 24);
    assert_eq!(st["inbox_accept_media"], false);
    assert_eq!(st["inbox_entries"], 1);
    let _ = std::fs::remove_dir_all(d);
}

fn edit(to: &str, event_id: &str, md: &str) -> Request {
    Request::EditMessage {
        to: to.into(),
        event_id: event_id.into(),
        markdown: md.into(),
        origin: "trains@nuc10".into(),
    }
}

async fn sends_left(e: &Engine<Mock>, name: &str) -> u64 {
    let r = e.handle(Request::ListRecipients).await;
    ["recipients", "rooms"]
        .iter()
        .flat_map(|k| r.data[*k].as_array().cloned().unwrap_or_default())
        .find(|v| v["name"] == name)
        .and_then(|v| v["sends_left_in_window"].as_u64())
        .unwrap()
}

#[tokio::test]
async fn edit_message_keeps_the_socket_contract() {
    let (e, d) = host_with("edit", InboxPolicy::default());
    let r = e
        .handle(edit("daily-updates", "$orig:x", "# Report v2"))
        .await;
    assert!(r.ok, "{r:?}");
    assert_eq!(r.data, json!({"event_id": "$ev1", "replaces": "$orig:x"}));
    let sent = e.transport().sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, Dest::Room("!daily:localhost".into()));
    assert_eq!(
        sent[0].1,
        "EDIT $orig:x # Report v2\n\n<sub>via `trains@nuc10`</sub>"
    );
    assert_eq!(
        sends_left(&e, "daily-updates").await,
        19,
        "an edit is one send"
    );
    let audit = std::fs::read_to_string(d.join("audit.jsonl")).unwrap();
    let last: Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
    assert_eq!(
        (
            last["event"].as_str(),
            last["kind"].as_str(),
            last["replaces"].as_str()
        ),
        (Some("sent"), Some("edit"), Some("$orig:x"))
    );
    assert!(!audit.contains("Report v2"), "audit holds no bodies");

    // refusals take no rate slot
    let err = |r: aqua_messenger::proto::Response| r.error.unwrap();
    assert!(err(e.handle(edit("mallory", "$o:x", "x")).await).starts_with("REFUSED"));
    assert!(err(e.handle(edit("tim", "not-an-event-id", "x")).await).contains("invalid event id"));
    assert!(
        err(e.handle(edit("tim", "$o:x", &"x".repeat(20_001))).await).contains("the cap is 20000")
    );
    let e2 = err(e.handle(edit("daily-updates", "$foreign:x", "x")).await);
    assert!(
        e2.starts_with("NOT delivered to daily-updates: REFUSED"),
        "{e2}"
    );
    assert_eq!(sends_left(&e, "daily-updates").await, 19);
    assert_eq!(sends_left(&e, "tim").await, 20);
    let _ = std::fs::remove_dir_all(d);
}

/// A transport that only sends: `edit_message` is refused by the default.
struct NoEdit;

#[async_trait]
impl Transport for NoEdit {
    async fn send_text(&self, _: &Dest, _: &str, _: Option<&ReplyRef>) -> Result<Sent, String> {
        Ok("$x".to_string().into())
    }
    async fn send_file(
        &self,
        _: &Dest,
        _: &Path,
        _: &str,
        _: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        Ok("$x".to_string().into())
    }
    async fn fetch(&self, _: FetchRequest) -> Result<Fetched, String> {
        Err("no".into())
    }
}

#[tokio::test]
async fn edit_is_refused_where_the_transport_cannot_edit() {
    let d = dir("noedit");
    let owner = Recipient {
        name: "owner".into(),
        mxid: OWNER.into(),
        note: None,
    };
    let e = Engine::new(
        Profile::embedded_agent("Marina", "owner"),
        &d,
        AllowList::owner_only(owner, None, "Marina"),
        NoEdit,
    );
    let r = e.handle(edit("owner", "$o:x", "x")).await;
    assert!(r.error.unwrap().contains("not supported"));
    let _ = std::fs::remove_dir_all(d);
}
