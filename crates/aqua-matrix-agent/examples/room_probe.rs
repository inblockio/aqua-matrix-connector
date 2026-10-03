//! `room_probe`: a small E2EE Matrix test client for the consultant rooms
//! canary (the consultant rooms plan in aqua-agents, Task 7). It plays the
//! humans around a consultant under test: the owner ("Tim"), an external
//! collaborator and a stranger.
//!
//! Every call is one short process: connect with the identity's did:key (the
//! first call creates the key file and, through siwx-oidc, the Matrix account),
//! catch up with non-blocking syncs, do one thing, exit. Crypto state persists
//! in `--store-dir`, so messages stay end-to-end encrypted and decryptable
//! across calls. Never run two calls on the same store at the same time.
//!
//! Output is meant for scripts: `key=value` lines, except `read` / `read-dm`,
//! which print one line per message, tab separated
//! (`ts_ms  sender  event_id  body`, newlines in the body escaped as `\n`), or
//! one JSON object per line with `--json`
//! (`{event_id, sender, ts, type, utd, encrypted, content}`, where `content`
//! is the decrypted event content including `m.mentions` and `formatted_body`,
//! and `encrypted` says the event arrived as `m.room.encrypted`).
//!
//! ```text
//! cargo build -p aqua-matrix-agent --example room_probe
//! P=target/debug/examples/room_probe; ID="--key-file owner/agent.pem --store-dir owner/store"
//! $P whoami $ID                                          # mxid=, did=, device_id=
//! $P create-room $ID --name canary --invite @collab:hs   # room_id=  (private, encrypted, not a DM)
//! $P invite $ID '!room:hs' @agent:hs                     # invited=
//! $P join $ID '!room:hs'                                 # joined=
//! $P send $ID '!room:hs' 'hello' [--mention @agent:hs]   # event_id=  (plain text)
//! $P send $ID '!room:hs' --file note.txt                 # the file is one message body
//! $P read $ID '!room:hs' [--since-ms 1759450000000] [--json] [--limit 200]
//! $P dm $ID @agent:hs 'hello'                            # room_id=, event_id=
//! $P read-dm $ID @agent:hs [--since-ms ..] [--json]
//! $P membership $ID '!room:hs' @agent:hs                 # membership=join|invite|leave|ban|knock|none
//! $P kick $ID '!room:hs' @agent:hs [--reason ..]         # kicked=  (also revokes an invite)
//! ```
//!
//! Exit codes: 0 ok, 1 error, 2 bad arguments, 3 no DM room with that user.
//! Logs go to stderr (`RUST_LOG`, default `error` without matrix-sdk's HTTP
//! client, whose first-login 404s for absent account data are noise).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use aqua_matrix_agent::{AgentClient, AgentConfig};
use clap::{Args, Parser, Subcommand};
use matrix_sdk::{
    room::MessagesOptions,
    ruma::{
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::{
            room::{encryption::RoomEncryptionEventContent, message::RoomMessageEventContent},
            InitialStateEvent, Mentions,
        },
        OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
    },
    Room, RoomState,
};

/// Matrix caps an event at 65,536 bytes; keep a file body well below it.
const MAX_BODY_BYTES: usize = 60_000;
/// Events fetched per `/messages` page.
const PAGE: u32 = 100;
/// Extra fetch rounds while undecryptable events remain (room keys can arrive
/// one sync after the event).
const UTD_RETRIES: u32 = 3;

#[derive(Parser, Debug)]
#[command(
    name = "room_probe",
    about = "E2EE Matrix test client for the consultant rooms canary"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The identity a call acts as.
#[derive(Args, Debug, Clone)]
struct Identity {
    /// Ed25519 PEM key (created, mode 600, if missing).
    #[arg(long)]
    key_file: PathBuf,
    /// Store dir: OIDC session (`config.toml`) and the matrix-sdk crypto store.
    #[arg(long)]
    store_dir: PathBuf,
    #[arg(long, default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,
    #[arg(long, default_value = "https://matrix.inblock.io")]
    matrix_url: String,
}

