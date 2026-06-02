//! The ops/heartbeat [`MessageHandler`]: status payloads on a timer + a
//! `#shell` command channel. All of this is host-specific telemetry (host
//! facts, the Claude Code transcript snoop, journal access, systemd control),
//! which is why it lives here and not in the generic relay.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use aqua_matrix_relay::{async_trait, AgentClient, InboundMessage, MessageHandler};
use aqua_matrix_template::{build_spawn_spec, load_instances, load_types, AgentType, InstanceBinding};
use tokio::sync::Mutex;

use aqua_matrix_orchestrator::ContainerManager;

const ROLE: &str = "heartbeat";
const UNIT: &str = "aqua-matrix-heartbeat";

/// Counters that survive client rotations — a token-rotation restart should
/// look like uninterrupted uptime to the operator, not a fresh boot.
pub struct HeartbeatStats {
    start: Instant,
    sent: u64,
    last_err: Option<String>,
    commands_handled: u64,
}

impl HeartbeatStats {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            sent: 0,
            last_err: None,
            commands_handled: 0,
        }
    }
}

pub struct OpsHandler {
    stats: Arc<Mutex<HeartbeatStats>>,
    interval: Duration,
    /// The Phase 3 orchestrator: drives rootless podman. `None` only if the
    /// podman socket connection could not even be set up (bollard is lazy, so
    /// this is rare — a malformed `DOCKER_HOST`); the heartbeat still starts and
    /// the orchestrator `#shell` commands report the failure.
    orchestrator: Option<ContainerManager>,
    /// Agent types loaded from `AGENT_TYPES_DIR` (default `./types`), by name.
    types: HashMap<String, AgentType>,
    /// Instance bindings loaded from `AGENT_INSTANCES_DIR` (default
    /// `./instances`). Carried for surfacing/operator reference; spawning is
    /// operator-driven, NOT auto-started at boot.
    instances: Vec<InstanceBinding>,
}

