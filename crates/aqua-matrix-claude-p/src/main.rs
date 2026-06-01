//! aqua-matrix-claude-p — the reference example backend.
//!
//! Each inbound DM (without a `#shell` prefix — that belongs to the ops
//! channel) becomes a `claude -p <prompt>` invocation; stdout is DM'd back.
//!
//! ## Chat confirmations (Phase A)
//!
//! A prompt that looks **destructive** (`rm`, `git push --force`, …; see
//! [`destructive`]) is not run blindly. Instead it follows a
//! **plan → approve → execute** flow:
//!   1. run `claude -p --permission-mode plan <prompt>` — this investigates
//!      with read-only tools and emits a *plan* but **executes no destructive
//!      tool** (the headless `ExitPlanMode` is auto-denied; verified empirically
//!      — see `docs/plans/chat-confirmations.md`);
//!   2. stream the plan back, then [`PendingMap::ask`] the authenticated user
//!      "Approve this plan? yes/no";
//!   3. on **yes** → resume the same session with `--resume <id>
//!      --permission-mode acceptEdits` to actually execute; on **no**/timeout →
//!      abort, touching nothing.
//!
//! Non-destructive prompts keep the original direct-stream behaviour. The
//! [`pending`] router (the shared `ask_user` primitive) is reused by Phases B/C.
//!
//! ## Chat confirmations (Phase B)
//!
//! Every non-destructive (`fresh`) run additionally carries an **`ask_human`
//! MCP tool** (see [`ask_bridge`]): `claude` can pause mid-run and ask the
//! authenticated user a free-form question over the same channel, blocking on
//! the answer. The tool is wired via a per-run unix socket that routes through
//! the same [`pending`] primitive. It is **advisory** (the model chooses to
//! call it) — enforced per-tool gating is Phase C. Phase A's gated destructive
//! flow is unchanged and does not carry the tool.
//!
//! This is the canonical "drop in a backend" example for [`aqua_matrix_relay`].
mod ask_bridge;
mod destructive;
mod pending;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use aqua_matrix_relay::{async_trait, load_dotenv, run_daemon, AgentClient, AgentConfig, MessageHandler, ReplyStream};
use clap::Parser;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use pending::PendingMap;

/// There is intentionally **no cap on total run time** — a legitimate task may
/// take many minutes. Instead an *inactivity* watchdog stops `claude` only if it
/// emits no output at all for this long. That still prevents a genuinely hung or
/// stalled process from holding the per-target run lock (and thus wedging the
/// channel) forever, without truncating long but healthy work. The watchdog is
/// reset on every line `claude` prints.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the user's `yes`/`no` to a plan, or for an answer to an
/// `ask_human` question. Independent of run length (runs are now unbounded):
/// this is a *safety* bound — default-DENY on timeout — so a forgotten question
/// can never hold a run, and its lock, open indefinitely. Set generously (7 min)
/// so a slow human reply during a long task isn't cut off.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(420);
const MAX_REPLY_BYTES: usize = 16_000; // Matrix can take more, but be polite.
const ROLE: &str = "claude-channel";
const UNIT: &str = "aqua-matrix-claude-channel";

#[derive(Parser)]
#[command(
    name = "aqua-matrix-claude-p",
    about = "LLM bridge: forward inbound DMs through `claude -p` and reply with stdout"
)]
struct Args {
    #[arg(long, env = "AGENT_KEY_FILE", default_value = "claude-channel.pem")]
    key_file: PathBuf,

    #[arg(long, env = "SIWX_URL", default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,

    #[arg(long, env = "MATRIX_URL", default_value = "https://matrix.inblock.io")]
    matrix_url: String,

    #[arg(long, env = "OIDC_CLIENT_ID", help = "OIDC client ID (auto-registered if omitted)")]
    client_id: Option<String>,

    #[arg(long, env = "OIDC_REDIRECT_URI", help = "OIDC redirect URI (defaults to http://localhost:0/callback)")]
    redirect_uri: Option<String>,

    #[arg(
        long,
        env = "AGENT_TARGET",
        help = "Matrix user ID whose DMs are forwarded to claude -p (set AGENT_TARGET, e.g. via this instance's .env file)"
    )]
    target: String,

    #[arg(long, env = "AGENT_STORE_DIR")]
    store_dir: Option<PathBuf>,
}

fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-matrix-claude-channel")
}

