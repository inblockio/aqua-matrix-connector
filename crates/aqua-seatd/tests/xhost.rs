//! Live cross-host e2e on DEV (never prod): the hub plus two SIMULATED hosts
//! (extra `aqua-seatd` instances with their own state dir and host key) going
//! through the endorsed knock flow (plan S5, D8-D10), including the negative
//! cases.
//!
//! Run: `cargo build && cargo test -p aqua-seatd --features e2e --test xhost -- --nocapture`
//!
//! Identities (permanent, reused by every run): the hub uses the real state root
//! (`~/.local/state/claude-seats`, seats `seat-test-a/b/c`); the simulated hosts
//! keep theirs under `~/.local/state/claude-seats-sim/` (`CLAUDE_SEATS_SIM_STATE`):
//! host2 runs another `seat-test-a`, host3 another `seat-test-b` (same names,
//! other hosts: this also tests `seat@host` addressing), and a plain Matrix
//! account `knocker` plays the attacker of the negative cases. The hub reads a
//! test-only host allow-list (`hub-hosts.toml` in the sim dir), so the real
//! `hosts.toml` is never touched. Prints a scenario table; never keys.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aqua_matrix_agent::{AgentClient, AgentConfig, DeviceRole};
use aqua_seat::client::SeatClient;
use aqua_seat::paths::{self, SeatPaths};
use aqua_seat::proto::{Item, Request};
use aqua_seatd::endorse::{make_reason, Endorsement};
use aqua_seatd::hostkey;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_aqua-seatd");

struct Proc {
    label: String,
    child: Child,
    client: SeatClient,
    sock: PathBuf,
    stopped: bool,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.sock);
        }
    }
}

