//! The daemon core: the registry of seats, lazy attach, idle detach, and the
//! dispatch of socket requests. One Client and crypto store per seat, owned by
//! that seat's runner task (`seat.rs`); nothing else ever opens them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aqua_matrix_agent::RoomMention;
use aqua_messenger::format;
use aqua_messenger::inbox::Actor;
use aqua_messenger::proto::Response;
use aqua_seat::paths::SeatPaths;
use aqua_seat::proto::{Item, Peer, Request, Via, MAX_MESSAGE_BYTES, MAX_WAIT_SECS};
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};

use crate::keys::ensure_key;
use crate::roster;
use crate::seat::{
    start_runner, unix_ms, RoomFile, RunHandle, RunState, SeatCmd, SeatShared, SendTo,
};

/// Daemon settings.
#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub sock: PathBuf,
    pub siwx_url: String,
    pub matrix_url: String,
    pub host: String,
    /// A seat nobody has touched for this long stops syncing.
    pub idle_detach: Duration,
    pub rate_count: usize,
    pub rate_window: Duration,
    /// How long an attach waits for login + roster join.
    pub attach_timeout: Duration,
    /// D9: this daemon is the hub: it creates the roster room, invites into
    /// it and decides on knocks from other hosts.
    pub hub: bool,
    /// This host's public DID (D8).
    pub host_did: String,
    /// The hub's allow-list of hosts.
    pub hosts_file: PathBuf,
    /// How long a seat waits to be let into the roster room.
    pub join_wait: Duration,
}

pub struct SeatEntry {
    pub shared: Arc<SeatShared>,
    run: tokio::sync::Mutex<Option<RunHandle>>,
}

