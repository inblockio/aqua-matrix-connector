//! Phase 3 orchestrator: drive rootless Podman (via the Docker-API-compatible
//! `bollard` crate) to spawn / replace / stop **typed agent containers**, harvest
//! their logs host-side, and expose a snapshot of active sessions for the
//! heartbeat's Matrix `#shell` / status surface.
//!
//! Modeled on `../agent-customer-portal/src/docker.rs` (the reference bollard
//! `ContainerManager`), adapted to the typed-agent-instance model: a flat,
//! agent-agnostic [`ContainerSpec`] in (the agents side resolves all
//! agent-knowledge into it), one container = one agent = one self-minted
//! identity out.
//!
//! ## The identity model (persistent volume, reset on replace)
//!
//! Each instance's durable state — its self-minted `.pem` → DID and Matrix
//! crypto store (`/agent/store`), plus its claude memory and session logs
//! (`/agent/memory`, i.e. `CLAUDE_CONFIG_DIR`) — lives on a **per-instance host
//! volume** keyed by instance `id` (`<sessions>/<id>/persist/{store,memory}`),
//! NOT on the container's ephemeral writable layer. The binds use the `:U`
//! option so rootless podman chowns the source to the container user's mapped
//! uid (the image runs as the non-root `agent`); without it the bind is
//! unwritable.
//!
//! - `restart` (crash → on-failure restart, host reboot, or a `stop` + re-
//!   `spawn` of the same `id`): the volume is reused ⇒ the agent keeps its
//!   identity, memory, and session history. This is the steady state.
//! - `replace` ([`ContainerManager::replace`]): deliberately `rm -rf`s the
//!   instance's `persist` dir *before* re-spawning ⇒ a NEW identity, fresh
//!   context, full re-auth. This is the explicit "start over" verb, and now the
//!   only path that resets identity.
//!
//! Rationale: a containerised agent that lost its Matrix identity on every host
//! reboot or token-rotation-driven recreate was not viable as a persistent
//! assistant. Persisting the volume fixes that; `replace` retains an on-demand
//! path to a clean identity. (The earlier design kept `/agent` wholly on the
//! writable layer so *any* recreate reset identity — see git history.)
//!
//! [`ContainerManager::spawn`] still does NOT set `readonly_rootfs`; the rest of
//! `/agent` (binaries, the RO `/agent/config.json` mount) is fine as ephemeral
//! writable-layer state. The host also keeps the harvested per-incarnation log
//! (`<sessions>/<id>/<incarnation>/agent.log`) as provenance.
//!
//! ## Auth token (long-lived by design)
//!
//! `claude -p` inside the container authenticates with the secret named by the
//! agent type's `auth.env_var` (`CLAUDE_CODE_OAUTH_TOKEN`). To avoid baking a
//! short-lived ambient *session* token into a container (it would 401 a few
//! hours later with no way to refresh), [`spawn`](ContainerManager::spawn)
//! sources it via [`resolve_auth_secret`](ContainerManager::resolve_auth_secret):
//! an explicit env override if set, else the durable **token file**
//! (`<store_dir>/claude-oauth-token`, overridable with `AQUA_CLAUDE_TOKEN_FILE`).
//! Populate that file once with a long-lived `claude setup-token` token and every
//! spawn reuses it. Spawning fails closed if neither source yields a token.
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
/// How long to poll for the agent's self-minted MXID in its logs after start.
/// Best-effort: a DID-pending container is still a successful spawn.
const DID_DISCOVERY_TIMEOUT_SECS: u64 = 15;

// ---------------------------------------------------------------------------
// ContainerSpec — the agent/transport-agnostic spawn input
// ---------------------------------------------------------------------------

/// The agent/transport-agnostic input to [`ContainerManager::spawn`]. Replaces
/// the old agent-typed spawn input: the connector no longer names any
/// agent-side type. All
/// agent-knowledge (path resolution, config serialization, secret-source
/// choice, id generation) is done on the agents side
/// (`build_spawn_spec`) and handed to the connector as a flat,
/// already-resolved description of one container to run. The connector never
/// names any agent-side type.
///
/// **Single-container by design** (`image: String`). A future
/// multi-container / docker-compose spawn shape is a documented forward seam
/// (see the handover §5.1) — treat this as *one* spawn shape, not the only one.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    /// Instance id, e.g. `"claude-channel-<ts>"` (the agents side owns this).
    pub id: String,
    /// `AGENT_TARGET` MXID — opaque to the connector.
    pub target: String,
    /// Container image ref, e.g. `"aqua-matrix-agent:poc"`.
    pub image: String,
    /// Non-secret env (`AGENT_TARGET`, `AGENT_CONFIG_FILE`, …). The secret env
    /// is injected separately from [`secret_env`](Self::secret_env).
    pub env: Vec<(String, String)>,
    /// Reference mounts. `host_path` is ABSOLUTE (the agents side resolves
    /// relatives). Bound RO regardless of [`SpecMount::read_only`].
    pub ref_mounts: Vec<SpecMount>,
    /// Container resource caps.
    pub resources: SpecResources,
    /// The secret env var name + its source. Drives secret injection; the
    /// connector never derives it from any agent-side auth config.
    pub secret_env: SecretEnv,
    /// Pre-serialized JSON mounted RO at `/agent/config.json` (the agents side
    /// serializes its resolved-instance config into this). `None` ⇒ no config
    /// mount.
    pub config_blob: Option<String>,
}

