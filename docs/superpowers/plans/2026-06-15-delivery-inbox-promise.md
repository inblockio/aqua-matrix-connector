# Delivery + inbox promise for claude-channel agents

**Date:** 2026-06-15
**Trigger:** Tim's 04:01 message to `aqua-agent-aqua-consultant-1` went unanswered.
claude produced a reply but delivery hit `401 M_UNKNOWN_TOKEN` (token rotation at send
time); the non-self-healing reply path retried the dead token and the prior fix
then logged-and-dropped it → no answer, no error.

## Goal (one sentence)
Every read user message ALWAYS yields a delivered answer-or-error (self-healing +
confirmed), and inbound messages are a durable inbox processed until empty —
surviving token rotation AND restart.

## Design decisions (locked)
- **Self-heal via cycle redelivery (NOT interior mutability):** the relay already
  survives token rotation by rebuilding a freshly-authed client each cycle. We reuse that:
  a reply that can't be delivered live stays durable (`ToDeliver`) and is **redelivered at
  the next cycle start with the fresh token**; the handler fires an immediate **recycle
  signal** (`Notify`) on a live-delivery failure so the redelivery happens in seconds, not
  minutes. This avoids surgery on the core `client` (a `RwLock`-swap would orphan the
  cycle's running sync — fleet-wide risk) while still healing across token rotation.
- **Seams (additive, non-breaking):** `MessageHandler::owns_completion() -> bool` (default
  false; claude-p = true so the relay doesn't auto-complete its detached task);
  `AgentClient` optional journal handle + `record_reply`/`mark_delivered`/`request_recycle`.
- **Unified durable work-journal** per target (`<store>/work-journal.jsonl`): each item
  `Pending → ToDeliver{text} → done`. `Pending` = needs claude (replay on restart);
  `ToDeliver` = claude done, redeliver cached text (no claude re-run). Inbox+outbox in one.
- **Streaming preserved** as happy-path delivery; journal `ToDeliver` redelivery is the
  fallback when live delivery isn't confirmed. Confirmed live delivery → mark done, no journal redelivery.
- **Session decoupled** from delivery (keep prior fix): delivery failure never wipes the session.

## Hypothesis Register

| ID | If | Then | Assumptions | Verification |
|----|-----|------|-------------|--------------|
| H1 | the final reply is durable (`ToDeliver`) and the relay redelivers it at the next cycle start with a fresh token (recycle-signalled on live failure) | a reply across a token rotation is delivered, never dropped | cycle reconnect mints a fresh token | unit: journal `ToDeliver` survives reload; E2E: cycle-drain delivers a `ToDeliver` item → B receives |
| H2 | every accepted inbound is appended to the durable journal before handling and removed only after confirmed delivery | a crash/restart mid-turn replays the message (no drop) | journal fsync survives kill | unit: persist+reload→Pending replayed; E2E restart |
| H3 | the final answer/error is recorded `ToDeliver{text}` and a delivery loop redelivers cached text (self-healing) until confirmed | a delivery failure is retried without re-running claude and survives restart | text fits journal | unit: state transitions, reload→ToDeliver redelivered |
| H4 | claude failure still produces an error message routed through the same guaranteed delivery | the user always gets an answer OR an error | claude exits/prints an error | E2E/unit: induced failure → error queued+delivered |
| H5 | delivery errors never return Err to the spawn-task self-heal | the claude session is preserved across delivery issues | session id captured at init | code review + E2E: session id stable |
| H6 | builds, unit tests pass, dep-direction holds | shippable | toolchain present | cargo build/test + check-dep-direction.sh |
| H7 | rebuilt image rolled fleet-wide | all agents run the fix, DIDs/memory intact | recreate preserves /agent/store | podman logs DIDs post-roll |

## Acceptance criteria
- AC1 (incident): a reply sent across token rotation is delivered, not dropped. → H1
- AC2: a message read but interrupted by restart is answered after restart. → H2
- AC3: claude failure → the user receives an error message. → H4
- AC4: answer and error delivery are confirmed (homeserver ack) or durably retried. → H1,H3
- AC5: fleet deployed; identities/memory preserved. → H7

## Tasks → hypotheses
- T9 self-healing client → H1, H5
- T10 durable work-journal → H2, H3
- T11 relay integration (enqueue/replay/deliver) → H2, H3
- T12 claude-p integration (set_reply/error/mark_done) → H3, H4, H5
- T13 build+gate → H6
- T14 E2E → H1, H2, H3
- T15 audit + deploy → H7

## Boundary conditions
- One-way dep rule (`check-dep-direction.sh`). DIDs/memory preserved on roll. Debug builds.
- At-least-once: a crash in a tiny window may re-answer one message (duplicate, never a drop) — accepted per user.
- v1 durability covers TEXT fully; media replay is best-effort (handle may expire) — logged, not silently dropped.
- No change to token-rotation cadence, call/transcript agents.
