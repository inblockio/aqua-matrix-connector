# Plan: Agent avatar support (set + deploy)

**Date:** 2026-06-15
**Goal:** Let any Matrix agent (esp. Aqua consultants) carry a profile **avatar**, set on connect the same way the display name is, and wire it through the consultant deploy flow so each consultant can be given its own image.

## CONTEXT

The connector can set an agent's Matrix **display name** but not its **avatar**. Display name flows: `AgentType.display_name` (config JSON) → `MessageHandler::display_name()` → relay `first_cycle` → `AgentClient::set_display_name()` → matrix-sdk `Account::set_display_name`. We mirror that exact path for avatars. matrix-sdk 0.17 exposes `Account::upload_avatar(&Mime, Vec<u8>) -> OwnedMxcUri` (uploads + sets URL in one call), so no new crate dependency is required.

Two repos, one-way dep rule (agents → connector, never reverse):
- **connector** (`aqua-matrix-connector`): generic capability — `set_avatar` + trait method + relay lifecycle + CLI affordance.
- **agents** (`aqua-agents`): the value — `AgentType.avatar_path` + claude-p handler override + config template.
- **deploy glue** (`spawn-consultant.sh`, in the connector repo): `--avatar` flag → read-only mount + rendered `avatar_path`.

Avatar **generation** is out-of-band and operator-supplied (a PNG/JPG path passed to `--avatar`); the deploy tooling never generates images itself (keeps it dependency-free and personal-cred-free).

## GOAL

Given an avatar image file, an agent sets it as its Matrix profile picture on connect, idempotently (no re-upload on reconnect), best-effort (never blocks startup), backward-compatible (configs without an avatar behave exactly as today), with the spawn flow able to attach one per consultant.

## INPUTS (verified by code map)

- `AgentClient::set_display_name` — `crates/aqua-matrix-agent/src/lib.rs:1387` (mirror target). Reaches SDK via `self.client.account()`. `AgentClient` holds `config: AgentConfig` (has `store_dir`).
- matrix-sdk `0.17.0`; `Account::upload_avatar(&Mime, Vec<u8>)`, `get_avatar_url()`.
- `MessageHandler` trait with `display_name()` + relay `first_cycle` apply at `crates/aqua-matrix-relay/src/lib.rs:~475-495`.
- `AgentType` struct — `aqua-agents/crates/aqua-matrix-template/src/lib.rs:43-87` (`#[serde(deny_unknown_fields)]`; already has `display_name`).
- `ClaudePHandler::display_name()` — `aqua-agents/crates/aqua-matrix-claude-p/src/main.rs:285-290`.
- `spawn-consultant.sh` — arg loop `:71-89`, python render `:287-299`, mounts `:365-368`.

## ACTIVITIES → OUTPUTS (tasks T1–T8; see task list)

T1 `set_avatar` · T2 relay trait+lifecycle · T3 `AgentType.avatar_path` · T4 claude-p override + template JSON · T5 spawn `--avatar` (ro mount + render) · T6 CLI `--avatar` (test affordance) · T7 personal generate+spawn wrapper · T8 build + unit tests.

## BOUNDARY CONDITIONS (must-not)

- Must **not** break existing display-name flow or existing configs (missing `avatar_path` ⇒ no-op).
- Must **not** put generation/personal creds into team repos.
- Must **not** violate the one-way dep rule (guard: `scripts/check-dep-direction.sh`).
- Must **not** weaken spawn-consultant security posture: avatar mount is **`:ro`**, caps/token-by-ref flag diff stays byte-identical.
- Must **not** re-upload the avatar on every reconnect (idempotency via content-hash sentinel in `store_dir` + `get_avatar_url` check).
- Must **not** block agent startup if the avatar set fails (best-effort, mirror display-name).
- Resource: **one cargo build at a time, no parallel Rust agents** (9.7 GB OOM history). EXECUTE runs sequentially/inline.

## Hypothesis Register

| ID | If | Then | Assumptions | Verification |
|----|-----|------|-------------|--------------|
| H1 | `set_avatar(path)` reads file, picks mime (png/jpeg), calls `account.upload_avatar` | agent's Matrix avatar is set | matrix-sdk 0.17 API as mapped | `cargo build` (connector); live set in deploy phase |
| H2 | set_avatar writes a content-hash sentinel in `store_dir` and skips when unchanged & `get_avatar_url().is_some()` | reconnects don't re-upload (no orphan media) | store_dir writable (`:U` mount) | unit test of pure decision fn; `cargo build` |
| H3 | `MessageHandler::avatar_path()` defaults to None and relay applies it only when Some | existing backends compile unchanged; opt-in per backend | trait default methods | `cargo build` (relay + all backends) |
| H4 | `AgentType.avatar_path: Option<String>` with serde default | configs without the field still deserialize | `deny_unknown_fields` ignores *missing* optionals | unit test: deser template JSON with & without field |
| H5 | `ClaudePHandler::avatar_path()` returns `agent_type.avatar_path` | a consultant config's avatar flows to set_avatar at connect | flatten/override preserved | `cargo build` (agents); unit test handler value |
| H6 | spawn `--avatar` mounts file ro at `/agent/avatar.png` and render sets `avatar_path` to it | container reads avatar; config points to it | python render path correct | run render on sample (assert field); grep mount; shellcheck |
| H7 | avatar file mounted `:ro` | `upload_avatar` (read-only op) works under cap-drop/ro posture | container user can read ro mount | reasoning now; live check in deploy |
| H8 | personal wrapper generates from identity and passes `--avatar` | each consultant gets a unique avatar, no grok in team repo | DID readable post-spawn (as `--onboard`) | wrapper dry-run + render check |
| H9 | image rebuilt + fleet rolled with avatars (DEPLOY, gated) | live profiles show avatars | prod host access; outward-facing | deploy: `podman logs … avatar set`; Element visual |

## Acceptance Criteria

| # | Criterion | Hyp |
|---|-----------|-----|
| AC1 | Connector compiles with set_avatar + relay wiring | H1,H2,H3 |
| AC2 | Agents compile with avatar_path + handler | H4,H5 |
| AC3 | Unit tests pass: deser (±field), mime, idempotency decision | H2,H4,H5 |
| AC4 | spawn `--avatar` renders avatar_path + `:ro` mount; render test passes; dep-direction + security flag diff green | H6,H7 |
| AC5 | Personal generate+spawn path works (dry-run) | H8 |
| AC6 | (DEPLOY, gated) a live consultant shows the avatar in Element; logs show avatar set | H9 |

## Scope boundary

This pipeline's EXECUTE delivers the **code + local verification** (AC1–AC5). **AC6 (live prod deploy)** is outward-facing on the remote fleet host and is a separately-confirmed step, not part of local EXECUTE.
