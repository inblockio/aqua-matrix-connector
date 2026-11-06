//! The in-process messenger backend for an agent that embeds the connector.
//!
//! **One Client per crypto store.** The messenger never builds a Matrix
//! Client: [`LiveClient`] borrows the agent's own live [`AgentClient`] between
//! [`AgentMessenger::attach`] and [`AgentMessenger::detach`]. Every Matrix
//! operation holds a read guard for its whole duration; `detach` takes the
//! write guard, so it waits for in-flight operations and the old Client is
//! never used (or kept alive) next to its successor after a rotation. Tool
//! calls made while detached fail fast with "not connected" (the model is told
//! to retry), they do not queue.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::Context as _;
use aqua_matrix_agent::AgentClient;
use aqua_messenger::allowlist::{is_valid_mxid, AllowList, Recipient};
use aqua_messenger::{
    server, Dest, Engine, FetchRequest, Fetched, Profile, ReplyRef, Sent, Transport, SOCK_ENV,
};
use async_trait::async_trait;
use matrix_sdk::event_handler::EventHandlerHandle;
use serde_json::{json, Value};
use tokio::sync::RwLock;

/// `--mcp-config` server key; tools appear to the model as
/// `mcp__messenger__<tool>`.
pub const MESSENGER_SERVER_KEY: &str = "messenger";

/// Upper bound on one send (the connector's own retries included).
const SEND_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound on one attachment download + decrypt.
const FETCH_TIMEOUT: Duration = Duration::from_secs(150);

/// Transport over the embedding agent's current Client (or none while
/// detached).
#[derive(Clone, Default)]
pub struct LiveClient {
    current: Arc<RwLock<Option<AgentClient>>>,
    /// The engine that owns this transport, so a send can read the allow-list
    /// (the observers) before it decides which room is the DM.
    engine: Arc<OnceLock<Weak<Engine<LiveClient>>>>,
}

const NOT_CONNECTED: &str = "the agent is not connected to Matrix right now (reconnecting); nothing was sent, try again shortly";

impl LiveClient {
    /// Remember the engine (once, right after it is built).
    fn bind_engine(&self, engine: &Arc<Engine<LiveClient>>) {
        let _ = self.engine.set(Arc::downgrade(engine));
    }

    fn engine(&self) -> Option<Arc<Engine<LiveClient>>> {
        self.engine.get().and_then(Weak::upgrade)
    }

    /// The observers in the engine's allow-list now (reloaded if the file
    /// changed). None when no engine is bound. What `sync_observers` hands to
    /// the client; kept apart so a test can read it without a Matrix client.
    #[cfg(test)]
    fn bound_observers(&self) -> Vec<aqua_messenger::allowlist::Observer> {
        self.engine()
            .map(|e| e.with_allow(|a| a.observers().to_vec()))
            .unwrap_or_default()
    }

    /// Hand the current observers to `agent` before a DM decision, as the host
    /// bridge does, so a hot reload of the allow-list reaches the send path.
    fn sync_observers(&self, agent: &AgentClient) {
        if let Some(engine) = self.engine() {
            crate::inbound::sync_dm_observers(agent, &engine);
        }
    }

    async fn set(&self, agent: Option<AgentClient>) {
        *self.current.write().await = agent;
    }

    pub async fn is_attached(&self) -> bool {
        self.current.read().await.is_some()
    }
}

fn timed_out(what: &str, d: Duration) -> String {
    format!("{what} timed out after {}s (outcome unknown)", d.as_secs())
}

/// The room a send to `to` goes to. A person's DM is the connector's true
/// 1:1 resolution (created if missing); when replying to an inbox message,
/// the room that message is in wins if it is still a 1:1 DM with them.
async fn room_for(
    live: &LiveClient,
    agent: &AgentClient,
    to: &Dest,
    reply: Option<&ReplyRef>,
) -> anyhow::Result<String> {
    live.sync_observers(agent);
    match to {
        Dest::Room(id) => Ok(id.clone()),
        Dest::Person(mxid) => {
            if let Some(rid) = reply.and_then(|r| r.room_id.as_deref()) {
                if agent.is_one_to_one_dm(rid, mxid).await.unwrap_or(false) {
                    return Ok(rid.to_string());
                }
            }
            agent.dm_room_for(mxid).await
        }
    }
}

#[async_trait]
impl Transport for LiveClient {
    async fn send_text(
        &self,
        to: &Dest,
        markdown: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        let guard = self.current.read().await;
        let agent = guard.as_ref().ok_or(NOT_CONNECTED)?;
        let fut = async {
            let room = room_for(self, agent, to, reply).await?;
            crate::outbound::send_text(agent, &room, markdown, reply).await
        };
        tokio::time::timeout(SEND_TIMEOUT, fut)
            .await
            .map_err(|_| timed_out("send", SEND_TIMEOUT))?
            .map_err(|e| format!("{e:#}"))
    }

