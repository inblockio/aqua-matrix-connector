# Repo-Split Execution — PROGRESS LEDGER (self-handover)

> **Purpose:** durable state for the autonomous execution of `repo-split-execution-handover.md`.
> If context was compacted: RE-READ this file + `repo-split-execution-handover.md`, run
> `git -C ~/aqua-matrix-agent log --oneline -15` and `git status`, then resume at the first
> unchecked phase below. Each phase is committed ONLY on a green gate (cargo build + cargo test).

## Mission (user-authorized, supersedes doc §8's "defer physical split")
1. Execute handover Phases 0–6 in-place on branch `refactor/connector-agents-split` (off `feat/typed-agent-instances-poc`).
2. THEN physical split: move agents-half crates+content into `~/aqua-agents` (repo exists, has remote), path-dep back to connector. Commit + push BOTH repos.
3. Final test: create MY OWN checker/recipient Matrix agent identity and E2E-test the spawned agents (heartbeat + claude-p) against it — independent of the user.
4. HARD CONSTRAINT: operate ONLY inside `~/aqua-matrix-agent` and `~/aqua-agents`. Never touch siwx-oidc/aqua-auth/aqua-spec/aqua-rs-sdk etc.
5. May install tools as needed.

## Invariant (the whole point): one-way deps aqua-agents → connector, NEVER reverse.
Connector crates = agent, relay, ask-mcp, orchestrator(NEW), gating(NEW).
Agents crates    = template, heartbeat, claude-p.

## Environment facts (verified 2026-06-02)
- podman 5.7.0, cargo 1.95.0, target/debug exists (23G → incremental builds fast).
- agent.pem / agent-b.pem ABSENT → existing `cargo test --test e2e` would mint throwaway live
  accounts. DO NOT run it blind. Final validation = my own checker agent (mission step 3).
- Root Cargo.toml members = literal list (agent, relay, heartbeat, claude-p, ask-mcp, template).
  [workspace.dependencies] lists agent, relay, ask-mcp, template (+ external).

## Phase status
- [x] Phase 0 — baseline GREEN. build exit0/0 warnings; `cargo test` = 45 passed / 0 failed (2+14+11+12+6 across crates). e2e NOT RUN (pems absent). One pre-existing fix applied: added `system_prompt: None` to the `AgentType` test literal in heartbeat/src/orchestrator.rs (broken on source branch by commit 6252fdb). Committed as baseline.
- [x] Phase 1 — extract aqua-matrix-gating (connector). Commit b03f01c. build 0 warn, test 46/0. Files tracked as renames (history preserved). gating names no backend ✓. claude-p now deps relay+gating+template; uuid+ask-mcp moved to gating. ASK_HUMAN_TOOL/ASK_SERVER_KEY/ASK_SYSTEM_PROMPT single-sourced in gating w/ drift-guard test.
- [x] Phase 2 — extract aqua-matrix-orchestrator (verbatim move). Commit 55ef47c. build 0 warn, test 46/0. `git mv` → zero content delta. heartbeat now deps orchestrator; bollard+futures-util moved out. ops.rs imports `aqua_matrix_orchestrator::{build_spawn_instance, ContainerManager}`. NOTE: orchestrator STILL deps template (REPO_ROOT/resolve_host_path/ResolvedInstance) — REMOVED in Phase 3; dep-check would flag this now, expected. (IDE diagnostics after this phase were stale; verified on-disk correct.)
- [x] Phase 3 — ContainerSpec redesign. Commit c587cd0. build 0 warn, test 47/0 (INDEPENDENTLY re-verified fresh; E0425 IDE diag was stale). Connector decoupled: orchestrator drops template dep + all AgentType/ResolvedInstance refs ✓. template now deps orchestrator (edge INVERTED). build_spawn_spec(agent_type,target,id) in template (id injectable, pure); REPO_ROOT const="/home/waldknoten-01/aqua-matrix-agent", AQUA_REPO_ROOT env override; config_blob via to_string_pretty (structural). resolve_auth_secret gained file_override param (None branch byte-identical to before); driven by spec.secret_env.source. ContainerSpec/SpecMount/SpecResources/SecretEnv/SecretSource added. Landmines carried verbatim. compose_ref FORWARD SEAM TODO on AgentType. aqua-security TODOs deferred to Phase 5 (correct).
- [x] Phase 4 — MessageHandler Moderate formalization. Commit 798b8a2. build 0 warn, test 47/0 (INDEPENDENTLY re-verified; IDE diags were stale mid-edit snapshots). I READ dispatch (lib.rs:532-604) + register_invite_autojoin (468) myself: ALL §2 invariants preserved — authorize default=eq_ignore_ascii_case; watermark fetch_update advance (575) BEFORE handle_message (601); ts<=watermark early-return (550); receipt (556) only after authorize+ts gates; non-text drop (570) no-advance; empty-body drop (585). InboundMessage borrows owned trimmed body. 3 impls migrated (ClaudeP, Ops, Echo). register_invite_autojoin now generic<H>, authorize-guard before room.join, call site lib.rs passes handler.clone(). relay never unwinds (warn on Err).
- [x] Phase 5 — aqua-security inert scaffolding. Commit 9009ea8. build 0 warn, test 47/0. 7 marker groups: authorize SEAM doc + user-proxy-key note (Phase 4), compose_ref (Phase 3); newly added: validate-on-load+hash-pin @template agent_type_schema, signed-writer seam @orchestrator start_log_capture + @gating PendingMap::ask grant, brokered-key @orchestrator resolve_auth_secret, per-run-socket-token @gating AskBridge::setup, seal/audit @agent recovery.rs write_key_0600 + @agent lib.rs wipe_crypto_store. aqua-sdk grep EMPTY ✓. No deps, no logic.
- [x] Phase 6 — check-dep-direction.sh + docs. Commit 573ed95. build 0 warn, test 47/0, check-dep-direction.sh exit0/OK (all 5 connector crates ✓; negative-tested: flags injected backend dep, ignores description fields; path-independent). docs/ARCHITECTURE.md + CLAUDE.md updated to 8 crates + Repo boundary section. Memory update deferred to me (orchestrator owns it; lives outside repos).
  >>> IN-WORKSPACE SEPARATION (Phases 0-6) COMPLETE <<<
