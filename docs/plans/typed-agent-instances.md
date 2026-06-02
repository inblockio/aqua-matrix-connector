# Typed, instantiable agent instances (Podman isolation)

Status: **POC VALIDATED end-to-end** (2026-06-01). All phases done & audited; the live consumer-agent + orchestrator qualification passed. See Progress log at the bottom. (Code is uncommitted in the working tree.)

This plan moves aqua-matrix from three hand-coded, single-identity daemons to a
**type → instance** model: a JSON-schema-defined agent *type* that can be
instantiated N times, where **the container is the unit of isolation, identity,
and lifetime**.

## Guiding principle (the decisive simplification)

> **One container = one agent = one identity, persistent for as long as that
> container lives.** "Ephemeral" is not a per-task worker — it is the container
> itself. Replace the container and you deliberately get a **new identity** and
> **fresh context**. The container *is* the isolation guarantee.

This eliminates nested per-task worker containers, docker-in-docker, and all
cross-replacement identity-persistence engineering. It *leverages* the existing
daemon, which already mints a `.pem` on first run and already preserves
`device_id` across restarts — under this model that machinery is exactly right,
unchanged.

### Linchpin: restart vs. replace

| Operation | Container object | Writable layer | Result |
|---|---|---|---|
| crash → `restart` / `stop`+`start` | same | intact | same agent: same `.pem`/DID/memory; `device_id` preserved via existing refresh logic |
| **replace** (`rm` + recreate) | new | fresh | new agent: self-mints a new `.pem` → new DID/account, fresh context, full re-auth |

**Invariant:** identity, Matrix crypto store, and claude memory live in the
container's **writable layer — never a named volume**. Named/host volumes would
make identity survive replacement, which is precisely what we do *not* want. The
only thing that persists across replacement is **orchestrator-harvested logs**
(host-side). This resolves "fresh context on respawn" vs. "logging-gated
transcripts": memory is the container's and dies with it; the audit trail is the
orchestrator's and outlives it.

## Locked decisions

- **Runtime:** rootless **Podman** (Docker-API-compatible socket; `bollard` talks to it unchanged).
- **Claude auth:** keep the **subscription** — inject a per-container `CLAUDE_CODE_OAUTH_TOKEN` (no shared `~/.claude`).
- **Orchestrator = the extended heartbeat.** `aqua-matrix-heartbeat` already owns `heartbeat.pem` → a DID; that is the spawning system's identity. We extend its `#shell` + status surface to drive Podman and report sessions.
- **Build:** iterative; start from one POC, audit each phase before moving on.

## Three-level model

- **Type** (aqua-pinnable JSON, later): image ref · role/display · scope contract (`allowedTools`/permissions) · memory policy · RO ref-mounts · resource limits · auth method.
- **Instance slot** (`instances/<id>.toml`): type ref · `AGENT_TARGET` (who it reports to / its allow-list) · label · overrides. **No identity stored** — identity is per-incarnation.
- **Incarnation:** the live container with its self-minted DID. Replace ⇒ new incarnation ⇒ new DID, same slot, same type.

## Target topology

```
HOST (WSL2 + systemd-user + rootless podman)
└─ aqua-matrix-heartbeat  ⇒  ORCHESTRATOR  (identity: heartbeat.pem → DID)
     • drives podman via bollard
     • reports over Matrix: active sessions + each agent DID + bound-to target did-key + activity
     • stores agent logs host-side: ~/.aqua-matrix-heartbeat/sessions/<instance>/<incarnation>/
     │
     └─ podman container = ONE agent instance  (entrypoint: aqua-matrix-claude-p relay daemon)
          • config in     : AGENT_TARGET + CLAUDE_CODE_OAUTH_TOKEN + frozen contract
          • self-mints    : its own .pem → DID  (replace ⇒ new identity; restart ⇒ same)
          • RO mounts     : ../aqua-rs-sdk + ../aqua-spec  (read-only reference knowledge)
          • mutable memory: /agent/memory (CLAUDE_CONFIG_DIR — per-user notes, in writable layer)
          • RO contract   : settings.json + --allowedTools  (frozen for the container's life)
          • hardening     : cap_drop=ALL · no-new-privs · ro-rootfs + tmpfs · mem/cpu/pids caps
```

## References we build on

- `../agent-customer-portal/src/docker.rs` — bollard `ContainerManager`: spawn/health/pause/stop/remove, port alloc, network DNS, `start_log_capture`→DB, `try_ingest_before_stop`, two-tier idle. **The orchestration blueprint.**
- `../aqua-payroll-agent/Dockerfile` — multi-stage Rust build, non-root user, `/data/sessions` volume, `HEALTHCHECK`. **The type-image blueprint.**

