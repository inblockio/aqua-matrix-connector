//! Phase 3 orchestrator: drive rootless Podman (via the Docker-API-compatible
//! `bollard` crate) to spawn / replace / stop **typed agent containers**, harvest
//! their logs host-side, and expose a snapshot of active sessions for the
//! heartbeat's Matrix `#shell` / status surface.
//!
//! Modeled on `../agent-customer-portal/src/docker.rs` (the reference bollard
//! `ContainerManager`), adapted to the typed-agent-instance model:
//! [`aqua_matrix_template::ResolvedInstance`] in, one container = one agent =
//! one self-minted identity out.
//!
//! ## The decisive invariant (restart vs. replace)
//!
//! `/agent` (the container's self-minted `.pem` → DID, its Matrix crypto store,
//! and its claude memory) MUST live on the container's **writable layer**:
//!
//! - `restart` (crash → on-failure restart, or `stop`+`start` of the SAME
//!   container object): the writable layer is intact ⇒ the agent keeps its
//!   identity + memory.
//! - `replace` (`rm` + recreate): a fresh writable layer ⇒ a NEW identity, fresh
//!   context, full re-auth.
//!
//! Therefore [`ContainerManager::spawn`] deliberately:
//!   * does NOT set `readonly_rootfs`, and
//!   * does NOT put `/agent` on a named or anonymous volume.
//! Either of those would let identity survive a `replace`, breaking the model.
//! The only thing that survives replacement is the orchestrator-harvested log on
//! the host (`<sessions>/<id>/<incarnation>/agent.log`).
//!
//! ## podman compatibility notes
//!
//! Connects to rootless podman's Docker-compatible socket (default
//! `unix:///run/user/<uid>/podman/podman.sock`). podman advertises a slightly
//! lower API version than bollard's `API_DEFAULT_VERSION`; bollard down-negotiates
//! on the wire so the default version is a safe ceiling. We connect synchronously
//! (bollard is lazy — no I/O until the first request), so `new()` stays sync.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use aqua_matrix_template::ResolvedInstance;
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::models::{HostConfig, RestartPolicy, RestartPolicyNameEnum};
use bollard::{Docker, API_DEFAULT_VERSION};
use futures_util::StreamExt;
use tokio::sync::Mutex;

/// Container name prefix; the full name is `aqua-agent-<id>`.
const CONTAINER_PREFIX: &str = "aqua-agent-";
/// The repo root that relative `ref_mounts` host paths resolve against. A
/// `host_path` like `../aqua-rs-sdk` is interpreted relative to this directory
/// (so it points at the sibling checkout next to the repo).
const REPO_ROOT: &str = "/home/waldknoten-01/aqua-matrix-hello";
/// How long to poll for the agent's self-minted MXID in its logs after start.
/// Best-effort: a DID-pending container is still a successful spawn.
const DID_DISCOVERY_TIMEOUT_SECS: u64 = 15;

/// A public, cloneable snapshot of one live agent session. No secrets.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Instance slot label (e.g. `claude-channel-1717245000`).
    pub id: String,
    /// Full container id from podman.
    pub container_id: String,
    /// Container name (`aqua-agent-<id>`).
    pub container_name: String,
    /// The `AGENT_TARGET` MXID this instance reports to / accepts from.
    pub target: String,
    /// The agent's self-minted DID (`did:key:…`), discovered from its logs when
    /// it comes up. `None` while still pending.
    pub did: Option<String>,
    /// Unix seconds at spawn — the incarnation tag; also the host log dir name.
    pub incarnation_ts: u64,
    /// Monotonic start instant, for uptime reporting.
    pub started_at: Instant,
}

/// Internal per-session record: the public [`SessionInfo`] plus the things the
/// manager needs but does not expose — the [`ResolvedInstance`] (so `replace`
/// can re-spawn the same slot) and the log-capture task handle (so `stop` can
/// let it drain).
struct Session {
    info: SessionInfo,
    resolved: ResolvedInstance,
    log_task: Option<tokio::task::JoinHandle<()>>,
}

