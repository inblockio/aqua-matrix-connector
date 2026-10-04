//! Socket side of the daemon. All policy (allow-list of people and rooms,
//! caps, rate limit, sensitive paths, reply_to checks, inbox, attachments,
//! audit) is the shared `aqua_messenger::Engine` with the host profile; this
//! module only provides its [`Transport`]: a queue into the Matrix cycle loop
//! ([`crate::matrix`]), which executes every send/fetch inline on the one live
//! Client (an `M_UNKNOWN_TOKEN` command is carried to the next Client, never
//! run on a second Client).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aqua_messenger::inbox::MediaRef;
use aqua_messenger::{Dest, FetchRequest, Fetched, ReplyRef, Sent, Transport};
use async_trait::async_trait;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::OwnedEventId;
use tokio::sync::{mpsc, oneshot};

/// The daemon's engine type.
pub type Engine = aqua_messenger::Engine<QueueTransport>;

/// How long a queued command may wait for the Matrix loop (reconnect, outage)
/// before it is abandoned unexecuted. The MCP side waits a little longer.
pub const SEND_DEADLINE: Duration = Duration::from_secs(170);

pub enum SendKind {
    Text(String),
    File {
        path: PathBuf,
        caption: String,
    },
    /// Replace `original` (checked on the live Client to be the bridge's own
    /// text message in the destination room) with `content`.
    Edit {
        original: OwnedEventId,
        content: Box<RoomMessageEventContent>,
    },
    /// Download + decrypt an inbound attachment (`to` is [`Target::Nobody`]).
    Fetch {
        event_id: String,
        room_id: String,
        media: Option<MediaRef>,
        max_bytes: u64,
    },
}

/// Outcome of a command executed on the live Client.
pub enum CmdOk {
    /// A send: the new event id (and the thread it went into).
    Sent(Sent),
    /// A fetch: the decrypted, verified bytes and the mime type if known.
    Fetched {
        bytes: Vec<u8>,
        mimetype: Option<String>,
    },
}

/// Where a command goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Dest(Dest),
    /// No destination (attachment fetches).
    Nobody,
}

pub struct SendCmd {
    pub to: Target,
    pub kind: SendKind,
    pub reply: Option<ReplyRef>,
    pub deadline: Instant,
    pub done: oneshot::Sender<Result<CmdOk, String>>,
}

/// Queue commands to the Matrix loop and await their outcome.
pub struct QueueTransport {
    tx: mpsc::Sender<SendCmd>,
}

impl QueueTransport {
    pub fn new(tx: mpsc::Sender<SendCmd>) -> Self {
        Self { tx }
    }

    async fn run(
        &self,
        to: Target,
        kind: SendKind,
        reply: Option<&ReplyRef>,
    ) -> Result<CmdOk, String> {
        let (tx, rx) = oneshot::channel();
        let cmd = SendCmd {
            to,
            kind,
            reply: reply.cloned(),
            deadline: Instant::now() + SEND_DEADLINE,
            done: tx,
        };
        if self.tx.send(cmd).await.is_err() {
            return Err("bridge Matrix loop is not running".into());
        }
        match tokio::time::timeout(SEND_DEADLINE + Duration::from_secs(10), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("bridge dropped the request (shutting down?)".to_string()),
            Err(_) => Err("send did not complete in time; outcome unknown".to_string()),
        }
    }
}

fn want_sent(r: Result<CmdOk, String>) -> Result<Sent, String> {
    match r? {
        CmdOk::Sent(s) => Ok(s),
        CmdOk::Fetched { .. } => Err("internal error: send answered with a fetch result".into()),
    }
}

#[async_trait]
impl Transport for QueueTransport {
    async fn send_text(
        &self,
        to: &Dest,
        markdown: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        want_sent(
            self.run(
                Target::Dest(to.clone()),
                SendKind::Text(markdown.to_string()),
                reply,
            )
            .await,
        )
    }

    async fn send_file(
        &self,
        to: &Dest,
        path: &Path,
        caption: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        want_sent(
            self.run(
                Target::Dest(to.clone()),
                SendKind::File {
                    path: path.to_path_buf(),
                    caption: caption.to_string(),
                },
                reply,
            )
            .await,
        )
    }

    async fn fetch(&self, req: FetchRequest) -> Result<Fetched, String> {
        let kind = SendKind::Fetch {
            event_id: req.event_id,
            room_id: req.room_id,
            media: req.media,
            max_bytes: req.max_bytes,
        };
        match self.run(Target::Nobody, kind, None).await {
            Ok(CmdOk::Fetched { bytes, mimetype }) => Ok(Fetched { bytes, mimetype }),
            Ok(CmdOk::Sent(_)) => Err("internal error: fetch answered with a send result".into()),
            Err(e) => Err(e),
        }
    }

