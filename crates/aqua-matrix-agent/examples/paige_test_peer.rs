//! Persistent, scriptable Matrix test user ("test peer") for the Paige
//! end-to-end loop.
//!
//! One long-running process holds ONE identity, keeps its E2EE client synced,
//! keeps the DM with exactly one peer (`--peer`, the agent under test) open,
//! and exposes a small JSON control API on 127.0.0.1, so a test script can
//! play the human side: send text, voice notes (as Element sends them),
//! photos and files, reply, edit, and read every event of the DM with the
//! peer's media decrypted to disk.
//!
//! State (persistent, never printed):
//! - `--key-file`: Ed25519 PEM. The connector creates it on the first run when
//!   missing (as the agents do); this process then restricts it to 0600.
//! - `--store-dir` (0700): crypto store, `config.toml` (the OIDC session) and
//!   `recovery.key` (0600), all written by the connector.
//! - `<store-dir>/../received/` (0700): decrypted media from the peer.
//! - `--token-file` (0600): the control API's bearer token, a random value
//!   created when the file is missing.
//!
//! DM: only an invite from `--peer` is joined (any other invite is declined);
//! an existing 1:1 with the peer is used. A newer invite from the peer
//! replaces the current DM.
//!
//! Control API (`127.0.0.1:<--port>`, every request carries
//! `Authorization: Bearer <token>`, JSON in and out, errors `{"error": ".."}`):
//!
//! ```text
//! GET  /status -> {"mxid","device","dm_room_id","joined","synced","peer"}
//! POST /send   {"text": ".."}
//!            | {"voice": "/abs/note.ogg", "duration_ms"?: n}  m.audio + MSC3245 voice, duration
//!            | {"image": "/abs/photo.png"}                    m.image, mimetype/size/w/h
//!            | {"file": "/abs/x.pdf", "mimetype"?: ".."}      m.file
//!              each with optional "reply_to": "$event"     -> {"event_id"}
//! POST /edit   {"event_id": "$own", "text": ".."}          -> {"event_id"} (the m.replace)
//! GET  /events?after=<cursor>&wait=<0..300 s>              -> {"events": [..], "cursor"}
//! ```
//!
//! Every `/events` entry: `event_id`, `sender`, `from_peer`, `origin_server_ts`,
//! `msgtype` (`m.room.encrypted` while undecryptable), `body`, `in_reply_to`,
//! `is_edit`, `replaces`, `decrypted`, `media` (null, or `kind`, `filename`,
//! `mimetype`, `size`, `duration_ms`, `width`, `height`, `path`, `error`;
//! `path` is set for the peer's media once it is saved). Oldest first, both
//! sides. An event that arrived undecryptable is listed again (same
//! `event_id`, `decrypted: true`) once its key arrives. Cursors are opaque and
//! per process: a cursor from an earlier run returns everything held now.
//!
//! Run (the first run creates the account):
//!
//! ```text
//! cargo run -p aqua-matrix-agent --example paige_test_peer -- \
//!   --key-file <dir>/peer.pem --store-dir <dir>/store \
//!   --siwx-url https://siwx-oidc.inblock.io --matrix-url https://matrix.inblock.io \
//!   --peer @<paige>:matrix.inblock.io --port <port> --token-file <dir>/api.token
//! curl -s -H "Authorization: Bearer $(cat <dir>/api.token)" http://127.0.0.1:<port>/status
//! ```
//!
//! Logs go to stderr without message bodies; `--debug` adds them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use aqua_matrix_agent::{
    is_unknown_token, reload_olm_if_store_changed, AgentClient, AgentConfig, MediaHandle,
};
use clap::Parser;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::relation::Reply;
use matrix_sdk::ruma::events::room::message::{
    AudioInfo, AudioMessageEventContent, FileInfo, FileMessageEventContent,
    ImageMessageEventContent, MessageType, Relation, ReplacementMetadata, RoomMessageEventContent,
    UnstableAmplitude, UnstableAudioDetailsContentBlock, UnstableVoiceContentBlock,
};
use matrix_sdk::ruma::events::room::{ImageInfo, MediaSource};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, Mentions};
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, UInt};
use matrix_sdk::sync::SyncResponse;
use matrix_sdk::{Room, RoomState};
use mime::Mime;
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Parser, Debug)]
#[command(about = "Persistent scriptable Matrix test user for the Paige e2e loop")]
struct Args {
    /// Ed25519 PEM of the test user (created on the first run when missing).
    #[arg(long)]
    key_file: PathBuf,
    /// Persistent crypto store and session directory.
    #[arg(long)]
    store_dir: PathBuf,
    #[arg(long)]
    siwx_url: String,
    #[arg(long)]
    matrix_url: String,
    #[arg(long, default_value = "Paige E2E (test user)")]
    display_name: String,
    /// The only account whose DM invites are accepted and whose DM is driven.
    #[arg(long)]
    peer: String,
    /// Control API port on 127.0.0.1.
    #[arg(long)]
    port: u16,
    /// Bearer token file (0600; created with a random token when missing).
    #[arg(long)]
    token_file: PathBuf,
    /// Also log message bodies.
    #[arg(long)]
    debug: bool,
}

/// Long-poll timeout of one `/sync`.
const SYNC_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound on one `/sync` call, so a wedged request never stalls rotation.
const SYNC_GUARD: Duration = Duration::from_secs(90);
/// Rotate the access token when it has less than this left (tokens live ~300 s).
const TOKEN_MARGIN_SECS: u64 = 90;
const SEND_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Largest file `/send` uploads and the largest download it saves.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// A text body above this would push the event past Matrix's 65 KiB cap.
const MAX_TEXT_BYTES: usize = 60_000;
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WAIT_SECS: u64 = 300;
const MAX_EVENTS_PER_RESPONSE: usize = 500;
const LOG_CAPACITY: usize = 20_000;
const BACKFILL_LIMIT: u32 = 50;
/// Undecryptable events younger than this are retried for their key.
const UTD_RETRY_WINDOW_MS: u64 = 15 * 60 * 1000;
const UTD_RETRY_EVERY: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Event log
// ---------------------------------------------------------------------------

/// Opaque `/events` cursor: `<boot>-<seq>`. `boot` names this process, so a
/// cursor from an earlier run is recognised and treated as "from the start".
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    boot: String,
    seq: u64,
}

impl Cursor {
    fn parse(s: &str) -> Option<Self> {
        let (boot, seq) = s.split_once('-')?;
        if boot.is_empty() || boot.len() > 32 || !boot.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self {
            boot: boot.to_ascii_lowercase(),
            seq: seq.parse().ok()?,
        })
    }

    fn render(&self) -> String {
        format!("{}-{}", self.boot, self.seq)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MediaRecord {
    /// `image`, `audio`, `voice` (m.audio with the MSC3245 marker), `video`, `file`.
    kind: &'static str,
    filename: String,
    mimetype: Option<String>,
    size: Option<u64>,
    duration_ms: Option<u64>,
    width: Option<u64>,
    height: Option<u64>,
    /// Where the decrypted bytes were saved (the peer's media only).
    path: Option<String>,
    /// Why the bytes could not be saved.
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct EventRecord {
    #[serde(skip)]
    seq: u64,
    event_id: String,
    sender: String,
    from_peer: bool,
    origin_server_ts: u64,
    /// The msgtype (of the new content for an edit); `m.room.encrypted` while
    /// the event cannot be decrypted.
    msgtype: Option<String>,
    body: Option<String>,
    in_reply_to: Option<String>,
    is_edit: bool,
    /// The event an edit replaces.
    replaces: Option<String>,
    decrypted: bool,
    media: Option<MediaRecord>,
}

impl EventRecord {
    fn undecryptable(event_id: String, sender: String, origin_server_ts: u64) -> Self {
        Self {
            seq: 0,
            event_id,
            sender,
            from_peer: false,
            origin_server_ts,
            msgtype: Some("m.room.encrypted".into()),
            body: None,
            in_reply_to: None,
            is_edit: false,
            replaces: None,
            decrypted: false,
            media: None,
        }
    }
}

/// Every DM event this process has seen, in arrival order, deduplicated by
/// event id (an undecryptable entry may be followed by its decrypted form).
struct EventLog {
    boot: String,
    last_seq: u64,
    entries: VecDeque<EventRecord>,
    /// event id -> whether the newest entry for it is decrypted.
    known: HashMap<String, bool>,
}

impl EventLog {
    fn new(boot: String) -> Self {
        Self {
            boot,
            last_seq: 0,
            entries: VecDeque::new(),
            known: HashMap::new(),
        }
    }

    /// Whether `rec` adds anything: a new event, or the decrypted form of one
    /// held only as undecryptable.
    fn wants(&self, rec: &EventRecord) -> bool {
        match self.known.get(&rec.event_id) {
            None => true,
            Some(decrypted) => !decrypted && rec.decrypted,
        }
    }

    /// Append `rec` and return its sequence number, or `None` for a duplicate.
    fn push(&mut self, mut rec: EventRecord) -> Option<u64> {
        if !self.wants(&rec) {
            return None;
        }
        self.last_seq += 1;
        rec.seq = self.last_seq;
        self.known.insert(rec.event_id.clone(), rec.decrypted);
        self.entries.push_back(rec);
        while self.entries.len() > LOG_CAPACITY {
            self.entries.pop_front();
        }
        Some(self.last_seq)
    }

    /// Entries after `after` (all of them for no cursor or one from another
    /// run), at most `max`, plus the cursor to pass next time.
    fn since(&self, after: Option<&Cursor>, max: usize) -> (Vec<EventRecord>, Cursor) {
        let from = match after {
            Some(c) if c.boot == self.boot => c.seq.min(self.last_seq),
            _ => 0,
        };
        let events: Vec<EventRecord> = self
            .entries
            .iter()
            .filter(|e| e.seq > from)
            .take(max)
            .cloned()
            .collect();
        let seq = events.last().map(|e| e.seq).unwrap_or(from);
        (
            events,
            Cursor {
                boot: self.boot.clone(),
                seq,
            },
        )
    }

    fn sender_of(&self, event_id: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.event_id == event_id)
            .map(|e| e.sender.as_str())
    }

    /// Ids of events still held only as undecryptable, younger than `window_ms`.
    fn undecrypted(&self, now_ms: u64, window_ms: u64) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| !e.decrypted && self.known.get(&e.event_id) == Some(&false))
            .filter(|e| now_ms.saturating_sub(e.origin_server_ts) < window_ms)
            .map(|e| e.event_id.clone())
            .collect()
    }
}