## Phased build (audit-gated)

- **Phase 0 — Podman + bollard reachability.** Install rootless podman; enable user socket; confirm `$XDG_RUNTIME_DIR/podman/podman.sock` answers the Docker REST API; validate cgroup-v2 freezer + subuid/subgid. Consult `~/DevOps.md` for WSL2 networking gotchas.
- **Phase 0.5 — Auth spike (de-risk first).** Run `claude` headless **inside a throwaway podman container** authed only by an injected `CLAUDE_CODE_OAUTH_TOKEN`, **no** shared `~/.claude`. Pass ⇒ proceed. Fail ⇒ auth flips to `ANTHROPIC_API_KEY` (flag before image work).
- **Phase 1 — Type image.** `Dockerfile` modeled on payroll-agent. Solve the build-context problem: `siwx-oidc-auth`/`aqua-auth` are path-dep siblings *outside* the repo → `scripts/build-image.sh` stages `aqua-matrix-agent` + `siwx-oidc` + `aqua-auth` into one context.
- **Phase 2 — Type/instance data model.** New `aqua-matrix-template` module/crate: `types/<type>.json` (JSON-Schema-validated) + `instances/<id>.toml`; loader + validator. No identity stored.
- **Phase 3 — Orchestrator (extend `OpsHandler`).** bollard `instances` module (spawn/health/restart/**replace**/stop + log-capture→host store + ingest-before-stop). Extend `handle_command`: `#shell sessions | spawn <type> [target] | replace <id> | stop <id> | logs <id> [N]`. Extend status with a **sessions panel** (instance · agent DID · bound-to did-key · state · uptime · last activity).
- **Phase 4 — Type-driven handler.** Generalize `ClaudePHandler`: `ROLE`/`UNIT`/hello/display + `--allowedTools`/settings come from the mounted type config + RO contract, not consts; memory at `/agent/memory`.
- **Phase 5 — End-to-end POC + qualification.** See below.

## Qualification harness (consumer-agent verification)

Qualify a spawned instance with **our own consumer/operator test agent**, reusing
the `/e2e-test` two-identity pattern:

1. Create a consumer identity: `consumer-test.pem` → DID → MXID (`aqua-matrix-agent --key-file consumer-test.pem --print-did`).
2. Spawn the agent instance with `AGENT_TARGET` = the consumer's MXID — i.e. the
   consumer DID *is* the receiver/allow-list (the relay's `dispatch()` already
   enforces `sender == target`).
3. From the consumer identity, DM the instance and read its reply
   (`aqua-matrix-agent --key-file consumer-test.pem --target <instance-mxid> --message ... --read`); assert it ran `claude` and replied correctly.
4. `#shell sessions` on the orchestrator shows the instance bound to the consumer DID + activity.
5. `#shell replace <id>` ⇒ instance gets a **new DID** + fresh memory ⇒ consumer re-establishes ⇒ confirms the identity/isolation model.

## Definition of done (POC)

One type → a live agent container managed by the orchestrator-heartbeat, with its
own DID, RO `aqua-rs-sdk`+`aqua-spec`, mutable in-container memory, subscription
auth, host-side provenance logs, and Matrix reporting of session/binding/activity
— plus a working `replace` that yields a new identity, **verified live by the
consumer test agent**. Stretch: a second `#shell spawn` showing two
provably-isolated instances.

## Seams left unused (no code now)

- aqua-fication: hash-pin the type schema, contract, skills bundle, and session logs.
- DID-based communication allow-list (beyond the single `AGENT_TARGET`).
- capability-gating-by-template.

## Top risks (assumptions to confirm before proceeding)

- **R0** rootless podman on WSL2: socket / cgroup-v2 freezer / subuid-subgid / networking (Phase 0).
- **R0.5** subscription `CLAUDE_CODE_OAUTH_TOKEN` working headless in-container (Phase 0.5 — gates image work).
- **R1** build-context path-deps (Phase 1 staging script).

## Progress log

