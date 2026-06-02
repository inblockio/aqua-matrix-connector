# Repo-Split Execution Handover — connector substrate vs. aqua-agents

**Audience:** a fresh **ultracode** (xhigh effort + workflow orchestration) session that will execute this **end-to-end in one shot, without interruption.**
**Status:** decisions locked (see §1). Scope is the **in-place refactor only** — the physical `git filter-repo` move into the `~/aqua-agents` repo is a *separately-approved follow-up*, not this phase.
**Branch:** create `refactor/connector-agents-split` off `feat/typed-agent-instances-poc` (the orchestrator + template POC lives there, not yet on `main`).

> **How to run this:** start a session with ultracode on, point it at this file, and say *"execute this handover end-to-end."* Work **phase by phase** (§6); after each phase the **verification gate** must be green before committing; run the **final audit** (§7) before declaring done. If your harness supports parallel sub-agents / workflow orchestration, use it for the per-phase analysis and the adversarial final audit; otherwise do them serially — they are optimizations, not requirements. The gates and acceptance criteria in §6–§7 stand regardless.

---

## 0. Reading order / inputs

This handover is self-contained, but the executing session should also have:

- The codebase at `/home/waldknoten-01/aqua-matrix-agent` (workspace root).
- Project memory: `repo-split-refactor` (locked decisions + mapping), `aqua-security-deferred` (**no `aqua-sdk` dep this phase**), `agent-instance-deploy-state` (token file, symlink drift, tim-channel).
- Design docs already in-repo: `docs/ARCHITECTURE.md`, `docs/RECOVERY.md`, `docs/REFERENCE.md`, `docs/plans/typed-agent-instances.md`, `docs/plans/chat-confirmations.md`.

Sibling layout (must be preserved — path-deps depend on it): `~/aqua-matrix-agent`, `~/siwx-oidc`, `~/aqua-auth`, `~/aqua-spec`, `~/aqua-rs-sdk`, and the (currently empty) `~/aqua-agents`.

---

## 1. Locked decisions (do not re-litigate)

| # | Decision | Choice | One-line rationale |
|---|----------|--------|--------------------|
| Q1 | Data-model home | **Minimal `ContainerSpec` in connector; rich `AgentType` template stays aqua-agents** | Keeps the aquafiable capability schema evolving independently of the substrate; orchestrator becomes a reusable "run any container" engine. |
| Q2 | `MessageHandler` seam | **Moderate**: enrich `body:&str` → `InboundMessage`, return `Result`, add an `authorize()` hook. Keep the Matrix `AgentClient` reply surface. | Adds the aqua-security + audit seams now; keeps the multi-transport door open without a speculative `Channel` rewrite. |
| Q3 | Repo topology | **Current `aqua-matrix-agent` repo = connector**; aqua-agents content extracts to `~/aqua-agents`; **path-dep** between them. | Keeps the e2e-hardened transport history with the substrate; matches the existing `../siwx-oidc` convention; release-dep is a trivial future upgrade. |
| Q4 | This phase's deliverable | **Refactor in-place first** (one workspace, enforced one-way deps); defer the physical split. | A wrong dep edge is a compile error now vs. a cross-repo problem later. Prove the boundary cheaply. |

**Hard dependency rule (the invariant the whole split exists to create):** one-way only, **aqua-agents → connector, never the reverse.** Connector crates must never name an aqua-agents crate in their `Cargo.toml`.

---

## 2. CONTEXT — current state (logic-model Phase 1)

**Workspace:** virtual Cargo workspace, 6 crates under `crates/`. Build with `cargo build` (debug — release is intentionally avoided for iteration). Tests: `cargo test`; live E2E: `cargo test --test e2e --features e2e` (needs the live inblock siwx-oidc + Matrix; hardcoded URLs).

**Current crate dependency graph** (→ = depends on):
```
aqua-matrix-relay      → aqua-matrix-agent
aqua-matrix-heartbeat  → aqua-matrix-relay, aqua-matrix-template          (+ bollard, futures-util)
aqua-matrix-claude-p   → aqua-matrix-relay, aqua-matrix-ask-mcp, aqua-matrix-template  (+ uuid)
aqua-matrix-agent      → siwx-oidc-auth (../siwx-oidc/...) → aqua-auth (../../aqua-auth)
leaf substrate (no internal deps): aqua-matrix-agent, aqua-matrix-ask-mcp, aqua-matrix-template
```

**The two crates that straddle the connector/agents line internally** (must be split before they can be assigned):
- **`aqua-matrix-heartbeat`** = a transport-neutral Podman engine (`orchestrator.rs`, zero Matrix imports) **+** a Matrix ops agent (`ops.rs`/`main.rs`: host telemetry, `#shell`, `OpsHandler`).
- **`aqua-matrix-claude-p`** = a generic confirmation/gating substrate (`pending.rs`/`destructive.rs`/`ask_bridge.rs` — none mention `claude`) **+** Claude-CLI glue (`main.rs`: `ClaudeRun`/`stream_claude`/`run_gated`/`SessionStore` + `ResolvedInstance` contract injection).

**Invariants that MUST survive untouched** (hard-won, e2e-validated — see `docs/RECOVERY.md` + `docs/plans/chat-confirmations-e2e-handover.md`):
1. **Addressing/case**: always address the canonical **lowercase** MXID; sender filter is case-insensitive (`eq_ignore_ascii_case`).
2. **Client rotation** ~30 s before token expiry (matrix-sdk has no in-place token swap); `std::process::exit(1)` after 3 connect failures for systemd `Restart=always`.
3. **device_id persistence** + the tier-3 store-mismatch crypto wipe (`AgentClient::connect`, `recovery.rs`).
4. **Dedupe watermark** advanced *before* dispatch; cross-cycle carry-forward.
5. **DM-room convergence**: sync-before-join, `mark_dm` on join, continuous invite auto-join, hello-guard deferring the greeting until a DM room exists (prevents duplicate-room Megolm splits).
6. **`m.direct` / `io.inblock.aqua.registry`** best-effort writes; SSSS recovery.key flow.