/// Drives rootless podman to manage typed agent containers.
pub struct ContainerManager {
    docker: Docker,
    /// `id` → live [`Session`].
    sessions: Mutex<HashMap<String, Session>>,
    /// Host-side session store: `<store_dir>/sessions`. Holds, per incarnation,
    /// the `config.json` mounted into the container and the harvested
    /// `agent.log` (provenance/audit that outlives a container replacement).
    sessions_dir: PathBuf,
}

impl ContainerManager {
    /// Connect to rootless podman and root the host-side session store under
    /// `store_dir` (defaulting to `~/.aqua-matrix-heartbeat`). Sync: bollard
    /// sets the connection up lazily, so no I/O happens here.
    pub fn new(store_dir: Option<&Path>) -> Result<Self> {
        let docker_host = podman_socket_uri();
        // `connect_with_socket` wants the cleaned path; it strips the scheme
        // itself, so passing the full `unix://...` URI is fine.
        let docker = Docker::connect_with_socket(&docker_host, 120, API_DEFAULT_VERSION)
            .with_context(|| format!("connect to podman socket {docker_host}"))?;

        let base = match store_dir {
            Some(p) => p.to_path_buf(),
            None => default_store_dir(),
        };
        let sessions_dir = base.join("sessions");

        Ok(Self {
            docker,
            sessions: Mutex::new(HashMap::new()),
            sessions_dir,
        })
    }