impl OpsHandler {
    /// Stays sync. Loads `types/` + `instances/` (a missing/empty dir is fine —
    /// log and continue so the heartbeat always starts) and connects the
    /// orchestrator to rootless podman lazily. Does NOT spawn anything.
    pub fn new(interval: Duration, store_dir: &Path) -> Self {
        let types_dir = std::env::var("AGENT_TYPES_DIR").unwrap_or_else(|_| "./types".into());
        let instances_dir =
            std::env::var("AGENT_INSTANCES_DIR").unwrap_or_else(|_| "./instances".into());

        let types = match load_types(Path::new(&types_dir)) {
            Ok(t) => {
                tracing::info!("loaded {} agent type(s) from {}", t.len(), types_dir);
                t
            }
            Err(e) => {
                tracing::warn!("no agent types loaded from {types_dir}: {e:#} (continuing)");
                HashMap::new()
            }
        };

        let instances = match load_instances(Path::new(&instances_dir)) {
            Ok(i) => {
                tracing::info!("loaded {} instance binding(s) from {}", i.len(), instances_dir);
                i
            }
            Err(e) => {
                tracing::warn!("no instance bindings loaded from {instances_dir}: {e:#} (continuing)");
                Vec::new()
            }
        };

        let orchestrator = match ContainerManager::new(Some(store_dir)) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!("orchestrator init failed: {e:#} (orchestrator #shell commands will report unavailable)");
                None
            }
        };

        Self {
            stats: Arc::new(Mutex::new(HeartbeatStats::new())),
            interval,
            orchestrator,
            types,
            instances,
        }
    }

    /// Pre-dispatch the async orchestrator `#shell` subcommands (these need
    /// `await` and so cannot live in the sync [`handle_command`]). Returns
    /// `Some(reply)` if it handled the command, `None` to fall through to the
    /// sync dispatcher.
    async fn handle_orchestrator_command(&self, input: &str) -> Option<String> {
        let parts: Vec<&str> = input.split_whitespace().collect();
        let cmd = parts.get(1).copied().unwrap_or("").to_lowercase();

        match cmd.as_str() {
            "sessions" => Some(self.cmd_sessions()),
            "types" => Some(self.cmd_types()),
            "spawn" => Some(self.cmd_spawn(parts.get(2).copied(), parts.get(3).copied()).await),
            "replace" => Some(self.cmd_replace(parts.get(2).copied()).await),
            "stop" => Some(self.cmd_stop(parts.get(2).copied()).await),
            "agent-logs" => {
                let n = parts
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(20)
                    .clamp(1, 100);
                Some(self.cmd_agent_logs(parts.get(2).copied(), n).await)
            }
            // Not an orchestrator command → fall through to sync handle_command.
            _ => None,
        }
    }

    fn orchestrator(&self) -> Result<&ContainerManager, String> {
        self.orchestrator
            .as_ref()
            .ok_or_else(|| "orchestrator unavailable (podman socket connection not set up)".to_string())
    }

    fn cmd_sessions(&self) -> String {
        let mgr = match self.orchestrator() {
            Ok(m) => m,
            Err(e) => return e,
        };
        let sessions = mgr.list();
        if sessions.is_empty() {
            return "no active agent sessions".to_string();
        }
        let mut out = String::from("```\nactive agent sessions\n");
        out.push_str("----------------------------------------\n");
        for s in &sessions {
            out.push_str(&format!(
                "{} | {} | did {} | → {} | up {}\n",
                s.id,
                s.container_name,
                s.did.as_deref().unwrap_or("pending"),
                s.target,
                format_duration(s.started_at.elapsed()),
            ));
        }
        out.push_str("```");
        out
    }

    fn cmd_types(&self) -> String {
        if self.types.is_empty() {
            return format!(
                "no agent types loaded ({} instance binding(s) on file)",
                self.instances.len()
            );
        }
        let mut names: Vec<&String> = self.types.keys().collect();
        names.sort();
        let list = names
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "loaded types ({}): {} | {} instance binding(s) on file",
            names.len(),
            list,
            self.instances.len()
        )
    }

    async fn cmd_spawn(&self, type_name: Option<&str>, target: Option<&str>) -> String {
        let Some(type_name) = type_name else {
            return "usage: #shell spawn <type> <target-mxid>".to_string();
        };
        let Some(target) = target else {
            return "usage: #shell spawn <type> <target-mxid>".to_string();
        };
        let Some(agent_type) = self.types.get(type_name) else {
            return format!("unknown type {type_name:?}; try `#shell types`");
        };
        let mgr = match self.orchestrator() {
            Ok(m) => m,
            Err(e) => return e,
        };

        // Agents-side owns id generation now (the orchestrator no longer mints
        // ids). The connector keeps its own independent per-incarnation ts.
        let id = format!("{}-{}", agent_type.name, unix_now_secs());
        let spec = build_spawn_spec(agent_type, target, &id);
        match mgr.spawn(&spec).await {
            Ok(info) => format!(
                "spawned {} (container {}) → {} | did {}",
                info.id,
                short_id(&info.container_id),
                info.target,
                info.did.as_deref().unwrap_or("pending"),
            ),
            Err(e) => format!("spawn failed: {e:#}"),
        }
    }

    async fn cmd_replace(&self, id: Option<&str>) -> String {
        let Some(id) = id else {
            return "usage: #shell replace <id>".to_string();
        };
        let mgr = match self.orchestrator() {
            Ok(m) => m,
            Err(e) => return e,
        };
        match mgr.replace(id).await {
            Ok(info) => format!(
                "replaced {} → new container {} | new did {}",
                info.id,
                short_id(&info.container_id),
                info.did.as_deref().unwrap_or("pending"),
            ),
            Err(e) => format!("replace failed: {e:#}"),
        }
    }

    async fn cmd_stop(&self, id: Option<&str>) -> String {
        let Some(id) = id else {
            return "usage: #shell stop <id>".to_string();
        };
        let mgr = match self.orchestrator() {
            Ok(m) => m,
            Err(e) => return e,
        };
        match mgr.stop(id).await {
            Ok(()) => format!("stopped + removed {id}"),
            Err(e) => format!("stop failed: {e:#}"),
        }
    }

    async fn cmd_agent_logs(&self, id: Option<&str>, n: usize) -> String {
        let Some(id) = id else {
            return "usage: #shell agent-logs <id> [N]".to_string();
        };
        let mgr = match self.orchestrator() {
            Ok(m) => m,
            Err(e) => return e,
        };
        match mgr.tail_log(id, n).await {
            Ok(logs) => format!("```\n{}\n```", logs.trim_end()),
            Err(e) => format!("agent-logs failed: {e:#}"),
        }
    }

    /// One-line orchestrator summary for [`build_status`]: count of active
    /// sessions + a short list of `id → target`.
    fn sessions_summary(&self) -> String {
        let Some(mgr) = self.orchestrator.as_ref() else {
            return "orchestrator unavailable".to_string();
        };
        let sessions = mgr.list();
        if sessions.is_empty() {
            return "0 active".to_string();
        }
        let preview = sessions
            .iter()
            .take(3)
            .map(|s| format!("{} → {}", s.id, s.target))
            .collect::<Vec<_>>()
            .join(", ");
        let more = if sessions.len() > 3 { ", …" } else { "" };
        format!("{} active ({}{})", sessions.len(), preview, more)
    }
}