> These all live in `aqua-matrix-agent` (lib) and `aqua-matrix-relay` (`run_daemon`/`dispatch`). The only relay change this phase is the **Moderate** `MessageHandler` formalization (§6 Phase 4), which must preserve the dispatch sender-gate + watermark + receipt behavior **exactly** (default `authorize()` == the current equality check).

---

## 3. GOAL (logic-model Phase 2)

> **One sentence:** Restructure the single workspace so that a clean **connector** crate-set (`aqua-matrix-agent`, `-relay`, `-ask-mcp`, **new** `-orchestrator`, **new** `-gating`) has a strictly one-way dependency relationship with an **aqua-agents** crate-set (`aqua-matrix-template`, the two backends), with the `MessageHandler` seam formalized and aqua-security hook-points scaffolded inert — all while `cargo build` / `cargo test` / live-e2e stay green and runtime behavior is unchanged.

**Done = all acceptance criteria in §7 pass.** Out of scope: the physical repo move, any `aqua-sdk` dependency, any aqua-security *implementation*, renaming the two backend binaries (deferred to the physical-split phase to avoid disrupting live systemd units/image).

---

## 4. TARGET ARCHITECTURE (this phase — one workspace, two conceptual halves)

Crates stay under `crates/` this phase (path churn is deferred to the physical move). Two NEW connector crates are extracted; `aqua-matrix-template` stays put but is now **agents-side** (no connector crate may depend on it).

```
CONNECTOR half (current repo is the substrate)
  aqua-matrix-agent        (unchanged: lib + one-shot CLI)
  aqua-matrix-relay        (Moderate MessageHandler change only)
  aqua-matrix-ask-mcp      (unchanged)
  aqua-matrix-orchestrator NEW  ← extracted from heartbeat/src/orchestrator.rs; consumes ContainerSpec
  aqua-matrix-gating       NEW  ← extracted from claude-p {pending,destructive,ask_bridge}; keyed on AgentClient

AGENTS half (extracts to ~/aqua-agents in the FUTURE phase)
  aqua-matrix-template     (AgentType/InstanceBinding/ResolvedInstance + NEW build_spawn_spec factory)
  aqua-matrix-heartbeat    (residual ops agent; deps: relay, template, orchestrator)   [rename → aqua-agent-ops later]
  aqua-matrix-claude-p     (residual Claude glue; deps: relay, template, gating)        [rename → aqua-agent-claude-p later]
  + content: types/*.json, instances/*.toml(.example), Dockerfile, scripts/build-image.sh, agent systemd units, Skills
```

**Target dependency edges (every one must hold):**
```
relay        → agent                                  (connector→connector)
orchestrator → (bollard, futures-util only; NO template)   ← KEY DECOUPLING
gating       → relay, ask-mcp                          (connector→connector)
template     → orchestrator                            (agents→connector; template names ContainerSpec via build_spawn_spec)
heartbeat    → relay, template, orchestrator           (agents→connector + agents→agents)
claude-p     → relay, template, gating                 (agents→connector + agents→agents)
```
No connector crate (`agent`, `relay`, `ask-mcp`, `orchestrator`, `gating`) may name `template`, `heartbeat`, or `claude-p`. This is enforced by the §6 Phase 6 script.

---

## 5. KEY DESIGN DETAILS (decided — implement as specified)