    /// Build the `m.replace` content and check its encoded size here, before
    /// anything is queued; whether the original may be edited is checked on
    /// the live Client in the destination room ([`crate::edit::ensure_editable`]).
    async fn edit_text(&self, to: &Dest, event_id: &str, markdown: &str) -> Result<Sent, String> {
        let original = OwnedEventId::try_from(event_id.trim())
            .map_err(|e| format!("invalid event id {event_id:?}: {e}"))?;
        let content = crate::edit::replacement(markdown, original.clone());
        crate::edit::check_size(&content)?;
        want_sent(
            self.run(
                Target::Dest(to.clone()),
                SendKind::Edit {
                    original,
                    content: Box::new(content),
                },
                None,
            )
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_messenger::allowlist::AllowList;
    use aqua_messenger::inbox::NewEntry;
    use aqua_messenger::proto::Request;
    use aqua_messenger::{Profile, SeenRoom};
    use serde_json::json;
    use std::sync::Arc;

    /// State under ~/.cache (disk), never /tmp (RAM-backed tmpfs on this host).
    fn engine_with(allowlist: &str, tag: &str) -> (Arc<Engine>, mpsc::Receiver<SendCmd>) {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base
            .join(".cache")
            .join(format!("asb-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("allowlist.toml"), allowlist).unwrap();
        let (tx, rx) = mpsc::channel(4);
        let allow = AllowList::new(dir.join("allowlist.toml"));
        (
            Arc::new(aqua_messenger::Engine::new(
                Profile::host(),
                &dir,
                allow,
                QueueTransport::new(tx),
            )),
            rx,
        )
    }

    const LIST: &str = r#"
[[recipients]]
name = "tim"
mxid = "@tim:x"

[[rooms]]
name = "daily-updates"
room_id = "!daily:x"
"#;

    #[test]
    fn to_resolves_person_room_or_refuses() {
        let (e, _rx) = engine_with(LIST, "to");
        let p = e.resolve_to("tim").unwrap();
        assert_eq!(p.dest, Dest::Person("@tim:x".into()));
        assert_eq!(p.id, "@tim:x");
        let r = e.resolve_to("daily-updates").unwrap();
        assert_eq!(r.dest, Dest::Room("!daily:x".into()));
        assert_eq!(r.id, "!daily:x");
        assert_eq!(e.resolve_to("!daily:x").unwrap().name, "daily-updates");
        assert!(e.resolve_to("mallory").unwrap_err().starts_with("REFUSED"));
        // a joined but unlisted room is refused with a specific reason
        e.note_joined_room("!internal:x", SeenRoom::default());
        let err = e.resolve_to("!internal:x").unwrap_err();
        assert!(err.contains("not listed under [[rooms]]"), "{err}");
        assert!(
            err.starts_with("REFUSED: the bridge has joined room"),
            "{err}"
        );
    }

    #[test]
    fn from_resolves_to_sender_or_room_filter() {
        use aqua_messenger::engine::FromFilter;
        let (e, _rx) = engine_with(LIST, "from");
        assert_eq!(
            e.resolve_from("tim"),
            Ok(FromFilter::Sender("@tim:x".into()))
        );
        assert_eq!(
            e.resolve_from("daily-updates"),
            Ok(FromFilter::Room("!daily:x".into()))
        );
        // since-removed people/rooms stay readable by id
        assert!(matches!(
            e.resolve_from("@old:x"),
            Ok(FromFilter::Sender(_))
        ));
        assert!(matches!(e.resolve_from("!old:x"), Ok(FromFilter::Room(_))));
        assert!(e.resolve_from("nobody").is_err());
    }

    fn add_file(e: &Engine, ev: &str, room_id: &str, sender: &str, room: Option<&str>) -> u64 {
        e.ingest(NewEntry {
            event_id: ev.into(),
            room_id: room_id.into(),
            sender: sender.into(),
            sender_name: None,
            room: room.map(Into::into),
            ts_ms: 1,
            kind: "file".into(),
            body: "a.pdf".into(),
            filename: Some("a.pdf".into()),
            media: Some(MediaRef {
                source: json!({"file": {"url": "mxc://x/secretmedia", "key": {"k": "SECRETKEY"}}}),
                mimetype: Some("application/pdf".into()),
                size: Some(10),
            }),
            in_reply_to: None,
            thread_root: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn fetch_gate_allows_listed_rooms_and_people_only() {
        let (e, rx) = engine_with(LIST, "fetchgate");
        drop(rx); // no Matrix loop: a permitted fetch fails only at the queue
        let room = add_file(&e, "$r", "!daily:x", "@stranger:x", Some("daily-updates"));
        let tim = add_file(&e, "$t", "!dm:x", "@TIM:x", None);
        let other = add_file(&e, "$o", "!dm2:x", "@stranger:x", None);
        for seq in [room, tim] {
            let r = e.handle(Request::FetchAttachment { inbox_seq: seq }).await;
            let err = r.error.unwrap();
            assert!(err.contains("Matrix loop is not running"), "{seq}: {err}");
        }
        let err = e
            .handle(Request::FetchAttachment { inbox_seq: other })
            .await
            .error
            .unwrap();
        assert!(err.starts_with("REFUSED"), "{err}");
        // removing tim from the list stops fetches of what he sent earlier
        std::fs::write(
            e.state_dir().join("allowlist.toml"),
            "[[rooms]]\nname = \"daily-updates\"\nroom_id = \"!daily:x\"\n",
        )
        .unwrap();
        // mtime granularity: force a distinct mtime
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        let f = std::fs::File::options()
            .append(true)
            .open(e.state_dir().join("allowlist.toml"))
            .unwrap();
        f.set_modified(later).unwrap();
        let err = e
            .handle(Request::FetchAttachment { inbox_seq: tim })
            .await
            .error
            .unwrap();
        assert!(err.starts_with("REFUSED"), "{err}");
    }

    #[tokio::test]
    async fn inbox_output_never_carries_the_media_source() {
        let (e, _rx) = engine_with(LIST, "public");
        add_file(&e, "$r", "!daily:x", "@stranger:x", Some("daily-updates"));
        let r = e
            .handle(Request::ReadInbox {
                from: Some("daily-updates".into()),
                since_seq: None,
                since_ts_ms: None,
                unread_only: false,
                mark_read: false,
                limit: None,
                by: None,
                session: None,
            })
            .await;
        let out = serde_json::to_string(&r).unwrap();
        assert!(out.contains("a.pdf"), "{out}");
        assert!(
            !out.contains("SECRETKEY") && !out.contains("secretmedia"),
            "{out}"
        );
        let r = e
            .handle(Request::WaitForReply {
                from: "daily-updates".into(),
                timeout_s: 1,
                after_seq: None,
                by: None,
                session: None,
            })
            .await;
        let out = serde_json::to_string(&r).unwrap();
        assert!(out.contains("a.pdf"), "{out}");
        assert!(
            !out.contains("SECRETKEY") && !out.contains("secretmedia"),
            "{out}"
        );
    }

    #[test]
    fn broken_allowlist_fails_closed_for_rooms_too() {
        let bad = format!("{LIST}\n[[rooms]]\nname = \"x\"\nroom_id = \"not-a-room\"\n");
        let (e, _rx) = engine_with(&bad, "bad");
        assert!(e.resolve_to("tim").is_err());
        assert!(e.resolve_to("daily-updates").is_err());
    }

    #[tokio::test]
    async fn sends_are_queued_with_dest_and_reply() {
        let (e, mut rx) = engine_with(LIST, "queue");
        let e2 = e.clone();
        let loop_ = tokio::spawn(async move {
            let cmd = rx.recv().await.unwrap();
            assert_eq!(cmd.to, Target::Dest(Dest::Room("!daily:x".into())));
            assert_eq!(cmd.reply.as_ref().unwrap().event_id, "$q");
            let _ = cmd.done.send(Ok(CmdOk::Sent(Sent {
                event_id: "$new".into(),
                thread_root: Some("$root".into()),
            })));
        });
        let seq = e2.ingest(NewEntry {
            event_id: "$q".into(),
            room_id: "!daily:x".into(),
            sender: "@stranger:x".into(),
            sender_name: None,
            room: Some("daily-updates".into()),
            ts_ms: 1,
            kind: "text".into(),
            body: "question".into(),
            filename: None,
            media: None,
            in_reply_to: None,
            thread_root: Some("$root".into()),
        });
        let r = e2
            .handle(Request::SendMessage {
                to: "daily-updates".into(),
                markdown: "answer".into(),
                origin: "t@h".into(),
                reply_to: Some(seq.unwrap().to_string()),
            })
            .await;
        assert!(r.ok, "{r:?}");
        assert_eq!(r.data["reply_to"], "$q");
        assert_eq!(r.data["thread_root"], "$root");
        loop_.await.unwrap();
    }

    #[tokio::test]
    async fn edits_are_size_checked_before_queueing_then_queued_as_edits() {
        let (e, mut rx) = engine_with(LIST, "edit");
        let edit = |event_id: &str, md: &str| Request::EditMessage {
            to: "daily-updates".into(),
            event_id: event_id.into(),
            markdown: md.into(),
            origin: "trains@nuc10".into(),
        };
        // within the send_message cap, but too large as one edit event: refused
        // before anything reaches the Matrix loop
        let prose = "word ".repeat(aqua_system_bridge::MAX_MESSAGE_BYTES / 5);
        let r = e.handle(edit("$o:x", &prose)).await;
        assert!(r.error.unwrap().contains("the edit would be"));
        assert!(rx.try_recv().is_err(), "nothing queued");
        let loop_ = tokio::spawn(async move {
            let cmd = rx.recv().await.unwrap();
            assert_eq!(cmd.to, Target::Dest(Dest::Room("!daily:x".into())));
            let SendKind::Edit { original, content } = &cmd.kind else {
                panic!("not an edit")
            };
            assert_eq!(original.as_str(), "$orig:x");
            assert!(
                content.body().starts_with("* # Report v2"),
                "{}",
                content.body()
            );
            let _ = cmd.done.send(Ok(CmdOk::Sent("$new:x".to_string().into())));
        });
        let r = e.handle(edit("$orig:x", "# Report v2")).await;
        assert_eq!(r.data, json!({"event_id": "$new:x", "replaces": "$orig:x"}));
        loop_.await.unwrap();
    }
}