#[derive(Args, Debug, Clone)]
struct ReadOpts {
    /// Only messages with origin_server_ts >= this (Unix milliseconds).
    #[arg(long)]
    since_ms: Option<u64>,
    /// One JSON object per message instead of tab-separated text.
    #[arg(long)]
    json: bool,
    /// At most this many messages (the newest ones).
    #[arg(long, default_value_t = 200)]
    limit: usize,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print this identity's MXID, DID and device id.
    Whoami {
        #[command(flatten)]
        id: Identity,
    },
    /// Create a private, encrypted group room (NOT marked as a DM).
    CreateRoom {
        #[command(flatten)]
        id: Identity,
        /// Room name.
        #[arg(long)]
        name: String,
        /// Invite this user (repeatable).
        #[arg(long = "invite", value_name = "MXID", value_parser = parse_user_id)]
        invite: Vec<OwnedUserId>,
    },
    /// Invite users into a joined room.
    Invite {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_room_id)]
        room_id: OwnedRoomId,
        #[arg(required = true, value_name = "MXID", value_parser = parse_user_id)]
        mxids: Vec<OwnedUserId>,
    },
    /// Join a room (accepting a pending invite).
    Join {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_room_id)]
        room_id: OwnedRoomId,
    },
    /// Send one plain-text message into a joined room.
    Send {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_room_id)]
        room_id: OwnedRoomId,
        /// Message text.
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        text: Option<String>,
        /// Send this text file's content as the message body.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Add this user to `m.mentions.user_ids`, like a client mention pill
        /// (repeatable). The body is sent as given.
        #[arg(long = "mention", value_name = "MXID", value_parser = parse_user_id)]
        mention: Vec<OwnedUserId>,
    },
    /// Print the decrypted messages of a joined room, oldest first.
    Read {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_room_id)]
        room_id: OwnedRoomId,
        #[command(flatten)]
        opts: ReadOpts,
    },
    /// Send a plain-text DM (creates the encrypted DM room if there is none).
    Dm {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_user_id)]
        mxid: OwnedUserId,
        text: String,
    },
    /// Print the decrypted messages of the DM with a user, oldest first.
    ReadDm {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_user_id)]
        mxid: OwnedUserId,
        #[command(flatten)]
        opts: ReadOpts,
    },
    /// Remove a user from a joined room, or revoke their invite (cleanup).
    Kick {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_room_id)]
        room_id: OwnedRoomId,
        #[arg(value_parser = parse_user_id)]
        mxid: OwnedUserId,
        /// Reason shown in the membership event.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Print a user's membership in a room this identity has joined.
    Membership {
        #[command(flatten)]
        id: Identity,
        #[arg(value_parser = parse_room_id)]
        room_id: OwnedRoomId,
        #[arg(value_parser = parse_user_id)]
        mxid: OwnedUserId,
    },
}

fn parse_room_id(s: &str) -> std::result::Result<OwnedRoomId, String> {
    RoomId::parse(s).map_err(|e| format!("not a room id (!opaque:server): {e}"))
}

fn parse_user_id(s: &str) -> std::result::Result<OwnedUserId, String> {
    UserId::parse(s).map_err(|e| format!("not an MXID (@user:server): {e}"))
}

/// A message event as `read` reports it.
struct Event {
    event_id: String,
    sender: String,
    ts: u64,
    utd: bool,
    /// Arrived as `m.room.encrypted` (decrypted here, or a UTD).
    encrypted: bool,
    /// The (decrypted) event JSON.
    raw: serde_json::Value,
}