### 5.1 `ContainerSpec` (NEW, in `aqua-matrix-orchestrator`) — the agent/transport-agnostic spawn input
Replaces `&ResolvedInstance` as the input to `ContainerManager::spawn`. The connector **never sees `AgentType`**.
```rust
pub struct ContainerSpec {
    pub id: String,                 // instance id, e.g. "claude-channel-<ts>" (agents side owns this)
    pub target: String,             // AGENT_TARGET MXID (opaque to the connector)
    pub image: String,              // e.g. "aqua-matrix-agent:poc"
    pub env: Vec<(String, String)>, // non-secret env (AGENT_TARGET, AGENT_CONFIG_FILE, …)
    pub ref_mounts: Vec<SpecMount>, // host_path ABSOLUTE (agents side resolves relatives), container_path, read_only
    pub resources: SpecResources,   // memory_mb, cpus, pids_limit
    pub secret_env: SecretEnv,      // { env_var, source } — see below
    pub config_blob: Option<String>,// pre-serialized JSON mounted RO at /agent/config.json (agents side serializes ResolvedInstance)
}
pub struct SpecMount { pub host_path: String, pub container_path: String, pub read_only: bool }
pub struct SpecResources { pub memory_mb: u64, pub cpus: f64, pub pids_limit: u64 }
pub struct SecretEnv { pub env_var: String, pub source: SecretSource }
pub enum SecretSource { EnvOverrideThenTokenFile, EnvOverride, TokenFile(std::path::PathBuf) }
```
- **Decoupling moves to the agents side:** the new `aqua_matrix_template::build_spawn_spec(agent_type: &AgentType, target: &str, id: &str) -> ContainerSpec` (replaces the connector's `build_spawn_instance`). **Keep `id` as an injectable param so the fn stays pure and the ported test can assert a fixed id**; the caller (`cmd_spawn`) owns id generation: `id = format!("{}-{}", agent_type.name, unix_now_secs())` (mirrors today). It (a) resolves relative `ref_mounts[].host_path` to **absolute** by porting `resolve_host_path`/`normalize` into the template crate, reading the base from env `AQUA_REPO_ROOT` (fallback: a template-side const equal to today's `REPO_ROOT` value), and (b) serializes `config_blob`. **This removes `REPO_ROOT` and the `aqua-matrix-template` dependency from the connector entirely.** `aqua-matrix-template` gains an `aqua-matrix-orchestrator` dep to name `ContainerSpec` (an allowed agents→connector edge). The connector's `spawn()` keeps its *own* per-incarnation timestamp for the persist dir — independent of the id's timestamp.
- **`config.json` contract is with a THIRD party:** the in-container `aqua-matrix-claude-p` reads `/agent/config.json` back via `serde_json::from_str::<ResolvedInstance>` (`AGENT_CONFIG_FILE`; `claude-p/main.rs:744`). So the agents-side serialization must produce a **structurally identical `ResolvedInstance`** (same field set/values) — formatting/whitespace is irrelevant (today's writer is `serde_json::to_string_pretty`, `orchestrator.rs:219`; do **not** chase byte-equality). Verify with a round-trip test: `build_spawn_spec(...).config_blob` → `from_str::<ResolvedInstance>` → assert equal to `resolve(...)`.
- **Secret injection:** the agents side emits `SecretEnv { env_var: "CLAUDE_CODE_OAUTH_TOKEN", source: SecretSource::EnvOverrideThenTokenFile }`. The connector keeps `resolve_auth_secret`/`token_file` **unchanged** (env override → `AQUA_CLAUDE_TOKEN_FILE` else `<store_dir>/claude-oauth-token` → **fail-closed**) and dispatches on `spec.secret_env.source`: `EnvOverrideThenTokenFile` = today's path; `TokenFile(p)` overrides the file; `EnvOverride` skips the file. The `<store_dir>`-relative default stays connector-owned (the agents side does not know it). Driven by `spec.secret_env`, never by `AgentType.auth`. Never log the value.
- **`ref_mounts` mounting is unchanged:** the connector continues to bind every `ref_mount` `:ro` **regardless of `SpecMount.read_only`** (semantics preserved from `orchestrator.rs:272`, which already ignores the flag). The field is carried for future use only — do **not** start honoring it (that would be a runtime behavior change). `ref_mounts` are also the **per-template seam for mapping additional content** into an agent (reference repos / data / configs): a new use case adds `ref_mounts` in its `types/*.json` without touching the connector.
- **`ContainerManager::new(store_dir: Option<&Path>)`** signature unchanged; change the `None` fallback so it no longer hard-codes `.aqua-matrix-heartbeat` (callers pass `Some` today; make the dormant default neutral, e.g. `~/.aqua-orchestrator`, and document "pass `Some`").
- **`SessionInfo` stays public** (`id, container_id, container_name, target, did: Option<String>, incarnation_ts, started_at: std::time::Instant`) and is re-exported; the agents side reads it for `#shell` formatting (`.started_at.elapsed()` ⇒ keep `Instant`).
- **Carry verbatim into the connector** (do not "clean up"): the `podman unshare rm -rf <persist>` shell-out in `wipe_persist` (needs the `podman` CLI in PATH, not just the socket), the `:U` bind chown, the read-while-write `agent.log` capture race, the in-memory session map (lost on restart), and the non-idempotent fixed container name. These are the riskiest lines; preserve semantics.

**FORWARD SEAM — multi-container / docker-compose (NOT this phase):** `ContainerSpec` is deliberately single-container (`image: String`). The user's requirement that a template *may reference a docker-compose file* is **anticipated, not built**: a future `AgentType` may grow an optional `compose_ref: Option<String>` (a podman-compose/compose file path, resolved absolute like `ref_mounts`), and a future `build_spawn_spec` variant would emit a compose-backed spec consumed by a new `ContainerManager` spawn path. Treat `ContainerSpec` as **one** spawn shape, not the only one. Add a `// TODO(extensibility): optional compose_ref for multi-container agent types` marker on `AgentType` in `aqua-matrix-template`. (No compose file exists in the repo yet; this is purely a forward seam.)

### 5.2 `aqua-matrix-gating` (NEW connector crate) — public API
Lift `pending.rs` + `destructive.rs` + `ask_bridge.rs` (incl. their inline `#[cfg(test)] mod tests`) verbatim. None mention `claude`. Public surface:
```rust
// pending (ask_user primitive)
pub struct PendingMap;                 // derive Clone, Default
impl PendingMap { pub fn new() -> Self; }               // add (alias for Default) per documented API
impl PendingMap { pub async fn ask(&self, agent:&aqua_matrix_relay::AgentClient, target:&str, question:&str, timeout:std::time::Duration) -> Option<String>; }
impl PendingMap { pub fn try_resolve(&self, target:&str, body:&str) -> bool; }
impl PendingMap { pub fn is_pending(&self, target:&str) -> bool; }   // drop #[allow(dead_code)] — as a pub lib item it is public API, so the lint no longer applies (no external caller is required)
impl PendingMap { pub fn clear(&self, target:&str); }               // PROMOTE from private to pub
// destructive (matcher substrate)
pub struct destructive::Rule { pub class:&'static str, pub words:&'static [&'static str], pub substrings:&'static [&'static str], pub scope:&'static str }
pub const destructive::RULES: &[Rule];
pub fn destructive::classify(text:&str) -> Option<&'static str>;
pub fn destructive::looks_destructive(text:&str) -> bool;
// ask bridge (per-run ask_human socket)
pub struct AskBridge;
impl AskBridge { pub async fn setup(agent:&AgentClient, pending:&PendingMap, target:&str, timeout:std::time::Duration) -> anyhow::Result<Self>; }
impl AskBridge { pub fn config_path(&self) -> &std::path::Path; }     // Drop impl is automatic
pub async fn accept_loop<F,Fut>(listener: tokio::net::UnixListener, ask: F) where F: Fn(String)->Fut, Fut: std::future::Future<Output=Option<String>>;  // generic-over-closure server (so non-claude backends can serve ask_human)
// shared tool-name consts — SINGLE SOURCE OF TRUTH (kills the current drift)
pub const ASK_SERVER_KEY: &str = "ask";                  // was ask_bridge::SERVER_KEY
pub const ASK_HUMAN_TOOL:  &str = "mcp__ask__ask_human";  // was main.rs ASK_HUMAN_TOOL — derive from ASK_SERVER_KEY so they can't drift
pub const ASK_SYSTEM_PROMPT: &str = "…";                  // re-exported; backend passes it to --append-system-prompt
```
- **deps that MOVE with gating:** `uuid` (socket filenames) and `aqua-matrix-ask-mcp` (`ipc`, `SOCK_ENV`) move from `aqua-matrix-claude-p`'s `Cargo.toml` → `aqua-matrix-gating`'s. Forgetting this is the single most likely mechanical miss.
- **Landmine — duplicated fact:** `ASK_HUMAN_TOOL = mcp__<ASK_SERVER_KEY>__ask_human`. Today encoded twice across two files. Expose ONE source in gating; the backend imports it (it must not keep its own copy).
- **Cross-boundary invariant to document:** `PendingMap` alone does NOT serialize runs. The "one open question per target" guarantee also needs the backend's per-target `run_lock` (stays agents-side). So the invariant is now split across the crate boundary — document it in both crates.

### 5.3 `MessageHandler` — Moderate formalization (in `aqua-matrix-relay`)
```rust
pub struct InboundMessage<'a> {
    pub sender_mxid: &'a str,
    pub sender_did: Option<String>, // SEAM(aqua-security): None this phase. The DID allow/deny key, and the anchor for a future user proxy key (or carry it via Matrix message artefacts — see §5.4).
    pub event_id: &'a str,
    pub room_id: &'a str,
    pub timestamp_ms: u64,
    pub msgtype: &'a str,           // "m.text", …
    pub body: &'a str,
}

#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    fn role(&self) -> &str;
    fn systemd_unit(&self) -> Option<&str> { None }
    fn display_name(&self) -> String { self.role().to_string() }
    fn hello(&self, _agent: &AgentClient) -> Option<String> { None }
    fn tick_interval(&self) -> Option<Duration> { None }
    async fn on_tick(&self, _agent: &AgentClient, _target: &str) {}

    /// DID/MXID allow-deny SEAM. Default == today's exact behavior (single target,
    /// case-insensitive). `dispatch` and `register_invite_autojoin` call THIS instead
    /// of the inline equality check. SEAM(aqua-security): this is the white/blacklist
    /// hook keyed on DIDs — a future signature will take `sender_did: Option<&str>` plus
    /// a per-template allow/deny policy object (BOTH an allow-list AND a deny-list); the
    /// bool default stays `{target}` this phase so behavior is unchanged.
    fn authorize(&self, sender_mxid: &str, target: &str) -> bool {
        sender_mxid.eq_ignore_ascii_case(target)
    }

    /// Now takes a structured message and returns Result so the relay owns uniform
    /// error logging (relay still never unwinds; it logs the Err).
    async fn handle_message(&self, agent: &AgentClient, target: &str, msg: &InboundMessage<'_>) -> anyhow::Result<()>;
}
```
- **`dispatch` (relay/lib.rs ~480-537):** replace the inline `if !ev.sender.eq_ignore_ascii_case(target)` with `if !handler.authorize(ev.sender.as_str(), target)`. Keep the watermark-advance-before-dispatch, the read-receipt (already after the sender gate, so only authorized senders are acked), the non-text drop, and the empty-body drop **exactly as-is**. Construct `InboundMessage` with: `sender_mxid = ev.sender.as_str()`, `sender_did = None`, `event_id = ev.event_id.as_str()`, `room_id = room.room_id().as_str()` (room_id is NOT on `ev`), `timestamp_ms = ts_ms` (the existing `u64::from(ev.origin_server_ts.0)`), `msgtype = ev.content.msgtype.msgtype()` (returns `&str`, e.g. `"m.text"`). **Keep the trimmed body as a local owned `String` (as today: `let body = t.body.trim().to_string();`) and borrow it into `InboundMessage.body`** — every field then borrows a live local or `ev`, satisfying `InboundMessage<'_>`. Call `handler.handle_message(agent, target, &msg).await` and log any `Err` at `warn` (the relay still never unwinds).
- **`register_invite_autojoin` (relay/lib.rs ~429-457) — requires a signature change:** today it is `fn register_invite_autojoin(agent: AgentClient, target: Arc<String>, role: String)` and has **no handler in scope**. Make it generic: `fn register_invite_autojoin<H: MessageHandler>(agent: AgentClient, target: Arc<String>, handler: Arc<H>)` (derive `role` from `handler.role()`, or keep the `role` param alongside), capture+clone the `Arc<H>` into the `async move` closure with the existing clones, and **update the call site in `run_cycle` (lib.rs:364)** to pass `handler.clone()`. Then add `if !handler.authorize(ev.sender.as_str(), &target) { return; }` before `room.join()`. Effect: **invite-acceptance is intentionally narrowed to target-only** (was: any inviter); messaging/dispatch behavior is unchanged. Intended hardening, safe given the strict single-peer DM design (no flow relies on non-target invites).
- **All THREE `MessageHandler` impls** (not two): `ClaudePHandler` (`claude-p/main.rs:223`), `OpsHandler` (`heartbeat/ops.rs:282`), **and `aqua-matrix-relay/examples/echo_agent.rs`** — `cargo test` compiles examples, so the example MUST migrate or the Phase 4 gate fails to compile. Each changes `body: &str` → `msg: &InboundMessage<'_>`, returns `anyhow::Result<()>` (ending `Ok(())`), and replaces `body` with `msg.body`; `target` stays a param. echo's body becomes `agent.send_dm(target, &format!("echo: {}", msg.body)).await?; Ok(())`. Add `InboundMessage` to each crate's `use aqua_matrix_relay::{…}` import. In relay's `lib.rs`, name the return type as `anyhow::Result<()>` (anyhow is already a relay dep; fully-qualify to avoid adding a `use`).

### 5.4 aqua-security seams — scaffold INERT (structure only; NO `aqua-sdk` dep)
Per `aqua-security-deferred`. Add the shapes/markers, leave them dormant:
- **DID allow/deny (white AND blacklist)** — the `authorize()` hook above (default = `{target}`). That IS the scaffold; the seam covers both an allow-list and a deny-list keyed on DID (see the `authorize` doc comment for the future signature). Do not add list-loading this phase.
- **Capability gating by template** — leave `Contract.allowed_tools` / `Permissions.{allow,deny}` flowing into `--allowedTools`/`--disallowedTools` as today; add a `// TODO(aqua-security): validate-on-load + hash-pin` at `aqua_matrix_template::agent_type_schema()` (the seam is already documented there).
- **Signed logs** — add a `// TODO(aqua-security): append-only/signed writer seam` comment at the orchestrator `start_log_capture` and at the `PendingMap::ask` grant return.
- **Proxy keys — two distinct concerns, do not conflate:**
  - *Container→backend auth secret* (the agent's own Claude token): `// TODO(aqua-security): brokered/minted key` at `resolve_auth_secret`, and `// TODO: per-run socket auth token` at `AskBridge::setup`.
  - *User proxy key (message-level)* — **the user's explicitly-flagged seam:** add a `// SEAM(aqua-security): user proxy key` note on `InboundMessage` beside `sender_did` (§5.3), documenting that a future aqua-fy pass may carry a user-minted proxy/delegation key **either** as a new `InboundMessage` field **or** via existing Matrix message artefacts (event signatures / `m.room.member` state / custom content keys) — and that **Matrix message artefacts may well suffice**, so no new field is added now. This lives where messages are parsed, not at the container-auth layer.
- **At-rest sealing** — `// TODO(aqua-security): seal/audit` at `recovery.rs` `write_key_0600` and `wipe_crypto_store`.
- **Multi-container / docker-compose (extensibility, not aqua-security)** — `// TODO(extensibility): optional compose_ref for multi-container agent types` on `AgentType`; see the FORWARD SEAM in §5.1. `ContainerSpec` stays single-container this phase.
Do **not** add a third `AuthMethod` variant, and add **no `aqua-sdk`/aqua-security dependency**. (The structural `aqua-matrix-template → aqua-matrix-orchestrator` dep from §5.1 is fine — it is not aqua-security.) Just the comments/notes + the live `authorize()` hook.

---

## 6. EXECUTION PLAN (logic-model Phase 4 — activities → outputs, with gates)

Commit after each phase **only when its gate is green.** Phases 1–3 are mechanical-ish; Phase 4 touches `dispatch` so it carries the e2e risk. The per-phase analysis can be parallelized with a Workflow, but commits are serial with gates.

> **Gate (run after every phase unless noted):** `cargo build` (clean, no new warnings) **and** `cargo test` (all crates green — this compiles examples too, e.g. `echo_agent`). The **live e2e** (`cargo test --test e2e --features e2e`) needs reachable inblock services **and** `agent.pem`/`agent-b.pem` in `crates/aqua-matrix-agent/`. **Those pems are currently ABSENT**, and `AgentClient::connect` auto-mints a key when missing — so running e2e as-is would **silently provision NEW throwaway DIDs/accounts on the live homeserver.** Therefore: run e2e (required after **Phase 4** + final audit) *only if* the keys exist and services are reachable; otherwise **document it as un-run with the reason** (`key material absent` / `services unreachable`) and rely on `cargo test` as the gate. Never silently mint throwaway live accounts as a refactor side effect. (To run it deliberately, generate the keys first, e.g. `aqua-matrix-agent --key-file crates/aqua-matrix-agent/agent.pem --print-did`.)

**Phase 0 — Baseline.** `git checkout -b refactor/connector-agents-split feat/typed-agent-instances-poc`. Run the full gate + e2e and record the baseline (counts, pass/fail). If e2e is unreachable, note it and proceed on `cargo test` as the gate.

**Phase 1 — Extract `aqua-matrix-gating` (connector).** Create `crates/aqua-matrix-gating` (`[lib]`). Move `pending.rs`, `destructive.rs`, `ask_bridge.rs` (+ their inline tests) in verbatim; add the shared `ASK_*` consts as the single source of truth; promote `PendingMap::clear` **and `accept_loop`** from private to `pub`, add `PendingMap::new`, drop the `#[allow(dead_code)]` on `is_pending` (as a `pub` lib item it is public API — no external caller needed). `Cargo.toml`: deps `aqua-matrix-relay`, `aqua-matrix-ask-mcp`, `uuid`, `tokio`, `anyhow`, `serde_json`, `tracing` (+ dev as needed). Add `crates/aqua-matrix-gating` to the root `[workspace] members` (a **literal list, not a glob**) and `aqua-matrix-gating = { path = "crates/aqua-matrix-gating" }` to `[workspace.dependencies]`. In `aqua-matrix-claude-p`: delete `mod {pending,destructive,ask_bridge};`, add `aqua-matrix-gating` dep, remove the `aqua-matrix-ask-mcp` and `uuid` lines from claude-p's **`[dependencies]` only** (leave the root `[workspace.dependencies]` entries — gating consumes both), repoint imports (`use aqua_matrix_gating::{PendingMap, AskBridge, destructive, ASK_HUMAN_TOOL, ASK_SYSTEM_PROMPT};`), delete the local `ASK_HUMAN_TOOL` const. **Gate.**

**Phase 2 — Extract `aqua-matrix-orchestrator` (connector), no redesign yet.** Create `crates/aqua-matrix-orchestrator` (`[lib]`). **Add `crates/aqua-matrix-orchestrator` to the root `[workspace] members` (literal list) and `aqua-matrix-orchestrator = { path = "crates/aqua-matrix-orchestrator" }` to `[workspace.dependencies]`** (symmetric with Phase 1). Move `orchestrator.rs` contents in **verbatim** (still consuming `ResolvedInstance` / depending on `aqua-matrix-template` for now) so the pure *move* is proven to compile in isolation. `Cargo.toml`: deps `bollard`, `futures-util`, `tokio`, `anyhow`, `serde_json`, `tracing`, **and (temporarily) `aqua-matrix-template`**. In `aqua-matrix-heartbeat`: remove `mod orchestrator;`, add `aqua-matrix-orchestrator` dep, remove `bollard`+`futures-util` from its `[dependencies]` (leave root `[workspace.dependencies]`), repoint `use aqua_matrix_orchestrator::{ContainerManager, SessionInfo, build_spawn_instance};`. **Gate.**

**Phase 3 — `ContainerSpec` redesign (the load-bearing change; §5.1).** In `aqua-matrix-orchestrator`: introduce `ContainerSpec`/`SpecMount`/`SpecResources`/`SecretEnv`/`SecretSource`; change `spawn(&self, spec:&ContainerSpec)`; store `ContainerSpec` in `Session` for `replace`; drive secret injection from `spec.secret_env`; have the connector mount `spec.config_blob` RO at `/agent/config.json`; **delete `REPO_ROOT` + `resolve_host_path` relative-resolution and the `aqua-matrix-template` dependency from the connector.** In `aqua-matrix-template`: **add `aqua-matrix-orchestrator` to its deps** (to name `ContainerSpec`), port `resolve_host_path`/`normalize`, and add `build_spawn_spec(agent_type:&AgentType, target:&str, id:&str) -> ContainerSpec` (id injectable → pure; resolve ref-mounts absolute against `AQUA_REPO_ROOT` env / template-side const default; serialize the `ResolvedInstance` JSON into `config_blob` — **structurally** identical to today, not byte-identical). `cmd_spawn` computes `id = format!("{}-{}", agent_type.name, unix_now_secs())` and passes it. Move `build_spawn_instance`'s test here (it asserts a fixed id — `id` injectable keeps it deterministic); add the **config_blob round-trip test** (serialize → `from_str::<ResolvedInstance>` → assert equal to `resolve(...)`). In `aqua-matrix-heartbeat` `cmd_spawn`: build a `ContainerSpec` via `build_spawn_spec` and call `mgr.spawn(&spec)`; `cmd_sessions/replace/stop/agent_logs/sessions_summary` only change the `SessionInfo` import path; keep agents-side `format_duration`/`short_id`/`unix_now_secs` (drop the latter if it goes unused). **Verify the connector crate's `Cargo.toml` no longer lists `aqua-matrix-template`.** **Gate.**

**Phase 4 — `MessageHandler` Moderate formalization (§5.3).** Edit `aqua-matrix-relay`: add `InboundMessage`, the `authorize()` default method, change `handle_message` to `(agent, target, &InboundMessage) -> anyhow::Result<()>`; reroute `dispatch` through `authorize` + build `InboundMessage` (owned trimmed `String` borrowed in; `msgtype()`; `room.room_id().as_str()`); apply the **`register_invite_autojoin` signature change + call-site update at lib.rs:364** (§5.3); log handler `Err` at `warn`. Update **all THREE** `MessageHandler` impls — `ClaudePHandler`, `OpsHandler`, **and `examples/echo_agent.rs`** (`cargo test` compiles examples) — to the new signature, `body`→`msg.body`, add `InboundMessage` to each import. **Preserve every invariant in §2.** **Gate + e2e per the gate/§7.3 rule** (this is the phase that can resurrect duplicate-room/sender-gate regressions).

**Phase 5 — aqua-security inert scaffolding (§5.4).** Add the `authorize()` seam doc + the `// TODO(aqua-security): …` markers at the listed sites. No deps, no behavior change. **Gate.**

**Phase 6 — Enforcement + docs + memory.**
- Add a **dependency-direction check** the project can run repeatedly. Create `scripts/check-dep-direction.sh` (bash). It must inspect **dependency lines only, never package metadata** (the `description` fields legitimately mention crate names — an unanchored grep would false-positive). For each connector crate (`aqua-matrix-agent`, `-relay`, `-ask-mcp`, `-orchestrator`, `-gating`): `grep -nE '^[[:space:]]*aqua-matrix-(template|heartbeat|claude-p)\b' crates/<connector>/Cargo.toml` must be **empty** (a dep entry like `aqua-matrix-template.workspace = true` or `... = { path = ... }` starts the line; the `description = "…"` line does not). Exit 1 and print the offending crate+line on any match; print `OK` otherwise. Wire it into the gate (note it in CLAUDE.md as the boundary guard).
- Split the docs: add a "Repo boundary" section to `docs/ARCHITECTURE.md` describing the connector vs agents halves, the one-way rule, the `ContainerSpec` seam, and the `MessageHandler` seam; cross-reference this handover.
- Update `CLAUDE.md`'s Architecture section **and `docs/ARCHITECTURE.md` (which already says "Four crates" — stale: the workspace has six pre-refactor and eight after)**: list all eight crates (agent, relay, ask-mcp, template, heartbeat, claude-p, **orchestrator**, **gating**), the one-way boundary rule, and the check script.
- Update memory `repo-split-refactor` "Locked decisions" if any detail changed during execution (keep it accurate).
- **Final gate** + §7 audit.

### 6.5 Abort / recovery (unattended runs)

This handover targets unattended execution, so failure handling is explicit:
- **Commit only on a green gate** (per phase) — the last commit is always a known-good state.
- **On an unrecoverable failure** (a gate that cannot go green, a compile error you cannot resolve, or a §8 boundary that would have to be violated): `git reset --hard HEAD` to drop the failed phase's WIP, leaving the last green commit intact. Do **not** force a partial phase through.
- **"Stop and surface it" in unattended mode** = HALT without committing the offending change, and write a final report naming: the phase that failed, the exact error/output, and which §2 invariant or §8 condition triggered the stop. Do not improvise around a protected-logic change.

---

## 7. ACCEPTANCE / AUDIT (logic-model Phase 4 outcome verification + Phase 5 boundaries)

Run an **independent adversarial audit** (a Workflow whose agents try to *falsify* each criterion). Done requires **all** green:

1. **Builds clean:** `cargo build` succeeds, no new warnings vs. baseline.
2. **Tests green:** `cargo test` passes for every crate (gating/orchestrator/template tests included, in their new homes).
3. **E2E:** if `agent.pem`/`agent-b.pem` exist *and* the inblock services are reachable, `cargo test --test e2e --features e2e` passes. Otherwise it is explicitly documented as **un-run** with the reason (`key material absent` and/or `services unreachable`) — never silently skipped, and never run in a way that mints throwaway live accounts.
4. **Dependency direction:** `scripts/check-dep-direction.sh` exits 0. Manually confirm **`aqua-matrix-orchestrator`'s `Cargo.toml` does NOT list `aqua-matrix-template`** (the key decoupling) and no connector crate names any backend.
5. **`ContainerSpec` decoupling real:** the connector never references `AgentType`/`ResolvedInstance`; the config_blob round-trip test proves the in-container `/agent/config.json` is **structurally** preserved (re-parsed via `from_str::<ResolvedInstance>`; formatting is irrelevant).
6. **`MessageHandler` behavior preserved:** default `authorize()` == the old equality check; dispatch still advances the watermark before dispatch; non-text/empty bodies still dropped; invite auto-join now gated by `authorize` (default unchanged); read-receipt only for authorized senders.
7. **No aqua-security implementation, no `aqua-sdk`:** `grep -ri "aqua-sdk\|aqua_rs_sdk" crates/*/Cargo.toml` is empty; the only aqua-security additions are the `authorize()` hook + `// TODO(aqua-security)` comments.
8. **Scope honored:** no physical repo move; backends NOT renamed; systemd units, `Dockerfile`, `build-image.sh`, `types/*.json`, `instances/*` unchanged (binary names preserved ⇒ no path edits needed this phase).
9. **No secrets added:** `git status`/`git diff` introduce no `*.pem`, real `.env`, or tokens; `.gitignore` still covers them.
10. **Invariants intact:** a reviewer agent reads `run_daemon`/`dispatch`/`AgentClient::connect` diffs and confirms none of the §2 invariants changed semantically.

---

## 8. BOUNDARY CONDITIONS — what must NOT happen (logic-model Phase 5)

- **Do NOT invert the dependency direction.** No connector crate may depend on `template`/`heartbeat`/`claude-p`. (Phase 6 script guards this.)
- **Do NOT add `aqua-sdk`/`aqua-rs-sdk` as a dependency** or implement any aqua-security logic. Seams only.
- **Do NOT do the physical `git filter-repo` split** into `~/aqua-agents` this phase. Do NOT push to the `aqua-agents` remote.
- **Do NOT rename the two backend binaries** (`aqua-matrix-heartbeat`, `aqua-matrix-claude-p`) — that would break live systemd `ExecStart` paths + the image. The rename to `aqua-agent-ops`/`aqua-agent-claude-p` is a physical-split-phase task.
- **Do NOT touch** the rotation / device_id / store-mismatch / SSSS / watermark / DM-convergence / addressing logic except the precisely-specified `dispatch`/`authorize` reroute. If a change seems to require touching them, **stop and surface it.**
- **Do NOT "clean up" the orchestrator landmines** (`podman unshare rm -rf`, `:U`, fixed container name, in-memory session map). Preserve semantics; only relocate.
- **Do NOT change the `/agent/config.json` serialized shape** — a third party (the in-container binary) depends on it.
- **Do NOT regenerate or move identity material** (`*.pem`, store dirs). They are local working-tree artifacts bound to live DIDs.
- **Single-container is a this-phase scope limit, not a design ceiling.** `ContainerSpec` stays single-container; the docker-compose / multi-container path is a documented forward seam (§5.1), not part of this phase. Do not implement `compose_ref` now.
- **Assumptions** (surface immediately if false): the sibling layout (`../siwx-oidc`, `../../aqua-auth`) is intact; the `podman` CLI + user socket are available for any spawn test; the inblock services are reachable for e2e.

---

## 9. TOP RISKS → mitigations

1. **Breaking an e2e transport invariant** in the `dispatch`/`authorize` reroute → keep the default `authorize` identical; run live e2e after Phase 4; diff-review `run_daemon`.
2. **Reverse dep via `template`** silently compiling in one workspace → Phase 6 script + manual `Cargo.toml` check (criterion 4); the whole point of in-place-first is to catch this as a check, not a cross-repo problem.
3. **Mechanical misses on moved deps** (`uuid`/`ask-mcp` → gating; `bollard`/`futures-util` → orchestrator) → criterion 1/2 will fail to compile; double-check each `Cargo.toml`.
4. **`config_blob` drift** changing what the in-container agent reads → the structural round-trip test (criterion 5; structural equality, not byte equality).
5. **Hidden host coupling moved into the connector** (`REPO_ROOT`, `.aqua-matrix-heartbeat` default) → §5.1 moves path resolution agents-side and neutralizes the `None` default; criterion confirms the connector is host/agent-agnostic.

---

## Appendix A — Extraction maps (item → target)

**`aqua-matrix-claude-p`:** → **`aqua-matrix-gating` (connector):** all of `destructive.rs` (`Rule`, `RULES`, `normalise`, `classify`, `looks_destructive`, tests); all of `pending.rs` (`PendingMap` + impls + tests); all of `ask_bridge.rs` (`AskBridge`, `setup`, `Drop`, `accept_loop`, `handle_conn`, `ask_mcp_bin_path`, `SERVER_KEY`→`ASK_SERVER_KEY`, `ASK_SYSTEM_PROMPT`, tests); `ASK_HUMAN_TOOL` (from main.rs). → **stays `aqua-matrix-claude-p` (agents):** `main.rs` everything else — `IDLE_TIMEOUT`, `APPROVAL_TIMEOUT`, `MAX_REPLY_BYTES`, `ROLE`, `UNIT`, `DEFAULT_CONFIG_FILE`, `Args`, `default_store_dir`, `SessionStore`(+impl), `ClaudePHandler`(+impls+`MessageHandler`), `run_turn`, `run_gated`, `ClaudeRun`(+impl), `RunOutcome`, `stream_claude`, `load_resolved_config`, `find_claude_bin`, `truncate`, `main`.

**`aqua-matrix-heartbeat`:** → **`aqua-matrix-orchestrator` (connector):** all of `orchestrator.rs` — module docs, `CONTAINER_PREFIX`, `REPO_ROOT`(→removed in Phase 3), `DID_DISCOVERY_TIMEOUT_SECS`, `SessionInfo`(pub), `Session`(priv), `ContainerManager`(+`new`/`persist_dir`/`token_file`/`resolve_auth_secret`/`spawn`/`replace`/`stop`/`list`/`tail_log`/`start_log_capture`/`discover_did`), `podman_socket_uri`, `current_uid`, `default_store_dir`, `persist_dir_for`, `path_str`, `wipe_persist`, `resolve_host_path`(→agents in Phase 3), `normalize`, `log_output_bytes`, `scan_agent_did`, `short_id`, `unix_now_secs`, tests. **`build_spawn_instance` → agents-side as `build_spawn_spec(agent_type, target, id)`** (id injectable → stays pure; its test ports with it; the single biggest rewrite). → **stays `aqua-matrix-heartbeat` (agents):** all of `ops.rs` (`OpsHandler` + `cmd_*` + host telemetry + `MessageHandler`) and `main.rs`.

**Note (relay, connector):** `aqua-matrix-relay/examples/echo_agent.rs` stays in relay but its `impl MessageHandler` migrates to the new `(agent, target, &InboundMessage) -> anyhow::Result<()>` signature in Phase 4 — `cargo test` compiles examples, so this is gate-blocking, not optional.

## Appendix B — `ContainerManager` public API (post-Phase-3)
```
ContainerManager::new(store_dir: Option<&Path>) -> Result<Self>        // sync
ContainerManager::spawn(&self, spec: &ContainerSpec) -> Result<SessionInfo>   // async  (was &ResolvedInstance)
ContainerManager::replace(&self, id: &str) -> Result<SessionInfo>      // async; wipes persist ⇒ new identity
ContainerManager::stop(&self, id: &str) -> Result<()>                  // async
ContainerManager::list(&self) -> Vec<SessionInfo>                      // sync; try_lock, empty on contention
ContainerManager::tail_log(&self, id: &str, n: usize) -> Result<String> // async
podman_socket_uri() -> String                                          // pub helper
SessionInfo { id, container_id, container_name, target, did: Option<String>, incarnation_ts: u64, started_at: Instant }  // pub, Debug+Clone
```

## Appendix C — Path/name references for the FUTURE physical split (informational; do NOT change this phase)
History audit: **clean** — no `*.pem`/real `.env`/`.credentials*`/`*.db` were ever committed; the legacy `aqua-matrix-hello` name has **zero** in-repo hits. So no history scrub is required for the physical move on the basis of secrets. When the physical split happens, these couple to the repo dir name / image tag and must be reconciled then: image tag `aqua-matrix-agent:poc` (in `build-image.sh`, both `types/*.json`); `%h/aqua-matrix-agent/target/debug/...` in the 3 systemd units; the `../siwx-oidc` / `../../aqua-auth` path-deps + `Dockerfile`/`build-image.sh` sibling staging; `../aqua-spec` / `../aqua-rs-sdk` ref-mount host_paths in `types/*.json`; the out-of-repo `~/aqua-matrix-hello` symlink (`agent-instance-deploy-state` memory).