/// Per-target store of the last `claude` session id, so the next DM resumes the
/// same conversation (`--resume`) instead of starting cold. `Arc`-backed and
/// cheap to clone into each run task (mirrors [`PendingMap`]).
///
/// Intentionally **in-memory**: a service restart clears it, which is the
/// documented way to start a fresh conversation (there is no in-chat reset).
#[derive(Clone, Default)]
struct SessionStore {
    inner: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl SessionStore {
    /// The session id to resume for `target`, if we've seen one.
    fn get(&self, target: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(target)
            .cloned()
    }

    /// Record the session id of `target`'s most recent run.
    fn set(&self, target: &str, id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(target.to_string(), id.to_string());
    }

    /// Forget `target`'s session — e.g. after a `--resume` failed because the
    /// stored id was stale, so the next turn self-heals by starting cold.
    fn clear(&self, target: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(target);
    }
}

/// The claude-channel backend. Holds the shared [`PendingMap`] (the `ask_user`
/// primitive), a per-target run lock so only one `claude` run — and thus at most
/// one open question — exists per user at a time, and the per-target
/// [`SessionStore`] that gives each user a continuous conversation.
#[derive(Default)]
struct ClaudePHandler {
    pending: PendingMap,
    /// One async mutex per `target`, gating its active run. `claude` is
    /// expensive and an open confirmation must not race a second prompt; this
    /// serialises runs per user without blocking other users.
    run_locks: std::sync::Mutex<std::collections::HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Last session id per target, for `--resume` continuity across DMs.
    sessions: SessionStore,
}

impl ClaudePHandler {
    /// Get-or-create the per-target run lock.
    fn run_lock(&self, target: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .run_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        locks
            .entry(target.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

#[async_trait]
impl MessageHandler for ClaudePHandler {
    fn role(&self) -> &str {
        ROLE
    }

    fn systemd_unit(&self) -> Option<&str> {
        Some(UNIT)
    }

    fn hello(&self, agent: &AgentClient) -> Option<String> {
        Some(format!(
            "[hello] aqua-matrix-claude-channel online (identity: {}). DM me any text (without `#shell` prefix) and I will run `claude -p <your message>` and reply with the output. We keep a **continuous conversation** — each message resumes the same session, so I remember context across DMs (restart the service to start fresh). Long tasks run to completion; I only give up if I go silent for {}s. Destructive requests (rm, force-push, …) are shown as a plan and only run after you reply `yes`. I may also pause mid-task and ask you a question (`[ask] …`) — your next reply is the answer.",
            agent.user_id(),
            IDLE_TIMEOUT.as_secs(),
        ))
    }

    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str) {
        // `#shell` belongs to the ops/heartbeat channel, not the LLM channel.
        if body.to_lowercase().starts_with("#shell") {
            return;
        }

        // FIRST: if a run is waiting on this user's answer, this DM IS the
        // answer — resolve the pending question instead of starting a fresh
        // run. This is the pending-reply inversion at the heart of `ask_user`.
        if self.pending.try_resolve(target, body) {
            tracing::info!("claude-channel: DM from {target} resolved a pending question");
            return;
        }

        tracing::info!("claude-channel prompt from {}: {} chars", target, body.len());

        // Run claude in its own task so the sync stream keeps flowing while the
        // (potentially long) invocation runs.
        let agent = agent.clone();
        let target = target.to_string();
        let prompt = body.to_string();
        let pending = self.pending.clone();
        let run_lock = self.run_lock(&target);
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            // Serialise: hold the per-target lock for the whole run so a second
            // prompt can't start (or open a second question) until this one
            // finishes. `try_lock` would drop the prompt; instead we await —
            // the relay watermark already prevents duplicate delivery.
            let _guard = run_lock.lock().await;

            // Continuity: resume this user's prior session so context carries
            // across DMs. The run returns the session id to remember for next
            // time (the same id when resumed; a new one when starting cold).
            let prior = sessions.get(&target);
            let mut result = run_turn(&agent, &pending, &target, &prompt, prior.as_deref()).await;

            // Self-heal a poisoned session id: if a resumed turn failed, the
            // stored id may be stale (e.g. claude's session store was pruned),
            // which would wedge every future DM. Clear it and retry once cold.
            if result.is_err() && prior.is_some() {
                tracing::warn!(
                    "claude-channel: turn resuming session {prior:?} failed; clearing it and retrying fresh"
                );
                sessions.clear(&target);
                result = run_turn(&agent, &pending, &target, &prompt, None).await;
            }

            match result {
                Ok(Some(session_id)) => sessions.set(&target, &session_id),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("claude-channel run failed: {e:#}");
                    // The error notice must reach the user — retry its send.
                    let _ = agent
                        .send_dm_reliable(&target, &format!("[claude-channel error] {e:#}"))
                        .await;
                }
            }
        });
    }
}