impl Event {
    fn body(&self) -> String {
        if self.utd {
            return "[unable to decrypt]".into();
        }
        self.raw
            .pointer("/content/body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn json_line(&self) -> serde_json::Value {
        serde_json::json!({
            "event_id": self.event_id,
            "sender": self.sender,
            "ts": self.ts,
            "type": self.raw.get("type").cloned().unwrap_or(serde_json::Value::Null),
            "utd": self.utd,
            "encrypted": self.encrypted,
            "content": if self.utd {
                serde_json::Value::Null
            } else {
                self.raw.get("content").cloned().unwrap_or(serde_json::Value::Null)
            },
        })
    }
}

/// One line of text: tabs become spaces, line breaks become a literal `\n`.
fn one_line(body: &str) -> String {
    body.replace('\\', "\\\\")
        .replace('\r', "")
        .replace('\n', "\\n")
        .replace('\t', " ")
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

async fn connect(id: &Identity) -> Result<AgentClient> {
    for dir in [id.key_file.parent(), Some(id.store_dir.as_path())]
        .into_iter()
        .flatten()
        .filter(|d| !d.as_os_str().is_empty())
    {
        if !dir.exists() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            restrict(dir, 0o700);
        }
    }
    let new_key = !id.key_file.exists();
    let agent = AgentClient::connect(AgentConfig {
        key_file: id.key_file.clone(),
        siwx_url: id.siwx_url.clone(),
        matrix_url: id.matrix_url.clone(),
        client_id: None,
        redirect_uri: None,
        store_dir: id.store_dir.clone(),
        // None: connect() derives the stable device id from the DID.
        device_id: None,
    })
    .await
    .context("connect failed")?;
    if new_key {
        restrict(&id.key_file, 0o600);
    }
    catch_up(&agent).await?;
    Ok(agent)
}

/// Non-blocking catch-up syncs: membership, invites and to-device room keys.
async fn catch_up(agent: &AgentClient) -> Result<()> {
    for _ in 0..2 {
        agent.sync_once_nowait().await?;
    }
    Ok(())
}

fn room(agent: &AgentClient, room_id: &RoomId) -> Result<Room> {
    agent.client().get_room(room_id).ok_or_else(|| {
        anyhow!(
            "room {room_id} unknown to {} (not invited or joined)",
            agent.user_id()
        )
    })
}

fn joined_room(agent: &AgentClient, room_id: &RoomId) -> Result<Room> {
    let r = room(agent, room_id)?;
    if r.state() != RoomState::Joined {
        bail!(
            "{} is not joined to {room_id} (state {:?})",
            agent.user_id(),
            r.state()
        );
    }
    Ok(r)
}

/// Join every pending invite sent by `inviter` (an agent may create the DM).
async fn join_invites_from(agent: &AgentClient, inviter: &UserId) -> Result<()> {
    let mut joined = false;
    for r in agent.client().invited_rooms() {
        let Ok(details) = r.invite_details().await else {
            continue;
        };
        if details
            .inviter_id
            .as_str()
            .eq_ignore_ascii_case(inviter.as_str())
        {
            r.join()
                .await
                .with_context(|| format!("join invite {}", r.room_id()))?;
            joined = true;
        }
    }
    if joined {
        catch_up(agent).await?;
    }
    Ok(())
}

/// Newest-first pagination until `since_ms` or `limit`, returned oldest first.
async fn fetch_once(r: &Room, since_ms: Option<u64>, limit: usize) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    let mut from: Option<String> = None;
    loop {
        let mut opts = MessagesOptions::backward().from(from.as_deref());
        opts.limit = UInt::from(PAGE);
        let resp = r.messages(opts).await.context("fetch messages")?;
        let mut older_than_since = false;
        for ev in &resp.chunk {
            let (Some(event_id), Some(sender), Some(ts)) =
                (ev.event_id(), ev.kind.sender(), ev.timestamp())
            else {
                continue;
            };
            let ts = u64::from(ts.0);
            if since_ms.is_some_and(|s| ts < s) {
                older_than_since = true;
                continue;
            }
            let utd = ev.kind.is_utd();
            let encrypted = utd || ev.kind.encryption_info().is_some();
            let raw: serde_json::Value =
                serde_json::from_str(ev.raw().json().get()).unwrap_or(serde_json::Value::Null);
            if !utd && raw.get("type").and_then(|t| t.as_str()) != Some("m.room.message") {
                continue;
            }
            out.push(Event {
                event_id: event_id.to_string(),
                sender: sender.to_string(),
                ts,
                utd,
                encrypted,
                raw,
            });
        }
        if older_than_since || out.len() >= limit || resp.chunk.is_empty() || resp.end.is_none() {
            break;
        }
        from = resp.end;
    }
    out.truncate(limit);
    out.reverse();
    Ok(out)
}

