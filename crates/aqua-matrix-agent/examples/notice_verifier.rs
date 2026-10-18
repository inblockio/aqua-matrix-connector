//! A read-only verifier identity for a notification bot's direct messages.
//!
//! A notification bot sends Markdown text messages into direct-message rooms.
//! This tool is a second, separate Matrix identity that proves those messages
//! arrive and decrypt at a real client: it joins the invites the bot sends,
//! reads the bot's messages in every room both share, and reports each message
//! as one JSON line on stdout, with a flag telling whether it decrypted.
//!
//! It never sends a message, never sets a profile, never logs out, never
//! deletes or revokes a device, and never declines an invite. An invite from
//! anyone other than `--from` is left untouched (one stderr note per room).
//! All sign-in, device and encryption handling is the library's
//! (`aqua_matrix_agent::AgentClient`); the tool adds only the selection and
//! reporting logic below. Note that `connect` is the library's normal client
//! start: for a brand-new identity it creates the key file and sets up
//! cross-signing and recovery for that identity, so use a dedicated key file.
//!
//! `listen` refuses to start without an existing key file, so a lost key can
//! never turn into a new identity silently; only `whoami` creates one.
//!
//! None of the four connection arguments has a default: the tool never guesses
//! which server to talk to. They go before the subcommand name.
//!
//! Run:
//!   cargo run -p aqua-matrix-agent --example notice_verifier -- \
//!     --key-file ./verifier.pem --store-dir ./verifier-store \
//!     --siwx-url https://siwx.example.org --matrix-url https://matrix.example.org \
//!     whoami
//!
//!   cargo run -p aqua-matrix-agent --example notice_verifier -- \
//!     --key-file ./verifier.pem --store-dir ./verifier-store \
//!     --siwx-url https://siwx.example.org --matrix-url https://matrix.example.org \
//!     listen --from '@bot:matrix.example.org' --since-ms 1760000000000 \
//!     --expect '$eventid1' --expect '$eventid2' --timeout-secs 180
//!
//! Output (stdout, one JSON object per line):
//!   whoami:  {"user_id":...,"device_id":...,"did":...}
//!   listen:  {"event_id":...,"room_id":...,"sender":...,"ts_ms":...,"utd":false,"body":...}
//!            printed the first time an event is seen, and again only when an
//!            event first seen undecryptable (`utd` true) later decrypts.
//!   finish:  {"done":true,"missing":[...],"undecryptable":[...]}
//!            `done` means the run has finished. `missing` lists `--expect`
//!            ids never seen, `undecryptable` those seen only undecryptable.
//!
//! Exit codes: 0 = every `--expect` id decrypted (or none was given and the
//! deadline passed), 1 = the deadline passed with an expected id missing or
//! undecryptable, 2 = error (message on stderr).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use aqua_matrix_agent::{AgentClient, AgentConfig, Message};
use clap::{Args, Parser, Subcommand};
use matrix_sdk::RoomMemberships;
use serde::Serialize;

/// Upper bound for `--timeout-secs`.
const TIMEOUT_CAP_SECS: u64 = 1800;
/// Longest body printed, in characters.
const BODY_MAX_CHARS: usize = 4096;
/// Pause between rounds when the sync does not long-poll, and after an error.
const PAUSE: Duration = Duration::from_secs(3);
/// Floor between rounds after a long-polling sync (guards against a spin).
const LONG_POLL_FLOOR: Duration = Duration::from_millis(200);
/// How long `AgentClient::sync_once` waits for news (matrix-sdk default).
const LONG_POLL: Duration = Duration::from_secs(30);
/// Rotate the access token when it has less than this many seconds left.
const TOKEN_MARGIN_SECS: u64 = 60;
/// Wait this long before retrying a failed token rotation.
const REFRESH_RETRY: Duration = Duration::from_secs(30);
/// Give up after this many failed syncs in a row.
const MAX_SYNC_ERRORS: u32 = 5;