### Phase 0 — rootless Podman (DONE, audited 2026-06-01)
- Installed **podman 5.7.0** (rootless) on Ubuntu 26.04 + deps (crun, conmon, netavark, aardvark-dns, slirp4netns, pasta/passt, fuse-overlayfs, uidmap, criu).
- User socket active: `systemctl --user enable --now podman.socket` → `/run/user/1000/podman/podman.sock`.
- **bollard target:** `DOCKER_HOST=unix:///run/user/1000/podman/podman.sock` — Docker REST API answers (`ApiVersion 1.41`). Independently re-verified by main loop (clean-state pull+run of alpine printed `audit-ok`).
- Facts: `rootless=true`, cgroup **v2** (systemd manager), storage **overlay** (fuse-overlayfs), net **netavark**. `cgroup.freeze` present on the user slice → `podman pause` should work (cgroup-v2 built-in freeze, not a v1 `freezer` controller).
- Caveats: only `cpu memory pids` controllers delegated rootless (no `io`/`cpuset`). WSL pauses during Windows Modern Standby → container uptime freezes across host sleep.

### Phase 0.5 — auth spike (DONE, audited 2026-06-01)
- **PASS:** `claude` ran headless in a podman container (ubuntu:26.04 base; host binary at `~/.local/share/claude/versions/2.1.158` mounted RO; glibc 2.43 match), authenticated by the **subscription** via `CLAUDE_CODE_OAUTH_TOKEN`, with an **empty isolated `CLAUDE_CONFIG_DIR`** → returned `SPIKE_OK`, exit 0.
- Confirms: subscription works headless (no `ANTHROPIC_API_KEY`); `CLAUDE_CODE_OAUTH_TOKEN` accepts the token (no credential-file mount → no refresh-rotation risk to the host login); memory isolation works (zero host `~/.claude` access).
- **Carry-forward (Phase 5):** the spike used the short-lived *access* token. Persistent instances should inject a long-lived token from `claude setup-token` (one-time interactive mint by the operator). The POC image can install `claude` properly rather than mount the host binary.

### Phase 2 — type/instance data model (DONE, audited 2026-06-01)
- New crate `crates/aqua-matrix-template`: `AgentType` / `InstanceBinding` / `ResolvedInstance` + loader/validator + `schemars`-derived JSON Schema (the aqua-pinnable artifact). All structs `deny_unknown_fields`. 6 tests pass (re-run by main loop).
- Example `types/claude-channel.json` + `instances/claude-channel-1.toml.example`; real `instances/*.toml` gitignored.
- Consumption: orchestrator `load_types`+`load_instances`+`resolve` → serialize `ResolvedInstance` to `/agent/config.json` (RO mount); in-container agent reads it back.

### Phase 4 — type-driven handler (DONE, audited 2026-06-01)
- `aqua-matrix-claude-p` reads `AGENT_CONFIG_FILE` (default `/agent/config.json`) → `ResolvedInstance`. Present ⇒ typed mode (role/display/hello/contract/memory from config); **absent ⇒ byte-for-byte legacy** (host systemd unaffected); present-but-unparseable ⇒ exit 1 (fail loud, no silent downgrade).
- Every `claude` run injects the frozen contract: `--allowedTools` (contract.allowed_tools [+ ask tool when bridge up]), `--disallowedTools` (permissions.deny), `CLAUDE_CONFIG_DIR`=memory.config_dir. Flags verified present on `claude` 2.1.158. Destructive-gate / ask_human / session-continuity preserved. 11 tests pass (re-run by main loop).
- **Image note:** the ask_human bridge shells out to a sibling `aqua-matrix-ask-mcp` binary (`ask_bridge.rs::ask_mcp_bin_path` looks beside the running exe) — the image must ship BOTH `aqua-matrix-claude-p` and `aqua-matrix-ask-mcp` co-located.

### Phase 1 — type image (DONE, audited 2026-06-01)
- `Dockerfile` (multi-stage: `rust:1.93-bookworm` builder → `ubuntu:26.04` runtime, glibc-matched) + `scripts/build-image.sh` (rootless podman; stages `aqua-matrix-agent`+`siwx-oidc`+`aqua-auth` into a mktemp context so the `../siwx-oidc/..`/`../../aqua-auth` path-deps resolve; stages the host `claude` binary; `--stage-only` flag) + `.dockerignore`. No secrets baked.
- Ships both `aqua-matrix-claude-p` + `aqua-matrix-ask-mcp` in `/usr/local/bin` co-located with `claude`. `/agent` writable layer (store/memory); ENV `AGENT_STORE_DIR`/`AGENT_KEY_FILE`/`AGENT_CONFIG_FILE`; non-root `agent` user.
- **Audit found a real blocker (fixed):** the runtime stage lacked `libsqlite3.so.0` (matrix-sdk crypto store) → daemon aborted at startup. Added `libsqlite3-0` to the runtime apt install. (Also: podman builds OCI, so the `HEALTHCHECK` is ignored — harmless; the orchestrator owns readiness.) Image `localhost/aqua-matrix-agent:poc` ~1.0 GB; `claude --version` works inside; both binaries present.