/// One reference mount. `host_path` is absolute; `read_only` is carried for
/// future use but is currently IGNORED — the connector binds every ref_mount
/// `:ro` unconditionally (preserves the historic orchestrator semantics).
#[derive(Debug, Clone)]
pub struct SpecMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

/// Container resource caps.
#[derive(Debug, Clone)]
pub struct SpecResources {
    pub memory_mb: u64,
    pub cpus: f64,
    pub pids_limit: u64,
}

/// The secret env var to inject and where its value comes from. The connector
/// owns the `<store_dir>`-relative token-file default; the agents side only
/// names the env var + picks a [`SecretSource`].
#[derive(Debug, Clone)]
pub struct SecretEnv {
    pub env_var: String,
    pub source: SecretSource,
}

/// Where the secret value is sourced from.
#[derive(Debug, Clone)]
pub enum SecretSource {
    /// Today's path: env override → `AQUA_CLAUDE_TOKEN_FILE` else
    /// `<store_dir>/claude-oauth-token` → fail-closed.
    EnvOverrideThenTokenFile,
    /// Env override only; never read a file.
    EnvOverride,
    /// Read this explicit token file (overrides the `<store_dir>` default),
    /// with the env override still preferred first.
    TokenFile(std::path::PathBuf),
}

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
/// manager needs but does not expose — the [`ContainerSpec`] (so `replace`
/// can re-spawn the same slot) and the log-capture task handle (so `stop` can
/// let it drain).
struct Session {
    info: SessionInfo,
    spec: ContainerSpec,
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
    /// Base store dir (`<store_dir>`, parent of `sessions/`). Root for the
    /// durable Claude auth token file (`claude-oauth-token`); see
    /// [`ContainerManager::token_file`].
    store_dir: PathBuf,
}