pub struct Daemon {
    pub cfg: Config,
    seats: Mutex<HashMap<String, Arc<SeatEntry>>>,
    /// Serialises "create the roster room" across seats.
    pub roster_create: tokio::sync::Mutex<()>,
    pub shutdown: Notify,
    started: Instant,
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn threads() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Threads:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn actor(by: Option<&str>, session: Option<&str>) -> Actor {
    Actor {
        by: format::sanitize_origin(by.unwrap_or("")),
        session: format::sanitize_session(session),
    }
}

impl Daemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            seats: Mutex::new(HashMap::new()),
            roster_create: tokio::sync::Mutex::new(()),
            shutdown: Notify::new(),
            started: Instant::now(),
        })
    }

    /// The seat's entry, created (key minted if the name is new) on first use.
    pub fn entry(&self, seat: &str) -> Result<Arc<SeatEntry>, String> {
        let paths = SeatPaths::new(&self.cfg.root, seat).map_err(|e| e.to_string())?;
        let mut seats = self.seats.lock().unwrap();
        if let Some(e) = seats.get(seat) {
            return Ok(e.clone());
        }
        let info = ensure_key(&paths).map_err(|e| format!("provisioning seat {seat}: {e:#}"))?;
        if info.minted {
            tracing::info!(%seat, did = %info.did, "minted a new seat identity");
        }
        let shared = SeatShared::new(
            seat,
            paths,
            &self.cfg.host,
            &info.did,
            (self.cfg.rate_count, self.cfg.rate_window),
        );
        let e = Arc::new(SeatEntry {
            shared,
            run: tokio::sync::Mutex::new(None),
        });
        seats.insert(seat.to_string(), e.clone());
        Ok(e)
    }

    fn entries(&self) -> Vec<Arc<SeatEntry>> {
        self.seats.lock().unwrap().values().cloned().collect()
    }

    /// Make sure the seat's runner is going; optionally wait until it is ready.
    pub async fn attach(self: &Arc<Self>, seat: &str, wait: bool) -> Result<Arc<SeatEntry>, String> {
        let e = self.entry(seat)?;
        e.shared.touch();
        {
            let mut run = e.run.lock().await;
            let dead = run.as_ref().is_none_or(|h| h.join.is_finished());
            if dead {
                *run = Some(start_runner(self.clone(), e.shared.clone()));
            }
        }
        if wait {
            self.wait_ready(&e).await?;
        }
        Ok(e)
    }

    async fn wait_ready(&self, e: &Arc<SeatEntry>) -> Result<(), String> {
        let mut rx = e.shared.run_state.subscribe();
        let wait = async {
            loop {
                match &*rx.borrow_and_update() {
                    RunState::Ready => return Ok(()),
                    RunState::Failed(m) => return Err(m.clone()),
                    RunState::Stopped => return Err("the seat was stopped".to_string()),
                    RunState::Starting => {}
                }
                if rx.changed().await.is_err() {
                    return Err("the seat runner disappeared".to_string());
                }
            }
        };
        match tokio::time::timeout(self.cfg.attach_timeout, wait).await {
            Ok(r) => r,
            Err(_) => {
                let last = e.shared.identity.lock().unwrap().last_error.clone();
                Err(format!(
                    "seat {} is not ready after {}s{}",
                    e.shared.seat,
                    self.cfg.attach_timeout.as_secs(),
                    last.map(|l| format!(" (last error: {l})")).unwrap_or_default()
                ))
            }
        }
    }

    pub async fn detach(&self, seat: &str) -> Result<bool, String> {
        let e = self.entry(seat)?;
        let handle = e.run.lock().await.take();
        let Some(h) = handle else {
            return Ok(false);
        };
        h.stop.notify_one();
        let mut join = h.join;
        if tokio::time::timeout(Duration::from_secs(30), &mut join).await.is_err() {
            join.abort();
            let _ = join.await;
        }
        e.shared.run_state.send_replace(RunState::Stopped);
        e.shared.identity.lock().unwrap().connected = false;
        tracing::info!(%seat, "seat detached");
        Ok(true)
    }

    pub async fn detach_all(&self) {
        for e in self.entries() {
            let _ = self.detach(&e.shared.seat).await;
        }
    }

    async fn send_cmd(&self, e: &Arc<SeatEntry>, cmd: SeatCmd) -> Result<(), String> {
        let tx = e
            .run
            .lock()
            .await
            .as_ref()
            .map(|h| h.tx.clone())
            .ok_or_else(|| format!("seat {} is not attached", e.shared.seat))?;
        tx.send(cmd)
            .await
            .map_err(|_| format!("seat {} is stopping", e.shared.seat))
    }

    /// The roster, from the cache when younger than `max_age`, else re-read on
    /// the seat's live Client.
    async fn roster_for(&self, e: &Arc<SeatEntry>, max_age: Duration) -> Result<Vec<Peer>, String> {
        {
            let c = e.shared.roster.lock().unwrap();
            if c.fetched.is_some_and(|t| t.elapsed() < max_age) {
                return Ok(c.peers.clone());
            }
        }
        let (tx, rx) = oneshot::channel();
        self.send_cmd(e, SeatCmd::RefreshRoster { done: tx }).await?;
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the seat stopped while reading the roster".into()),
            Err(_) => Err("reading the roster timed out".into()),
        }
    }

    /// Best-effort roster for rendering: never fails, may be stale.
    async fn peers_best_effort(&self, e: &Arc<SeatEntry>, max_age: Duration) -> Vec<Peer> {
        match self.roster_for(e, max_age).await {
            Ok(p) => p,
            Err(_) => e.shared.peers(),
        }
    }

    /// Ask the hub seat (the roster room's creator, the only member who may
    /// invite) to invite `mxid`. The creator is attached first if needed.
    pub async fn request_roster_invite(self: &Arc<Self>, requester: &str, mxid: &str, rf: &RoomFile) {
        if rf.created_by.is_empty() || rf.created_by == requester {
            return;
        }
        match self.attach(&rf.created_by, true).await {
            Ok(e) => {
                if !self.try_invite(&e, mxid).await {
                    tracing::warn!(seat = %rf.created_by, "the hub seat could not invite");
                }
            }
            Err(err) => tracing::warn!("attaching the hub seat {}: {err}", rf.created_by),
        }
    }

    /// The hub seat: the roster room's creator, on a hub daemon. It stays
    /// attached (it decides on knocks and invites).
    pub fn is_hub_seat(&self, seat: &str) -> bool {
        self.cfg.hub && RoomFile::load(&self.cfg.root).is_some_and(|rf| rf.created_by == seat)
    }

    /// On a hub daemon, bring the hub seat up at startup (if the room exists).
    pub fn start_hub_seat(self: &Arc<Self>) {
        if !self.cfg.hub {
            return;
        }
        let Some(rf) = RoomFile::load(&self.cfg.root).filter(|rf| !rf.created_by.is_empty()) else {
            return;
        };
        let d = self.clone();
        tokio::spawn(async move {
            match d.attach(&rf.created_by, false).await {
                Ok(_) => tracing::info!(seat = %rf.created_by, "hub seat attached"),
                Err(e) => tracing::warn!("attaching the hub seat {}: {e}", rf.created_by),
            }
        });
    }

    async fn try_invite(&self, e: &Arc<SeatEntry>, mxid: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .send_cmd(e, SeatCmd::Invite { mxid: mxid.to_string(), done: tx })
            .await
            .is_err()
        {
            return false;
        }
        matches!(tokio::time::timeout(Duration::from_secs(30), rx).await, Ok(Ok(Ok(()))))
    }

    /// Detach seats nobody has touched for `idle_detach`.
    pub async fn idle_sweep(&self) {
        let now = unix_ms();
        let idle = self.cfg.idle_detach.as_millis() as u64;
        for e in self.entries() {
            let running = e.run.lock().await.is_some();
            if !running
                || e.shared.waiters.load(Ordering::Relaxed) > 0
                || self.is_hub_seat(&e.shared.seat)
            {
                continue;
            }
            if now.saturating_sub(e.shared.last_touch.load(Ordering::Relaxed)) > idle {
                tracing::info!(seat = %e.shared.seat, "idle, detaching");
                let _ = self.detach(&e.shared.seat).await;
            }
        }
    }

    pub async fn idle_loop(self: Arc<Self>) {
        let period = (self.cfg.idle_detach / 4).clamp(Duration::from_secs(1), Duration::from_secs(30));
        let mut t = tokio::time::interval(period);
        loop {
            t.tick().await;
            self.idle_sweep().await;
        }
    }

    // ---- request dispatch --------------------------------------------------

    pub async fn handle(self: &Arc<Self>, req: Request) -> Response {
        match self.dispatch(req).await {
            Ok(v) => Response::ok(v),
            Err(e) => Response::err(e),
        }
    }

    async fn dispatch(self: &Arc<Self>, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(json!({"pong": true, "host": self.cfg.host})),
            Request::Stats => Ok(self.stats()),
            Request::Shutdown => {
                self.shutdown.notify_one();
                Ok(json!({"stopping": true}))
            }
            Request::Attach { seat } => {
                let e = self.attach(&seat, true).await?;
                Ok(self.whoami(&e))
            }
            Request::Detach { seat } => {
                let was = self.detach(&seat).await?;
                Ok(json!({"seat": seat, "was_attached": was}))
            }
            Request::Whoami { seat } => {
                let e = self.attach(&seat, true).await?;
                Ok(self.whoami(&e))
            }
            Request::Roster { seat } => {
                let e = self.attach(&seat, true).await?;
                let peers = self.roster_for(&e, Duration::from_secs(5)).await?;
                Ok(self.roster_json(&e, &peers))
            }
            Request::Send { seat, to, text, reply_to, mention } => {
                self.op_send(&seat, &to, &text, reply_to.as_deref(), mention.as_ref()).await
            }
            Request::Inbox { seat, since, addressed_only, limit, by, session } => {
                let e = self.attach(&seat, true).await?;
                let a = actor(by.as_deref(), session.as_deref());
                let peers = self.peers_best_effort(&e, Duration::from_secs(30)).await;
                let items = {
                    let mut st = e.shared.store.lock().unwrap();
                    let entries = st.read(since, addressed_only, limit.unwrap_or(100).clamp(1, 500));
                    // Shown with the state they had before this read: `new`
                    // means this is the first time any session sees it.
                    let items = st.items(&entries, &peers);
                    let seqs: Vec<u64> = entries.iter().map(|x| x.seq).collect();
                    st.mark_seen(&seqs, &a);
                    items
                };
                Ok(items_json(&items, "inbox"))
            }
            Request::Wait { seat, timeout_secs, after_seq, addressed_only, by, session } => {
                let e = self.attach(&seat, true).await?;
                let a = actor(by.as_deref(), session.as_deref());
                let entries = self
                    .wait_new(&e, timeout_secs.clamp(1, MAX_WAIT_SECS), after_seq, addressed_only)
                    .await;
                let peers = if entries.is_empty() {
                    Vec::new()
                } else {
                    let known = e.shared.peers();
                    let all_known = entries
                        .iter()
                        .all(|x| known.iter().any(|p| p.mxid.eq_ignore_ascii_case(&x.sender)));
                    if all_known {
                        known
                    } else {
                        self.peers_best_effort(&e, Duration::ZERO).await
                    }
                };
                let items = {
                    let mut st = e.shared.store.lock().unwrap();
                    let items = st.items(&entries, &peers);
                    let seqs: Vec<u64> = entries.iter().map(|x| x.seq).collect();
                    st.mark_seen(&seqs, &a);
                    items
                };
                Ok(items_json(&items, if items.is_empty() { "timed out" } else { "wait" }))
            }
            Request::MarkProcessed { seat, seqs, note, by, session } => {
                let e = self.attach(&seat, false).await?;
                let note = format::sanitize_note(&note).ok_or("a note saying what was done is required")?;
                let a = actor(by.as_deref(), session.as_deref());
                let r = e.shared.store.lock().unwrap().mark_processed(&seqs, &a, &note);
                Ok(json!({
                    "marked": r.marked,
                    "already_processed": r.already.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
                    "missing": r.missing,
                }))
            }
            Request::Listen { seat, timeout_secs } => {
                let e = self.attach(&seat, true).await?;
                self.op_listen(&e, timeout_secs.clamp(1, MAX_WAIT_SECS)).await
            }
            Request::Unread { seat } => {
                // Non-blocking: start syncing in the background, answer from disk.
                let e = self.attach(&seat, false).await?;
                let n = e.shared.store.lock().unwrap().unread_direct();
                Ok(json!({"seat": seat, "unread_direct": n}))
            }
        }
    }

    fn whoami(&self, e: &Arc<SeatEntry>) -> Value {
        let sh = &e.shared;
        let id = sh.identity.lock().unwrap().clone();
        let shape = sh.room_shape.lock().unwrap().clone();
        json!({
            "seat": sh.seat,
            "did": sh.did,
            "mxid": id.mxid,
            "device_id": id.device_id,
            "host": sh.host,
            "display_name": roster::display_name(&sh.seat, &sh.host),
            "roster_room": sh.roster_room_id(),
            "roster_join_rule": shape.as_ref().map(|s| s.join_rule.clone()),
            "roster_alias": shape.as_ref().and_then(|s| s.alias.clone()),
            "roster_alias_resolves_here": shape.as_ref().map(|s| s.alias_resolves_here),
            "host_did": self.cfg.host_did,
            "hub": self.cfg.hub,
            "connected": id.connected,
            "homeserver": self.cfg.matrix_url,
        })
    }

    fn roster_json(&self, e: &Arc<SeatEntry>, peers: &[Peer]) -> Value {
        let own = e.shared.mxid().unwrap_or_default();
        let local: Vec<(String, bool)> = self
            .entries()
            .iter()
            .filter_map(|x| {
                x.shared
                    .mxid()
                    .map(|m| (m.to_ascii_lowercase(), *x.shared.run_state.borrow() == RunState::Ready))
            })
            .collect();
        let list: Vec<Value> = peers
            .iter()
            .map(|p| {
                let l = local.iter().find(|(m, _)| m.eq_ignore_ascii_case(&p.mxid));
                json!({
                    "seat": p.seat, "did": p.did, "host": p.host, "mxid": p.mxid,
                    "self": p.mxid.eq_ignore_ascii_case(&own),
                    "local": l.is_some(),
                    "attached": l.is_some_and(|(_, up)| *up),
                })
            })
            .collect();
        json!({"room_id": e.shared.roster_room_id(), "peers": list, "note": "claims are each seat's own assertion; the DID-MXID binding is siwx-oidc's"})
    }

    async fn wait_new(
        &self,
        e: &Arc<SeatEntry>,
        timeout_secs: u64,
        after_seq: Option<u64>,
        addressed_only: bool,
    ) -> Vec<aqua_messenger::inbox::InboxEntry> {
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
        impl<'a> Waiting<'a> {
            fn new(c: &'a std::sync::atomic::AtomicUsize) -> Self {
                c.fetch_add(1, Ordering::Relaxed);
                Self(c)
            }
        }
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _w = Waiting::new(&e.shared.waiters);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let notified = e.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            e.shared.touch();
            let found = e.shared.store.lock().unwrap().new_entries(after_seq, addressed_only);
            if !found.is_empty() {
                return found;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return Vec::new(),
            }
        }
    }

    async fn op_listen(&self, e: &Arc<SeatEntry>, timeout_secs: u64) -> Result<Value, String> {
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        e.shared.waiters.fetch_add(1, Ordering::Relaxed);
        let _w = Waiting(&e.shared.waiters);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let notified = e.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            e.shared.touch();
            let next = {
                let mut st = e.shared.store.lock().unwrap();
                let n = st.next_unannounced_direct();
                if let Some(x) = &n {
                    st.advance_listen_cursor(x.seq);
                }
                n
            };
            if let Some(entry) = next {
                let peers = self.peers_best_effort(e, Duration::from_secs(30)).await;
                let items = e.shared.store.lock().unwrap().items(&[entry], &peers);
                let it = &items[0];
                return Ok(json!({
                    "found": true,
                    "seq": it.seq, "event_id": it.event_id, "ts_ms": it.ts_ms,
                    "via": it.via, "from_mxid": it.from_mxid,
                    "from_seat": it.from_seat, "from_did": it.from_did, "from_host": it.from_host,
                    "body": it.body,
                }));
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(json!({"found": false})),
            }
        }
    }

    async fn op_send(
        self: &Arc<Self>,
        seat: &str,
        to: &str,
        text: &str,
        reply_to: Option<&str>,
        mention: Option<&Value>,
    ) -> Result<Value, String> {
        check_text(text)?;
        let e = self.attach(seat, true).await?;
        let sh = &e.shared;
        let own = sh.mxid().ok_or("the seat has no Matrix identity yet")?;
        let peers = self.roster_for(&e, Duration::from_secs(5)).await?;
        let room_id = sh.roster_room_id().ok_or("the seat has no roster room yet")?;

        let to_trim = to.trim();
        let to_room = matches!(to_trim.to_ascii_lowercase().as_str(), "room" | "roster" | "claude-seats");
        let mention_flag = match mention {
            None | Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(None),
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(Some(s.trim().to_string())),
            Some(other) => return Err(format!("mention must be true or a seat name, got {other}")),
        };

        // Who the message is for, where it goes.
        let (dest, target, via): (SendTo, Option<Peer>, &str);
        if to_room {
            match mention_flag {
                None => {
                    dest = SendTo::Room(room_id.clone());
                    target = None;
                    via = "room";
                }
                Some(Some(name)) => {
                    let p = roster::resolve(&peers, &name)?.clone();
                    dest = SendTo::Room(room_id.clone());
                    target = Some(p);
                    via = "mention";
                }
                Some(None) => {
                    return Err("`mention: true` needs a seat in `to`; for the room, name the seat to mention".into())
                }
            }
        } else {
            let p = roster::resolve(&peers, to_trim)?.clone();
            if p.mxid.eq_ignore_ascii_case(&own) {
                return Err("that is your own seat".into());
            }
            if mention_flag.is_some() {
                dest = SendTo::Room(room_id.clone());
                via = "mention";
            } else {
                dest = SendTo::Dm(p.mxid.clone());
                via = "dm";
            }
            target = Some(p);
        }
        if let Some(p) = &target {
            if p.mxid.eq_ignore_ascii_case(&own) {
                return Err("you cannot mention your own seat".into());
            }
        }

        let mut content = match (&target, via) {
            (Some(p), "mention") => aqua_matrix_agent::mention_content(
                text,
                &RoomMention { user_id: &p.mxid, display: &p.seat },
            )
            .map_err(|e| format!("{e:#}"))?,
            _ => RoomMessageEventContent::text_markdown(text),
        };
        if let Some(r) = reply_to.map(str::trim).filter(|r| !r.is_empty()) {
            let event_id = if let Ok(seq) = r.parse::<u64>() {
                sh.store
                    .lock()
                    .unwrap()
                    .event_id_of_seq(seq)
                    .ok_or_else(|| format!("no inbox entry with seq {seq}"))?
            } else if r.starts_with('$') && r.len() > 1 && r.len() <= 255 && !r.chars().any(|c| c.is_whitespace() || c.is_control()) {
                r.to_string()
            } else {
                return Err(format!("reply_to {r:?} is neither an inbox seq nor a Matrix event id"));
            };
            let id: matrix_sdk::ruma::OwnedEventId = event_id.as_str().try_into().map_err(|e| format!("bad event id: {e}"))?;
            content.relates_to = Some(Relation::Reply(matrix_sdk::ruma::events::relation::Reply::with_event_id(id)));
        }

        // Per-recipient budget: the recipient's MXID, or the room.
        let key = match &dest {
            SendTo::Dm(m) => m.clone(),
            SendTo::Room(r) => match &target {
                Some(p) if via == "mention" => format!("{r}#{}", p.mxid),
                _ => r.clone(),
            },
        };
        acquire_budget(
            &mut sh.rate.lock().unwrap(),
            &key,
            Instant::now(),
            self.cfg.rate_count,
            self.cfg.rate_window,
        )?;
        let (tx, rx) = oneshot::channel();
        let sent = async {
            self.send_cmd(&e, SeatCmd::Send { to: dest, content: Box::new(content), done: tx }).await?;
            match tokio::time::timeout(Duration::from_secs(150), rx).await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => Err("the seat stopped before the message was sent".to_string()),
                Err(_) => Err("no result within 150s; the outcome is unknown (check the room before retrying)".to_string()),
            }
        }
        .await;
        match sent {
            Ok(info) => {
                let remaining = sh.rate.lock().unwrap().remaining(&key, Instant::now());
                Ok(json!({
                    "event_id": info.event_id,
                    "encrypted": info.encrypted,
                    "via": via,
                    "to": target.as_ref().map(|p| json!({"seat": p.seat, "did": p.did, "host": p.host, "mxid": p.mxid})),
                    "sends_left_to_this_recipient": remaining,
                }))
            }
            Err(err) => {
                sh.rate.lock().unwrap().release(&key);
                Err(err)
            }
        }
    }

    fn stats(&self) -> Value {
        let seats: Vec<Value> = self
            .entries()
            .iter()
            .map(|e| {
                let sh = &e.shared;
                let id = sh.identity.lock().unwrap().clone();
                let c = &sh.counters;
                json!({
                    "seat": sh.seat,
                    "state": format!("{:?}", *sh.run_state.borrow()),
                    "connected": id.connected,
                    "mxid": id.mxid,
                    "ingested": c.ingested.load(Ordering::Relaxed),
                    "sent": c.sent.load(Ordering::Relaxed),
                    "utd_seen": c.utd_seen.load(Ordering::Relaxed),
                    "utd_recovered": c.utd_recovered.load(Ordering::Relaxed),
                    "utd_unresolved": sh.utd_open.lock().unwrap().len(),
                    "backfilled": c.backfilled.load(Ordering::Relaxed),
                    "cycles": c.cycles.load(Ordering::Relaxed),
                    "waiters": sh.waiters.load(Ordering::Relaxed),
                    "inbox_len": sh.store.lock().unwrap().len(),
                })
            })
            .collect();
        json!({
            "pid": std::process::id(),
            "rss_kb": rss_kb(),
            "threads": threads(),
            "uptime_s": self.started.elapsed().as_secs(),
            "host": self.cfg.host,
            "seats": seats,
        })
    }
}