/// Run one conversational turn and return the session id to remember (for
/// `--resume` next time), or `None` if the run produced none. Routes a
/// destructive prompt through the gated plan→approve→execute flow and an
/// ordinary prompt through a direct streamed run; both resume `prior` when set.
async fn run_turn(
    agent: &AgentClient,
    pending: &PendingMap,
    target: &str,
    prompt: &str,
    prior: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if destructive::looks_destructive(prompt) {
        run_gated(agent, pending, target, prompt, prior).await
    } else {
        let outcome =
            stream_claude(agent, pending, target, &ClaudeRun::conversational(prompt, prior)).await?;
        Ok(outcome.session_id)
    }
}

/// Phase A plan → approve → execute flow for a destructive prompt.
///
/// 1. Run plan mode (no destructive tool executes — verified), resuming the
///    caller's `prior` conversation so "delete the temp files we discussed"
///    works, and stream the plan while capturing the session id.
/// 2. Ask the authenticated user to approve.
/// 3. On `yes` → resume that session in `acceptEdits` mode to execute.
///    On `no`/timeout/anything else → abort, fail closed (do nothing).
///
/// Returns the session id to remember for continuity — in **all** non-error
/// cases, including an abort, so the conversation (and the fact that the action
/// was declined) carries into the next turn.
async fn run_gated(
    agent: &AgentClient,
    pending: &PendingMap,
    target: &str,
    prompt: &str,
    prior: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let class = destructive::classify(prompt).unwrap_or("destructive action");
    tracing::info!("claude-channel: gating {class:?} prompt from {target}");

    // 1. Plan mode. The streamed message is the plan; we also need its session
    //    id to resume on approval.
    let plan = stream_claude(agent, pending, target, &ClaudeRun::plan(prompt, prior)).await?;
    let Some(session_id) = plan.session_id else {
        // No session id → we cannot resume to execute. Fail closed.
        anyhow::bail!("plan produced no session id to resume; aborting (fail closed)");
    };

    // 2. Ask for approval. Default-DENY on timeout / send failure.
    let question = format!(
        "[confirm] The request above involves a **{class}**. I have planned it but not executed anything. Reply `yes` to proceed, or `no` to abort.",
    );
    let answer = pending
        .ask(agent, target, &question, APPROVAL_TIMEOUT)
        .await;

    let approved = matches!(
        answer.as_deref().map(str::trim).map(str::to_lowercase).as_deref(),
        Some("yes" | "y" | "approve" | "approved" | "ok" | "proceed")
    );
    if !approved {
        let note = match answer {
            Some(a) => format!("[aborted] Not approved (you said: {:?}). Nothing was executed.", a.trim()),
            None => "[aborted] No approval within the timeout. Nothing was executed.".to_string(),
        };
        tracing::info!("claude-channel: {class:?} from {target} NOT approved; aborting");
        let _ = agent.send_dm(target, &note).await;
        // Remember the (planned-but-aborted) session so the conversation
        // continues — the next message resumes with this context.
        return Ok(Some(session_id));
    }

    // 3. Approved → resume the SAME session in execution mode.
    tracing::info!("claude-channel: {class:?} from {target} approved; resuming {session_id}");
    let exec = stream_claude(
        agent,
        pending,
        target,
        &ClaudeRun::resume(&session_id, "Approved. Proceed with the plan."),
    )
    .await?;
    // Prefer the execute run's reported id; fall back to the plan's (the
    // resumed session keeps the same id, so these normally match).
    Ok(exec.session_id.or(Some(session_id)))
}

/// One `claude -p` invocation's mode/arguments. Built via
/// [`ClaudeRun::conversational`], [`ClaudeRun::plan`], or [`ClaudeRun::resume`].
struct ClaudeRun<'a> {
    /// The prompt / message to send.
    prompt: &'a str,
    /// `--permission-mode <mode>` when `Some`. `plan` previews without
    /// executing; `acceptEdits` lets a resumed, already-approved run execute.
    permission_mode: Option<&'a str>,
    /// `--resume <session_id>` when `Some`, to continue a prior plan session.
    resume_session: Option<&'a str>,
    /// Phase B: wire the `ask_human` MCP tool into this run (a per-run socket
    /// bridge routed through [`PendingMap::ask`]). Enabled for `conversational`
    /// runs; the Phase A `plan`/`resume` flow does not carry it.
    ask_human: bool,
}