#[derive(Parser)]
#[command(
    name = "notice_verifier",
    about = "Read-only verifier identity for a notification bot's direct messages"
)]
struct Cli {
    /// Ed25519 key file of the verifier identity. Use a dedicated file: the
    /// library creates it when it does not exist.
    #[arg(long, value_name = "PATH")]
    key_file: PathBuf,
    /// Directory for the verifier's session and encryption store.
    #[arg(long, value_name = "PATH")]
    store_dir: PathBuf,
    /// Base URL of the siwx-oidc server (for example https://siwx.example.org).
    #[arg(long, value_name = "URL")]
    siwx_url: String,
    /// Base URL of the Matrix homeserver (for example https://matrix.example.org).
    #[arg(long, value_name = "URL")]
    matrix_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect and print the identity as one JSON line.
    Whoami,
    /// Watch for the bot's messages and report them as JSON lines.
    Listen(ListenArgs),
}

#[derive(Args)]
struct ListenArgs {
    /// Matrix user id of the bot (case-insensitive).
    #[arg(long, value_name = "MXID")]
    from: String,
    /// Only report messages sent at or after this time (Unix milliseconds).
    #[arg(long, value_name = "MILLIS")]
    since_ms: u64,
    /// Event id that must arrive and decrypt (repeatable).
    #[arg(long = "expect", value_name = "EVENT_ID")]
    expect: Vec<String>,
    /// Give up after this many seconds (capped at 1800).
    #[arg(long, value_name = "SECS", default_value_t = 180)]
    timeout_secs: u64,
    /// How many recent events to read per room and round.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: u32,
}

// ---------------------------------------------------------------------------
// Pure logic (unit tested below)
// ---------------------------------------------------------------------------

/// Clamp the requested listen time to `1..=TIMEOUT_CAP_SECS`.
fn clamp_timeout_secs(secs: u64) -> u64 {
    secs.clamp(1, TIMEOUT_CAP_SECS)
}