/// The record of one `m.room.message`, plus the source of its media. `None`
/// for anything else (state, reactions, redacted messages).
fn record_from_event(ev: &AnySyncTimelineEvent) -> Option<(EventRecord, Option<MediaSource>)> {
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) = ev else {
        return None;
    };
    let original = msg.as_original()?;
    let content = &original.content;
    let (is_edit, replaces, msgtype) = match &content.relates_to {
        Some(Relation::Replacement(r)) => {
            (true, Some(r.event_id.to_string()), &r.new_content.msgtype)
        }
        _ => (false, None, &content.msgtype),
    };
    let (in_reply_to, _thread_root) =
        aqua_matrix_agent::reply::reply_fields(content.relates_to.as_ref());
    let (media, source) = match media_of(msgtype) {
        Some((m, s)) => (Some(m), Some(s)),
        None => (None, None),
    };
    Some((
        EventRecord {
            seq: 0,
            event_id: original.event_id.to_string(),
            sender: original.sender.to_string(),
            from_peer: false,
            origin_server_ts: u64::from(original.origin_server_ts.0),
            msgtype: Some(msgtype.msgtype().to_owned()),
            body: Some(msgtype.body().to_owned()),
            in_reply_to,
            is_edit,
            replaces,
            decrypted: true,
            media,
        },
        source,
    ))
}

fn ms(d: Option<Duration>) -> Option<u64> {
    d.map(|d| d.as_millis() as u64)
}