impl<'a> ClaudeRun<'a> {
    /// A normal conversational turn — no plan gate, carries the Phase B
    /// `ask_human` tool so the model can ask before a risky step. Resumes
    /// `session` (the user's prior conversation) when set, else starts fresh.
    fn conversational(prompt: &'a str, session: Option<&'a str>) -> Self {
        Self { prompt, permission_mode: None, resume_session: session, ask_human: true }
    }

    /// Plan mode: investigate + emit a plan, execute no destructive tool.
    /// Resumes `session` when set so the plan has the prior conversation's
    /// context (read-only — plan mode still executes nothing destructive).
    fn plan(prompt: &'a str, session: Option<&'a str>) -> Self {
        Self { prompt, permission_mode: Some("plan"), resume_session: session, ask_human: false }
    }

    /// Resume a plan session in execution mode (post-approval).
    fn resume(session_id: &'a str, prompt: &'a str) -> Self {
        Self {
            prompt,
            permission_mode: Some("acceptEdits"),
            resume_session: Some(session_id),
            ask_human: false,
        }
    }
}

/// Outcome of a streamed run that callers may need afterwards.
#[derive(Default)]
struct RunOutcome {
    /// The `claude` session id (from the `init` event), used to resume a plan.
    session_id: Option<String>,
}