/// Take one send from the recipient's budget, or say when the next is free.
pub fn acquire_budget(
    rl: &mut aqua_messenger::ratelimit::RateLimiter,
    key: &str,
    now: Instant,
    count: usize,
    window: Duration,
) -> Result<(), String> {
    rl.try_acquire(key, now).map_err(|wait| {
        format!(
            "rate limit: at most {count} sends per {} s to the same recipient; retry in {} s",
            window.as_secs(),
            wait.as_secs() + 1
        )
    })
}

/// The size cap on an outgoing message.
pub fn check_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("empty message".into());
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(format!(
            "message is {} bytes; the cap is {MAX_MESSAGE_BYTES}",
            text.len()
        ));
    }
    Ok(())
}

fn items_json(items: &[Item], note: &str) -> Value {
    let direct = items.iter().filter(|i| i.direct).count();
    let unread_hint = format!("{} entries ({} direct); {note}", items.len(), direct);
    json!({"items": items, "note": unread_hint})
}

/// Keep `Via` linked for docs.
#[allow(dead_code)]
fn _via(_: Via) {}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_messenger::ratelimit::RateLimiter;

    #[test]
    fn twenty_sends_per_ten_minutes_per_recipient_then_refused() {
        let (count, window) = (aqua_seat::proto::RATE_LIMIT_COUNT, Duration::from_secs(aqua_seat::proto::RATE_LIMIT_WINDOW_SECS));
        assert_eq!((count, window.as_secs()), (20, 600));
        let mut rl = RateLimiter::new(count, window);
        let t0 = Instant::now();
        for i in 0..20 {
            acquire_budget(&mut rl, "@b:x", t0, count, window).unwrap_or_else(|e| panic!("send {i}: {e}"));
        }
        let e = acquire_budget(&mut rl, "@b:x", t0, count, window).unwrap_err();
        assert!(e.contains("rate limit") && e.contains("20 sends per 600"), "{e}");
        // another recipient has its own budget
        acquire_budget(&mut rl, "@c:x", t0, count, window).unwrap();
        // and the window recovers
        acquire_budget(&mut rl, "@b:x", t0 + window, count, window).unwrap();
    }

    #[test]
    fn message_size_cap_is_20_kb() {
        check_text("hello").unwrap();
        check_text(&"x".repeat(20_000)).unwrap();
        let e = check_text(&"x".repeat(20_001)).unwrap_err();
        assert!(e.contains("cap"), "{e}");
        assert!(check_text("  \n ").is_err());
    }
}
