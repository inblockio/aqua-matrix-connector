//! KEYLOSS-1 H3 live check: does [`RtcMembership::rejoin`] make a real
//! Element Call re-send its media key? (2026-10-05)
//!
//! Element Call shares its media key per `(user, device, createdTs())`; a
//! rejoin in place re-publishes our `call.member` with a fresh `created_ts`,
//! which must read as a new joiner and bring a fresh
//! `io.element.call.encryption_keys` to-device message from every peer.
//!
//! One human, about two minutes, against a dev stack:
//!
//!   cargo run -p aqua-matrix-agent --example rtc_rejoin_probe -- \
//!     --key-file <path to a throwaway dev identity key file> \
//!     --human @<you>:dev.matrix.inblock.io
//!
//! The probe signs in as that throwaway dev identity (fresh device and fresh
//! store under `~/.cache/keyloss-h3/` per run, so no live store and no stale
//! one-time keys), opens an encrypted DM with `--human`, holds a call
//! membership there (no LiveKit media), and then:
//!
//!   1. waits for the human's first media key (the human joined the call),
//!   2. CONTROL: 20 s with no membership change; expects no new key,
//!   3. rejoins in place and waits up to 20 s for a new key from the human.
//!
//! The rejoin's event id is compared with the join's (the membership event
//! this client has synced just before the rejoin), and the probe says which
//! case it is: the SAME id means Synapse dropped the rejoin as identical
//! state; a DIFFERENT id means Synapse accepted a new membership event, so a
//! missing key then means Element Call did not react to it.
//!
//! Verdict on stdout (`H3: PASS|FAIL|INCONCLUSIVE ...`), exit 0 / 1 / 2. Key
//! material is never printed. Talks only to the URLs given (dev by default).

use std::path::PathBuf;
use std::time::Duration;

use aqua_matrix_agent::{AgentClient, AgentConfig, CallEncryptionKeys, RtcMemberTiming};
use clap::Parser;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[derive(Parser)]
struct Args {
    /// A throwaway dev identity's key file (never a live agent's key).
    #[arg(long)]
    key_file: PathBuf,
    /// The human who joins the call (their MXID on the dev homeserver).
    #[arg(long)]
    human: String,
    /// Use this existing room (the probe must be invited or joined) instead
    /// of opening a new encrypted DM with `--human`.
    #[arg(long)]
    room: Option<String>,
    #[arg(long, default_value = "https://dev.siwx.inblock.io")]
    siwx_url: String,
    #[arg(long, default_value = "https://dev.matrix.inblock.io")]
    matrix_url: String,
    /// How long the human has to accept the invite, and then to join the call.
    #[arg(long, default_value_t = 180)]
    wait_secs: u64,
    /// The quiet control window before the rejoin.
    #[arg(long, default_value_t = 20)]
    control_secs: u64,
    /// How long after the rejoin a new key counts as caused by it.
    #[arg(long, default_value_t = 20)]
    verdict_secs: u64,
}

fn verdict(code: i32, line: &str) -> ! {
    println!("H3: {line}");
    std::process::exit(code)
}

/// Feed one client's key receiver into the probe's single inbox (the same
/// shape as the Scribe's `KeyInbox`).
fn pump(mut src: mpsc::Receiver<CallEncryptionKeys>, to: mpsc::Sender<CallEncryptionKeys>) {
    tokio::spawn(async move {
        while let Some(k) = src.recv().await {
            if to.send(k).await.is_err() {
                return;
            }
        }
    });
}