- [x] §7 ADVERSARIAL AUDIT (Workflow wf_eaa348f2-af8, 7 falsification agents). ALL CONFIRMED_PASS. gate: forced cargo clean+full recompile → 0 warn, 47/0, dep-guard exit0. depdir: cargo metadata → orchestrator ZERO aqua-matrix deps, no connector reaches backend (direct or transitive). containerspec: ResolvedInstance struct byte-identical across branches → config.json shape preserved; round-trip test real. msghandler: all §2 invariants verified vs baseline. nosec: SecretSource=injection plumbing not policy; no aqua-sdk; aqua-rs-sdk strings are ref_mount paths. scope: renames are intra-crates/ extractions, backends not renamed. secrets: none added, .gitignore intact. (criterion 3 e2e = deferred to checker-agent test.)
- [ ] PHYSICAL SPLIT into ~/aqua-agents (path-deps; do NOT rename binaries; do NOT move *.pem/store dirs)
- [ ] COMMIT + PUSH both repos
- [ ] E2E with my own checker agent (mission step 3)
- [ ] Final report to user

## PHYSICAL SPLIT PLAN (user-approved follow-up) — recon done 2026-06-02
Target topology (handover Q3): connector repo = aqua-matrix-agent (5 crates); aqua-agents = 3 agents crates with PATH-DEPS back into ../aqua-matrix-agent/crates.
- MOVE to ~/aqua-agents: crates/aqua-matrix-{template,heartbeat,claude-p}; types/*.json; instances/*.example; Dockerfile; scripts/build-image.sh; agent systemd units (heartbeat, claude-channel); agent Skills (heartbeat, claude-channel, claude-bridge). KEEP in connector: matrix-message + e2e-test skills (transport-side).
- aqua-agents needs its OWN [workspace] (members: template,heartbeat,claude-p) + [workspace.dependencies] mirroring connector externals (matrix-sdk 0.17 markdown, tokio, anyhow, serde, serde_json, clap, toml, tracing, async-trait, schemars, bollard, futures-util, uuid) PLUS path-deps: aqua-matrix-relay/-orchestrator/-gating/-ask-mcp/-agent = { path="../aqua-matrix-agent/crates/<c>" }, and aqua-matrix-template = { path="crates/aqua-matrix-template" } (self). Crates keep `.workspace=true` (now resolve vs aqua-agents workspace).
- Edges to satisfy: template→orchestrator; heartbeat→relay,template,orchestrator; claude-p→relay,template,gating(→ask-mcp). VERSIONS must match connector to unify.
- Connector repo: drop the 3 crates from members + [workspace.dependencies]; must still build 5 crates + dep-check OK.
- GATE BOTH repos: cargo build 0-warn + cargo test green in aqua-matrix-agent AND aqua-agents.
- Image `localhost/aqua-matrix-agent:poc` EXISTS (reuse for e2e; do NOT need rebuild). Cross-repo Docker build is the known-gnarly bit (Appendix C) — update build-image.sh/Dockerfile for new layout + DOCUMENT; do not block on it.
- DO NOT: rename backend binaries; touch INSTALLED systemd units (outside repos); disrupt the 2 live running services; regenerate/move *.pem or store dirs. Document the deploy-migration for the user instead.
- COMMIT + PUSH both repos (user-approved).

## E2E PLAN (my own checker agent — mission step 3)
Create a throwaway checker identity (checker.pem) via the aqua-matrix-agent CLI. Then exercise the refactored backends against it over REAL Matrix:
- heartbeat (OpsHandler): run binary w/ AGENT_TARGET=checker MXID + own key → verify status DM arrives (checker --read) and `#shell ping`/`status` from checker gets a reply (tests dispatch/authorize/handle_message end-to-end). No claude dep.
- claude-p &/or orchestrator-spawned container: spawn an instance via image:poc targeting checker; verify it connects + replies. Needs claude token in store; degrade gracefully + document if absent.
Use ONLY throwaway identities; never the user's live agent.pem/heartbeat.pem/claude-channel.pem.

## Notes / decisions taken during execution
- Phase 0: fixed pre-existing AgentType test (system_prompt) — necessary for a functional gate.
- IDE rust-analyzer diagnostics lagged after git mv / mid-edit in Phases 2/3/4 — ALL were stale; every phase independently re-verified GREEN on disk + fresh cargo build/test.
- Test count 45(base)→46(P1 drift-guard)→47(P3 round-trip). 0 warnings throughout.