/// Run `claude -p` in streaming mode and pipe its output into a single Matrix
/// message that is edited in place as tokens arrive. A typing indicator covers
/// the wait for the first token. There is no total-runtime cap; an inactivity
/// watchdog ([`IDLE_TIMEOUT`]) stops a run only if `claude` goes silent. Uses
/// whatever `claude` is on PATH plus the absolute fallback matching the systemd
/// unit's `Environment=PATH`. Returns the run's [`RunOutcome`] (session id) so a
/// plan can be resumed for execution and so the conversation can be continued.
async fn stream_claude(
    agent: &AgentClient,
    pending: &PendingMap,
    target: &str,
    run: &ClaudeRun<'_>,
) -> anyhow::Result<RunOutcome> {
    let claude_bin = find_claude_bin();
    tracing::debug!(
        "invoking {} -p (stream-json, mode={:?}, resume={:?}, ask_human={})",
        claude_bin,
        run.permission_mode,
        run.resume_session,
        run.ask_human,
    );

    let mut cmd = Command::new(&claude_bin);
    cmd.arg("-p").arg(run.prompt);
    if let Some(session) = run.resume_session {
        cmd.arg("--resume").arg(session);
    }
    if let Some(mode) = run.permission_mode {
        cmd.arg("--permission-mode").arg(mode);
    }
    // Phase B: stand up the `ask_human` bridge for this run and add the MCP
    // flags. The guard must outlive the child (it serves ask_human calls during
    // the run), so it is held in this function's scope until after `wait`.
    let _ask_bridge = if run.ask_human {
        match ask_bridge::AskBridge::setup(agent, pending, target, APPROVAL_TIMEOUT).await {
            Ok(bridge) => {
                cmd.arg("--mcp-config").arg(bridge.config_path());
                cmd.arg("--allowedTools").arg("mcp__ask__ask_human");
                cmd.arg("--append-system-prompt").arg(ask_bridge::ASK_SYSTEM_PROMPT);
                Some(bridge)
            }
            Err(e) => {
                // Degrade gracefully: a missing bridge just means no ask_human
                // for this run, not a failed run.
                tracing::warn!("ask-bridge setup failed; running without ask_human: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let mut child = cmd
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // reap claude if this task is dropped/errors early
        .spawn()
        .with_context(|| format!("failed to spawn {claude_bin} -p"))?;

    let stdout = child.stdout.take().context("claude produced no stdout pipe")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();

    // "typing…" until the first visible token; after that the growing message
    // itself signals progress.
    let mut typing = agent.typing_guard(target).await;
    let mut stream: Option<ReplyStream> = None;
    let mut final_text: Option<String> = None;
    let mut err: Option<String> = None;
    let mut outcome = RunOutcome::default();

    // Inactivity watchdog: no cap on total runtime (long tasks are legitimate),
    // but if `claude` emits nothing for `IDLE_TIMEOUT` we treat it as hung and
    // stop it, so a stalled process can't hold the per-target run lock forever.
    // Reset on every line received — any output proves liveness.
    let mut idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
    loop {
        let line = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(idle_deadline) => {
                let _ = child.start_kill();
                err = Some(format!(
                    "no output for {}s — assumed stalled and stopped",
                    IDLE_TIMEOUT.as_secs()
                ));
                break;
            }
            l = lines.next_line() => l,
        };
        // Got a line (or EOF/err) — claude is alive; push the idle deadline out.
        idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
        let line = match line {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF — claude exited
            Err(e) => {
                err = Some(format!("reading claude output: {e}"));
                break;
            }
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            // {"type":"system","subtype":"init","session_id":"…"} — capture the
            // session id so a plan run can be resumed for execution.
            Some("system") => {
                if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        outcome.session_id = Some(sid.to_string());
                    }
                }
            }
            // Incremental output: {"type":"stream_event","event":{"type":
            // "content_block_delta","delta":{"type":"text_delta","text":"…"}}}
            Some("stream_event") => {
                let event = v.get("event");
                let is_text_delta = event.and_then(|e| e.get("type")).and_then(|t| t.as_str())
                    == Some("content_block_delta")
                    && event
                        .and_then(|e| e.get("delta"))
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("text_delta");
                if is_text_delta {
                    if let Some(text) = event
                        .and_then(|e| e.get("delta"))
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        if !text.is_empty() {
                            if stream.is_none() {
                                // First token: stop "typing", open the live message.
                                typing.take();
                                stream = Some(agent.reply_stream(target).await?);
                            }
                            if let Some(s) = stream.as_mut() {
                                s.push(text).await?;
                            }
                        }
                    }
                }
            }
            // Terminal: {"type":"result","is_error":bool,"result":"<full
            // text>","session_id":"…"}. The session id also rides here — keep
            // it as a fallback if the init event was missed.
            Some("result") => {
                if outcome.session_id.is_none() {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        outcome.session_id = Some(sid.to_string());
                    }
                }
                if v.get("is_error").and_then(|b| b.as_bool()) == Some(true) {
                    err = Some(
                        v.get("result")
                            .and_then(|r| r.as_str())
                            .unwrap_or("claude reported an error")
                            .to_string(),
                    );
                } else {
                    final_text = v.get("result").and_then(|r| r.as_str()).map(String::from);
                }
                break;
            }
            _ => {}
        }
    }

    let _ = child.wait().await;
    typing.take(); // clear the indicator if no token ever streamed

    match (stream, err) {
        // Normal: finalize with the authoritative full result. `finish` retries
        // the final edit internally until the homeserver acknowledges it, so the
        // complete reply is not left half-streamed by a transient send failure.
        (Some(s), None) => s.finish(final_text.as_deref()).await?,
        // Streamed some, then failed/timed out — finalize gracefully in place.
        // `finish` already retries; if it still can't land, log it (the partial
        // text is the best we have and the error is surfaced to the caller too).
        (Some(s), Some(e)) => {
            let mut text = final_text.unwrap_or_default();
            text.push_str(&format!("\n\n[claude-channel error] {e}"));
            if let Err(fe) = s.finish(Some(&text)).await {
                tracing::warn!("claude-channel: failed to finalize partial reply after retries: {fe:#}");
            }
        }
        // Nothing streamed but a final result arrived — send it as one message.
        // Use the reliable send so this one-shot reply isn't silently dropped.
        (None, None) => {
            let text = final_text.unwrap_or_default();
            let text = if text.trim().is_empty() {
                "[claude-channel] (no output)".to_string()
            } else {
                truncate(&text, MAX_REPLY_BYTES)
            };
            agent.send_dm_reliable(target, &text).await?;
        }
        // Failed before any output — surface the error to the caller.
        (None, Some(e)) => anyhow::bail!("{e}"),
    }
    Ok(outcome)
}

fn find_claude_bin() -> String {
    // Try absolute path first (matches the systemd unit Environment).
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = format!("{home}/.local/bin/claude");
    if std::path::Path::new(&candidate).exists() {
        return candidate;
    }
    // Fall back to PATH lookup.
    "claude".to_string()
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Cut on a char boundary to avoid splitting UTF-8.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n[...truncated]");
    out
}

#[tokio::main]
async fn main() {
    // Load this instance's config from its `.env` before parsing args (see
    // `load_dotenv`): AGENT_TARGET et al. become file-driven and per-instance.
    load_dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_agent=info,aqua_matrix_relay=info,aqua_matrix_claude_p=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = AgentConfig {
        key_file: args.key_file,
        siwx_url: args.siwx_url,
        matrix_url: args.matrix_url,
        client_id: args.client_id,
        redirect_uri: args.redirect_uri,
        store_dir: args.store_dir.unwrap_or_else(default_store_dir),
    };

    run_daemon(config, &args.target, ClaudePHandler::default()).await;
}
