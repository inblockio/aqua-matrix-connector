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
- [ ] §7 ADVERSARIAL AUDIT (Workflow, falsify each of the 10 criteria)
- [ ] PHYSICAL SPLIT into ~/aqua-agents (path-deps; do NOT rename binaries; do NOT move *.pem/store dirs)
- [ ] COMMIT + PUSH both repos
- [ ] E2E with my own checker agent (mission step 3)
- [ ] Final report to user

## Notes / decisions taken during execution
(append as we go)