/// Synapse lowercases user ids, the id given on the command line may not be.
fn same_user(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// A cheap shape check for a Matrix user id: `@local:server`, no spaces.
fn looks_like_mxid(s: &str) -> bool {
    match s.strip_prefix('@').and_then(|r| r.split_once(':')) {
        Some((local, server)) => {
            !local.is_empty() && !server.is_empty() && !s.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InviteAction {
    /// The invite comes from the bot: join it.
    Join,
    /// Anyone else, or an unknown inviter: touch nothing.
    Ignore,
}

/// Join only an invite whose inviter is known and is `from`. An unknown
/// inviter is never joined on a guess; nothing is ever declined.
fn invite_action(inviter: Option<&str>, from: &str) -> InviteAction {
    match inviter {
        Some(i) if same_user(i, from) => InviteAction::Join,
        _ => InviteAction::Ignore,
    }
}

/// Whether a message is one to report: sent by `from` at or after `since_ms`.
fn is_wanted(sender: &str, ts_ms: u64, from: &str, since_ms: u64) -> bool {
    same_user(sender, from) && ts_ms >= since_ms
}

/// The first `max` characters of `s` (never splits a character).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// A message reduced to what the verifier decides on and prints.
struct Observed {
    event_id: String,
    sender: String,
    ts_ms: u64,
    utd: bool,
    body: String,
}

impl From<Message> for Observed {
    fn from(m: Message) -> Self {
        let utd = m.is_undecryptable();
        Observed {
            event_id: m.event_id,
            sender: m.sender,
            ts_ms: m.timestamp_ms,
            utd,
            body: m.body,
        }
    }
}

#[derive(Serialize)]
struct MessageLine<'a> {
    event_id: &'a str,
    room_id: &'a str,
    sender: &'a str,
    ts_ms: u64,
    utd: bool,
    body: String,
}

fn message_line(room_id: &str, m: &Observed) -> String {
    let line = MessageLine {
        event_id: &m.event_id,
        room_id,
        sender: &m.sender,
        ts_ms: m.ts_ms,
        utd: m.utd,
        body: truncate_chars(&m.body, BODY_MAX_CHARS),
    };
    serde_json::to_string(&line).expect("a struct of strings, numbers and bools serializes")
}

#[derive(Serialize)]
struct DoneLine {
    done: bool,
    missing: Vec<String>,
    undecryptable: Vec<String>,
}

#[derive(Serialize)]
struct WhoamiLine<'a> {
    user_id: &'a str,
    device_id: Option<String>,
    did: &'a str,
}

/// Per-event state and the bookkeeping for the `--expect` ids.
struct Tracker {
    /// Expected event ids, de-duplicated, in the order given.
    expected: Vec<String>,
    /// Every reported event id and whether it has only been seen undecryptable.
    seen: HashMap<String, bool>,
}

impl Tracker {
    fn new(expect: &[String]) -> Self {
        let mut expected: Vec<String> = Vec::new();
        for id in expect {
            if !expected.contains(id) {
                expected.push(id.clone());
            }
        }
        Tracker {
            expected,
            seen: HashMap::new(),
        }
    }

    /// Record a sighting. True when the event is to be printed now: the first
    /// time it is seen, and when it turns from undecryptable to decrypted. A
    /// decrypted event never goes back to undecryptable.
    fn observe(&mut self, event_id: &str, utd: bool) -> bool {
        match self.seen.get_mut(event_id) {
            None => {
                self.seen.insert(event_id.to_owned(), utd);
                true
            }
            Some(only_utd) if *only_utd && !utd => {
                *only_utd = false;
                true
            }
            Some(_) => false,
        }
    }

    /// Every expected id has been printed decrypted. With nothing expected the
    /// run is never done early; it listens until the deadline.
    fn is_done(&self) -> bool {
        !self.expected.is_empty()
            && self
                .expected
                .iter()
                .all(|id| self.seen.get(id) == Some(&false))
    }

    /// Expected ids never seen.
    fn missing(&self) -> Vec<String> {
        self.expected
            .iter()
            .filter(|id| !self.seen.contains_key(*id))
            .cloned()
            .collect()
    }

    /// Expected ids seen only undecryptable.
    fn undecryptable(&self) -> Vec<String> {
        self.expected
            .iter()
            .filter(|id| self.seen.get(*id) == Some(&true))
            .cloned()
            .collect()
    }

    fn done_line(&self) -> String {
        let line = DoneLine {
            done: true,
            missing: self.missing(),
            undecryptable: self.undecryptable(),
        };
        serde_json::to_string(&line).expect("a struct of strings and bools serializes")
    }

    /// 0 when nothing expected is missing or undecryptable, else 1.
    fn exit_code(&self) -> u8 {
        u8::from(!self.missing().is_empty() || !self.undecryptable().is_empty())
    }
}

/// Filter one room's messages down to the bot's, update the tracker and give
/// back the JSON lines to print, oldest first.
fn collect_prints(
    tracker: &mut Tracker,
    room_id: &str,
    msgs: &[Observed],
    from: &str,
    since_ms: u64,
) -> Vec<String> {
    let mut lines = Vec::new();
    for m in msgs {
        if is_wanted(&m.sender, m.ts_ms, from, since_ms) && tracker.observe(&m.event_id, m.utd) {
            lines.push(message_line(room_id, m));
        }
    }
    lines
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum SyncMode {
    /// `sync_once`: waits up to 30 s for news.
    LongPoll,
    /// `sync_once_nowait`: returns at once; used near the deadline so the
    /// last round cannot overrun it by a whole long poll.
    CatchUp,
}

fn sync_mode(remaining: Duration) -> SyncMode {
    if remaining > LONG_POLL + Duration::from_secs(2) {
        SyncMode::LongPoll
    } else {
        SyncMode::CatchUp
    }
}

/// Pause after a round's sync. A long poll has already waited.
fn pause_after(mode: SyncMode) -> Duration {
    match mode {
        SyncMode::LongPoll => LONG_POLL_FLOOR,
        SyncMode::CatchUp => PAUSE,
    }
}

/// Whether the access token is within `margin_secs` of expiring.
fn needs_refresh(expires_at_unix: u64, now_unix: u64, margin_secs: u64) -> bool {
    expires_at_unix <= now_unix.saturating_add(margin_secs)
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn connect(cli: &Cli) -> Result<AgentClient> {
    if !cli.key_file.exists() {
        eprintln!(
            "notice_verifier: {} does not exist; the library will create a new identity key there",
            cli.key_file.display()
        );
    }
    AgentClient::connect(AgentConfig {
        key_file: cli.key_file.clone(),
        siwx_url: cli.siwx_url.clone(),
        matrix_url: cli.matrix_url.clone(),
        client_id: None,
        redirect_uri: None,
        store_dir: cli.store_dir.clone(),
        // None: connect() derives a stable device id from the DID.
        device_id: None,
        device_role: Default::default(),
    })
    .await
    .context("connecting to the homeserver")
}

/// Join the bot's invites; leave every other invite alone.
async fn handle_invites(agent: &AgentClient, from: &str, noted: &mut HashSet<String>) {
    for invite in agent.pending_invites().await {
        match invite_action(invite.inviter.as_deref(), from) {
            InviteAction::Join => match agent.join_invited_room(&invite.room_id).await {
                Ok(()) => eprintln!("notice_verifier: joined {}", invite.room_id),
                Err(e) => eprintln!("notice_verifier: could not join {}: {e:#}", invite.room_id),
            },
            InviteAction::Ignore => {
                if noted.insert(invite.room_id.clone()) {
                    eprintln!(
                        "notice_verifier: ignoring invite to {} (inviter {})",
                        invite.room_id,
                        invite.inviter.as_deref().unwrap_or("unknown")
                    );
                }
            }
        }
    }
}

/// Read every joined room that has the bot as a member and print what is new.
async fn scan_rooms(agent: &AgentClient, args: &ListenArgs, tracker: &mut Tracker) {
    for room in agent.client().joined_rooms() {
        let room_id = room.room_id().to_string();
        let has_bot = match room.members(RoomMemberships::JOIN).await {
            Ok(members) => members
                .iter()
                .any(|m| same_user(m.user_id().as_str(), &args.from)),
            Err(e) => {
                eprintln!("notice_verifier: members of {room_id}: {e:#}");
                continue;
            }
        };
        if !has_bot {
            continue;
        }
        match agent.messages(&room_id, args.limit).await {
            Ok(msgs) => {
                let observed: Vec<Observed> = msgs.into_iter().map(Observed::from).collect();
                for line in collect_prints(tracker, &room_id, &observed, &args.from, args.since_ms)
                {
                    println!("{line}");
                }
            }
            Err(e) => eprintln!("notice_verifier: messages of {room_id}: {e:#}"),
        }
    }
}

async fn listen(mut agent: AgentClient, args: &ListenArgs) -> Result<u8> {
    let deadline = Instant::now() + Duration::from_secs(clamp_timeout_secs(args.timeout_secs));
    let mut tracker = Tracker::new(&args.expect);
    let mut noted_invites: HashSet<String> = HashSet::new();
    let mut sync_errors: u32 = 0;
    let mut refresh_retry_at: Option<Instant> = None;
    // `connect` has just run a catch-up sync, so look before waiting for more.
    let mut first_round = true;

    loop {
        if !first_round {
            if needs_refresh(agent.expires_at_unix(), unix_now(), TOKEN_MARGIN_SECS)
                && refresh_retry_at.is_none_or(|t| Instant::now() >= t)
            {
                if let Err(e) = agent.reauth_token_only().await {
                    eprintln!("notice_verifier: token rotation failed: {e:#}");
                    refresh_retry_at = Some(Instant::now() + REFRESH_RETRY);
                }
            }
            let mode = sync_mode(deadline.saturating_duration_since(Instant::now()));
            let synced = match mode {
                SyncMode::LongPoll => agent.sync_once().await,
                SyncMode::CatchUp => agent.sync_once_nowait().await,
            };
            let pause = match synced {
                Ok(()) => {
                    sync_errors = 0;
                    pause_after(mode)
                }
                Err(e) => {
                    sync_errors += 1;
                    eprintln!(
                        "notice_verifier: sync failed ({sync_errors}/{MAX_SYNC_ERRORS}): {e:#}"
                    );
                    if sync_errors >= MAX_SYNC_ERRORS {
                        bail!("giving up after {MAX_SYNC_ERRORS} failed syncs in a row");
                    }
                    PAUSE
                }
            };
            tokio::time::sleep(pause.min(deadline.saturating_duration_since(Instant::now()))).await;
        }
        first_round = false;

        handle_invites(&agent, &args.from, &mut noted_invites).await;
        scan_rooms(&agent, args, &mut tracker).await;

        if tracker.is_done() || Instant::now() >= deadline {
            break;
        }
    }

    println!("{}", tracker.done_line());
    Ok(tracker.exit_code())
}

async fn run(cli: Cli) -> Result<u8> {
    match &cli.command {
        Command::Whoami => {
            let agent = connect(&cli).await?;
            let line = WhoamiLine {
                user_id: agent.user_id(),
                device_id: agent.device_id(),
                did: agent.did(),
            };
            println!("{}", serde_json::to_string(&line)?);
            Ok(0)
        }
        Command::Listen(args) => {
            if !looks_like_mxid(&args.from) {
                bail!("--from must be a Matrix user id like @bot:matrix.example.org");
            }
            // A verifier is one fixed identity: listening must never mint a new
            // one because a key file went missing. Only `whoami` may create it.
            if !cli.key_file.exists() {
                bail!(
                    "{} does not exist; listen never creates an identity (run whoami once to create it)",
                    cli.key_file.display()
                );
            }
            let agent = connect(&cli).await?;
            listen(agent, args).await
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr so stdout carries only the JSON lines.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .try_init()
        .ok();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("notice_verifier: {e:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    const BOT: &str = "@bot:matrix.example.org";

    fn obs(id: &str, sender: &str, ts: u64, utd: bool, body: &str) -> Observed {
        Observed {
            event_id: id.into(),
            sender: sender.into(),
            ts_ms: ts,
            utd,
            body: body.into(),
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn invite_from_the_bot_is_joined_case_insensitively() {
        assert_eq!(invite_action(Some(BOT), BOT), InviteAction::Join);
        assert_eq!(
            invite_action(Some("@bot:matrix.example.org"), "@Bot:Matrix.Example.org"),
            InviteAction::Join
        );
    }

    #[test]
    fn other_or_unknown_inviters_are_ignored() {
        assert_eq!(
            invite_action(Some("@mallory:matrix.example.org"), BOT),
            InviteAction::Ignore
        );
        assert_eq!(invite_action(None, BOT), InviteAction::Ignore);
        // A prefix of the bot's id is not the bot.
        assert_eq!(
            invite_action(Some("@bot:matrix.example.org.evil.test"), BOT),
            InviteAction::Ignore
        );
    }

    #[test]
    fn wanted_means_the_bot_at_or_after_since() {
        assert!(is_wanted(BOT, 1000, BOT, 1000));
        assert!(is_wanted("@BOT:matrix.example.org", 1001, BOT, 1000));
        assert!(!is_wanted(BOT, 999, BOT, 1000));
        assert!(!is_wanted("@other:matrix.example.org", 5000, BOT, 1000));
    }

    #[test]
    fn mxid_shape() {
        assert!(looks_like_mxid("@bot:matrix.example.org"));
        assert!(looks_like_mxid("@bot:localhost:8008"));
        assert!(!looks_like_mxid("bot:matrix.example.org"));
        assert!(!looks_like_mxid("@:matrix.example.org"));
        assert!(!looks_like_mxid("@bot:"));
        assert!(!looks_like_mxid("@bot"));
        assert!(!looks_like_mxid("@bo t:matrix.example.org"));
    }

    #[test]
    fn truncation_is_by_character() {
        assert_eq!(truncate_chars("abc", 4096), "abc");
        let long = "\u{e9}".repeat(5000);
        let cut = truncate_chars(&long, BODY_MAX_CHARS);
        assert_eq!(cut.chars().count(), BODY_MAX_CHARS);
        assert!(long.starts_with(&cut));
    }

    #[test]
    fn message_line_has_the_documented_shape_and_order() {
        let m = obs("$e1", BOT, 1234, false, "hi \"there\"\nline 2");
        assert_eq!(
            message_line("!r:matrix.example.org", &m),
            "{\"event_id\":\"$e1\",\"room_id\":\"!r:matrix.example.org\",\
             \"sender\":\"@bot:matrix.example.org\",\"ts_ms\":1234,\"utd\":false,\
             \"body\":\"hi \\\"there\\\"\\nline 2\"}"
        );
    }

    #[test]
    fn message_line_cuts_the_body() {
        let m = obs("$e1", BOT, 1, false, &"x".repeat(10_000));
        let v: serde_json::Value = serde_json::from_str(&message_line("!r:x", &m)).unwrap();
        assert_eq!(v["body"].as_str().unwrap().chars().count(), BODY_MAX_CHARS);
    }

    #[test]
    fn whoami_line_shape() {
        let line = WhoamiLine {
            user_id: BOT,
            device_id: None,
            did: "did:key:z6Mk",
        };
        assert_eq!(
            serde_json::to_string(&line).unwrap(),
            "{\"user_id\":\"@bot:matrix.example.org\",\"device_id\":null,\"did\":\"did:key:z6Mk\"}"
        );
    }

    #[test]
    fn tracker_prints_first_sight_and_utd_to_decrypted_only() {
        let mut t = Tracker::new(&[]);
        assert!(t.observe("$a", true), "first sight prints");
        assert!(!t.observe("$a", true), "still undecryptable: silent");
        assert!(t.observe("$a", false), "turned decrypted: prints again");
        assert!(!t.observe("$a", false), "decrypted again: silent");
        assert!(!t.observe("$a", true), "never goes back to undecryptable");
        assert!(t.observe("$b", false), "another event, first sight");
    }

    #[test]
    fn tracker_done_needs_every_expected_id_decrypted() {
        let mut t = Tracker::new(&ids(&["$a", "$b"]));
        assert!(!t.is_done());
        t.observe("$a", false);
        assert!(!t.is_done());
        t.observe("$b", true);
        assert!(!t.is_done(), "undecryptable does not count");
        t.observe("$b", false);
        assert!(t.is_done());
        assert_eq!(t.exit_code(), 0);
        assert_eq!(
            t.done_line(),
            "{\"done\":true,\"missing\":[],\"undecryptable\":[]}"
        );
    }

    #[test]
    fn tracker_without_expectations_is_never_done_and_exits_zero() {
        let mut t = Tracker::new(&[]);
        t.observe("$a", false);
        assert!(!t.is_done());
        assert_eq!(t.exit_code(), 0);
        assert_eq!(
            t.done_line(),
            "{\"done\":true,\"missing\":[],\"undecryptable\":[]}"
        );
    }

    #[test]
    fn tracker_reports_missing_and_undecryptable_at_the_deadline() {
        let mut t = Tracker::new(&ids(&["$a", "$b", "$c", "$a"]));
        t.observe("$a", false);
        t.observe("$b", true);
        t.observe("$other", false);
        assert_eq!(t.missing(), ids(&["$c"]));
        assert_eq!(t.undecryptable(), ids(&["$b"]));
        assert_eq!(t.exit_code(), 1);
        assert_eq!(
            t.done_line(),
            "{\"done\":true,\"missing\":[\"$c\"],\"undecryptable\":[\"$b\"]}"
        );
    }

    #[test]
    fn tracker_exit_is_one_for_missing_alone_and_for_undecryptable_alone() {
        let t = Tracker::new(&ids(&["$a"]));
        assert_eq!(t.exit_code(), 1);
        let mut t = Tracker::new(&ids(&["$a"]));
        t.observe("$a", true);
        assert_eq!(t.exit_code(), 1);
    }

    #[test]
    fn collect_prints_filters_and_prints_each_event_once() {
        let mut t = Tracker::new(&ids(&["$a"]));
        let msgs = vec![
            obs("$old", BOT, 10, false, "before the window"),
            obs(
                "$x",
                "@other:matrix.example.org",
                2000,
                false,
                "not the bot",
            ),
            obs(
                "$a",
                "@BOT:matrix.example.org",
                2000,
                true,
                "[unable to decrypt]",
            ),
            obs("$b", BOT, 2001, false, "second"),
        ];
        let first = collect_prints(&mut t, "!r:x", &msgs, BOT, 1000);
        assert_eq!(first.len(), 2);
        assert!(first[0].contains("\"event_id\":\"$a\"") && first[0].contains("\"utd\":true"));
        assert!(first[1].contains("\"event_id\":\"$b\""));
        assert!(!t.is_done());

        // Same sight again: nothing. Then $a decrypts: only $a prints.
        assert!(collect_prints(&mut t, "!r:x", &msgs, BOT, 1000).is_empty());
        let msgs = vec![
            obs("$a", BOT, 2000, false, "hello"),
            obs("$b", BOT, 2001, false, "second"),
        ];
        let again = collect_prints(&mut t, "!r:x", &msgs, BOT, 1000);
        assert_eq!(again.len(), 1);
        assert!(again[0].contains("\"event_id\":\"$a\"") && again[0].contains("\"utd\":false"));
        assert!(t.is_done());
    }

    #[test]
    fn timeout_is_clamped() {
        assert_eq!(clamp_timeout_secs(0), 1);
        assert_eq!(clamp_timeout_secs(180), 180);
        assert_eq!(clamp_timeout_secs(1800), 1800);
        assert_eq!(clamp_timeout_secs(100_000), 1800);
    }

    #[test]
    fn sync_mode_avoids_overrunning_the_deadline() {
        assert_eq!(sync_mode(Duration::from_secs(180)), SyncMode::LongPoll);
        assert_eq!(sync_mode(Duration::from_secs(33)), SyncMode::LongPoll);
        assert_eq!(sync_mode(Duration::from_secs(32)), SyncMode::CatchUp);
        assert_eq!(sync_mode(Duration::ZERO), SyncMode::CatchUp);
        assert!(pause_after(SyncMode::LongPoll) < pause_after(SyncMode::CatchUp));
        assert_eq!(pause_after(SyncMode::CatchUp), Duration::from_secs(3));
    }

    #[test]
    fn token_refresh_window() {
        assert!(needs_refresh(1000, 1000, 60));
        assert!(needs_refresh(1050, 1000, 60));
        assert!(!needs_refresh(1061, 1000, 60));
        assert!(needs_refresh(0, u64::MAX, 60));
    }

    const CONN: [&str; 8] = [
        "--key-file",
        "k.pem",
        "--store-dir",
        "store",
        "--siwx-url",
        "https://siwx.example.org",
        "--matrix-url",
        "https://matrix.example.org",
    ];

    fn argv(first: &[&str], rest: &[&str]) -> Vec<String> {
        ["notice_verifier"]
            .into_iter()
            .chain(first.iter().copied())
            .chain(rest.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn connection_arguments_are_required_and_have_no_default() {
        let cmd = Cli::command();
        for name in ["key_file", "store_dir", "siwx_url", "matrix_url"] {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == name)
                .unwrap_or_else(|| panic!("missing arg {name}"));
            assert!(arg.is_required_set(), "{name} must be required");
            assert!(arg.get_default_values().is_empty(), "{name} has a default");
        }
        assert!(Cli::try_parse_from(argv(&[], &["whoami"])).is_err());
        assert!(Cli::try_parse_from(argv(&CONN[..6], &["whoami"])).is_err());
    }

    #[test]
    fn connection_arguments_come_before_the_subcommand() {
        assert!(Cli::try_parse_from(argv(&CONN, &["whoami"])).is_ok());
        assert!(Cli::try_parse_from(argv(&["whoami"], &CONN)).is_err());
    }

    #[test]
    fn listen_arguments() {
        let cli = Cli::try_parse_from(argv(
            &CONN,
            &[
                "listen",
                "--from",
                BOT,
                "--since-ms",
                "1760000000000",
                "--expect",
                "$a",
                "--expect",
                "$b",
            ],
        ))
        .unwrap();
        let Command::Listen(args) = cli.command else {
            panic!("expected listen");
        };
        assert_eq!(args.from, BOT);
        assert_eq!(args.since_ms, 1_760_000_000_000);
        assert_eq!(args.expect, ids(&["$a", "$b"]));
        assert_eq!(args.timeout_secs, 180);
        assert_eq!(args.limit, 50);
        // --from and --since-ms are required.
        assert!(Cli::try_parse_from(argv(&CONN, &["listen", "--since-ms", "1"])).is_err());
        assert!(Cli::try_parse_from(argv(&CONN, &["listen", "--from", BOT])).is_err());
    }
}