    async fn send_file(
        &self,
        to: &Dest,
        path: &Path,
        caption: &str,
        reply: Option<&ReplyRef>,
    ) -> Result<Sent, String> {
        let guard = self.current.read().await;
        let agent = guard.as_ref().ok_or(NOT_CONNECTED)?;
        let fut = async {
            let room = room_for(self, agent, to, reply).await?;
            crate::outbound::send_file(agent, &room, path, caption, reply).await
        };
        tokio::time::timeout(SEND_TIMEOUT, fut)
            .await
            .map_err(|_| timed_out("send", SEND_TIMEOUT))?
            .map_err(|e| format!("{e:#}"))
    }

    async fn fetch(&self, req: FetchRequest) -> Result<Fetched, String> {
        let guard = self.current.read().await;
        let agent = guard.as_ref().ok_or(NOT_CONNECTED)?;
        let fut = crate::media::fetch(
            agent.client(),
            &req.room_id,
            &req.event_id,
            req.media,
            req.max_bytes,
        );
        let (bytes, mimetype) = tokio::time::timeout(FETCH_TIMEOUT, fut)
            .await
            .map_err(|_| timed_out("fetch", FETCH_TIMEOUT))?
            .map_err(|e| format!("{e:#}"))?;
        Ok(Fetched { bytes, mimetype })
    }
}

/// Explicit configuration. [`AgentMessenger::enable_default`] covers the
/// common case.
#[derive(Debug, Clone)]
pub struct MessengerConfig {
    /// Inbox, audit log and attachments (mode 700). Must NOT be the crypto
    /// store directory itself; a sibling or subdirectory is fine.
    pub state_dir: PathBuf,
    /// The owner: always allow-listed, the default `to`.
    pub owner_mxid: String,
    /// Allow-list name of the owner (default `owner`).
    pub owner_name: String,
    /// Optional extra allow-list file (same TOML as the host bridge:
    /// `[[recipients]]` and `[[rooms]]`). Absent = owner only. Anything beyond
    /// the owner is explicit config.
    pub extra_allowlist: Option<PathBuf>,
    /// Tools, limits and texts. Default: [`Profile::embedded_agent`].
    pub profile: Profile,
}

impl MessengerConfig {
    pub fn owner_only(state_dir: &Path, label: &str, owner_mxid: &str) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            owner_mxid: owner_mxid.to_string(),
            owner_name: "owner".into(),
            extra_allowlist: None,
            profile: Profile::embedded_agent(label, "owner"),
        }
    }
}

/// The embedded messenger: engine + live-Client transport + the inbound
/// handler registration for the current Client.
pub struct AgentMessenger {
    engine: Arc<Engine<LiveClient>>,
    live: LiveClient,
    /// The attached agent and its handler, removed on detach.
    attached: Mutex<Option<(AgentClient, EventHandlerHandle)>>,
}

impl AgentMessenger {
    /// The default tooling in one call: owner-only allow-list, default limits,
    /// tools `send_message`, `send_file`, `list_recipients`, `read_inbox`,
    /// `fetch_attachment` (no `wait_for_reply`: Tim, 2026-09-29).
    pub fn enable_default(state_dir: &Path, label: &str, owner_mxid: &str) -> anyhow::Result<Self> {
        Self::new(MessengerConfig::owner_only(state_dir, label, owner_mxid))
    }