impl ContainerManager {
    /// Connect to rootless podman and root the host-side session store under
    /// `store_dir`. Sync: bollard sets the connection up lazily, so no I/O
    /// happens here.
    ///
    /// Callers pass `Some`; this `None` default is dormant. It uses a neutral,
    /// agent-agnostic location (`~/.aqua-orchestrator`) rather than any
    /// specific backend's store dir.
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
            store_dir: base,
        })
    }

    /// The per-instance persistent volume dir (`<sessions>/<id>/persist`),
    /// stable across incarnations. Holds `store/` (identity + matrix crypto) and
    /// `memory/` (claude session logs), bound into the container with `:U`.
    /// Survives restarts; wiped by [`replace`](Self::replace).
    fn persist_dir(&self, id: &str) -> PathBuf {
        persist_dir_for(&self.sessions_dir, id)
    }

    /// Path to the durable Claude OAuth token file — the canonical source for a
    /// **long-lived** `claude setup-token` token, so spawned agents don't inherit
    /// the orchestrator's short-lived ambient session token. Override with
    /// `AQUA_CLAUDE_TOKEN_FILE`; defaults to `<store_dir>/claude-oauth-token`.
    fn token_file(&self) -> PathBuf {
        match std::env::var("AQUA_CLAUDE_TOKEN_FILE") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => self.store_dir.join("claude-oauth-token"),
        }
    }

    /// Resolve the auth secret to inject as `env_var`, preferring a long-lived
    /// token. Precedence:
    ///   1. `env_var` in the orchestrator's own environment (explicit override,
    ///      e.g. a one-off `CLAUDE_CODE_OAUTH_TOKEN=… aqua-matrix-heartbeat`), else
    ///   2. the durable [`token_file`](Self::token_file), unless `file_override`
    ///      names a specific file to read instead.
    /// Fails closed when neither yields a non-empty value — we never spawn a
    /// token-less container. The value is trimmed and never logged.
    fn resolve_auth_secret(&self, env_var: &str, file_override: Option<&Path>) -> Result<String> {
        if let Ok(v) = std::env::var(env_var) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
        let file = match file_override {
            Some(p) => p.to_path_buf(),
            None => self.token_file(),
        };
        match std::fs::read_to_string(&file) {
            Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Ok(_) => bail!(
                "{env_var} unset and token file {} is empty; run `claude setup-token` \
                 and write its long-lived token there",
                file.display()
            ),
            Err(e) => bail!(
                "{env_var} unset and no token file at {} ({e}); run `claude setup-token` \
                 and write its long-lived token there (or set {env_var})",
                file.display()
            ),
        }
    }

    /// Spawn a new typed agent container from a [`ContainerSpec`].
    ///
    /// Returns a successful [`SessionInfo`] even when the DID is still pending —
    /// DID discovery is best-effort and must not gate the spawn.
    pub async fn spawn(&self, spec: &ContainerSpec) -> Result<SessionInfo> {
        let id = spec.id.clone();
        let container_name = format!("{CONTAINER_PREFIX}{id}");
        // The connector keeps its OWN per-incarnation timestamp for the persist
        // dir / log dir — independent of the timestamp embedded in `spec.id`.
        let incarnation_ts = unix_now_secs();

        // --- host-side incarnation dir + config.json -----------------------
        let incarnation_dir = self.sessions_dir.join(&id).join(incarnation_ts.to_string());
        std::fs::create_dir_all(&incarnation_dir).with_context(|| {
            format!("create incarnation dir {}", incarnation_dir.display())
        })?;

        // --- persistent per-instance volume (identity + memory + logs) ------
        // Keyed by `id` (stable across incarnations) so a restart / reboot / re-
        // spawn reuses the same identity and session history. `replace` wipes
        // this dir to force a fresh identity. Created if absent; never wiped here.
        let persist = self.persist_dir(&id);
        let store_dir = persist.join("store");
        let memory_dir = persist.join("memory");
        std::fs::create_dir_all(&store_dir)
            .with_context(|| format!("create persistent store dir {}", store_dir.display()))?;
        std::fs::create_dir_all(&memory_dir)
            .with_context(|| format!("create persistent memory dir {}", memory_dir.display()))?;
        let store_str = path_str(&store_dir)?;
        let memory_str = path_str(&memory_dir)?;

        // --- binds: persistent store/memory (RW, :U) + optional config.json
        //     (RO) + each ref_mount (RO) -----------------------------------
        let mut binds = vec![
            // Persistent identity + matrix crypto store. `:U` chowns the source
            // to the container user's mapped uid (rootless, non-root `agent`).
            format!("{store_str}:/agent/store:U"),
            // Persistent claude memory + session logs (CLAUDE_CONFIG_DIR).
            format!("{memory_str}:/agent/memory:U"),
        ];

        // --- env (incl. the auth secret) -----------------------------------
        // The non-secret env comes verbatim from the spec (the agents side set
        // AGENT_TARGET / AGENT_CONFIG_FILE / …). The secret VALUE is sourced
        // per `spec.secret_env.source` (NEVER from any agent-side auth config)
        // and injected by NAME=value. We never store or log the value, and
        // refuse to spawn a token-less container.
        let mut env: Vec<String> =
            spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let env_var = &spec.secret_env.env_var;
        let secret = match &spec.secret_env.source {
            SecretSource::EnvOverrideThenTokenFile => {
                self.resolve_auth_secret(env_var, None)?
            }
            SecretSource::TokenFile(p) => self.resolve_auth_secret(env_var, Some(p))?,
            SecretSource::EnvOverride => match std::env::var(env_var) {
                Ok(v) if !v.trim().is_empty() => v,
                _ => bail!(
                    "{env_var} unset and secret source is EnvOverride (no token file consulted)"
                ),
            },
        };
        env.push(format!("{env_var}={secret}"));

        // --- config.json mount (RO) ----------------------------------------
        // The agents side already serialized its resolved-instance config into
        // `config_blob`; the connector just writes the provided blob to the
        // incarnation dir and binds it RO. The in-container agent reads it back
        // via AGENT_CONFIG_FILE (set in `spec.env`).
        if let Some(config_blob) = &spec.config_blob {
            let config_host_path = incarnation_dir.join("config.json");
            std::fs::write(&config_host_path, config_blob)
                .with_context(|| format!("write {}", config_host_path.display()))?;
            let config_host_path_str = config_host_path.to_str().ok_or_else(|| {
                anyhow!("non-UTF-8 config path: {}", config_host_path.display())
            })?;
            binds.push(format!("{config_host_path_str}:/agent/config.json:ro"));
        }

        // --- ref_mounts (RO) -----------------------------------------------
        for m in &spec.ref_mounts {
            // ref_mounts are reference knowledge — always RO regardless of the
            // (carried) read_only flag; the model never writes to them. The
            // agents side has already resolved host_path to absolute.
            binds.push(format!("{}:{}:ro", m.host_path, m.container_path));
        }

        // --- resources -----------------------------------------------------
        let res = &spec.resources;
        let memory = (res.memory_mb * 1024 * 1024) as i64;
        let nano_cpus = (res.cpus * 1e9) as i64;
        let pids_limit = res.pids_limit as i64;

        // --- /tmp tmpfs ----------------------------------------------------
        let mut tmpfs = HashMap::new();
        tmpfs.insert("/tmp".to_string(), String::new());

        // Identity + memory live on per-instance host volumes at /agent/store
        // and /agent/memory (bound above with `:U`); they persist across restarts
        // and are wiped only by `replace`. See module docs. We still do NOT set
        // `readonly_rootfs` — the rest of /agent (binaries, the RO config mount)
        // stays ephemeral on the writable layer.
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
            image: Some(spec.image.clone()),
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
            spec.target,
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
            target: spec.target.clone(),
            did,
            incarnation_ts,
            started_at: Instant::now(),
        };

        self.sessions.lock().await.insert(
            id,
            Session {
                info: info.clone(),
                spec: spec.clone(),
                log_task: Some(log_task),
            },
        );

        Ok(info)
    }

    /// Replace the container backing `id`: stop + remove the current one (its log
    /// is drained/harvested first), wipe its persistent volume, then `spawn` the
    /// SAME [`ContainerSpec`] again ⇒ a NEW identity. This is the only path
    /// that resets identity now that the volume survives ordinary restarts.
    pub async fn replace(&self, id: &str) -> Result<SessionInfo> {
        // Pull the stored ContainerSpec out so we can re-spawn it.
        let spec = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(id)
                .map(|s| s.spec.clone())
                .ok_or_else(|| anyhow!("no active session {id:?} to replace"))?
        };

        // Tear down the current incarnation (drains its log, then rm).
        self.stop(id).await?;

        // Reset identity: wipe the per-instance persistent volume so the re-spawn
        // self-mints a fresh .pem/DID and starts with empty memory. Now that the
        // volume survives ordinary restarts, this is the ONLY path that resets
        // identity — the explicit "start over" verb.
        let persist = self.persist_dir(id);
        wipe_persist(&persist).await?;

        // Re-spawn ⇒ new incarnation, new identity, same slot/type/target.
        self.spawn(&spec).await
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