impl Proc {
    async fn start(label: &str, env: &[(&str, String)], sock: PathBuf) -> Proc {
        paths::ensure_private_dir(sock.parent().unwrap()).unwrap();
        let client = SeatClient::new(sock.clone());
        assert!(
            client
                .call(&Request::Ping, Duration::from_secs(2))
                .await
                .is_err(),
            "{label}: a daemon already answers on {}",
            sock.display()
        );
        let logdir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap()
            .join(".cache/claude-seats-dev");
        std::fs::create_dir_all(&logdir).unwrap();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logdir.join(format!("xhost-{label}.log")))
            .unwrap();
        let mut c = Command::new(BIN);
        for (k, v) in env {
            c.env(k, v);
        }
        #[allow(clippy::zombie_processes)] // reaped in stop() / Drop
        let child = c
            .env(paths::SOCK_ENV, &sock)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        for _ in 0..150 {
            if client
                .call(&Request::Ping, Duration::from_secs(2))
                .await
                .is_ok()
            {
                return Proc {
                    label: label.into(),
                    child,
                    client,
                    sock,
                    stopped: false,
                };
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{label}: aqua-seatd did not answer");
    }

    async fn call(&self, req: Request) -> Value {
        self.try_call(req.clone())
            .await
            .unwrap_or_else(|e| panic!("{}: {req:?}: {e}", self.label))
    }

    async fn try_call(&self, req: Request) -> Result<Value, String> {
        self.client.call(&req, Duration::from_secs(400)).await
    }

    async fn stop(mut self) {
        self.stopped = true;
        let _ = self
            .client
            .call(&Request::Shutdown, Duration::from_secs(10))
            .await;
        for _ in 0..150 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

fn items(v: &Value) -> Vec<Item> {
    serde_json::from_value(v["items"].clone()).expect("items")
}

async fn wait_for(p: &Proc, seat: &str, needle: &str, secs: u64) -> Item {
    let t0 = Instant::now();
    loop {
        let r = p
            .call(Request::Inbox {
                seat: seat.into(),
                since: Some(0),
                addressed_only: false,
                limit: Some(500),
                by: Some("xhost".into()),
                session: None,
            })
            .await;
        if let Some(i) = items(&r).into_iter().find(|i| i.body.contains(needle)) {
            return i;
        }
        let left = secs.saturating_sub(t0.elapsed().as_secs());
        assert!(
            left > 0,
            "{}/{seat}: no message containing {needle:?} within {secs}s",
            p.label
        );
        let _ = p
            .call(Request::Wait {
                seat: seat.into(),
                timeout_secs: left.min(10),
                after_seq: None,
                addressed_only: false,
                by: Some("xhost".into()),
                session: None,
            })
            .await;
    }
}

async fn send(
    p: &Proc,
    from: &str,
    to: &str,
    text: &str,
    mention: Option<Value>,
) -> Result<Value, String> {
    p.try_call(Request::Send {
        seat: from.into(),
        to: to.into(),
        text: text.into(),
        reply_to: None,
        mention,
    })
    .await
}

fn host_did(state: &Path) -> String {
    let out = Command::new(BIN)
        .arg("--print-host-did")
        .env(paths::STATE_ENV, state)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Audit {
    path: PathBuf,
    skip: usize,
}

impl Audit {
    fn new(root: &Path) -> Self {
        let path = root.join("hub-audit.jsonl");
        let skip = std::fs::read_to_string(&path)
            .map(|t| t.lines().count())
            .unwrap_or(0);
        Audit { path, skip }
    }
    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .skip(self.skip)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
    async fn wait_for(&self, mxid: &str, verdict: &str, code: Option<&str>, secs: u64) -> Value {
        let t0 = Instant::now();
        loop {
            if let Some(l) = self.lines().into_iter().rev().find(|l| {
                l["mxid"] == mxid && l["verdict"] == verdict && code.is_none_or(|c| l["code"] == c)
            }) {
                return l;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "no audit line {verdict}/{code:?} for {mxid} within {secs}s; saw {:?}",
                self.lines()
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

struct Rep(usize);
impl Rep {
    fn ok(&mut self, name: &str, detail: impl Into<String>) {
        self.0 += 1;
        println!("PASS  {name:<58} {}", detail.into());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s5_endorsed_knock_across_simulated_hosts() {
    let mut rep = Rep(0);
    let n = unix_now();
    let hub_root = paths::state_root();
    let sim = std::env::var_os("CLAUDE_SEATS_SIM_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/state/claude-seats-sim")
        });
    let (r2, r3) = (sim.join("host2"), sim.join("host3"));
    paths::ensure_private_dir(&r2).unwrap();
    paths::ensure_private_dir(&r3).unwrap();
    let (d2, d3) = (host_did(&r2), host_did(&r3));
    assert_ne!(d2, d3);
    let hub_hosts = sim.join("hub-hosts.toml");
    std::fs::write(
        &hub_hosts,
        format!("[[hosts]]\ndid = \"{d2}\"\nname = \"sim-host2\"\n\n[[hosts]]\ndid = \"{d3}\"\nname = \"sim-host3\"\n"),
    )
    .unwrap();
    let rt = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join("claude-seats");

    let audit = Audit::new(&hub_root);
    let hub_env = [
        ("CLAUDE_SEATS_HUB", "1".to_string()),
        ("CLAUDE_SEATS_HOSTS_FILE", hub_hosts.display().to_string()),
        ("CLAUDE_SEATS_RATE_COUNT", "1000".to_string()),
    ];
    let hub = Proc::start("hub", &hub_env, rt.join("xhost-hub.sock")).await;
    let h2 = Proc::start(
        "host2",
        &[
            (paths::STATE_ENV, r2.display().to_string()),
            ("CLAUDE_SEATS_HOST", "sim-host2".into()),
            ("CLAUDE_SEATS_RATE_COUNT", "1000".into()),
        ],
        rt.join("xhost-h2.sock"),
    )
    .await;
    let h3 = Proc::start(
        "host3",
        &[
            (paths::STATE_ENV, r3.display().to_string()),
            ("CLAUDE_SEATS_HOST", "sim-host3".into()),
            ("CLAUDE_SEATS_RATE_COUNT", "1000".into()),
        ],
        rt.join("xhost-h3.sock"),
    )
    .await;

    // -- hub identity -------------------------------------------------------------------------------------------
    let hub_a = hub
        .call(Request::Whoami {
            seat: "seat-test-a".into(),
        })
        .await;
    let hub_b = hub
        .call(Request::Whoami {
            seat: "seat-test-b".into(),
        })
        .await;
    let hub_c = hub
        .call(Request::Whoami {
            seat: "seat-test-c".into(),
        })
        .await;
    let room_id = hub_a["roster_room"].as_str().unwrap().to_string();
    let hub_host = hub_a["host"].as_str().unwrap().to_string();
    assert_eq!(hub_a["hub"], true);
    assert_eq!(hub_a["roster_join_rule"], "knock");
    rep.ok(
        "hub up, roster room is a knock room",
        format!("host {hub_host}, room {room_id}"),
    );

    // -- positive: each simulated host's seat is admitted by the endorsed knock ------------------------------------
    let mut admitted = Vec::new();
    for (proc, seat, host) in [
        (&h2, "seat-test-a", "sim-host2"),
        (&h3, "seat-test-b", "sim-host3"),
    ] {
        let _ = proc.try_call(Request::Leave { seat: seat.into() }).await; // re-run the knock path every time
        let t0 = Instant::now();
        let w = proc.call(Request::Whoami { seat: seat.into() }).await;
        assert_eq!(w["connected"], true, "{w}");
        assert_eq!(
            w["roster_room"].as_str().unwrap(),
            room_id,
            "the sim host found the hub's room by alias"
        );
        assert_eq!(w["hub"], false);
        let mxid = w["mxid"].as_str().unwrap().to_string();
        let line = audit.wait_for(&mxid, "invited", None, 30).await;
        assert_eq!(line["host"], host);
        assert_eq!(line["seat"], seat);
        assert_eq!(
            line["host_did"],
            if host == "sim-host2" { &d2 } else { &d3 }.as_str()
        );
        admitted.push((seat, host, w["did"].as_str().unwrap().to_string(), mxid));
        rep.ok(
            &format!("endorsed knock admits {seat}@{host}"),
            format!("{} ms, hub audit: invited", t0.elapsed().as_millis()),
        );
    }

    // -- the roster is cross-host ----------------------------------------------------------------------------------
    let t0 = Instant::now();
    let peers = loop {
        let r = hub
            .call(Request::Roster {
                seat: "seat-test-b".into(),
            })
            .await;
        let ps = r["peers"].as_array().unwrap().clone();
        let has =
            |seat: &str, host: &str| ps.iter().any(|p| p["seat"] == seat && p["host"] == host);
        if has("seat-test-a", "sim-host2")
            && has("seat-test-b", "sim-host3")
            && has("seat-test-a", &hub_host)
        {
            break ps;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(90),
            "roster never listed the sim hosts: {ps:?}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    for (seat, host, did, mxid) in &admitted {
        let p = peers
            .iter()
            .find(|p| p["seat"] == *seat && p["host"] == *host)
            .unwrap();
        assert_eq!(p["did"].as_str().unwrap(), did);
        assert_eq!(p["mxid"].as_str().unwrap(), mxid);
        assert_eq!(p["address"].as_str().unwrap(), format!("{seat}@{host}"));
    }
    rep.ok(
        "hub roster lists seats of three hosts with DIDs",
        format!("{} peers", peers.len()),
    );

    // -- cross-host messaging, E2EE, direct address ---------------------------------------------------------------------
    let t_dm = format!("xhost dm hub-b to sim2-a {n}");
    let s = send(&hub, "seat-test-b", "seat-test-a@sim-host2", &t_dm, None)
        .await
        .unwrap();
    assert_eq!(s["encrypted"], true);
    let got = wait_for(&h2, "seat-test-a", &t_dm, 120).await;
    assert!(got.direct);
    assert_eq!(got.from_seat.as_deref(), Some("seat-test-b"));
    assert_eq!(got.from_host.as_deref(), Some(hub_host.as_str()));
    assert_eq!(got.from_did.as_deref(), hub_b["did"].as_str());
    rep.ok(
        "DM hub-b -> sim2-a (other host, D6 sender host/DID)",
        format!("event {}", s["event_id"].as_str().unwrap()),
    );

    let t_back = format!("xhost dm sim2-a to hub-b {n}");
    let s = send(
        &h2,
        "seat-test-a",
        &format!("seat-test-b@{hub_host}"),
        &t_back,
        None,
    )
    .await
    .unwrap();
    let got = wait_for(&hub, "seat-test-b", &t_back, 120).await;
    assert!(got.direct && got.from_host.as_deref() == Some("sim-host2"));
    assert_eq!(got.from_did.as_deref(), Some(admitted[0].2.as_str()));
    rep.ok(
        "DM sim2-a -> hub-b (reply, other direction)",
        format!("event {}", s["event_id"].as_str().unwrap()),
    );

    let t_men = format!("xhost mention sim3-b to hub-c {n}");
    let s = send(
        &h3,
        "seat-test-b",
        "room",
        &t_men,
        Some(json!("seat-test-c")),
    )
    .await
    .unwrap();
    assert_eq!(s["via"], "mention");
    let at_c = wait_for(&hub, "seat-test-c", &t_men, 120).await;
    assert!(at_c.direct && at_c.from_host.as_deref() == Some("sim-host3"));
    let at_a2 = wait_for(&h2, "seat-test-a", &t_men, 120).await;
    assert!(!at_a2.direct, "host2 was not mentioned: chatter");
    rep.ok(
        "room @mention sim3-b -> hub-c (direct), sim2 sees chatter",
        format!("event {}", s["event_id"].as_str().unwrap()),
    );

    // -- seat@host addressing -----------------------------------------------------------------------------------------------
    let e = send(&hub, "seat-test-c", "seat-test-a", "x", None)
        .await
        .unwrap_err();
    assert!(
        e.contains("ambiguous")
            && e.contains("seat-test-a@sim-host2")
            && e.contains(&format!("seat-test-a@{hub_host}")),
        "{e}"
    );
    let e = send(&hub, "seat-test-c", "seat-test-nobody", "x", None)
        .await
        .unwrap_err();
    assert!(e.contains("known seats"), "{e}");
    send(
        &hub,
        "seat-test-c",
        "seat-test-b@sim-host3",
        &format!("seat@host works {n}"),
        None,
    )
    .await
    .map(|_| ())
    .unwrap_err_or_ok();
    rep.ok(
        "bare name ambiguous across hosts -> error lists candidates; seat@host works",
        "ok",
    );

    // -- negative knocks ----------------------------------------------------------------------------------------------------------
    let kroot = sim.join("knocker-root");
    let kpaths = SeatPaths::new(&kroot, "seat-neg").unwrap();
    let kinfo = aqua_seatd::keys::ensure_key(&kpaths).unwrap();
    let kagent = AgentClient::connect(AgentConfig {
        key_file: kpaths.key(),
        siwx_url: aqua_seat::devguard::DEFAULT_SIWX_URL.into(),
        matrix_url: aqua_seat::devguard::DEFAULT_MATRIX_URL.into(),
        client_id: None,
        redirect_uri: None,
        store_dir: kpaths.store(),
        device_id: None,
        device_role: DeviceRole::Primary,
    })
    .await
    .unwrap();
    let kmxid = kagent.user_id().to_string();
    let server: matrix_sdk::ruma::OwnedServerName =
        kmxid.split_once(':').unwrap().1.try_into().unwrap();
    let rid: matrix_sdk::ruma::OwnedRoomOrAliasId = room_id.as_str().try_into().unwrap();
    let key2 = hostkey::load_signing_key(&r2).unwrap();
    let random = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let random_did = {
        let mut b = vec![0xED, 0x01];
        b.extend_from_slice(random.verifying_key().as_bytes());
        format!("did:key:z{}", bs58::encode(b).into_string())
    };
    let end = |seat: &str, seat_did: &str, host: &str, host_did: &str, ts: u64| Endorsement {
        seat: seat.into(),
        seat_did: seat_did.into(),
        host: host.into(),
        host_did: host_did.into(),
        ts,
    };
    let sim2_seat_did = admitted[0].2.clone();
    let cases: Vec<(&str, &str, String)> = vec![
        (
            "malformed reason",
            "malformed",
            "not-an-endorsement".to_string(),
        ),
        (
            "unknown host DID (valid signature, host not in hosts.toml)",
            "unknown_host",
            make_reason(
                &random,
                &end("neg-seat", &kinfo.did, "sim-host9", &random_did, unix_now()),
            ),
        ),
        (
            "bad signature (listed host DID, signed by another key)",
            "bad_signature",
            make_reason(
                &random,
                &end("neg-seat", &kinfo.did, "sim-host2", &d2, unix_now()),
            ),
        ),
        (
            "stale ts (11 minutes old, valid signature)",
            "stale",
            make_reason(
                &key2,
                &end("neg-seat", &kinfo.did, "sim-host2", &d2, unix_now() - 660),
            ),
        ),
        (
            "seat name outside the D1 charset",
            "bad_seat_name",
            make_reason(
                &key2,
                &end("Bad Seat!", &kinfo.did, "sim-host2", &d2, unix_now()),
            ),
        ),
        (
            "host name differs from hosts.toml",
            "host_name_mismatch",
            make_reason(
                &key2,
                &end("neg-seat", &kinfo.did, "sim-host-evil", &d2, unix_now()),
            ),
        ),
        (
            "MXID does not resolve to seat_did (replayed endorsement)",
            "mxid_mismatch",
            make_reason(
                &key2,
                &end("stolen-seat", &sim2_seat_did, "sim-host2", &d2, unix_now()),
            ),
        ),
    ];
    for (name, code, reason) in cases {
        let r = kagent
            .client()
            .knock(rid.clone(), Some(reason), vec![server.clone()])
            .await;
        r.unwrap_or_else(|e| panic!("{name}: the knock itself must reach the hub: {e}"));
        let line = audit.wait_for(&kmxid, "declined", Some(code), 60).await;
        assert_eq!(line["code"], code);
        // never invited, never joined
        let _ = kagent.sync_once_nowait().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = kagent.sync_once_nowait().await;
        let state = kagent
            .client()
            .get_room(<&matrix_sdk::ruma::RoomId>::try_from(room_id.as_str()).unwrap())
            .map(|r| r.state());
        assert!(
            !matches!(
                state,
                Some(matrix_sdk::RoomState::Joined | matrix_sdk::RoomState::Invited)
            ),
            "{name}: the knocker must not get in, state {state:?}"
        );
        rep.ok(&format!("negative: {name}"), format!("declined: {code}"));
    }
    let r = hub
        .call(Request::Roster {
            seat: "seat-test-b".into(),
        })
        .await;
    assert!(
        !r["peers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["mxid"].as_str() == Some(kmxid.as_str())),
        "the knocker never appears on the roster"
    );
    rep.ok("knocker (all 7 negative cases) never on the roster", "ok");

    // -- hub offline: members keep working ------------------------------------------------------------------------------------------
    hub.stop().await;
    let t_off = format!("xhost hub offline {n}");
    let s = send(
        &h3,
        "seat-test-b",
        "room",
        &t_off,
        Some(json!("seat-test-a@sim-host2")),
    )
    .await
    .unwrap();
    let got = wait_for(&h2, "seat-test-a", &t_off, 90).await;
    assert!(got.direct && got.from_host.as_deref() == Some("sim-host3"));
    rep.ok(
        "hub offline: seats already in the room keep messaging",
        format!("event {}", s["event_id"].as_str().unwrap()),
    );

    // -- new joins wait for the hub, then complete when it is back ---------------------------------------------------------------------
    let _ = h2
        .try_call(Request::Leave {
            seat: "seat-test-a".into(),
        })
        .await;
    let c2 = h2.client.clone();
    let pending = tokio::spawn(async move {
        c2.call(
            &Request::Whoami {
                seat: "seat-test-a".into(),
            },
            Duration::from_secs(400),
        )
        .await
    });
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert!(
        !pending.is_finished(),
        "with the hub offline the join must wait, not fail or be admitted"
    );
    let audit2 = Audit::new(&hub_root);
    let hub = Proc::start("hub-again", &hub_env, rt.join("xhost-hub.sock")).await;
    let w = tokio::time::timeout(Duration::from_secs(170), pending)
        .await
        .expect("the waiting join completes once the hub is back")
        .unwrap()
        .expect("whoami after the hub returned");
    assert_eq!(w["connected"], true);
    audit2.wait_for(&admitted[0].3, "invited", None, 30).await;
    rep.ok(
        "hub offline: a new join waits, then completes when the hub returns",
        "invited after restart",
    );
    hub.stop().await;
    h2.stop().await;
    h3.stop().await;
    let _ = (hub_c, std::fs::remove_file(&hub_hosts));
    println!("\nS5 summary: {} scenarios passed (run {n})", rep.0);
}

trait UnwrapErrOrOk {
    fn unwrap_err_or_ok(self);
}
impl UnwrapErrOrOk for Result<(), String> {
    fn unwrap_err_or_ok(self) {
        // the unique-bare-name send must work
        if let Err(e) = self {
            panic!("a unique bare name must resolve: {e}");
        }
    }
}