    /// Spawn a new typed agent container from a [`ResolvedInstance`].
    ///
    /// Returns a successful [`SessionInfo`] even when the DID is still pending —
    /// DID discovery is best-effort and must not gate the spawn.
    pub async fn spawn(&self, resolved: &ResolvedInstance) -> Result<SessionInfo> {
        let id = resolved.id.clone();
        let container_name = format!("{CONTAINER_PREFIX}{id}");
        let incarnation_ts = unix_now_secs();

        // --- host-side incarnation dir + config.json -----------------------
        let incarnation_dir = self.sessions_dir.join(&id).join(incarnation_ts.to_string());
        std::fs::create_dir_all(&incarnation_dir).with_context(|| {
            format!("create incarnation dir {}", incarnation_dir.display())
        })?;
        let config_host_path = incarnation_dir.join("config.json");
        let config_json = serde_json::to_string_pretty(resolved)
            .context("serialize ResolvedInstance to config.json")?;
        std::fs::write(&config_host_path, &config_json)
            .with_context(|| format!("write {}", config_host_path.display()))?;
        let config_host_path_str = config_host_path
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 config path: {}", config_host_path.display()))?
            .to_string();

        // --- env (incl. the auth secret) -----------------------------------
        // The secret VALUE is read from the orchestrator's own environment and
        // injected by NAME=value. We never store or log the value. Refuse to
        // spawn a token-less container.
        let env_var = &resolved.agent_type.auth.env_var;
        let secret = std::env::var(env_var).map_err(|_| {
            anyhow!(
                "orchestrator env lacks {env_var}; run `claude setup-token` and set it before spawning"
            )
        })?;
        let env = vec![
            format!("AGENT_TARGET={}", resolved.target),
            "AGENT_CONFIG_FILE=/agent/config.json".to_string(),
            format!("{env_var}={secret}"),
        ];

        // --- binds: config.json (RO) + each ref_mount (RO) -----------------
        let mut binds = vec![format!("{config_host_path_str}:/agent/config.json:ro")];
        for m in &resolved.agent_type.ref_mounts {
            let host = resolve_host_path(&m.host_path);
            let host_str = host
                .to_str()
                .ok_or_else(|| anyhow!("non-UTF-8 ref_mount host path: {}", host.display()))?;
            // ref_mounts are reference knowledge — always RO regardless of the
            // (carried) read_only flag; the model never writes to them.
            binds.push(format!("{host_str}:{}:ro", m.container_path));
        }

        // --- resources -----------------------------------------------------
        let res = &resolved.agent_type.resources;
        let memory = (res.memory_mb * 1024 * 1024) as i64;
        let nano_cpus = (res.cpus * 1e9) as i64;
        let pids_limit = res.pids_limit as i64;

        // --- /tmp tmpfs ----------------------------------------------------
        let mut tmpfs = HashMap::new();
        tmpfs.insert("/tmp".to_string(), String::new());

        // CRITICAL INVARIANT: NO `readonly_rootfs` and NO volume at `/agent`.
        // `/agent` must live on the writable layer so that `restart` (same
        // container) preserves the self-minted identity + memory while `replace`
        // (rm + recreate) resets them. A read-only rootfs, or a named/anonymous
        // volume mounted at `/agent`, would let identity survive replacement and
        // break the restart-vs-replace model. (See module docs.)
        let host_config = HostConfig {
            binds: Some(binds),
            memory: Some(memory),
            nano_cpus: Some(nano_cpus),
            pids_limit: Some(pids_limit),
            cap_drop: Some(vec!["ALL".to_string()]),
            security_opt: Some(vec!["no-new-privileges".to_string()]),
            tmpfs: Some(tmpfs),
            // on-failure with a small cap: a crash restarts the SAME container
            // (identity preserved). Boot-persistence across host reboots is a
            // later concern (systemd-managed; not handled here).
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::ON_FAILURE),
                maximum_retry_count: Some(5),
            }),
            ..Default::default()
        };

        let config = Config {
            image: Some(resolved.agent_type.image.clone()),
            env: Some(env),
            host_config: Some(host_config),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions {
            name: container_name.clone(),
            platform: None,
        };

        let created = self
            .docker
            .create_container(Some(create_opts), config)
            .await
            .with_context(|| format!("create container {container_name}"))?;

        self.docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("start container {container_name}"))?;

        tracing::info!(
            "spawned agent container {} ({}) for instance {} → target {}",
            short_id(&created.id),
            container_name,
            id,
            resolved.target,
        );

        // --- host-side log capture task ------------------------------------
        let log_path = incarnation_dir.join("agent.log");
        let log_task = self.start_log_capture(&created.id, &id, &log_path);

        // --- best-effort readiness + DID discovery -------------------------
        let did = self
            .discover_did(&created.id, &id, &log_path)
            .await;

        let info = SessionInfo {
            id: id.clone(),
            container_id: created.id.clone(),
            container_name,
            target: resolved.target.clone(),
            did,
            incarnation_ts,
            started_at: Instant::now(),
        };

        self.sessions.lock().await.insert(
            id,
            Session {
                info: info.clone(),
                resolved: resolved.clone(),
                log_task: Some(log_task),
            },
        );

        Ok(info)
    }

    /// Replace the container backing `id`: stop + remove the current one (its log
    /// is drained/harvested first), then `spawn` the SAME [`ResolvedInstance`]
    /// again. The new container has a fresh writable layer ⇒ a NEW identity.
    pub async fn replace(&self, id: &str) -> Result<SessionInfo> {
        // Pull the stored ResolvedInstance out so we can re-spawn it.
        let resolved = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(id)
                .map(|s| s.resolved.clone())
                .ok_or_else(|| anyhow!("no active session {id:?} to replace"))?
        };

        // Tear down the current incarnation (drains its log, then rm).
        self.stop(id).await?;

        // Re-spawn ⇒ new incarnation, new identity, same slot/type/target.
        self.spawn(&resolved).await
    }

    /// Stop and remove the container for `id`: let its log task drain, then
    /// `stop_container` (≈5 s grace) and `remove_container` (force, volumes).
    pub async fn stop(&self, id: &str) -> Result<()> {
        let session = self.sessions.lock().await.remove(id);
        let Some(mut session) = session else {
            bail!("no active session {id:?} to stop");
        };

        let container_id = session.info.container_id.clone();
        tracing::info!(
            "stopping agent container {} (instance {})",
            short_id(&container_id),
            id
        );

        // 1. SIGTERM → 5 s grace → SIGKILL. Best-effort: a podman hiccup must not
        //    leave the session half-tracked, so we log and continue to removal.
        if let Err(e) = self
            .docker
            .stop_container(&container_id, Some(StopContainerOptions { t: 5 }))
            .await
        {
            tracing::warn!("stop_container {} failed: {e}", short_id(&container_id));
        }

        // 2. Let the log-capture task drain the tail of output (up to 5 s) so the
        //    host-side agent.log is complete before the container disappears.
        if let Some(handle) = session.log_task.take() {
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("log task for {id} panicked: {e}"),
                Err(_) => tracing::warn!("log drain for {id} timed out (5s); removing anyway"),
            }
        }

        // 3. Remove (force; volumes — harmless here, /agent is on the writable
        //    layer, not a volume — but keeps an anonymous volume from leaking if
        //    the image ever declares one).
        self.docker
            .remove_container(
                &container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("remove container {}", short_id(&container_id)))?;

        Ok(())
    }

    /// Snapshot of all active sessions (cheap clone of public info).
    pub fn list(&self) -> Vec<SessionInfo> {
        // `try_lock` keeps this sync. Contention is only ever with our own async
        // ops, which are brief; on the rare miss we return an empty snapshot
        // rather than block, which is acceptable for a status read.
        match self.sessions.try_lock() {
            Ok(sessions) => sessions.values().map(|s| s.info.clone()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Last `n` lines of the current incarnation's host-side `agent.log`.
    pub async fn tail_log(&self, id: &str, n: usize) -> Result<String> {
        let log_path = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(id)
                .ok_or_else(|| anyhow!("no active session {id:?}"))?;
            self.sessions_dir
                .join(&session.info.id)
                .join(session.info.incarnation_ts.to_string())
                .join("agent.log")
        };

        let content = std::fs::read_to_string(&log_path)
            .with_context(|| format!("read {}", log_path.display()))?;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(n);
        Ok(lines[start..].join("\n"))
    }

    // -- internals ----------------------------------------------------------

    /// Spawn a tokio task that follows the container's stdout+stderr and appends
    /// each line to the host-side `agent.log` (provenance that survives a
    /// `replace`). Returns the JoinHandle so `stop` can let it drain.
    fn start_log_capture(
        &self,
        container_id: &str,
        id: &str,
        log_path: &Path,
    ) -> tokio::task::JoinHandle<()> {
        use std::io::Write;

        let docker = self.docker.clone();
        let container_id = container_id.to_string();
        let id = id.to_string();
        let log_path = log_path.to_path_buf();

        tokio::spawn(async move {
            let opts = LogsOptions::<String> {
                follow: true,
                stdout: true,
                stderr: true,
                timestamps: true,
                tail: "all".to_string(),
                ..Default::default()
            };

            let mut file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("log capture for {id}: open {} failed: {e}", log_path.display());
                    return;
                }
            };

            let mut stream = docker.logs(&container_id, Some(opts));
            while let Some(item) = stream.next().await {
                match item {
                    Ok(output) => {
                        let bytes = log_output_bytes(&output);
                        if bytes.is_empty() {
                            continue;
                        }
                        if let Err(e) = file.write_all(bytes) {
                            tracing::warn!("log capture for {id}: write failed: {e}");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("log stream for {id} ended: {e}");
                        break;
                    }
                }
            }
            let _ = file.flush();
        })
    }

    /// Best-effort: wait for the container to report Running, then scan the
    /// harvested log for the agent's self-minted MXID (DID-derived). Returns the
    /// MXID if seen within [`DID_DISCOVERY_TIMEOUT_SECS`], else `None`.
    async fn discover_did(&self, container_id: &str, id: &str, log_path: &Path) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(DID_DISCOVERY_TIMEOUT_SECS);
        let mut running = false;

        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;

            if !running {
                if let Ok(inspect) = self
                    .docker
                    .inspect_container(container_id, None::<InspectContainerOptions>)
                    .await
                {
                    running = inspect
                        .state
                        .as_ref()
                        .and_then(|s| s.running)
                        .unwrap_or(false);
                }
            }

            // The log-capture task writes to the host file as the agent boots;
            // scan what's there so far for the `agent DID: did:key:…` it logs.
            if let Ok(content) = std::fs::read_to_string(log_path) {
                if let Some(did) = scan_agent_did(&content) {
                    tracing::info!("instance {id}: discovered agent DID {did}");
                    return Some(did);
                }
            }
        }

        tracing::info!("instance {id}: DID still pending after {DID_DISCOVERY_TIMEOUT_SECS}s");
        None
    }
}

