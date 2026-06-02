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
- [ ] Phase 2 — extract aqua-matrix-orchestrator (verbatim move)
- [ ] Phase 3 — ContainerSpec redesign (load-bearing; drop template dep from connector)
- [ ] Phase 4 — MessageHandler Moderate formalization (SENSITIVE: dispatch/authorize; inspect diff)
- [ ] Phase 5 — aqua-security inert scaffolding (comments + authorize hook only)
- [ ] Phase 6 — check-dep-direction.sh + docs + memory + final gate
- [ ] §7 ADVERSARIAL AUDIT (Workflow, falsify each of the 10 criteria)
- [ ] PHYSICAL SPLIT into ~/aqua-agents (path-deps; do NOT rename binaries; do NOT move *.pem/store dirs)
- [ ] COMMIT + PUSH both repos
- [ ] E2E with my own checker agent (mission step 3)
- [ ] Final report to user

## Notes / decisions taken during execution
(append as we go)