async fn fetch(
    agent: &AgentClient,
    r: &Room,
    since_ms: Option<u64>,
    limit: usize,
) -> Result<Vec<Event>> {
    let mut events = fetch_once(r, since_ms, limit).await?;
    for _ in 0..UTD_RETRIES {
        if !events.iter().any(|e| e.utd) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        catch_up(agent).await?;
        events = fetch_once(r, since_ms, limit).await?;
    }
    Ok(events)
}

fn print_events(events: &[Event], json: bool) {
    for e in events {
        if json {
            println!("{}", e.json_line());
        } else {
            println!(
                "{}\t{}\t{}\t{}",
                e.ts,
                e.sender,
                e.event_id,
                one_line(&e.body())
            );
        }
    }
}

fn read_body(text: Option<String>, file: Option<PathBuf>) -> Result<String> {
    let body = match (text, file) {
        (Some(t), None) => t,
        (None, Some(f)) => {
            let s = std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display()))?;
            s.strip_suffix('\n').map(str::to_string).unwrap_or(s)
        }
        _ => bail!("give the message text or --file, not both"),
    };
    if body.trim().is_empty() {
        bail!("refusing to send an empty message");
    }
    if body.len() > MAX_BODY_BYTES {
        bail!(
            "message is {} bytes, the cap is {MAX_BODY_BYTES}",
            body.len()
        );
    }
    Ok(body)
}