// ---------------------------------------------------------------------------
// Free helpers (unit-testable, no live podman)
// ---------------------------------------------------------------------------

/// The rootless podman Docker-API socket URI. Honors `DOCKER_HOST`; otherwise
/// `unix:///run/user/<uid>/podman/podman.sock` with the uid from
/// `$XDG_RUNTIME_DIR`, then the `id -u`-equivalent, then `1000`.
pub fn podman_socket_uri() -> String {
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        if !h.is_empty() {
            return h;
        }
    }
    let uid = current_uid();
    format!("unix:///run/user/{uid}/podman/podman.sock")
}

/// The current uid, derived without spawning `id`: prefer the runtime dir name
/// (`/run/user/<uid>`), fall back to `1000`.
fn current_uid() -> String {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        if let Some(name) = Path::new(&rt).file_name().and_then(|n| n.to_str()) {
            if name.chars().all(|c| c.is_ascii_digit()) && !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "1000".to_string()
}

fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-matrix-heartbeat")
}

/// Resolve a `ref_mount` host path to an absolute path. Absolute paths pass
/// through; relative paths (e.g. `../aqua-rs-sdk`) are resolved against the repo
/// root [`REPO_ROOT`] so siblings of the repo are reachable. Lexically
/// normalized (no filesystem touch) so it works even before the path exists.
fn resolve_host_path(host_path: &str) -> PathBuf {
    let p = Path::new(host_path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    normalize(&Path::new(REPO_ROOT).join(p))
}

/// Lexical path normalization: collapse `.` and `..` components without hitting
/// the filesystem (so `/repo/../aqua-rs-sdk` → `/aqua-rs-sdk`).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Extract the raw bytes of a [`LogOutput`] frame (stdout/stderr/console).
fn log_output_bytes(output: &LogOutput) -> &[u8] {
    match output {
        LogOutput::StdOut { message }
        | LogOutput::StdErr { message }
        | LogOutput::Console { message }
        | LogOutput::StdIn { message } => message,
    }
}

/// Scan text for the agent's **own** DID, which it logs at startup as
/// `agent DID: did:key:<base58btc>`. Returns the first `did:key:…` token.
///
/// NB: the agent's configured *target* appears in logs as an MXID
/// (`@did-key-…:server` — hyphen form), which does NOT contain the `did:key:`
/// (colon) substring, so this matches only the agent's own identity, never the
/// target. (An earlier version scanned for the first `@…:…` MXID and wrongly
/// picked up the target, conflating agent-DID with bound-to-DID.)
fn scan_agent_did(text: &str) -> Option<String> {
    const NEEDLE: &str = "did:key:";
    let idx = text.find(NEEDLE)?;
    let rest = &text[idx + NEEDLE.len()..];
    // multibase-base58btc body is ASCII alphanumeric; stop at the first other
    // char (`)`, whitespace, newline, …).
    let body: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    if body.is_empty() {
        return None;
    }
    Some(format!("{NEEDLE}{body}"))
}

fn short_id(id: &str) -> &str {
    &id[..12.min(id.len())]
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build an on-the-fly [`ResolvedInstance`] for `#shell spawn <type> <target>`:
/// the given type with `id = <type>-<incarnation_ts>` and `target` = the MXID.
/// Pure (no I/O), so it is unit-testable without podman.
pub fn build_spawn_instance(
    agent_type: &aqua_matrix_template::AgentType,
    target: &str,
    incarnation_ts: u64,
) -> ResolvedInstance {
    ResolvedInstance {
        id: format!("{}-{}", agent_type.name, incarnation_ts),
        target: target.to_string(),
        agent_type: agent_type.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_uri_honors_docker_host() {
        // Can't safely mutate process env in parallel tests; just assert the
        // default-shape branch directly via current_uid()/format.
        let uid = current_uid();
        let uri = format!("unix:///run/user/{uid}/podman/podman.sock");
        assert!(uri.starts_with("unix:///run/user/"));
        assert!(uri.ends_with("/podman/podman.sock"));
    }

    #[test]
    fn relative_ref_mount_resolves_against_repo_root() {
        let resolved = resolve_host_path("../aqua-rs-sdk");
        assert_eq!(resolved, PathBuf::from("/home/waldknoten-01/aqua-rs-sdk"));

        let resolved = resolve_host_path("../aqua-spec");
        assert_eq!(resolved, PathBuf::from("/home/waldknoten-01/aqua-spec"));
    }

    #[test]
    fn absolute_ref_mount_passes_through() {
        let resolved = resolve_host_path("/opt/refs/thing");
        assert_eq!(resolved, PathBuf::from("/opt/refs/thing"));
    }

    #[test]
    fn scan_agent_did_finds_own_did_not_target() {
        // Real-shape log: the target is an `@did-key-…` MXID (hyphen form); the
        // agent's own identity is `agent DID: did:key:…`. We must pick the latter.
        let log = "claude-channel daemon starting (target: @did-key-z6mkeuktarget:matrix.inblock.io)\nagent DID: did:key:z6MkwfyHwRW3msHTk2657rjYSkfqiyZx3h8Wh2rcF6YPRybA\nready\n";
        assert_eq!(
            scan_agent_did(log).as_deref(),
            Some("did:key:z6MkwfyHwRW3msHTk2657rjYSkfqiyZx3h8Wh2rcF6YPRybA")
        );
    }

    #[test]
    fn scan_agent_did_none_when_absent() {
        // Only a target MXID (hyphen form), no `did:key:` ⇒ None.
        assert_eq!(scan_agent_did("only @did-key-z6mktarget:matrix.inblock.io here\n"), None);
        assert_eq!(scan_agent_did("no dids here at all\n"), None);
    }

    #[test]
    fn build_spawn_instance_tags_id_and_target() {
        let ty = aqua_matrix_template::AgentType {
            name: "claude-channel".into(),
            version: "0.1.0".into(),
            description: "t".into(),
            image: "aqua-matrix-agent:poc".into(),
            role: "claude-channel".into(),
            display_name: "claude-channel".into(),
            hello: None,
            contract: aqua_matrix_template::Contract {
                allowed_tools: vec![],
                permissions: aqua_matrix_template::Permissions {
                    allow: vec![],
                    deny: vec![],
                },
            },
            memory: aqua_matrix_template::MemoryPolicy {
                config_dir: "/agent/memory".into(),
                log_transcripts: true,
            },
            ref_mounts: vec![],
            resources: aqua_matrix_template::Resources {
                memory_mb: 2048,
                cpus: 2.0,
                pids_limit: 512,
            },
            auth: aqua_matrix_template::Auth {
                method: aqua_matrix_template::AuthMethod::SubscriptionOauthToken,
                env_var: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            },
            egress_allowlist: None,
        };
        let r = build_spawn_instance(&ty, "@tim:matrix.inblock.io", 1717245000);
        assert_eq!(r.id, "claude-channel-1717245000");
        assert_eq!(r.target, "@tim:matrix.inblock.io");
        assert_eq!(r.agent_type.image, "aqua-matrix-agent:poc");
    }
}