    pub fn new(cfg: MessengerConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            is_valid_mxid(&cfg.owner_mxid),
            "owner {:?} is not a valid MXID",
            cfg.owner_mxid
        );
        ensure_private_dir(&cfg.state_dir)
            .with_context(|| format!("cannot create {}", cfg.state_dir.display()))?;
        let mut profile = cfg.profile;
        profile.default_to = Some(cfg.owner_name.clone());
        let owner = Recipient {
            name: cfg.owner_name,
            mxid: cfg.owner_mxid,
            note: Some("the agent's owner".into()),
        };
        let allow = AllowList::owner_only(owner, cfg.extra_allowlist, &profile.label);
        let live = LiveClient::default();
        let engine = Arc::new(Engine::new(profile, &cfg.state_dir, allow, live.clone()));
        live.bind_engine(&engine);
        let pruned = engine.prune_attachments();
        if pruned > 0 {
            tracing::info!(
                pruned,
                "messenger: removed fetched attachments past retention"
            );
        }
        Ok(Self {
            engine,
            live,
            attached: Mutex::new(None),
        })
    }

    /// The engine, for direct in-process calls (`engine.handle(Request)` or
    /// `aqua_messenger::mcp::call_tool(&**engine, ...)`).
    pub fn engine(&self) -> &Arc<Engine<LiveClient>> {
        &self.engine
    }

    /// Start using `agent` (the agent's own live Client): snapshot its joined
    /// rooms, register the inbound handler and route tool calls to it. Call
    /// once per connect cycle, after the catch-up sync. Replaces any previous
    /// attachment.
    pub async fn attach(&self, agent: &AgentClient) {
        self.detach().await;
        crate::inbound::survey_rooms(agent, &self.engine, false).await;
        let handle = crate::inbound::register_message_handler(agent, self.engine.clone());
        *self.attached.lock().unwrap() = Some((agent.clone(), handle));
        self.live.set(Some(agent.clone())).await;
        self.engine.set_status(|s| {
            s.connected = true;
            s.did = Some(agent.did().to_string());
            s.user_id = Some(agent.user_id().to_string());
            s.device_id = agent.device_id();
        });
    }

    /// Ingest recent history (messages that arrived while detached).
    pub async fn backfill(&self, agent: &AgentClient) -> usize {
        crate::inbound::backfill(agent, &self.engine).await
    }

    /// Stop using the current Client: waits for in-flight operations, removes
    /// the inbound handler and drops every reference to the Client. Call it
    /// BEFORE the agent drops or rotates its Client.
    pub async fn detach(&self) {
        self.live.set(None).await;
        let prev = self.attached.lock().unwrap().take();
        if let Some((agent, handle)) = prev {
            agent.client().remove_event_handler(handle);
        }
        self.engine.set_status(|s| s.connected = false);
    }

    /// Serve the tools to a stdio MCP server (`aqua-messenger-mcp`) on a unix
    /// socket (mode 600). The returned endpoint stops serving when dropped.
    pub fn serve_mcp(&self, sock: &Path, mcp_binary: &Path) -> anyhow::Result<McpEndpoint> {
        let listener = server::bind_socket(sock)?;
        let task = tokio::spawn(server::serve(listener, self.engine.clone()));
        Ok(McpEndpoint {
            sock: sock.to_path_buf(),
            binary: mcp_binary.to_path_buf(),
            tools: self
                .engine
                .profile()
                .tools
                .iter()
                .map(|t| t.to_string())
                .collect(),
            task,
        })
    }
}

/// A served messenger socket plus what `claude` needs to reach it.
pub struct McpEndpoint {
    sock: PathBuf,
    binary: PathBuf,
    tools: Vec<String>,
    task: tokio::task::JoinHandle<()>,
}

impl McpEndpoint {
    /// `--mcp-config` JSON: one server, key [`MESSENGER_SERVER_KEY`].
    pub fn mcp_config(&self) -> Value {
        json!({"mcpServers": {MESSENGER_SERVER_KEY: {
            "command": self.binary,
            "args": [],
            "env": {SOCK_ENV: self.sock}
        }}})
    }

    /// The `--allowedTools` entries (`mcp__messenger__send_message`, ...).
    pub fn allowed_tools(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| format!("mcp__{MESSENGER_SERVER_KEY}__{t}"))
            .collect()
    }

    pub fn sock(&self) -> &Path {
        &self.sock
    }
}

impl Drop for McpEndpoint {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.sock);
    }
}

fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "@owner:matrix.test";

    fn dir(tag: &str) -> PathBuf {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let d = base
            .join(".cache")
            .join(format!("msgr-inproc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn at(p: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(std::time::SystemTime::now() + Duration::from_secs(secs))
            .unwrap();
    }

    /// An embedded agent's send path must see the observers the engine's
    /// allow-list holds NOW, also after a hot reload, not the ones it saw at
    /// attach (review C2/F2).
    #[test]
    fn the_send_path_reads_the_current_observers() {
        let d = dir("observers");
        let extra = d.join("extra.toml");
        std::fs::write(
            &extra,
            "[[observers]]\nname = \"reader\"\nmxid = \"@reader:matrix.test\"\nroom_id = \"!dm:matrix.test\"\n",
        )
        .unwrap();
        let mut cfg = MessengerConfig::owner_only(&d.join("state"), "Marina", OWNER);
        cfg.extra_allowlist = Some(extra.clone());
        let m = AgentMessenger::new(cfg).unwrap();
        let seen = m.live.bound_observers();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].room_id, "!dm:matrix.test");
        // the row is removed: the next read sees an empty set
        std::fs::write(&extra, "").unwrap();
        at(&extra, 10);
        assert!(m.live.bound_observers().is_empty());
        // a transport that was never bound to an engine has none
        assert!(LiveClient::default().bound_observers().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
