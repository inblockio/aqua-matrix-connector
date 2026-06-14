# Audit — streaming-edit 413 rollover fix (2026-06-14)

## Layer 1: Hypothesis Trace

| ID | Hypothesis | Status | Evidence |
|----|-----------|--------|----------|
| H1 | rolling before a safe budget keeps every encrypted edit < 65,536 B | Confirmed | live E2E: 32,638 B streamed → 10 messages, **zero 413** (`streaming_rollover_delivers_large_reply_without_413` passes) |
| H2 | split at last paragraph/sentence/word boundary → never mid-sentence | Confirmed | `split_*` (9 unit tests) pass; E2E: all 60 paragraph sentinels arrived intact inside single messages (no mid-token split) |
| H3 | push() swallows edit failures → never aborts the run | Confirmed | code: `edit_best_effort` + `let _ = s.push(...)`; build clean; E2E push/finish returned Ok over 32 KB |
| H4 | a Matrix send error no longer returns Err from the streamed run | Confirmed | code: all delivery paths log-not-propagate; self-heal gated; only empty-claude-failure bails |
| H5 | finish() rollover-aware, no duplication | Confirmed | `messages<=1 ? final_text : buf` split; E2E: exactly 60 sentinels, no duplicate/missing, across 10 messages |
| H6 | builds, unit tests pass, dep-direction holds | Confirmed | connector 31 tests + claude-p 6 tests pass; `check-dep-direction.sh` OK; both `cargo build` clean (no warnings) |
| H7 | rebuilt image rolled fleet-wide, DIDs/memory intact | _pending deploy_ | _TBD_ |

## Layer 2: Acceptance Criteria

| # | Criterion | Met? | Evidence |
|---|----------|------|----------|
| AC1 | A >6 KB streamed reply no longer 413s | Met | E2E 32 KB stream, zero 413, finish Ok |
| AC2 | Long replies continue in new messages, not mid-sentence | Met | E2E: 10 messages, all 60 paragraph sentinels intact |
| AC3 | A delivery failure can't crash the run or wipe the session | Confirmed | self-heal gated to genuine claude failure; delivery errors logged |
| AC4 | Fleet runs the fix; DIDs/memory preserved | _TBD_ | post-roll `podman logs` DIDs |

## Crash investigation answer (for the user)

YES — the 413 crashed the turn and wiped memory. Confirmed from `aqua-agent-aqua-consultant-1`
logs (2026-06-14T15:49:55Z): the 413 on a streaming edit returned Err → `kill_on_drop` killed
claude mid-turn → the spawn task misread it as a stale resume session and ran
`sessions.clear()` → the conversation context was discarded → the next DM started cold
("did not remember anymore"). The cold retry then raced token rotation (401) and 413'd again.