/// Event id of our own current `call.member` state event in `room_id`, as
/// this client has synced it (the join, unless a refresh came since): the
/// non-empty membership event sent by us from `device`. `None` when it is not
/// in the state store (not synced yet, or the room is unknown).
async fn own_member_event_id(agent: &AgentClient, room_id: &str, device: &str) -> Option<String> {
    let room = agent.client().get_room(<&matrix_sdk::ruma::RoomId>::try_from(room_id).ok()?)?;
    let events = room
        .get_state_events(matrix_sdk::ruma::events::StateEventType::from(
            "org.matrix.msc3401.call.member",
        ))
        .await
        .ok()?;
    events.iter().find_map(|e| {
        let v = serde_json::to_value(e).ok()?;
        let ours = v["sender"].as_str() == Some(agent.user_id())
            && v["state_key"].as_str().is_some_and(|k| k.contains(device));
        let live = v["content"].as_object().is_some_and(|c| !c.is_empty());
        if ours && live {
            v["event_id"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

/// Which case a rejoin is, from its event id and the join's: the same id
/// means Synapse returned the existing event (it dropped the rejoin as
/// identical state), a different one means it stored a new event.
fn rejoin_case(join_event: Option<&str>, rejoin_event: &str) -> &'static str {
    match join_event {
        Some(j) if j == rejoin_event => {
            "rejoin event id == join event id: Synapse DROPPED the rejoin as identical state"
        }
        Some(_) => "rejoin event id != join event id: Synapse ACCEPTED a new membership event",
        None => "join event id unavailable: cannot tell whether Synapse accepted a new event",
    }
}

/// Next key set from `human` within `limit`, as (seconds waited, key count).
async fn next_human_key(
    inbox: &mut mpsc::Receiver<CallEncryptionKeys>,
    human: &str,
    limit: Duration,
) -> Option<(f64, usize)> {
    let start = Instant::now();
    let deadline = start + limit;
    loop {
        match tokio::time::timeout_at(deadline, inbox.recv()).await {
            Ok(Some(k)) if k.sender_user_id == human => {
                return Some((start.elapsed().as_secs_f64(), k.keys.len()))
            }
            Ok(Some(k)) => println!("H3: (key from {} ignored)", k.sender_user_id),
            Ok(None) | Err(_) => return None,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,aqua_matrix_agent=info".into()),
        )
        .try_init()
        .ok();
    let args = Args::parse();
    let run = format!("{:x}", std::process::id() ^ (aqua_now() as u32));
    let store_dir = dirs_home().join(format!(".cache/keyloss-h3/store-{run}"));
    let device = format!("AQUA_H3{}", run.to_uppercase());
    let config = AgentConfig {
        key_file: args.key_file.clone(),
        siwx_url: args.siwx_url.clone(),
        matrix_url: args.matrix_url.clone(),
        client_id: None,
        redirect_uri: None,
        store_dir,
        device_id: Some(device.clone()),
        device_role: Default::default(),
    };
    let agent = AgentClient::connect(config)
        .await
        .unwrap_or_else(|e| verdict(2, &format!("INCONCLUSIVE connect failed: {e:#}")));
    println!("H3: probe is {} device {device}", agent.user_id());

    let (inbox_tx, mut inbox) = mpsc::channel(256);
    pump(agent.on_call_encryption_keys(64), inbox_tx.clone());

    // The single syncing client. Its token is rotated without a sync and the
    // fresh client's key handler is attached BEFORE its first sync.
    let human: matrix_sdk::ruma::OwnedUserId = args
        .human
        .as_str()
        .try_into()
        .unwrap_or_else(|e| verdict(2, &format!("INCONCLUSIVE bad --human: {e}")));
    let room_id = match &args.room {
        Some(r) => {
            let _ = agent.join_invited_room(r).await;
            r.clone()
        }
        None => {
            let room = agent
                .client()
                .create_dm(&human)
                .await
                .unwrap_or_else(|e| verdict(2, &format!("INCONCLUSIVE create_dm: {e}")));
            room.room_id().to_string()
        }
    };
    println!("H3: room {room_id}: accept the invite, then start a call in it");
    let syncer = {
        let mut a = agent.clone();
        let tx = inbox_tx.clone();
        tokio::spawn(async move {
            loop {
                if a.expires_at_unix().saturating_sub(aqua_now()) < 60 {
                    let mut fresh = a.clone();
                    match fresh.reauth_token_only().await {
                        Ok(()) => {
                            pump(fresh.on_call_encryption_keys(64), tx.clone());
                            a = fresh;
                        }
                        Err(e) => println!("H3: (token rotation failed, retrying: {e:#})"),
                    }
                }
                if let Err(e) = a.sync_once().await {
                    println!("H3: (sync failed: {e:#})");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    };

    // Hold our membership (keeper: refresh + delayed leave).
    let focus = agent.rtc_focus_service_url().await;
    let membership = agent
        .hold_rtc_member(&room_id, &room_id, &focus, RtcMemberTiming::default())
        .await
        .unwrap_or_else(|e| verdict(2, &format!("INCONCLUSIVE hold_rtc_member: {e:#}")));
    println!("H3: membership held; waiting up to {} s for the human's first media key", args.wait_secs);

    let wait = Duration::from_secs(args.wait_secs);
    let Some((t, n)) = next_human_key(&mut inbox, human.as_str(), wait).await else {
        let _ = membership.leave().await;
        verdict(2, "INCONCLUSIVE no first key from the human (did they join the call?)");
    };
    println!("H3: first key from the human after {t:.1} s ({n} keys)");
    // Drain the burst around the join (a join can bring several sends).
    while next_human_key(&mut inbox, human.as_str(), Duration::from_secs(5))
        .await
        .is_some()
    {}

    let control = Duration::from_secs(args.control_secs);
    if let Some((t, _)) = next_human_key(&mut inbox, human.as_str(), control).await {
        let _ = membership.leave().await;
        verdict(
            2,
            &format!("INCONCLUSIVE the human re-sent a key {t:.1} s into the quiet control window"),
        );
    }
    println!("H3: control window {} s: no new key without a membership change", args.control_secs);

    // The join's event id, read just before the rejoin (a join and the
    // hourly refresh are the only membership events before it).
    let join_event = own_member_event_id(&agent, &room_id, &device).await;
    match &join_event {
        Some(id) => println!("H3: join event {id}"),
        None => println!("H3: (join event id not in the synced state)"),
    }
    let rejoined = match membership.rejoin().await {
        Ok(r) => r,
        Err(e) => {
            let _ = membership.leave().await;
            verdict(1, &format!("FAIL rejoin errored: {e:#}"));
        }
    };
    println!(
        "H3: rejoined in place: event {} created_ts {}",
        rejoined.event_id, rejoined.created_ts_ms
    );
    let case = rejoin_case(join_event.as_deref(), &rejoined.event_id);
    println!("H3: {case}");
    let after = next_human_key(&mut inbox, human.as_str(), Duration::from_secs(args.verdict_secs)).await;
    let _ = membership.leave().await;
    syncer.abort();
    match after {
        Some((t, n)) => verdict(0, &format!("PASS new key from the human {t:.1} s after the rejoin ({n} keys)")),
        None => verdict(
            1,
            &format!(
                "FAIL no key from the human within {} s of the rejoin ({case})",
                args.verdict_secs
            ),
        ),
    }
}

fn aqua_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}