#[async_trait]
impl MessageHandler for OpsHandler {
    fn role(&self) -> &str {
        ROLE
    }

    fn systemd_unit(&self) -> Option<&str> {
        Some(UNIT)
    }

    fn hello(&self, agent: &AgentClient) -> Option<String> {
        Some(format!(
            "[hello] aqua-matrix-heartbeat online @ {} (identity: {}). I send a status payload every {}s. Reply with `#shell help` for the command list.",
            now_string(),
            agent.user_id(),
            self.interval.as_secs(),
        ))
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(self.interval)
    }

    async fn on_tick(&self, agent: &AgentClient, target: &str) {
        // Refresh the fleet-registry entry every tick so `last_online` stays
        // current. Best-effort — a registry failure must never skip the status.
        if let Err(e) = agent.update_registry(ROLE, Some(UNIT)).await {
            tracing::warn!("registry update failed: {e:#}");
        }

        let body = {
            let s = self.stats.lock().await;
            build_status(&s, &self.sessions_summary())
        };
        match agent.send_dm(target, &body).await {
            Ok(event_id) => {
                let mut s = self.stats.lock().await;
                s.sent += 1;
                s.last_err = None;
                tracing::info!("heartbeat sent (event: {event_id})");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!("heartbeat send failed: {msg}");
                let mut s = self.stats.lock().await;
                s.last_err = Some(msg);
            }
        }
    }