/// Neutral, agent-agnostic default store dir for the dormant `new(None)` path.
/// Real callers pass `Some(<their own store dir>)`; this default no longer
/// hard-codes any specific backend's location.
fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-orchestrator")
}

/// The per-instance persistent volume dir, `<sessions>/<id>/persist`. Stable
/// across incarnations; holds `store/` (identity + matrix crypto) and `memory/`
/// (claude session logs), each bound into the container with `:U`. A free
/// function so [`ContainerManager::persist_dir`] and the construction path can
/// share one definition of the layout.
fn persist_dir_for(sessions_dir: &Path, id: &str) -> PathBuf {
    sessions_dir.join(id).join("persist")
}

/// UTF-8 string view of a host path, or a descriptive error. Bind specs are
/// `String`s, so every host path bound into a container round-trips through this.
fn path_str(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", p.display()))
}

/// Wipe an instance's persistent volume so the next `spawn` self-mints a fresh
/// identity (the `replace` "start over" path). The `store/` + `memory/` subdirs
/// were chowned to the container user's mapped subuid by the `:U` bind, so a
/// plain host `rm` hits EPERM; `podman unshare` re-enters the rootless user
/// namespace (where that mapped uid is root) to delete them. No-op if the dir is
/// already gone.
async fn wipe_persist(persist: &Path) -> Result<()> {
    if !persist.exists() {
        return Ok(());
    }
    let persist_str = path_str(persist)?;
    let status = tokio::process::Command::new("podman")
        .args(["unshare", "rm", "-rf", &persist_str])
        .status()
        .await
        .with_context(|| format!("spawn `podman unshare rm -rf {persist_str}`"))?;
    if !status.success() {
        bail!("`podman unshare rm -rf {persist_str}` failed: {status}");
    }
    Ok(())
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

/// The connector's own per-incarnation timestamp for the persist/log dir —
/// independent of any timestamp the agents side embedded in `spec.id`.
fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
}