fn media_of(msgtype: &MessageType) -> Option<(MediaRecord, MediaSource)> {
    let record =
        |kind, filename: &str, mimetype: Option<&String>, size: Option<UInt>| MediaRecord {
            kind,
            filename: filename.to_owned(),
            mimetype: mimetype.cloned(),
            size: size.map(u64::from),
            duration_ms: None,
            width: None,
            height: None,
            path: None,
            error: None,
        };
    match msgtype {
        MessageType::Image(c) => {
            let info = c.info.as_deref();
            let mut m = record(
                "image",
                c.filename(),
                info.and_then(|i| i.mimetype.as_ref()),
                info.and_then(|i| i.size),
            );
            m.width = info.and_then(|i| i.width).map(u64::from);
            m.height = info.and_then(|i| i.height).map(u64::from);
            Some((m, c.source.clone()))
        }
        MessageType::Audio(c) => {
            let info = c.info.as_deref();
            let kind = if c.voice.is_some() { "voice" } else { "audio" };
            let mut m = record(
                kind,
                c.filename(),
                info.and_then(|i| i.mimetype.as_ref()),
                info.and_then(|i| i.size),
            );
            m.duration_ms = ms(c.audio.as_ref().map(|a| a.duration))
                .or_else(|| ms(info.and_then(|i| i.duration)));
            Some((m, c.source.clone()))
        }
        MessageType::Video(c) => {
            let info = c.info.as_deref();
            let mut m = record(
                "video",
                c.filename(),
                info.and_then(|i| i.mimetype.as_ref()),
                info.and_then(|i| i.size),
            );
            m.duration_ms = ms(info.and_then(|i| i.duration));
            m.width = info.and_then(|i| i.width).map(u64::from);
            m.height = info.and_then(|i| i.height).map(u64::from);
            Some((m, c.source.clone()))
        }
        MessageType::File(c) => {
            let info = c.info.as_deref();
            let m = record(
                "file",
                c.filename(),
                info.and_then(|i| i.mimetype.as_ref()),
                info.and_then(|i| i.size),
            );
            Some((m, c.source.clone()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Outgoing message content
// ---------------------------------------------------------------------------

/// What a media send uploads, decided from the file alone (no network).
#[derive(Debug, Clone, PartialEq)]
struct MediaPlan {
    filename: String,
    mime: Mime,
    msg: MediaMsg,
}

#[derive(Debug, Clone, PartialEq)]
enum MediaMsg {
    Voice {
        duration_ms: u64,
    },
    Image {
        width: Option<u64>,
        height: Option<u64>,
    },
    File,
}

/// Duration of an Ogg Opus stream from its last granule position (48 kHz)
/// minus the OpusHead pre-skip. No decoding.
fn ogg_opus_duration_ms(data: &[u8]) -> Result<u64> {
    let mut pos = 0usize;
    let mut serial: Option<u32> = None;
    let mut pre_skip = 0u64;
    let mut last_granule: Option<u64> = None;
    while pos < data.len() {
        let header = data
            .get(pos..pos + 27)
            .context("truncated Ogg page header")?;
        if &header[..4] != b"OggS" {
            bail!("not an Ogg stream (no page at byte {pos})");
        }
        let granule = i64::from_le_bytes(header[6..14].try_into().expect("8 bytes"));
        let page_serial = u32::from_le_bytes(header[14..18].try_into().expect("4 bytes"));
        let segments = header[26] as usize;
        let table = data
            .get(pos + 27..pos + 27 + segments)
            .context("truncated Ogg segment table")?;
        let body_len: usize = table.iter().map(|&b| b as usize).sum();
        let body_start = pos + 27 + segments;
        let body = data
            .get(body_start..body_start + body_len)
            .context("truncated Ogg page")?;
        match serial {
            None => {
                if body.len() < 19 || &body[..8] != b"OpusHead" {
                    bail!("not an Ogg Opus file (the first packet is not OpusHead)");
                }
                pre_skip = u64::from(u16::from_le_bytes([body[10], body[11]]));
                serial = Some(page_serial);
            }
            Some(s) if s == page_serial && granule > 0 => last_granule = Some(granule as u64),
            _ => {}
        }
        pos = body_start + body_len;
    }
    let samples = last_granule.unwrap_or(0).saturating_sub(pre_skip);
    if samples == 0 {
        bail!("the Ogg Opus stream holds no audio");
    }
    Ok((samples * 1000 + 24_000) / 48_000)
}

/// Content-sniffed image type (what the photo intake keys on), else the
/// extension's if it names an image.
fn image_mime(data: &[u8], path: &Path) -> Option<Mime> {
    let sniffed = if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    };
    match sniffed {
        Some(m) => m.parse().ok(),
        None => mime_guess::from_path(path)
            .first()
            .filter(|m| m.type_() == mime::IMAGE),
    }
}

fn basename(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn plan_media(kind: &SendKind, data: &[u8]) -> Result<MediaPlan, Resp> {
    match kind {
        SendKind::Text(_) => Err(Resp::err(500, "text is not media")),
        SendKind::Voice { path, duration_ms } => {
            let parsed = ogg_opus_duration_ms(data);
            let duration_ms = match (duration_ms, &parsed) {
                (Some(d), _) => *d,
                (None, Ok(d)) => *d,
                (None, Err(e)) => {
                    return Err(Resp::err(
                        400,
                        format!(
                            "{}: {e:#}; pass duration_ms for audio that is not Ogg Opus",
                            path.display()
                        ),
                    ))
                }
            };
            let mime = if parsed.is_ok() {
                "audio/ogg".parse().expect("static mime")
            } else {
                mime_guess::from_path(path)
                    .first()
                    .filter(|m| m.type_() == mime::AUDIO)
                    .unwrap_or_else(|| "audio/ogg".parse().expect("static mime"))
            };
            Ok(MediaPlan {
                filename: basename(path, "voice.ogg"),
                mime,
                msg: MediaMsg::Voice { duration_ms },
            })
        }
        SendKind::Image { path } => {
            let mime = image_mime(data, path).ok_or_else(|| {
                Resp::err(400, format!("{} is not a recognised image", path.display()))
            })?;
            let (width, height) = match imagesize::blob_size(data) {
                Ok(d) => (Some(d.width as u64), Some(d.height as u64)),
                Err(_) => (None, None),
            };
            Ok(MediaPlan {
                filename: basename(path, "image"),
                mime,
                msg: MediaMsg::Image { width, height },
            })
        }
        SendKind::File { path, mimetype } => {
            let mime = match mimetype {
                Some(m) => m
                    .parse()
                    .map_err(|_| Resp::err(400, format!("mimetype {m:?} is not a MIME type")))?,
                None => mime_guess::from_path(path).first_or_octet_stream(),
            };
            Ok(MediaPlan {
                filename: basename(path, "file"),
                mime,
                msg: MediaMsg::File,
            })
        }
    }
}

/// A plausible voice waveform (MSC3246 amplitudes, 0..=1024), about one bar
/// per 60 ms, so Element draws a voice bubble of the right length.
fn synth_waveform(duration_ms: u64) -> Vec<UnstableAmplitude> {
    let bars = (duration_ms / 60).clamp(24, 120) as usize;
    (0..bars)
        .map(|i| {
            let t = i as f32;
            let v = (0.45 + 0.30 * (t * 0.7).sin() + 0.15 * (t * 0.27).cos()).clamp(0.05, 1.0);
            UnstableAmplitude::new((v * 1024.0) as u16)
        })
        .collect()
}

/// An Element-style voice message: `m.audio` with `info.duration`, the
/// MSC1767 audio block (duration + waveform) and the MSC3245 voice marker.
fn voice_content(
    source: MediaSource,
    filename: &str,
    mime: &Mime,
    size: u64,
    duration_ms: u64,
) -> RoomMessageEventContent {
    let duration = Duration::from_millis(duration_ms);
    let mut info = AudioInfo::new();
    info.duration = Some(duration);
    info.mimetype = Some(mime.essence_str().to_owned());
    info.size = UInt::new(size);
    let mut audio = AudioMessageEventContent::new(filename.to_owned(), source).info(Box::new(info));
    audio.audio = Some(UnstableAudioDetailsContentBlock::new(
        duration,
        synth_waveform(duration_ms),
    ));
    audio.voice = Some(UnstableVoiceContentBlock::new());
    RoomMessageEventContent::new(MessageType::Audio(audio))
}

fn image_content(
    source: MediaSource,
    filename: &str,
    mime: &Mime,
    size: u64,
    width: Option<u64>,
    height: Option<u64>,
) -> RoomMessageEventContent {
    let mut info = ImageInfo::new();
    info.mimetype = Some(mime.essence_str().to_owned());
    info.size = UInt::new(size);
    info.width = width.and_then(UInt::new);
    info.height = height.and_then(UInt::new);
    let image = ImageMessageEventContent::new(filename.to_owned(), source).info(Box::new(info));
    RoomMessageEventContent::new(MessageType::Image(image))
}

fn file_content(
    source: MediaSource,
    filename: &str,
    mime: &Mime,
    size: u64,
) -> RoomMessageEventContent {
    let mut info = FileInfo::new();
    info.mimetype = Some(mime.essence_str().to_owned());
    info.size = UInt::new(size);
    let file = FileMessageEventContent::new(filename.to_owned(), source).info(Box::new(info));
    RoomMessageEventContent::new(MessageType::File(file))
}

fn media_content(plan: &MediaPlan, source: MediaSource, size: u64) -> RoomMessageEventContent {
    match plan.msg {
        MediaMsg::Voice { duration_ms } => {
            voice_content(source, &plan.filename, &plan.mime, size, duration_ms)
        }
        MediaMsg::Image { width, height } => {
            image_content(source, &plan.filename, &plan.mime, size, width, height)
        }
        MediaMsg::File => file_content(source, &plan.filename, &plan.mime, size),
    }
}

/// Make `content` a rich reply to `reply_to` (`m.relates_to.m.in_reply_to`,
/// no fallback), mentioning the original's sender as Element does.
fn apply_reply(
    content: &mut RoomMessageEventContent,
    reply_to: &EventId,
    mention: Option<OwnedUserId>,
) {
    content.relates_to = Some(Relation::Reply(Reply::with_event_id(reply_to.to_owned())));
    if let Some(user) = mention {
        content.mentions = Some(Mentions::with_user_ids([user]));
    }
}

/// An `m.replace` edit of `event_id` with Markdown `text`.
fn edit_content(event_id: &EventId, text: &str) -> RoomMessageEventContent {
    RoomMessageEventContent::text_markdown(text)
        .make_replacement(ReplacementMetadata::new(event_id.to_owned(), None))
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Resp {
    status: u16,
    body: Value,
}

impl Resp {
    fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }

    fn err(status: u16, msg: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": msg.into() }),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

fn render(resp: &Resp) -> Vec<u8> {
    let mut body = serde_json::to_vec(&resp.body).unwrap_or_else(|_| b"{}".to_vec());
    body.push(b'\n');
    let challenge = if resp.status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{challenge}\r\n",
        resp.status,
        reason(resp.status),
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}

type Head = (String, String, String, Vec<(String, String)>);

/// Request line and headers from the bytes before the blank line.
fn parse_head(head: &[u8]) -> Result<Head, Resp> {
    let text =
        std::str::from_utf8(head).map_err(|_| Resp::err(400, "request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Resp::err(400, "malformed request line"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Resp::err(400, "unsupported HTTP version"));
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(Resp::err(400, "malformed method"));
    }
    if !target.starts_with('/') {
        return Err(Resp::err(400, "the request target must be a path"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(Resp::err(400, "malformed header line"));
        };
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(Resp::err(400, "malformed header name"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Ok((
        method.to_string(),
        path.to_string(),
        query.to_string(),
        headers,
    ))
}

/// Read one request (head, then a `Content-Length` body) with size limits.
async fn read_request<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<HttpRequest, Resp> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(Resp::err(431, "request head too large"));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| Resp::err(400, "read error"))?;
        if n == 0 {
            return Err(Resp::err(400, "connection closed inside the request head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end > MAX_HEAD_BYTES {
        return Err(Resp::err(431, "request head too large"));
    }
    let (method, path, query, headers) = parse_head(&buf[..head_end])?;
    let mut req = HttpRequest {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
    };
    if req.header("transfer-encoding").is_some() {
        return Err(Resp::err(
            400,
            "chunked bodies are not supported; send Content-Length",
        ));
    }
    let len = match req.header("content-length") {
        None => 0,
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| Resp::err(400, "malformed Content-Length"))?,
    };
    if len > MAX_BODY_BYTES {
        return Err(Resp::err(
            413,
            format!("body larger than {MAX_BODY_BYTES} bytes"),
        ));
    }
    let mut body = buf[head_end + 4..].to_vec();
    if body.len() < len
        && req
            .header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|_| Resp::err(400, "write error"))?;
    }
    while body.len() < len {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| Resp::err(400, "read error"))?;
        if n == 0 {
            return Err(Resp::err(400, "connection closed inside the body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized(req: &HttpRequest, token: &str) -> bool {
    let Some((scheme, credential)) = req.header("authorization").and_then(|v| v.split_once(' '))
    else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && ct_eq(credential.trim().as_bytes(), token.as_bytes())
}

fn pct_decode(s: &str) -> Result<String, Resp> {
    let bad = || Resp::err(400, "malformed percent-encoding in the query");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3).ok_or_else(bad)?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

fn parse_query(q: &str) -> Result<Vec<(String, String)>, Resp> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((pct_decode(k)?, pct_decode(v)?))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// API requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum SendKind {
    Text(String),
    Voice {
        path: PathBuf,
        duration_ms: Option<u64>,
    },
    Image {
        path: PathBuf,
    },
    File {
        path: PathBuf,
        mimetype: Option<String>,
    },
}

impl SendKind {
    fn label(&self) -> &'static str {
        match self {
            SendKind::Text(_) => "text",
            SendKind::Voice { .. } => "voice",
            SendKind::Image { .. } => "image",
            SendKind::File { .. } => "file",
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            SendKind::Text(_) => None,
            SendKind::Voice { path, .. }
            | SendKind::Image { path }
            | SendKind::File { path, .. } => Some(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SendReq {
    kind: SendKind,
    reply_to: Option<OwnedEventId>,
}

#[derive(Debug, Clone, PartialEq)]
struct EditReq {
    event_id: OwnedEventId,
    text: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Api {
    Status,
    Send(SendReq),
    Edit(EditReq),
    Events {
        after: Option<Cursor>,
        wait: Duration,
    },
}

fn parse_api(req: &HttpRequest) -> Result<Api, Resp> {
    let method = match req.path.as_str() {
        "/status" | "/events" => "GET",
        "/send" | "/edit" => "POST",
        other => return Err(Resp::err(404, format!("no endpoint {other}"))),
    };
    if req.method != method {
        return Err(Resp::err(405, format!("{} takes {method}", req.path)));
    }
    match req.path.as_str() {
        "/status" => Ok(Api::Status),
        "/events" => parse_events_query(&req.query),
        "/send" => parse_send(&json_object(&req.body)?).map(Api::Send),
        _ => parse_edit(&json_object(&req.body)?).map(Api::Edit),
    }
}

fn json_object(body: &[u8]) -> Result<Map<String, Value>, Resp> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(Resp::err(400, "the body must be a JSON object")),
        Err(e) => Err(Resp::err(400, format!("the body is not JSON: {e}"))),
    }
}

fn reject_unknown(m: &Map<String, Value>, allowed: &[&str]) -> Result<(), Resp> {
    match m.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(Resp::err(
            400,
            format!("unknown field {k:?} (allowed: {})", allowed.join(", ")),
        )),
        None => Ok(()),
    }
}

/// A present, non-null field.
fn field<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.get(key).filter(|v| !v.is_null())
}

fn string_field(m: &Map<String, Value>, key: &str) -> Result<String, Resp> {
    match field(m, key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(Resp::err(400, format!("{key} must be a string"))),
        None => Err(Resp::err(400, format!("{key} is required"))),
    }
}

fn text_field(m: &Map<String, Value>, key: &str) -> Result<String, Resp> {
    let text = string_field(m, key)?;
    if text.trim().is_empty() {
        return Err(Resp::err(400, format!("{key} must not be empty")));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(Resp::err(
            413,
            format!("{key} is longer than {MAX_TEXT_BYTES} bytes"),
        ));
    }
    Ok(text)
}

fn path_field(m: &Map<String, Value>, key: &str) -> Result<PathBuf, Resp> {
    let path = PathBuf::from(string_field(m, key)?);
    if !path.is_absolute() {
        return Err(Resp::err(400, format!("{key} must be an absolute path")));
    }
    Ok(path)
}

fn event_id_field(m: &Map<String, Value>, key: &str) -> Result<OwnedEventId, Resp> {
    let s = string_field(m, key)?;
    OwnedEventId::try_from(s.as_str())
        .map_err(|_| Resp::err(400, format!("{key} is not an event id: {s:?}")))
}

fn parse_send(m: &Map<String, Value>) -> Result<SendReq, Resp> {
    reject_unknown(
        m,
        &[
            "text",
            "voice",
            "image",
            "file",
            "mimetype",
            "duration_ms",
            "reply_to",
        ],
    )?;
    let kinds: Vec<&str> = ["text", "voice", "image", "file"]
        .into_iter()
        .filter(|k| field(m, k).is_some())
        .collect();
    let [kind] = kinds.as_slice() else {
        return Err(Resp::err(
            400,
            "exactly one of text, voice, image, file is required",
        ));
    };
    if field(m, "mimetype").is_some() && *kind != "file" {
        return Err(Resp::err(400, "mimetype goes with file only"));
    }
    if field(m, "duration_ms").is_some() && *kind != "voice" {
        return Err(Resp::err(400, "duration_ms goes with voice only"));
    }
    let kind = match *kind {
        "text" => SendKind::Text(text_field(m, "text")?),
        "voice" => {
            let duration_ms = match field(m, "duration_ms") {
                None => None,
                Some(v) => match v.as_u64() {
                    Some(n) if n > 0 => Some(n),
                    _ => return Err(Resp::err(400, "duration_ms must be a positive integer")),
                },
            };
            SendKind::Voice {
                path: path_field(m, "voice")?,
                duration_ms,
            }
        }
        "image" => SendKind::Image {
            path: path_field(m, "image")?,
        },
        _ => {
            let mimetype = match field(m, "mimetype") {
                None => None,
                Some(_) => {
                    let s = string_field(m, "mimetype")?;
                    match s.parse::<Mime>() {
                        Ok(_) => Some(s),
                        Err(_) => {
                            return Err(Resp::err(
                                400,
                                format!("mimetype {s:?} is not a MIME type"),
                            ))
                        }
                    }
                }
            };
            SendKind::File {
                path: path_field(m, "file")?,
                mimetype,
            }
        }
    };
    let reply_to = match field(m, "reply_to") {
        None => None,
        Some(_) => Some(event_id_field(m, "reply_to")?),
    };
    Ok(SendReq { kind, reply_to })
}

fn parse_edit(m: &Map<String, Value>) -> Result<EditReq, Resp> {
    reject_unknown(m, &["event_id", "text"])?;
    Ok(EditReq {
        event_id: event_id_field(m, "event_id")?,
        text: text_field(m, "text")?,
    })
}

fn parse_events_query(query: &str) -> Result<Api, Resp> {
    let mut after = None;
    let mut wait = Duration::ZERO;
    for (key, value) in parse_query(query)? {
        match key.as_str() {
            "after" if value.is_empty() => after = None,
            "after" => {
                after =
                    Some(Cursor::parse(&value).ok_or_else(|| {
                        Resp::err(400, "after is not a cursor returned by /events")
                    })?)
            }
            "wait" => {
                let secs: u64 = value
                    .parse()
                    .map_err(|_| Resp::err(400, "wait must be whole seconds"))?;
                if secs > MAX_WAIT_SECS {
                    return Err(Resp::err(
                        400,
                        format!("wait is at most {MAX_WAIT_SECS} seconds"),
                    ));
                }
                wait = Duration::from_secs(secs);
            }
            other => {
                return Err(Resp::err(
                    400,
                    format!("unknown query parameter {other:?} (allowed: after, wait)"),
                ))
            }
        }
    }
    Ok(Api::Events { after, wait })
}

// ---------------------------------------------------------------------------
// Shared state and handlers
// ---------------------------------------------------------------------------

struct State {
    own: Option<OwnedUserId>,
    device: Option<String>,
    dm_room: Option<OwnedRoomId>,
    dm_marked: bool,
    joined: bool,
    synced: bool,
    log: EventLog,
    /// Event ids this process sent (an edit target may not be synced back yet).
    sent: HashSet<String>,
}

struct Peer {
    peer: OwnedUserId,
    token: String,
    received_dir: PathBuf,
    /// The live client; a send holds the read lock, a token rotation the write lock.
    agent: tokio::sync::RwLock<Option<AgentClient>>,
    state: Mutex<State>,
    /// Bumped on every new log entry (wakes `/events` long-polls).
    seq_tx: tokio::sync::watch::Sender<u64>,
}

impl Peer {
    fn new(peer: OwnedUserId, token: String, received_dir: PathBuf, boot: String) -> Self {
        Self {
            peer,
            token,
            received_dir,
            agent: tokio::sync::RwLock::new(None),
            state: Mutex::new(State {
                own: None,
                device: None,
                dm_room: None,
                dm_marked: false,
                joined: false,
                synced: false,
                log: EventLog::new(boot),
                sent: HashSet::new(),
            }),
            seq_tx: tokio::sync::watch::channel(0).0,
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_peer(&self, mxid: &str) -> bool {
        // Synapse canonicalises (lowercases) MXIDs.
        mxid.eq_ignore_ascii_case(self.peer.as_str())
    }

    fn push(&self, rec: EventRecord) {
        let summary = (
            rec.event_id.clone(),
            rec.sender.clone(),
            rec.msgtype.clone(),
            rec.is_edit,
            rec.body.clone(),
        );
        let saved = rec.media.as_ref().and_then(|m| m.path.as_ref()).is_some();
        let pushed = self.st().log.push(rec);
        if let Some(seq) = pushed {
            let (event_id, sender, msgtype, is_edit, body) = summary;
            tracing::info!(%event_id, %sender, msgtype = msgtype.as_deref().unwrap_or("-"), is_edit, media_saved = saved, "dm event");
            if let Some(body) = body {
                tracing::debug!(%event_id, %body, "dm event body");
            }
            self.seq_tx.send_replace(seq);
        }
    }
}

async fn dispatch(peer: &Peer, req: &HttpRequest) -> Resp {
    if !authorized(req, &peer.token) {
        return Resp::err(401, "missing or wrong bearer token");
    }
    let api = match parse_api(req) {
        Ok(api) => api,
        Err(resp) => return resp,
    };
    match api {
        Api::Status => status(peer),
        Api::Events { after, wait } => events(peer, after, wait).await,
        Api::Send(r) => send(peer, r).await,
        Api::Edit(r) => edit(peer, r).await,
    }
}

fn status(peer: &Peer) -> Resp {
    let st = peer.st();
    Resp::ok(json!({
        "mxid": st.own.as_ref().map(|u| u.to_string()),
        "device": st.device,
        "dm_room_id": st.dm_room.as_ref().map(|r| r.to_string()),
        "joined": st.joined,
        "synced": st.synced,
        "peer": peer.peer.to_string(),
    }))
}

async fn events(peer: &Peer, after: Option<Cursor>, wait: Duration) -> Resp {
    let deadline = Instant::now() + wait;
    let mut rx = peer.seq_tx.subscribe();
    loop {
        let (events, next) = peer.st().log.since(after.as_ref(), MAX_EVENTS_PER_RESPONSE);
        let left = deadline.saturating_duration_since(Instant::now());
        if !events.is_empty() || left.is_zero() {
            return Resp::ok(json!({ "events": events, "cursor": next.render() }));
        }
        if let Ok(Err(_)) = tokio::time::timeout(left, rx.changed()).await {
            tokio::time::sleep(left).await;
        }
    }
}

/// The joined DM room, or the reason there is none.
fn dm_room(peer: &Peer, agent: &AgentClient) -> Result<Room, Resp> {
    let room_id = peer.st().dm_room.clone().ok_or_else(|| {
        Resp::err(
            409,
            "no DM with the peer yet (waiting for the peer's invite)",
        )
    })?;
    let room = agent
        .client()
        .get_room(&room_id)
        .ok_or_else(|| Resp::err(409, format!("DM room {room_id} is not known to the client")))?;
    if room.state() != RoomState::Joined {
        return Err(Resp::err(
            409,
            format!("not joined to the DM room {room_id}"),
        ));
    }
    Ok(room)
}

async fn read_local(path: &Path) -> Result<Vec<u8>, Resp> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| Resp::err(400, format!("cannot read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Resp::err(
            400,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.len() == 0 {
        return Err(Resp::err(400, format!("{} is empty", path.display())));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(Resp::err(
            413,
            format!("{} is larger than {MAX_FILE_BYTES} bytes", path.display()),
        ));
    }
    tokio::fs::read(path)
        .await
        .map_err(|e| Resp::err(400, format!("cannot read {}: {e}", path.display())))
}

/// Upload `data` for `room`: encrypted (keys in the event) in an encrypted
/// room, plain otherwise.
async fn upload(
    agent: &AgentClient,
    room: &Room,
    data: Vec<u8>,
    mime: &Mime,
) -> Result<MediaSource> {
    if room.latest_encryption_state().await?.is_encrypted() {
        let mut reader = std::io::Cursor::new(data);
        let file = agent.client().upload_encrypted_file(&mut reader).await?;
        Ok(MediaSource::Encrypted(Box::new(file)))
    } else {
        let resp = agent.client().media().upload(mime, data, None).await?;
        Ok(MediaSource::Plain(resp.content_uri))
    }
}

async fn send_content(
    peer: &Peer,
    room: &Room,
    content: RoomMessageEventContent,
) -> Result<String, Resp> {
    match tokio::time::timeout(SEND_TIMEOUT, async { room.send(content).await }).await {
        Ok(Ok(resp)) => {
            let id = resp.response.event_id.to_string();
            peer.st().sent.insert(id.clone());
            Ok(id)
        }
        Ok(Err(e)) => Err(Resp::err(
            502,
            format!("send failed: {:#}", anyhow::Error::from(e)),
        )),
        Err(_) => Err(Resp::err(
            504,
            "the homeserver did not acknowledge the send in time",
        )),
    }
}

async fn send(peer: &Peer, req: SendReq) -> Resp {
    // Read and check the file before touching Matrix: a bad path is a 400 even offline.
    let media = match req.kind.path() {
        None => None,
        Some(path) => {
            let data = match read_local(path).await {
                Ok(d) => d,
                Err(r) => return r,
            };
            match plan_media(&req.kind, &data) {
                Ok(plan) => Some((plan, data)),
                Err(r) => return r,
            }
        }
    };
    let guard = peer.agent.read().await;
    let Some(agent) = guard.as_ref() else {
        return Resp::err(503, "not connected to Matrix yet");
    };
    let room = match dm_room(peer, agent) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let mut content = match (&req.kind, media) {
        (SendKind::Text(text), _) => RoomMessageEventContent::text_markdown(text),
        (_, Some((plan, data))) => {
            let size = data.len() as u64;
            match tokio::time::timeout(SEND_TIMEOUT, upload(agent, &room, data, &plan.mime)).await {
                Ok(Ok(source)) => media_content(&plan, source, size),
                Ok(Err(e)) => return Resp::err(502, format!("upload failed: {e:#}")),
                Err(_) => return Resp::err(504, "the upload did not finish in time"),
            }
        }
        (_, None) => return Resp::err(500, "media without a plan"),
    };
    if let Some(reply_to) = &req.reply_to {
        let mention = {
            let st = peer.st();
            st.log
                .sender_of(reply_to.as_str())
                .filter(|s| {
                    st.own
                        .as_ref()
                        .is_none_or(|own| !s.eq_ignore_ascii_case(own.as_str()))
                })
                .and_then(|s| OwnedUserId::try_from(s).ok())
        };
        apply_reply(&mut content, reply_to, mention);
    }
    match send_content(peer, &room, content).await {
        Ok(event_id) => {
            tracing::info!(%event_id, kind = req.kind.label(), reply = req.reply_to.is_some(), "sent");
            if let SendKind::Text(text) = &req.kind {
                tracing::debug!(%event_id, body = %text, "sent body");
            }
            Resp::ok(json!({ "event_id": event_id }))
        }
        Err(r) => r,
    }
}

async fn edit(peer: &Peer, req: EditReq) -> Resp {
    let ours = {
        let st = peer.st();
        st.sent.contains(req.event_id.as_str())
            || st
                .log
                .sender_of(req.event_id.as_str())
                .zip(st.own.as_ref())
                .is_some_and(|(sender, own)| sender.eq_ignore_ascii_case(own.as_str()))
    };
    if !ours {
        return Resp::err(
            404,
            format!(
                "{} is not a message this test user sent in the DM",
                req.event_id
            ),
        );
    }
    let guard = peer.agent.read().await;
    let Some(agent) = guard.as_ref() else {
        return Resp::err(503, "not connected to Matrix yet");
    };
    let room = match dm_room(peer, agent) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match send_content(peer, &room, edit_content(&req.event_id, &req.text)).await {
        Ok(event_id) => {
            tracing::info!(%event_id, replaces = %req.event_id, "sent edit");
            tracing::debug!(%event_id, body = %req.text, "edit body");
            Resp::ok(json!({ "event_id": event_id }))
        }
        Err(r) => r,
    }
}

async fn handle_conn(mut sock: tokio::net::TcpStream, peer: Arc<Peer>) {
    let resp = match tokio::time::timeout(REQUEST_READ_TIMEOUT, read_request(&mut sock)).await {
        Err(_) => Resp::err(400, "request not received in time"),
        Ok(Err(r)) => r,
        Ok(Ok(req)) => {
            let resp = dispatch(&peer, &req).await;
            tracing::debug!(method = %req.method, path = %req.path, status = resp.status, "api");
            resp
        }
    };
    let _ = sock.write_all(&render(&resp)).await;
    let _ = sock.shutdown().await;
}

async fn serve(listener: TcpListener, peer: Arc<Peer>) {
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                tokio::spawn(handle_conn(sock, peer.clone()));
            }
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Matrix side: sync, DM, ingest
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn secs_left(agent: &AgentClient) -> u64 {
    agent.expires_at_unix().saturating_sub(now_ms() / 1000)
}

fn safe_component(s: &str, max: usize) -> String {
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_start_matches('.');
    let name = if trimmed.is_empty() { "file" } else { trimmed };
    name.chars().take(max).collect()
}

fn received_name(event_id: &str, filename: &str) -> String {
    format!(
        "{}-{}",
        safe_component(event_id.trim_start_matches('$'), 64),
        safe_component(filename, 96)
    )
}

/// Download (decrypting) the peer's media into the received dir and record
/// the path, or why it could not be saved.
async fn save_media(
    agent: &AgentClient,
    dir: &Path,
    event_id: &str,
    media: &mut MediaRecord,
    source: MediaSource,
) {
    let path = dir.join(received_name(event_id, &media.filename));
    if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() > 0) {
        media.path = Some(path.display().to_string());
        return;
    }
    if media.size.is_some_and(|n| n > MAX_FILE_BYTES) {
        media.error = Some(format!(
            "declared size above {MAX_FILE_BYTES} bytes, not downloaded"
        ));
        return;
    }
    let handle = MediaHandle::new(
        source,
        media.filename.clone(),
        media.mimetype.clone(),
        media.size,
    );
    let bytes = match tokio::time::timeout(DOWNLOAD_TIMEOUT, agent.download_media(&handle)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            media.error = Some(format!("{e:#}"));
            return;
        }
        Err(_) => {
            media.error = Some("download timed out".into());
            return;
        }
    };
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(&bytes));
    match written {
        Ok(()) => media.path = Some(path.display().to_string()),
        Err(e) => media.error = Some(format!("could not write {}: {e}", path.display())),
    }
}

async fn ingest(peer: &Peer, agent: &AgentClient, events: Vec<TimelineEvent>) {
    for ev in events {
        let decoded = if ev.kind.is_utd() {
            match (ev.event_id(), ev.sender()) {
                (Some(id), Some(sender)) => Some((
                    EventRecord::undecryptable(
                        id.to_string(),
                        sender.to_string(),
                        ev.timestamp().map(|t| u64::from(t.0)).unwrap_or(0),
                    ),
                    None,
                )),
                _ => None,
            }
        } else {
            ev.raw()
                .deserialize()
                .ok()
                .as_ref()
                .and_then(record_from_event)
        };
        let Some((mut rec, source)) = decoded else {
            continue;
        };
        rec.from_peer = peer.is_peer(&rec.sender);
        if !peer.st().log.wants(&rec) {
            continue;
        }
        if rec.from_peer {
            if let (Some(media), Some(source)) = (rec.media.as_mut(), source) {
                save_media(agent, &peer.received_dir, &rec.event_id, media, source).await;
            }
        }
        peer.push(rec);
    }
}

async fn backfill(peer: &Peer, agent: &AgentClient, room: &Room) {
    let mut opts = MessagesOptions::backward();
    opts.limit = UInt::from(BACKFILL_LIMIT);
    match room.messages(opts).await {
        Ok(resp) => {
            let mut events = resp.chunk;
            events.reverse();
            ingest(peer, agent, events).await;
        }
        Err(e) => tracing::warn!("backfill of {} failed: {e:#}", room.room_id()),
    }
}

async fn adopt_dm(peer: &Peer, agent: &AgentClient, room_id: OwnedRoomId) {
    {
        let mut st = peer.st();
        if st.dm_room.as_ref() == Some(&room_id) {
            return;
        }
        if let Some(old) = &st.dm_room {
            tracing::info!(%old, new = %room_id, "the peer invited again; switching the DM");
        } else {
            tracing::info!(%room_id, "DM with the peer");
        }
        st.dm_room = Some(room_id.clone());
        st.dm_marked = false;
    }
    if let Some(room) = agent.client().get_room(&room_id) {
        backfill(peer, agent, &room).await;
    }
}

/// Join the peer's invites (decline anyone else's), resolve the DM, record it
/// in `m.direct` once its membership is known, refresh `joined`.
async fn tend_dm(peer: &Peer, agent: &AgentClient) {
    for invite in agent.pending_invites().await {
        match invite.inviter.as_deref() {
            Some(inviter) if peer.is_peer(inviter) => {
                match agent.join_invited_room(&invite.room_id).await {
                    Ok(()) => {
                        tracing::info!(room_id = %invite.room_id, "joined the peer's invite");
                        if let Ok(room_id) = OwnedRoomId::try_from(invite.room_id.as_str()) {
                            adopt_dm(peer, agent, room_id).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("joining the peer's invite {} failed: {e:#}", invite.room_id)
                    }
                }
            }
            Some(inviter) => match agent.decline_invite(&invite.room_id).await {
                Ok(()) => {
                    tracing::info!(%inviter, room_id = %invite.room_id, "declined an invite from someone other than the peer")
                }
                Err(e) => tracing::warn!("declining {} failed: {e:#}", invite.room_id),
            },
            None => {}
        }
    }
    if peer.st().dm_room.is_none() {
        if let Ok(Some(room_id)) = agent.dm_room_id(peer.peer.as_str()).await {
            if let Ok(room_id) = OwnedRoomId::try_from(room_id.as_str()) {
                adopt_dm(peer, agent, room_id).await;
            }
        }
    }
    let (dm, marked) = {
        let st = peer.st();
        (st.dm_room.clone(), st.dm_marked)
    };
    let Some(dm) = dm else {
        peer.st().joined = false;
        return;
    };
    let joined = agent
        .client()
        .get_room(&dm)
        .is_some_and(|r| r.state() == RoomState::Joined);
    if joined && !marked {
        match agent.mark_dm(dm.as_str(), peer.peer.as_str()).await {
            Ok(()) => peer.st().dm_marked = true,
            Err(e) => tracing::debug!("m.direct not recorded yet: {e:#}"),
        }
    }
    peer.st().joined = joined;
}

/// Fetch undecryptable events again; the ones whose key has arrived are
/// listed again, decrypted.
async fn retry_utds(peer: &Peer, agent: &AgentClient) {
    let (room_id, pending) = {
        let st = peer.st();
        (
            st.dm_room.clone(),
            st.log.undecrypted(now_ms(), UTD_RETRY_WINDOW_MS),
        )
    };
    let (Some(room_id), false) = (room_id, pending.is_empty()) else {
        return;
    };
    let Some(room) = agent.client().get_room(&room_id) else {
        return;
    };
    let mut fresh = Vec::new();
    for id in pending {
        let Ok(event_id) = OwnedEventId::try_from(id.as_str()) else {
            continue;
        };
        if let Ok(Ok(ev)) =
            tokio::time::timeout(Duration::from_secs(10), room.event(&event_id, None)).await
        {
            if !ev.kind.is_utd() {
                fresh.push(ev);
            }
        }
    }
    ingest(peer, agent, fresh).await;
}

async fn ingest_sync(peer: &Peer, agent: &AgentClient, mut resp: SyncResponse) {
    let Some(room_id) = peer.st().dm_room.clone() else {
        return;
    };
    let Some(update) = resp.rooms.joined.remove(&room_id) else {
        return;
    };
    if update.timeline.limited {
        // A gap: pick up what the sync skipped (duplicates are dropped).
        if let Some(room) = agent.client().get_room(&room_id) {
            backfill(peer, agent, &room).await;
        }
    }
    ingest(peer, agent, update.timeline.events).await;
}

fn restrict_key_file(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => tracing::info!("restricted {} to 0600", path.display()),
                Err(e) => tracing::warn!("could not chmod 600 {}: {e}", path.display()),
            }
        }
    }
}

async fn connect_until_ok(config: &AgentConfig) -> AgentClient {
    let mut backoff = Duration::from_secs(5);
    loop {
        let attempt = AgentClient::connect(config.clone()).await;
        // A first run generates the key with the default umask, even when the
        // login after it fails.
        restrict_key_file(&config.key_file);
        match attempt {
            Ok(agent) => return agent,
            Err(e) => {
                tracing::warn!("connect failed: {e:#}; retrying in {}s", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(120));
            }
        }
    }
}

async fn adopt_client(peer: &Peer, agent: &AgentClient, display_name: &str) {
    if let Err(e) = agent.set_display_name(display_name).await {
        tracing::warn!("setting the display name failed: {e:#}");
    }
    {
        let mut st = peer.st();
        st.own = OwnedUserId::try_from(agent.user_id()).ok();
        st.device = agent.device_id();
    }
    *peer.agent.write().await = Some(agent.clone());
    tracing::info!(mxid = agent.user_id(), device = ?agent.device_id(), did = agent.did(), "connected");
}

/// New access token, new inner client for the same device and store. Sends
/// wait on the write lock, so no send runs on a client being replaced.
async fn rotate(peer: &Peer, agent: &AgentClient) -> Result<AgentClient> {
    let mut guard = peer.agent.write().await;
    let mut next = agent.clone();
    next.reauth_token_only().await?;
    *guard = Some(next.clone());
    Ok(next)
}

fn set_synced(peer: &Peer, synced: bool) {
    peer.st().synced = synced;
}

async fn drive(peer: Arc<Peer>, config: AgentConfig, display_name: String) {
    let mut agent = connect_until_ok(&config).await;
    adopt_client(&peer, &agent, &display_name).await;
    let mut backoff = MIN_BACKOFF;
    let mut force_rotate = false;
    let mut last_utd_retry: Option<Instant> = None;
    loop {
        if force_rotate || secs_left(&agent) < TOKEN_MARGIN_SECS {
            match rotate(&peer, &agent).await {
                Ok(next) => {
                    agent = next;
                    force_rotate = false;
                    tracing::debug!("access token rotated");
                }
                Err(e) => {
                    tracing::warn!("token rotation failed: {e:#}");
                    if force_rotate || secs_left(&agent) == 0 {
                        tracing::warn!("reconnecting from the key");
                        set_synced(&peer, false);
                        agent = connect_until_ok(&config).await;
                        adopt_client(&peer, &agent, &display_name).await;
                        force_rotate = false;
                    } else {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }
            }
        }
        tend_dm(&peer, &agent).await;
        if last_utd_retry.is_none_or(|t| t.elapsed() >= UTD_RETRY_EVERY) {
            retry_utds(&peer, &agent).await;
            last_utd_retry = Some(Instant::now());
        }
        reload_olm_if_store_changed(agent.client()).await;
        let settings = SyncSettings::default().timeout(SYNC_TIMEOUT);
        match tokio::time::timeout(SYNC_GUARD, agent.client().sync_once(settings)).await {
            Ok(Ok(resp)) => {
                set_synced(&peer, true);
                backoff = MIN_BACKOFF;
                ingest_sync(&peer, &agent, resp).await;
            }
            Ok(Err(e)) => {
                set_synced(&peer, false);
                let e = anyhow::Error::from(e);
                tracing::warn!("sync failed: {e:#}");
                force_rotate = is_unknown_token(&e);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(_) => {
                set_synced(&peer, false);
                tracing::warn!("sync did not return within {}s", SYNC_GUARD.as_secs());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("reading /dev/urandom")?;
    Ok(hex::encode(buf))
}

/// Create `dir` (0700) when missing; with `tighten`, also drop group/other
/// access from an existing one.
fn private_dir(dir: &Path, tighten: bool) -> Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    } else if tighten {
        let mode = std::fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 700 {}", dir.display()))?;
        }
    }
    Ok(())
}

/// The bearer token from `path`, or a new random one written there (0600).
fn load_or_create_token(path: &Path) -> Result<String> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if meta.permissions().mode() & 0o077 != 0 {
                bail!(
                    "{} is readable by group or others; chmod 600 it",
                    path.display()
                );
            }
            let token = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?
                .trim()
                .to_string();
            if token.len() < 16 || token.chars().any(char::is_whitespace) {
                bail!(
                    "{} must hold one token of at least 16 characters",
                    path.display()
                );
            }
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                private_dir(parent, false)?;
            }
            let token = random_hex(32)?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .and_then(|mut f| f.write_all(format!("{token}\n").as_bytes()))
                .with_context(|| format!("creating {}", path.display()))?;
            tracing::info!("created the API token file {}", path.display());
            Ok(token)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let filter = if args.debug {
        "warn,paige_test_peer=debug,aqua_matrix_agent=info"
    } else {
        "warn,paige_test_peer=info,aqua_matrix_agent=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .init();

    let peer_id = OwnedUserId::try_from(args.peer.as_str())
        .map_err(|e| anyhow::anyhow!("--peer {:?} is not a Matrix user id: {e}", args.peer))?;
    let token = load_or_create_token(&args.token_file)?;
    if let Some(parent) = args.key_file.parent().filter(|p| !p.as_os_str().is_empty()) {
        private_dir(parent, false)?;
    }
    private_dir(&args.store_dir, true)?;
    let store_dir = std::fs::canonicalize(&args.store_dir)?;
    let received_dir = store_dir
        .parent()
        .context("--store-dir has no parent directory for received/")?
        .join("received");
    private_dir(&received_dir, true)?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, args.port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", args.port))?;
    let peer = Arc::new(Peer::new(
        peer_id,
        token,
        received_dir.clone(),
        random_hex(4)?,
    ));
    tracing::info!(
        "control API on http://127.0.0.1:{} (bearer token in {}); peer {}; media to {}",
        args.port,
        args.token_file.display(),
        peer.peer,
        received_dir.display()
    );
    tokio::spawn(serve(listener, peer.clone()));

    let config = AgentConfig {
        key_file: args.key_file.clone(),
        siwx_url: args.siwx_url.clone(),
        matrix_url: args.matrix_url.clone(),
        client_id: None,
        redirect_uri: None,
        store_dir,
        device_id: None,
        device_role: Default::default(),
    };
    tokio::select! {
        _ = drive(peer, config, args.display_name.clone()) => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted; exiting"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (no Matrix, no siwx: the HTTP layer, validation, serialization,
// content builders)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::OwnedMxcUri;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn req(method: &str, target: &str, body: &str, auth: Option<&str>) -> HttpRequest {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut headers = vec![("Host".to_string(), "127.0.0.1".to_string())];
        if let Some(a) = auth {
            headers.push(("Authorization".into(), a.into()));
        }
        HttpRequest {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            headers,
            body: body.as_bytes().to_vec(),
        }
    }

    fn api(method: &str, target: &str, body: &str) -> Result<Api, Resp> {
        parse_api(&req(method, target, body, None))
    }

    fn status_of(r: Result<Api, Resp>) -> u16 {
        r.err().map(|r| r.status).unwrap_or(200)
    }

    fn plain(uri: &str) -> MediaSource {
        MediaSource::Plain(OwnedMxcUri::from(uri))
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    fn ogg_page(granule: i64, serial: u32, seq: u32, body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 255);
        let mut p = b"OggS".to_vec();
        p.push(0);
        p.push(0);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&serial.to_le_bytes());
        p.extend_from_slice(&seq.to_le_bytes());
        p.extend_from_slice(&[0, 0, 0, 0]);
        p.push(1);
        p.push(body.len() as u8);
        p.extend_from_slice(body);
        p
    }

    fn ogg_opus(pre_skip: u16, final_granule: i64) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(1);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut v = ogg_page(0, 7, 0, &head);
        v.extend(ogg_page(0, 7, 1, b"OpusTags\0\0\0\0\0\0\0\0"));
        v.extend(ogg_page(final_granule / 2, 7, 2, &[0xFC; 40]));
        v.extend(ogg_page(final_granule, 7, 3, &[0xFC; 40]));
        v
    }

    // -- HTTP parsing ---------------------------------------------------------

    #[test]
    fn head_parses_method_path_query_headers() {
        let (m, p, q, h) = parse_head(
            b"GET /events?after=ab-3&wait=5 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t",
        )
        .unwrap();
        assert_eq!(
            (m.as_str(), p.as_str(), q.as_str()),
            ("GET", "/events", "after=ab-3&wait=5")
        );
        assert_eq!(h[1], ("Authorization".to_string(), "Bearer t".to_string()));
    }

    #[test]
    fn malformed_heads_are_400() {
        for head in [
            &b"GET /status"[..],
            b"GET /status HTTP/2",
            b"get /status HTTP/1.1",
            b"GET http://x/status HTTP/1.1",
            b"GET /status HTTP/1.1\r\nno-colon-here",
            b"GET /status HTTP/1.1\r\nBad Name: v",
            b"GET /status HTTP/1.1 extra",
        ] {
            assert_eq!(
                parse_head(head).unwrap_err().status,
                400,
                "{}",
                String::from_utf8_lossy(head)
            );
        }
    }

    #[tokio::test]
    async fn read_request_reads_a_content_length_body_and_drops_the_excess() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        client
            .write_all(
                b"POST /send HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"text\":\"hi\"}trailing",
            )
            .await
            .unwrap();
        let r = read_request(&mut server).await.unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/send"));
        assert_eq!(r.body, br#"{"text":"hi"}"#);
    }

    #[tokio::test]
    async fn read_request_answers_expect_100_continue() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        client
            .write_all(b"POST /send HTTP/1.1\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n")
            .await
            .unwrap();
        let reader = tokio::spawn(async move { read_request(&mut server).await.map(|r| r.body) });
        let mut interim = [0u8; 25];
        client.read_exact(&mut interim).await.unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        client.write_all(b"{}").await.unwrap();
        assert_eq!(reader.await.unwrap().unwrap(), b"{}");
    }

    #[tokio::test]
    async fn read_request_refuses_big_bodies_chunking_and_huge_heads() {
        let big = format!(
            "POST /send HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let chunked = "POST /send HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_string();
        let huge = format!(
            "GET /status HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES + 10)
        );
        for (raw, want) in [(big, 413), (chunked, 400), (huge, 431)] {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let writer = tokio::spawn(async move {
                let _ = client.write_all(raw.as_bytes()).await;
                client
            });
            assert_eq!(read_request(&mut server).await.unwrap_err().status, want);
            drop(writer.await);
        }
    }

    #[test]
    fn auth_needs_the_exact_bearer_token() {
        let check = |a: Option<&str>| authorized(&req("GET", "/status", "", a), TOKEN);
        assert!(check(Some(&format!("Bearer {TOKEN}"))));
        assert!(check(Some(&format!("bearer {TOKEN}"))));
        assert!(!check(None));
        assert!(!check(Some(TOKEN)));
        assert!(!check(Some(&format!("Basic {TOKEN}"))));
        assert!(!check(Some(&format!("Bearer {TOKEN}x"))));
        assert!(!check(Some("Bearer 0123456789abcdef0123456789abcdeX")));
        assert!(!check(Some("Bearer ")));
    }

    #[test]
    fn render_writes_status_length_and_json() {
        let out = String::from_utf8(render(&Resp::err(401, "no"))).unwrap();
        assert!(out.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(out.contains("WWW-Authenticate: Bearer\r\n"));
        assert!(out.contains("Content-Length: 15\r\n"));
        assert!(out.ends_with("\r\n\r\n{\"error\":\"no\"}\n"));
    }

    // -- API validation -------------------------------------------------------

    #[test]
    fn routes_and_methods() {
        assert_eq!(api("GET", "/status", ""), Ok(Api::Status));
        assert_eq!(status_of(api("GET", "/nope", "")), 404);
        assert_eq!(status_of(api("POST", "/status", "")), 405);
        assert_eq!(status_of(api("GET", "/send", "")), 405);
        assert_eq!(status_of(api("GET", "/edit", "")), 405);
        assert_eq!(status_of(api("POST", "/events", "")), 405);
    }

    #[test]
    fn send_accepts_each_kind_with_its_options() {
        assert_eq!(
            api("POST", "/send", r#"{"text":"hello","reply_to":"$abc:x"}"#),
            Ok(Api::Send(SendReq {
                kind: SendKind::Text("hello".into()),
                reply_to: Some(OwnedEventId::try_from("$abc:x").unwrap()),
            }))
        );
        assert_eq!(
            api(
                "POST",
                "/send",
                r#"{"voice":"/a/n.ogg","duration_ms":3200}"#
            ),
            Ok(Api::Send(SendReq {
                kind: SendKind::Voice {
                    path: "/a/n.ogg".into(),
                    duration_ms: Some(3200)
                },
                reply_to: None,
            }))
        );
        assert_eq!(
            api("POST", "/send", r#"{"image":"/a/p.png","reply_to":null}"#),
            Ok(Api::Send(SendReq {
                kind: SendKind::Image {
                    path: "/a/p.png".into()
                },
                reply_to: None
            }))
        );
        assert_eq!(
            api(
                "POST",
                "/send",
                r#"{"file":"/a/p.png","mimetype":"image/png"}"#
            ),
            Ok(Api::Send(SendReq {
                kind: SendKind::File {
                    path: "/a/p.png".into(),
                    mimetype: Some("image/png".into())
                },
                reply_to: None,
            }))
        );
    }

    #[test]
    fn send_rejects_bad_requests() {
        for body in [
            "",
            "[]",
            "not json",
            "{}",
            r#"{"text":"a","voice":"/a.ogg"}"#,
            r#"{"text":""}"#,
            r#"{"text":"   "}"#,
            r#"{"text":5}"#,
            r#"{"text":"a","extra":1}"#,
            r#"{"text":"a","mimetype":"text/plain"}"#,
            r#"{"image":"/a.png","duration_ms":5}"#,
            r#"{"voice":"rel/n.ogg"}"#,
            r#"{"voice":"/n.ogg","duration_ms":0}"#,
            r#"{"voice":"/n.ogg","duration_ms":"5"}"#,
            r#"{"file":"/x","mimetype":"not a mime"}"#,
            r#"{"text":"a","reply_to":"abc"}"#,
            r#"{"text":"a","reply_to":7}"#,
        ] {
            assert_eq!(status_of(api("POST", "/send", body)), 400, "{body}");
        }
        let long = json!({ "text": "x".repeat(MAX_TEXT_BYTES + 1) }).to_string();
        assert_eq!(status_of(api("POST", "/send", &long)), 413);
    }

    #[test]
    fn edit_validation() {
        assert_eq!(
            api("POST", "/edit", r#"{"event_id":"$e:x","text":"new"}"#),
            Ok(Api::Edit(EditReq {
                event_id: OwnedEventId::try_from("$e:x").unwrap(),
                text: "new".into()
            }))
        );
        for body in [
            r#"{"event_id":"$e:x"}"#,
            r#"{"text":"new"}"#,
            r#"{"event_id":"e","text":"new"}"#,
            r#"{"event_id":"$e:x","text":""}"#,
            r#"{"event_id":"$e:x","text":"a","reply_to":"$q"}"#,
        ] {
            assert_eq!(status_of(api("POST", "/edit", body)), 400, "{body}");
        }
    }

    #[test]
    fn events_query_validation() {
        assert_eq!(
            api("GET", "/events", ""),
            Ok(Api::Events {
                after: None,
                wait: Duration::ZERO
            })
        );
        assert_eq!(
            api("GET", "/events?after=0a1b-42&wait=30", ""),
            Ok(Api::Events {
                after: Some(Cursor {
                    boot: "0a1b".into(),
                    seq: 42
                }),
                wait: Duration::from_secs(30)
            })
        );
        assert_eq!(
            api("GET", "/events?after=0A1B%2D42", ""),
            Ok(Api::Events {
                after: Some(Cursor {
                    boot: "0a1b".into(),
                    seq: 42
                }),
                wait: Duration::ZERO
            })
        );
        for q in [
            "after=xyz",
            "after=ab-",
            "after=-4",
            "after=ab-4x",
            "wait=-1",
            "wait=1.5",
            "wait=301",
            "limit=4",
            "after=%zz",
        ] {
            assert_eq!(
                status_of(api("GET", &format!("/events?{q}"), "")),
                400,
                "{q}"
            );
        }
    }

    // -- Event log and serialization -----------------------------------------

    fn rec(id: &str, sender: &str, decrypted: bool) -> EventRecord {
        let mut r = EventRecord::undecryptable(id.into(), sender.into(), now_ms());
        if decrypted {
            r.decrypted = true;
            r.msgtype = Some("m.text".into());
            r.body = Some(format!("body of {id}"));
        }
        r
    }

    #[test]
    fn log_cursors_dedup_and_late_decryption() {
        let mut log = EventLog::new("beef".into());
        assert_eq!(log.push(rec("$a", "@p:x", true)), Some(1));
        assert_eq!(log.push(rec("$a", "@p:x", true)), None);
        assert_eq!(log.push(rec("$b", "@p:x", false)), Some(2));
        assert_eq!(
            log.undecrypted(now_ms(), UTD_RETRY_WINDOW_MS),
            vec!["$b".to_string()]
        );
        assert_eq!(log.push(rec("$b", "@p:x", false)), None);

        let (all, cursor) = log.since(None, 100);
        assert_eq!(
            all.iter().map(|e| e.event_id.as_str()).collect::<Vec<_>>(),
            ["$a", "$b"]
        );
        assert_eq!(cursor.render(), "beef-2");
        let (none, same) = log.since(Some(&cursor), 100);
        assert!(none.is_empty());
        assert_eq!(same, cursor);

        // The key arrives: the decrypted form is listed again, after the cursor.
        assert_eq!(log.push(rec("$b", "@p:x", true)), Some(3));
        assert!(log.undecrypted(now_ms(), UTD_RETRY_WINDOW_MS).is_empty());
        let (late, next) = log.since(Some(&cursor), 100);
        assert_eq!(late.len(), 1);
        assert!(late[0].decrypted && late[0].event_id == "$b");
        assert_eq!(next.render(), "beef-3");

        // A cursor of an earlier run reads everything; a future one is clamped.
        let (again, _) = log.since(
            Some(&Cursor {
                boot: "0001".into(),
                seq: 99,
            }),
            100,
        );
        assert_eq!(again.len(), 3);
        let (_, clamped) = log.since(
            Some(&Cursor {
                boot: "beef".into(),
                seq: 99,
            }),
            100,
        );
        assert_eq!(clamped.seq, 3);
        let (two, at) = log.since(None, 2);
        assert_eq!((two.len(), at.seq), (2, 2));
        assert_eq!(log.sender_of("$a"), Some("@p:x"));
    }

    fn event(content: Value) -> AnySyncTimelineEvent {
        serde_json::from_value(json!({
            "type": "m.room.message",
            "event_id": "$ev:x",
            "sender": "@paige:x",
            "origin_server_ts": 1_700_000_000_000u64,
            "content": content,
        }))
        .unwrap()
    }

    #[test]
    fn records_from_text_reply_edit_voice_and_image_events() {
        let (r, src) = record_from_event(&event(json!({
            "msgtype": "m.text", "body": "hi",
            "m.relates_to": {"m.in_reply_to": {"event_id": "$q:x"}}
        })))
        .unwrap();
        assert_eq!(
            (
                r.msgtype.as_deref(),
                r.body.as_deref(),
                r.in_reply_to.as_deref()
            ),
            (Some("m.text"), Some("hi"), Some("$q:x"))
        );
        assert!(!r.is_edit && r.media.is_none() && src.is_none());
        assert_eq!(r.origin_server_ts, 1_700_000_000_000);

        let (r, _) = record_from_event(&event(json!({
            "msgtype": "m.text", "body": "* final",
            "m.new_content": {"msgtype": "m.notice", "body": "final"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$orig:x"}
        })))
        .unwrap();
        assert!(r.is_edit);
        assert_eq!(
            (
                r.replaces.as_deref(),
                r.body.as_deref(),
                r.msgtype.as_deref()
            ),
            (Some("$orig:x"), Some("final"), Some("m.notice"))
        );
        assert_eq!(r.in_reply_to, None);

        let (r, src) = record_from_event(&event(json!({
            "msgtype": "m.audio", "body": "Voice message.ogg", "url": "mxc://x/v",
            "info": {"mimetype": "audio/ogg", "size": 999, "duration": 2500},
            "org.matrix.msc1767.audio": {"duration": 2400, "waveform": [1, 2]},
            "org.matrix.msc3245.voice": {}
        })))
        .unwrap();
        let m = r.media.unwrap();
        assert_eq!(
            (m.kind, m.duration_ms, m.size, m.mimetype.as_deref()),
            ("voice", Some(2400), Some(999), Some("audio/ogg"))
        );
        assert_eq!(
            serde_json::to_value(src.unwrap()).unwrap(),
            json!({"url": "mxc://x/v"})
        );

        let (r, _) = record_from_event(&event(json!({
            "msgtype": "m.image", "body": "p.png", "url": "mxc://x/i",
            "info": {"mimetype": "image/png", "size": 10, "w": 4, "h": 3}
        })))
        .unwrap();
        let m = r.media.unwrap();
        assert_eq!((m.kind, m.width, m.height), ("image", Some(4), Some(3)));

        let state: AnySyncTimelineEvent = serde_json::from_value(json!({
            "type": "m.room.topic", "event_id": "$t:x", "sender": "@a:x",
            "origin_server_ts": 1, "state_key": "", "content": {"topic": "t"}
        }))
        .unwrap();
        assert!(record_from_event(&state).is_none());
    }

    #[test]
    fn event_record_serializes_to_the_documented_shape() {
        let mut r = rec("$a:x", "@paige:x", true);
        r.from_peer = true;
        r.origin_server_ts = 5;
        r.media = Some(MediaRecord {
            kind: "voice",
            filename: "v.ogg".into(),
            mimetype: Some("audio/ogg".into()),
            size: Some(9),
            duration_ms: Some(1000),
            width: None,
            height: None,
            path: Some("/r/a_x-v.ogg".into()),
            error: None,
        });
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "event_id": "$a:x", "sender": "@paige:x", "from_peer": true, "origin_server_ts": 5,
                "msgtype": "m.text", "body": "body of $a:x", "in_reply_to": null, "is_edit": false,
                "replaces": null, "decrypted": true,
                "media": {"kind": "voice", "filename": "v.ogg", "mimetype": "audio/ogg", "size": 9,
                          "duration_ms": 1000, "width": null, "height": null,
                          "path": "/r/a_x-v.ogg", "error": null}
            })
        );
        let utd = serde_json::to_value(EventRecord::undecryptable("$u".into(), "@p:x".into(), 1))
            .unwrap();
        assert_eq!(utd["msgtype"], "m.room.encrypted");
        assert_eq!(utd["body"], Value::Null);
        assert_eq!(utd["decrypted"], false);
    }

    // -- Content builders -----------------------------------------------------

    #[test]
    fn ogg_opus_duration_from_granule_and_pre_skip() {
        assert_eq!(
            ogg_opus_duration_ms(&ogg_opus(312, 48_000 * 3 + 312)).unwrap(),
            3000
        );
        assert_eq!(ogg_opus_duration_ms(&ogg_opus(0, 24_000)).unwrap(), 500);
        assert!(ogg_opus_duration_ms(b"RIFF....WAVE").is_err());
        let mut truncated = ogg_opus(0, 48_000);
        truncated.truncate(truncated.len() - 5);
        assert!(ogg_opus_duration_ms(&truncated).is_err());
        let vorbis = ogg_page(0, 1, 0, b"\x01vorbis\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        assert!(ogg_opus_duration_ms(&vorbis).is_err());
        assert!(
            ogg_opus_duration_ms(&ogg_opus(0, 0)).is_err(),
            "headers only"
        );
    }

    #[test]
    fn plan_media_reads_type_size_and_duration_from_the_file() {
        let voice = SendKind::Voice {
            path: "/v/note.ogg".into(),
            duration_ms: None,
        };
        let plan = plan_media(&voice, &ogg_opus(312, 48_000 * 2 + 312)).unwrap();
        assert_eq!(
            plan,
            MediaPlan {
                filename: "note.ogg".into(),
                mime: "audio/ogg".parse().unwrap(),
                msg: MediaMsg::Voice { duration_ms: 2000 }
            }
        );
        assert_eq!(plan_media(&voice, b"not ogg").unwrap_err().status, 400);
        let given = SendKind::Voice {
            path: "/v/note.mp3".into(),
            duration_ms: Some(1500),
        };
        let plan = plan_media(&given, b"ID3...").unwrap();
        assert_eq!(
            (plan.mime.essence_str(), &plan.msg),
            ("audio/mpeg", &MediaMsg::Voice { duration_ms: 1500 })
        );

        // Sniffed, not trusted from the extension.
        let image = SendKind::Image {
            path: "/i/photo.jpg".into(),
        };
        let plan = plan_media(&image, &png(640, 480)).unwrap();
        assert_eq!(
            (plan.mime.essence_str(), &plan.msg),
            (
                "image/png",
                &MediaMsg::Image {
                    width: Some(640),
                    height: Some(480)
                }
            )
        );
        let not_image = SendKind::Image {
            path: "/i/notes.txt".into(),
        };
        assert_eq!(plan_media(&not_image, b"hello").unwrap_err().status, 400);

        let file = SendKind::File {
            path: "/f/report.pdf".into(),
            mimetype: None,
        };
        assert_eq!(
            plan_media(&file, b"%PDF").unwrap().mime.essence_str(),
            "application/pdf"
        );
        let declared = SendKind::File {
            path: "/f/blob".into(),
            mimetype: Some("image/webp".into()),
        };
        assert_eq!(
            plan_media(&declared, b"x").unwrap().mime.essence_str(),
            "image/webp"
        );
    }

    #[test]
    fn voice_content_is_an_element_voice_message() {
        let mime: Mime = "audio/ogg".parse().unwrap();
        let mut content = voice_content(plain("mxc://x/v"), "note.ogg", &mime, 1234, 3200);
        apply_reply(
            &mut content,
            &OwnedEventId::try_from("$q:x").unwrap(),
            Some(OwnedUserId::try_from("@paige:x").unwrap()),
        );
        let v = serde_json::to_value(&content).unwrap();
        assert_eq!(v["msgtype"], "m.audio");
        assert_eq!(v["body"], "note.ogg");
        assert_eq!(v["url"], "mxc://x/v");
        assert_eq!(
            v["info"],
            json!({"duration": 3200, "mimetype": "audio/ogg", "size": 1234})
        );
        assert_eq!(v["org.matrix.msc3245.voice"], json!({}));
        assert_eq!(v["org.matrix.msc1767.audio"]["duration"], 3200);
        let bars = v["org.matrix.msc1767.audio"]["waveform"]
            .as_array()
            .unwrap();
        assert!((24..=120).contains(&bars.len()));
        assert!(bars.iter().all(|b| b.as_u64().is_some_and(|n| n <= 1024)));
        assert_eq!(
            v["m.relates_to"],
            json!({"m.in_reply_to": {"event_id": "$q:x"}})
        );
        assert_eq!(v["m.mentions"], json!({"user_ids": ["@paige:x"]}));

        // What the runtime's relay reads back: a voice note with its duration and no caption.
        let back: RoomMessageEventContent = serde_json::from_value(v).unwrap();
        let MessageType::Audio(audio) = back.msgtype else {
            panic!("not m.audio")
        };
        assert!(audio.voice.is_some());
        assert_eq!(
            audio.audio.as_ref().map(|a| a.duration),
            Some(Duration::from_millis(3200))
        );
        assert_eq!(audio.caption(), None);
    }

    #[test]
    fn image_and_file_content_carry_mimetype_size_and_dimensions() {
        let png_mime: Mime = "image/png".parse().unwrap();
        let v = serde_json::to_value(image_content(
            plain("mxc://x/i"),
            "p.png",
            &png_mime,
            2048,
            Some(640),
            Some(480),
        ))
        .unwrap();
        assert_eq!(v["msgtype"], "m.image");
        assert_eq!(
            v["info"],
            json!({"mimetype": "image/png", "size": 2048, "w": 640, "h": 480})
        );
        assert!(v.get("m.relates_to").is_none());

        let v =
            serde_json::to_value(file_content(plain("mxc://x/f"), "p.png", &png_mime, 7)).unwrap();
        assert_eq!(v["msgtype"], "m.file");
        assert_eq!(v["info"], json!({"mimetype": "image/png", "size": 7}));

        let plan = MediaPlan {
            filename: "a.ogg".into(),
            mime: "audio/ogg".parse().unwrap(),
            msg: MediaMsg::Voice { duration_ms: 10 },
        };
        assert_eq!(
            serde_json::to_value(media_content(&plan, plain("mxc://x/a"), 1)).unwrap()["msgtype"],
            "m.audio"
        );
    }

    #[test]
    fn text_reply_and_edit_content() {
        let mut content = RoomMessageEventContent::text_markdown("**hi**");
        apply_reply(&mut content, &OwnedEventId::try_from("$q:x").unwrap(), None);
        let v = serde_json::to_value(&content).unwrap();
        assert_eq!(v["body"], "**hi**");
        assert!(v["formatted_body"]
            .as_str()
            .is_some_and(|f| f.contains("<strong>hi</strong>")));
        assert_eq!(
            v["m.relates_to"],
            json!({"m.in_reply_to": {"event_id": "$q:x"}})
        );
        assert!(v.get("m.mentions").is_none());

        let v = serde_json::to_value(edit_content(
            &OwnedEventId::try_from("$o:x").unwrap(),
            "fixed",
        ))
        .unwrap();
        assert_eq!(
            v["m.relates_to"],
            json!({"rel_type": "m.replace", "event_id": "$o:x"})
        );
        assert_eq!(v["m.new_content"]["body"], "fixed");
        assert_eq!(v["body"], "* fixed");
    }

    #[test]
    fn received_names_cannot_escape_the_directory() {
        assert_eq!(
            received_name("$abc:x.y", "../../etc/passwd"),
            "abc_x.y-passwd"
        );
        assert_eq!(received_name("$e", ".hidden"), "e-hidden");
        assert_eq!(received_name("$e", "a b/ü.png"), "e-_.png");
        assert_eq!(received_name("$e", ""), "e-file");
    }

    // -- Startup files --------------------------------------------------------

    fn scratch(name: &str) -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = home
            .join(".cache/paige-test-peer")
            .join(format!("test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn token_file_is_created_0600_and_reused() {
        let dir = scratch("token");
        let path = dir.join("sub/api.token");
        let first = load_or_create_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(load_or_create_token(&path).unwrap(), first);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_create_token(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "short\n").unwrap();
        assert!(load_or_create_token(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn args_require_identity_peer_port_and_token_file() {
        let base = [
            "paige_test_peer",
            "--key-file",
            "/k.pem",
            "--store-dir",
            "/s",
            "--siwx-url",
            "https://siwx",
            "--matrix-url",
            "https://m",
            "--peer",
            "@p:x",
            "--port",
            "8795",
            "--token-file",
            "/t",
        ];
        let args = Args::try_parse_from(base).unwrap();
        assert_eq!(
            (args.display_name.as_str(), args.port, args.debug),
            ("Paige E2E (test user)", 8795, false)
        );
        assert!(Args::try_parse_from(base[..base.len() - 2].iter().copied()).is_err());
    }

    // -- The server end to end (loopback only, no Matrix client) --------------

    async fn call(port: u16, raw: String) -> (u16, Value) {
        let mut s = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        let status = out[9..12].parse().unwrap();
        let body = out.split_once("\r\n\r\n").unwrap().1;
        (status, serde_json::from_str(body).unwrap())
    }

    fn get(path: &str, token: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\r\n")
    }

    fn post(path: &str, token: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn server_auth_status_send_and_long_poll() {
        let peer = Arc::new(Peer::new(
            OwnedUserId::try_from("@paige:x").unwrap(),
            TOKEN.into(),
            scratch("srv"),
            "c0de".into(),
        ));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve(listener, peer.clone()));

        let (code, body) = call(port, get("/status", "wrong-token-wrong-token")).await;
        assert_eq!((code, body["error"].is_string()), (401, true));
        let (code, _) = call(port, "GET /nope HTTP/1.1\r\n\r\n".into()).await;
        assert_eq!(code, 401, "auth comes before routing");

        let (code, body) = call(port, get("/status", TOKEN)).await;
        assert_eq!(code, 200);
        assert_eq!(
            body,
            json!({"mxid": null, "device": null, "dm_room_id": null, "joined": false, "synced": false, "peer": "@paige:x"})
        );

        let (code, body) = call(port, post("/send", TOKEN, r#"{"text":"hi"}"#)).await;
        assert_eq!(
            (code, body["error"].as_str()),
            (503, Some("not connected to Matrix yet"))
        );
        let (code, _) = call(
            port,
            post("/send", TOKEN, r#"{"image":"/nonexistent/x.png"}"#),
        )
        .await;
        assert_eq!(
            code, 400,
            "a bad file is refused before the connection check"
        );
        let (code, _) = call(
            port,
            post("/edit", TOKEN, r#"{"event_id":"$x:y","text":"z"}"#),
        )
        .await;
        assert_eq!(code, 404, "only this user's own messages can be edited");

        let (code, body) = call(port, get("/events", TOKEN)).await;
        assert_eq!(
            (
                code,
                body["events"].as_array().map(Vec::len),
                body["cursor"].as_str()
            ),
            (200, Some(0), Some("c0de-0"))
        );

        // A long-poll returns as soon as an event lands.
        let waiter =
            tokio::spawn(
                async move { call(port, get("/events?after=c0de-0&wait=20", TOKEN)).await },
            );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let started = Instant::now();
        let mut r = rec("$new:x", "@paige:x", true);
        r.from_peer = true;
        peer.push(r);
        let (code, body) = waiter.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(code, 200);
        assert_eq!(body["events"][0]["event_id"], "$new:x");
        assert_eq!(body["events"][0]["from_peer"], true);
        assert_eq!(body["cursor"], "c0de-1");

        // And times out empty, keeping the cursor.
        let (code, body) = call(port, get("/events?after=c0de-1&wait=1", TOKEN)).await;
        assert_eq!(
            (
                code,
                body["events"].as_array().map(Vec::len),
                body["cursor"].as_str()
            ),
            (200, Some(0), Some("c0de-1"))
        );
    }
}