    async fn handle_message(
        &self,
        agent: &AgentClient,
        target: &str,
        msg: &InboundMessage<'_>,
    ) -> anyhow::Result<()> {
        let body = msg.body;
        // The relay forwards every text message from `target`; the ops channel
        // only acts on `#shell` commands and ignores the rest.
        let lower = body.to_lowercase();
        if !(lower.starts_with("#shell ") || lower == "#shell") {
            return Ok(());
        }

        tracing::info!("command from {}: {}", target, body);

        // Async orchestrator commands (`sessions`/`spawn`/`replace`/`stop`/
        // `agent-logs`/`types`) need `await`, so they are pre-dispatched here.
        // Everything else falls through to the sync `handle_command` below,
        // which keeps the existing commands working byte-for-byte.
        let reply = match self.handle_orchestrator_command(body).await {
            Some(reply) => reply,
            None => {
                let stats_guard = self.stats.lock().await;
                handle_command(body, &stats_guard, &self.sessions_summary())
            }
        };

        {
            let mut s = self.stats.lock().await;
            s.commands_handled += 1;
        }

        if let Err(e) = agent.send_dm(target, &reply).await {
            tracing::warn!("command reply send failed: {e:#}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

fn handle_command(input: &str, stats: &HeartbeatStats, sessions_summary: &str) -> String {
    // input starts with "#shell" (case-insensitive). parts[0]="#shell",
    // parts[1]=subcommand, parts[2..]=args.
    let parts: Vec<&str> = input.split_whitespace().collect();
    let cmd = parts.get(1).copied().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "" | "help" => help_text(),
        "ping" => format!("pong @ {}", now_string()),
        "status" => build_status(stats, sessions_summary),
        "uptime" => format!(
            "agent up {} | host up {}",
            format_duration(stats.start.elapsed()),
            host_uptime().unwrap_or_else(|| "?".into()),
        ),
        "restart" => spawn_systemctl_restart("aqua-matrix-heartbeat"),
        "respawn" => spawn_systemctl_restart("claude-bridge"),
        "respawn-channel" => spawn_systemctl_restart("aqua-matrix-claude-channel"),
        "logs" => {
            let n = parts
                .get(2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(10)
                .clamp(1, 50);
            recent_logs("aqua-matrix-heartbeat", n)
                .map(|logs| format!("```\n{}\n```", logs.trim_end()))
                .unwrap_or_else(|| "could not read journal logs".into())
        }
        other => format!("unknown command: {other}\n\n{}", help_text()),
    }
}

fn help_text() -> String {
    // The aligned command table is wrapped in a fenced code block so it renders
    // as a monospace panel in Element (HTML collapses the alignment spaces and
    // would treat the `#shell`-prefixed lines as headings otherwise). The
    // surrounding sentences stay as prose so they render as normal text.
    [
        "**aqua-matrix-heartbeat** — supported commands (prefix `#shell`):",
        "```",
        "#shell help              this message",
        "#shell status            send a status payload now",
        "#shell ping              reply pong + timestamp",
        "#shell uptime            agent + host uptime",
        "#shell restart           restart the heartbeat systemd unit",
        "#shell respawn           restart claude-bridge (local interactive Claude in tmux)",
        "#shell respawn-channel   restart aqua-matrix-claude-channel (the Matrix LLM channel)",
        "#shell logs [N]          last N journal lines (default 10, max 50)",
        "",
        "orchestrator (typed agent containers, Phase 3):",
        "#shell types                  list loaded agent type names",
        "#shell sessions               list active agent sessions (id · did · target · uptime)",
        "#shell spawn <type> <mxid>    spawn a typed agent container bound to <mxid>",
        "#shell replace <id>           rm + recreate <id> (yields a NEW identity)",
        "#shell stop <id>              stop + remove agent container <id>",
        "#shell agent-logs <id> [N]    last N lines of agent <id>'s host log (default 20, max 100)",
        "```",
        "Commands are honored only when sender matches the configured `--target`.",
    ]
    .join("\n")
}

fn spawn_systemctl_restart(unit: &str) -> String {
    match std::process::Command::new("systemctl")
        .args(["--user", "restart", unit])
        .spawn()
    {
        Ok(_) => format!("restarting {unit} (systemctl --user restart {unit})"),
        Err(e) => format!("restart of {unit} failed to spawn systemctl: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Status payload
// ---------------------------------------------------------------------------

fn build_status(stats: &HeartbeatStats, sessions_summary: &str) -> String {
    // Wrapped in a fenced code block: the panel uses a `----` divider and
    // column-aligned labels, which Element would otherwise mangle (the divider
    // turns the line above into a setext heading and HTML collapses alignment).
    let mut out = String::from("```\n");
    out.push_str(&format!("aqua-matrix-heartbeat @ {}\n", now_string()));
    out.push_str("----------------------------------------\n");

    out.push_str(&format!(
        "agent : up {}, sent {}, cmds {}",
        format_duration(stats.start.elapsed()),
        stats.sent,
        stats.commands_handled,
    ));
    if let Some(err) = &stats.last_err {
        out.push_str(&format!(", last_err: {}", truncate(err, 120)));
    }
    out.push('\n');

    out.push_str("host  : ");
    out.push_str(&host_summary());
    out.push('\n');

    if let Some(claude) = claude_session_summary() {
        out.push_str("claude: ");
        out.push_str(&claude);
        out.push('\n');
    } else {
        out.push_str("claude: no active transcript\n");
    }

    // Phase 3 orchestrator: one-line summary of the managed agent containers.
    out.push_str("agents: ");
    out.push_str(sessions_summary);
    out.push('\n');

    out.push_str("```");
    out
}

// ---------------------------------------------------------------------------
// Host facts
// ---------------------------------------------------------------------------

fn host_summary() -> String {
    let hostname = read_trim("/proc/sys/kernel/hostname").unwrap_or_else(|| "?".into());
    let uptime = host_uptime().unwrap_or_else(|| "?".into());
    let load = read_trim("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "?".into());
    let mem = memory_summary().unwrap_or_else(|| "?".into());
    let disk = disk_summary("/").unwrap_or_else(|| "?".into());
    format!("{hostname} | up {uptime} | load {load} | mem {mem} | disk {disk}")
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(|s| s.trim().to_string()))
}

fn host_uptime() -> Option<String> {
    let s = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(format_duration(Duration::from_secs(secs as u64)))
}

fn memory_summary() -> Option<String> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_meminfo_kb(rest)?;
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = parse_meminfo_kb(rest)?;
        }
    }
    if total_kb == 0 {
        return None;
    }
    let total_gb = total_kb as f64 / 1024.0 / 1024.0;
    let avail_gb = avail_kb as f64 / 1024.0 / 1024.0;
    let used_pct = ((total_kb - avail_kb) as f64 / total_kb as f64) * 100.0;
    Some(format!("{avail_gb:.1}/{total_gb:.1}GB free ({used_pct:.0}% used)"))
}

fn parse_meminfo_kb(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

fn disk_summary(path: &str) -> Option<String> {
    let out = std::process::Command::new("df")
        .args(["-BG", "--output=avail,pcent", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 2 {
        return None;
    }
    Some(format!("{} free ({} used)", fields[0], fields[1]))
}

// ---------------------------------------------------------------------------
// Claude Code session facts
// ---------------------------------------------------------------------------

fn claude_session_summary() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let projects = PathBuf::from(home).join(".claude/projects");
    let transcript = find_latest_transcript(&projects)?;

    let content = std::fs::read_to_string(&transcript).ok()?;

    let mut input_tokens: u64 = 0;
    let mut model: Option<String> = None;
    let mut last_user: Option<String> = None;
    let mut last_tool: Option<String> = None;

    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg = v.get("message").unwrap_or(&v);
        let role = msg.get("role").and_then(|r| r.as_str());

        match role {
            Some("user") => {
                last_user = extract_text_from_content(msg.get("content"));
            }
            Some("assistant") => {
                if let Some(usage) = msg.get("usage") {
                    let total = field_u64(usage, "input_tokens")
                        + field_u64(usage, "cache_read_input_tokens")
                        + field_u64(usage, "cache_creation_input_tokens");
                    if total > input_tokens {
                        input_tokens = total;
                        model = msg.get("model").and_then(|v| v.as_str()).map(String::from);
                    }
                }
                if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                    for item in arr {
                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                last_tool = Some(name.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if input_tokens == 0 {
        return None;
    }

    let window = context_window_for(model.as_deref());
    let pct = (input_tokens as f64 / window as f64 * 100.0).round() as u64;

    let session = transcript
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let project = transcript
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_short: String = session.chars().take(8).collect();

    let model_str = model.as_deref().unwrap_or("?");
    let mut out = format!(
        "{} | ctx ~{}% of {} ({}) | session {}",
        project,
        pct,
        format_tokens(window),
        model_str,
        session_short,
    );
    if let Some(tool) = last_tool {
        out.push_str(&format!(" | last_tool: {tool}"));
    }
    if let Some(user) = last_user {
        out.push_str(&format!(" | last_user: \"{}\"", truncate(&user, 80)));
    }
    Some(out)
}

fn field_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn extract_text_from_content(content: Option<&serde_json::Value>) -> Option<String> {
    let content = content?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn find_latest_transcript(root: &Path) -> Option<PathBuf> {
    let mut latest: Option<(PathBuf, SystemTime)> = None;
    walk(root, &mut |p| {
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return;
        }
        let Ok(meta) = std::fs::metadata(p) else { return };
        let Ok(modified) = meta.modified() else { return };
        match &latest {
            None => latest = Some((p.to_path_buf(), modified)),
            Some((_, lm)) if modified > *lm => latest = Some((p.to_path_buf(), modified)),
            _ => {}
        }
    });
    latest.map(|(p, _)| p)
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, f);
        } else {
            f(&path);
        }
    }
}

fn context_window_for(model: Option<&str>) -> u64 {
    if let Ok(s) = std::env::var("CONTEXT_WINDOW") {
        if let Ok(n) = s.parse() {
            return n;
        }
    }
    match model {
        Some(m) if m.contains("[1m]") => 1_000_000,
        _ => 200_000,
    }
}

// ---------------------------------------------------------------------------
// Journal access
// ---------------------------------------------------------------------------

fn recent_logs(unit: &str, n: usize) -> Option<String> {
    let out = std::process::Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "--no-pager",
            "-n",
            &n.to_string(),
            "-o",
            "short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Misc formatting
// ---------------------------------------------------------------------------

fn now_string() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "?".into())
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out.replace('\n', " ")
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short_id(id: &str) -> &str {
    &id[..12.min(id.len())]
}

// ---------------------------------------------------------------------------
// Tests — parsing / arg handling only. NONE of these touch live podman.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror of the orchestrator command pre-dispatch's parse step, so we can
    /// assert subcommand + arg extraction without an [`OpsHandler`] (which would
    /// need a live podman socket / loaded types).
    fn parse(input: &str) -> (String, Vec<&str>) {
        let parts: Vec<&str> = input.split_whitespace().collect();
        let cmd = parts.get(1).copied().unwrap_or("").to_lowercase();
        let args: Vec<&str> = parts.iter().skip(2).copied().collect();
        (cmd, args)
    }

    #[test]
    fn orchestrator_subcommands_parse() {
        assert_eq!(parse("#shell sessions").0, "sessions");
        assert_eq!(parse("#shell types").0, "types");

        let (cmd, args) = parse("#shell spawn claude-channel @tim:matrix.inblock.io");
        assert_eq!(cmd, "spawn");
        assert_eq!(args, vec!["claude-channel", "@tim:matrix.inblock.io"]);

        let (cmd, args) = parse("#shell replace claude-channel-1717");
        assert_eq!(cmd, "replace");
        assert_eq!(args, vec!["claude-channel-1717"]);

        let (cmd, args) = parse("#shell stop claude-channel-1717");
        assert_eq!(cmd, "stop");
        assert_eq!(args, vec!["claude-channel-1717"]);
    }

    #[test]
    fn agent_logs_n_defaults_and_clamps() {
        // Mirrors the clamp in handle_orchestrator_command: default 20, max 100.
        let clamp = |raw: Option<&str>| {
            raw.and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20)
                .clamp(1, 100)
        };
        assert_eq!(clamp(None), 20); // `#shell agent-logs <id>`
        assert_eq!(clamp(Some("50")), 50);
        assert_eq!(clamp(Some("999")), 100); // capped at 100
        assert_eq!(clamp(Some("0")), 1); // floored at 1
        assert_eq!(clamp(Some("notanum")), 20); // garbage → default
    }

    #[test]
    fn agent_logs_is_distinct_from_journal_logs() {
        // The orchestrator log tail is `agent-logs`; the heartbeat's OWN journal
        // tail stays `logs`. They must not collide.
        assert_eq!(parse("#shell agent-logs id 30").0, "agent-logs");
        assert_eq!(parse("#shell logs 10").0, "logs");
        assert_ne!(parse("#shell agent-logs id").0, parse("#shell logs").0);
    }

    #[test]
    fn build_status_includes_agents_line() {
        let stats = HeartbeatStats::new();
        let out = build_status(&stats, "2 active (claude-channel-1 → @tim:x.io)");
        assert!(out.contains("agents: 2 active"));
        assert!(out.contains("claude-channel-1 → @tim:x.io"));
        // Existing panel lines still present.
        assert!(out.contains("agent :"));
        assert!(out.contains("host  :"));
    }

    #[test]
    fn help_text_documents_orchestrator_commands() {
        let h = help_text();
        for needle in ["#shell sessions", "#shell spawn", "#shell replace", "#shell stop", "#shell agent-logs", "#shell types"] {
            assert!(h.contains(needle), "help should mention {needle}");
        }
        // The existing commands are still documented byte-for-byte.
        assert!(h.contains("#shell logs [N]          last N journal lines (default 10, max 50)"));
    }

    #[test]
    fn existing_sync_commands_unaffected() {
        let stats = HeartbeatStats::new();
        let summary = "0 active";
        assert!(handle_command("#shell ping", &stats, summary).starts_with("pong @"));
        assert_eq!(handle_command("#shell help", &stats, summary), help_text());
        assert!(handle_command("#shell bogus", &stats, summary).starts_with("unknown command: bogus"));
        // status still renders the panel.
        assert!(handle_command("#shell status", &stats, summary).contains("aqua-matrix-heartbeat @"));
    }
}