async fn run(cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Whoami { id } => {
            let agent = connect(&id).await?;
            println!("mxid={}", agent.user_id());
            println!("did={}", agent.did());
            println!("device_id={}", agent.device_id().unwrap_or_default());
        }
        Cmd::CreateRoom { id, name, invite } => {
            let agent = connect(&id).await?;
            let mut req = CreateRoomRequest::new();
            req.name = Some(name);
            req.preset = Some(RoomPreset::PrivateChat);
            req.is_direct = false;
            req.invite = invite;
            req.initial_state = vec![InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any()];
            let r = agent
                .client()
                .create_room(req)
                .await
                .context("create room")?;
            println!("room_id={}", r.room_id());
        }
        Cmd::Invite { id, room_id, mxids } => {
            let agent = connect(&id).await?;
            let r = joined_room(&agent, &room_id)?;
            for m in mxids {
                r.invite_user_by_id(&m)
                    .await
                    .with_context(|| format!("invite {m}"))?;
                println!("invited={m}");
            }
        }
        Cmd::Join { id, room_id } => {
            let agent = connect(&id).await?;
            match agent.client().get_room(&room_id) {
                Some(r) if r.state() == RoomState::Joined => {}
                Some(r) if r.state() == RoomState::Invited => {
                    r.join().await.context("accept invite")?;
                }
                _ => {
                    agent
                        .client()
                        .join_room_by_id(&room_id)
                        .await
                        .context("join room")?;
                }
            }
            catch_up(&agent).await?;
            println!("joined={room_id}");
        }
        Cmd::Send {
            id,
            room_id,
            text,
            file,
            mention,
        } => {
            let body = read_body(text, file)?;
            let agent = connect(&id).await?;
            joined_room(&agent, &room_id)?;
            let mut content = RoomMessageEventContent::text_plain(body);
            if !mention.is_empty() {
                content = content.add_mentions(Mentions::with_user_ids(mention));
            }
            let event_id = agent
                .send_content_to_room(room_id.as_str(), content)
                .await?;
            println!("event_id={event_id}");
        }
        Cmd::Read { id, room_id, opts } => {
            let agent = connect(&id).await?;
            let r = joined_room(&agent, &room_id)?;
            print_events(
                &fetch(&agent, &r, opts.since_ms, opts.limit).await?,
                opts.json,
            );
        }
        Cmd::Dm { id, mxid, text } => {
            let body = read_body(Some(text), None)?;
            let agent = connect(&id).await?;
            join_invites_from(&agent, &mxid).await?;
            let room_id = match agent.dm_room_id(mxid.as_str()).await? {
                Some(r) => r,
                None => agent
                    .client()
                    .create_dm(&mxid)
                    .await
                    .context("create DM")?
                    .room_id()
                    .to_string(),
            };
            let event_id = agent
                .send_content_to_room(&room_id, RoomMessageEventContent::text_plain(body))
                .await?;
            println!("room_id={room_id}");
            println!("event_id={event_id}");
        }
        Cmd::ReadDm { id, mxid, opts } => {
            let agent = connect(&id).await?;
            join_invites_from(&agent, &mxid).await?;
            let Some(room_id) = agent.dm_room_id(mxid.as_str()).await? else {
                eprintln!("no DM room with {mxid}");
                return Ok(3);
            };
            let r = joined_room(&agent, <&RoomId>::try_from(room_id.as_str())?)?;
            print_events(
                &fetch(&agent, &r, opts.since_ms, opts.limit).await?,
                opts.json,
            );
        }
        Cmd::Kick {
            id,
            room_id,
            mxid,
            reason,
        } => {
            let agent = connect(&id).await?;
            joined_room(&agent, &room_id)?
                .kick_user(&mxid, reason.as_deref())
                .await
                .with_context(|| format!("kick {mxid}"))?;
            println!("kicked={mxid}");
        }
        Cmd::Membership { id, room_id, mxid } => {
            let agent = connect(&id).await?;
            let r = room(&agent, &room_id)?;
            let state = match r.get_member(&mxid).await.context("get member")? {
                Some(m) => m.membership().as_str().to_string(),
                None => "none".to_string(),
            };
            println!("membership={state}");
        }
    }
    Ok(0)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "error,matrix_sdk::http_client=off".into()),
        )
        .try_init()
        .ok();
    let cli = Cli::parse();
    let code = match run(cli.cmd).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    const ID: [&str; 4] = ["--key-file", "k.pem", "--store-dir", "s"];
    const ROOM: &str = "!abc:matrix.inblock.io";
    const USER: &str = "@collab:matrix.inblock.io";

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        let mut v = vec!["room_probe"];
        v.extend_from_slice(args);
        v.extend_from_slice(&ID);
        Cli::try_parse_from(v)
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn identity_is_required() {
        assert!(Cli::try_parse_from(["room_probe", "whoami"]).is_err());
        assert!(Cli::try_parse_from(["room_probe", "whoami", "--key-file", "k.pem"]).is_err());
        let cli = parse(&["whoami"]).unwrap();
        let Cmd::Whoami { id } = cli.cmd else {
            panic!("not whoami")
        };
        assert_eq!(id.key_file, PathBuf::from("k.pem"));
        assert_eq!(id.store_dir, PathBuf::from("s"));
        assert_eq!(id.matrix_url, "https://matrix.inblock.io");
    }

    #[test]
    fn create_room_takes_a_name_and_repeated_invites() {
        let cli = parse(&[
            "create-room",
            "--name",
            "canary",
            "--invite",
            USER,
            "--invite",
            "@x:hs",
        ])
        .unwrap();
        let Cmd::CreateRoom { name, invite, .. } = cli.cmd else {
            panic!("not create-room")
        };
        assert_eq!(name, "canary");
        assert_eq!(invite.len(), 2);
        assert!(
            parse(&["create-room", "--invite", USER]).is_err(),
            "name is required"
        );
        assert!(
            parse(&["create-room", "--name", "n", "--invite", "collab"]).is_err(),
            "bad MXID"
        );
    }

    #[test]
    fn room_ids_and_mxids_are_validated() {
        assert!(parse(&["join", ROOM]).is_ok());
        assert!(parse(&["join", "#alias:matrix.inblock.io"]).is_err());
        assert!(
            parse(&["invite", ROOM]).is_err(),
            "invite needs at least one MXID"
        );
        assert!(parse(&["invite", ROOM, "not-an-mxid"]).is_err());
        assert!(parse(&["membership", ROOM, USER]).is_ok());
        assert!(parse(&["kick", ROOM, USER, "--reason", "cleanup"]).is_ok());
        assert!(parse(&["kick", ROOM]).is_err(), "kick needs an MXID");
    }

    #[test]
    fn send_takes_text_or_file_not_both() {
        let cli = parse(&["send", ROOM, "hello"]).unwrap();
        let Cmd::Send {
            text,
            file,
            mention,
            ..
        } = cli.cmd
        else {
            panic!("not send")
        };
        assert_eq!(text.as_deref(), Some("hello"));
        assert!(file.is_none() && mention.is_empty());

        let cli = parse(&["send", ROOM, "--file", "a.txt"]).unwrap();
        let Cmd::Send { text, file, .. } = cli.cmd else {
            panic!("not send")
        };
        assert!(text.is_none());
        assert_eq!(file, Some(PathBuf::from("a.txt")));

        assert!(
            parse(&["send", ROOM]).is_err(),
            "text or --file is required"
        );
        assert!(
            parse(&["send", ROOM, "hi", "--file", "a.txt"]).is_err(),
            "not both"
        );
    }

    #[test]
    fn send_mentions_repeat() {
        let cli = parse(&[
            "send",
            ROOM,
            "@Testa stop",
            "--mention",
            USER,
            "--mention",
            "@b:hs",
        ])
        .unwrap();
        let Cmd::Send { mention, .. } = cli.cmd else {
            panic!("not send")
        };
        assert_eq!(mention.len(), 2);
    }

    #[test]
    fn read_options() {
        let cli = parse(&["read", ROOM, "--since-ms", "1759450000000", "--json"]).unwrap();
        let Cmd::Read { opts, .. } = cli.cmd else {
            panic!("not read")
        };
        assert_eq!(opts.since_ms, Some(1_759_450_000_000));
        assert!(opts.json);
        assert_eq!(opts.limit, 200);
        assert!(parse(&["read", ROOM, "--since-ms", "yesterday"]).is_err());

        let cli = parse(&["read-dm", USER, "--limit", "5"]).unwrap();
        let Cmd::ReadDm { opts, .. } = cli.cmd else {
            panic!("not read-dm")
        };
        assert_eq!((opts.since_ms, opts.json, opts.limit), (None, false, 5));
    }

    #[test]
    fn dm_takes_mxid_and_text() {
        let cli = parse(&["dm", USER, "hi there"]).unwrap();
        let Cmd::Dm { mxid, text, .. } = cli.cmd else {
            panic!("not dm")
        };
        assert_eq!(mxid.as_str(), USER);
        assert_eq!(text, "hi there");
        assert!(parse(&["dm", USER]).is_err(), "text is required");
    }

    #[test]
    fn one_line_escapes_breaks_and_tabs() {
        assert_eq!(one_line("a\nb\tc\r\n"), "a\\nb c\\n");
        assert_eq!(one_line("x\\ny"), "x\\\\ny");
    }

    #[test]
    fn read_body_rules() {
        assert!(read_body(Some("  ".into()), None).is_err());
        assert!(read_body(Some("x".repeat(MAX_BODY_BYTES + 1)), None).is_err());
        assert_eq!(read_body(Some("ok".into()), None).unwrap(), "ok");
    }
}