### Phase 3 — orchestrator = extended heartbeat (DONE, audited 2026-06-01)
- `crates/aqua-matrix-heartbeat/src/orchestrator.rs`: bollard 0.18.1 (= portal) `ContainerManager` (`spawn`/`replace`/`stop`/`list`/`tail_log`) against the rootless podman socket; host-side log capture (`<store>/sessions/<id>/<incarnation>/agent.log`); best-effort DID discovery from logs.
- **Writable-layer invariant verified in code:** `HostConfig` sets NO `readonly_rootfs` and NO `/agent` volume (only `config.json:ro` + `ref_mounts:ro` binds); cap_drop=ALL, no-new-privileges, tmpfs /tmp, mem/cpu/pids caps, on-failure restart. Token read from orchestrator env by name, never logged, token-less spawn refused.
- `OpsHandler` extended: async pre-dispatch for `#shell sessions | spawn <type> <target> | replace <id> | stop <id> | agent-logs <id> [N] | types`; legacy sync commands (help/ping/status/uptime/restart/respawn/logs) unchanged; status panel gains a sessions summary. 12 tests pass (re-run by main loop); workspace builds.

### Phase 5 — live qualification (IN PROGRESS, 2026-06-01)
- **Consumer-agent e2e — PROVEN.** Consumer identity `qual-test` (`did:key:z6Mkeuk…`) created. Agent spawned from `localhost/aqua-matrix-agent:poc` bound to the consumer DID as `AGENT_TARGET`, under the orchestrator's exact HostConfig (cap_drop=ALL, no-new-privileges, tmpfs /tmp, 2 GB/2 cpu/512 pids). Container booted in **typed mode** (`allowed_tools=6 deny=7`), self-minted a DID, connected to Matrix from inside the container, auto-joined the consumer's room, ran `claude` on the **subscription** under the contract, and replied **`QUAL_OK`** — read back by the consumer. (Subscription token = the short-lived access token; durable `setup-token` is the lasting-deployment step.)
- **Identity model — PROVEN.** `podman restart` (same container) ⇒ **same** DID (`z6MkkCpp…`). `podman rm` + recreate (replace) ⇒ **new** DID (`z6MktD9j…`). Restart=same identity, replace=new identity, as designed.
- **Orchestrator live spawn — PROVEN.** A *test* orchestrator (separate identity/store; production `aqua-matrix-heartbeat.service` left untouched) received `#shell spawn claude-channel <consumer-did>` from the consumer, auto-joined, ran bollard `create`+`start`, and spawned `aqua-agent-claude-channel-…` → reported `did did:key:z6MknBYh…` (the agent's OWN self-minted DID) bound-to the consumer. That orchestrator-spawned agent then answered the consumer's DM with `ORCH_QUAL_OK`. `#shell sessions` renders: `instance | container | did did:key:… | → <target> | up 5m0s` — active sessions + who they're bound to (did-keys) + activity.
- **Bug found & fixed during qualification:** `discover_did` scanned for the first `@…:…` MXID — which is the *target* — conflating agent-DID with bound-to-DID. Fixed to parse the agent's own `agent DID: did:key:…` (the target only ever appears in `@did-key-…` hyphen form). Rebuilt; 12 tests pass.
- **Pre-existing relay edge observed (NOT introduced here):** a DM arriving during the ~30 s client-rotation gap is dropped — the next cycle resets its watermark to "now" and treats it as backlog. Amplified by the test token's short TTL (~270 s ⇒ frequent rotations); one `#shell sessions` was missed this way, then succeeded mid-cycle. Harden later (persist/carry the watermark across rotation).
- **Polish:** the daemon doesn't trap SIGTERM, so stop/restart hits the 10 s SIGKILL grace — add graceful shutdown.

**POC COMPLETE — all headline claims validated end-to-end:** container = isolation + identity + lifetime (restart=same DID, replace=new DID); typed/instantiable via `config.json`; frozen contract (`--allowedTools`/`--disallowedTools`/isolated `CLAUDE_CONFIG_DIR`); subscription auth headless in-container; orchestrator (extended heartbeat) spawns + reports sessions/bindings; consumer-agent verification (QUAL_OK / ORCH_QUAL_OK). Deliverable image: `localhost/aqua-matrix-agent:poc`.




