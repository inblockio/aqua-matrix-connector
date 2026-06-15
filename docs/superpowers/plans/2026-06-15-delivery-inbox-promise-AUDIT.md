# Audit — delivery + inbox promise (2026-06-15)

## Layer 1: Hypothesis Trace

| ID | Hypothesis | Status | Evidence |
|----|-----------|--------|----------|
| H1 | journalled `ToDeliver` reply redelivered on a fresh-token cycle → delivered, never dropped | Confirmed | live `durable_journal_redelivers_reply_to_peer` passed: reply survived reload, delivered to peer over E2EE, journal drained (101s) |
| H2 | accepted inbound persisted before handling, removed only after confirmed delivery → crash replays it | Confirmed | `durable::tests::survives_reload_replaying_unfinished_work` (Pending survives reload); relay `replay_inbox` re-dispatches `pending_work()` on startup (code) |
| H3 | final answer/error recorded `ToDeliver`, redelivered (cached) until confirmed, survives restart | Confirmed | `durable::tests::lifecycle_*` + `survives_reload_*`; live E2E reload→redeliver→drain passed |
| H4 | claude failure → error delivered with the same guarantee | Confirmed (code) | claude-p spawn `Err(e)` arm: `record_reply(errtext)` → `send_dm_reliable` → `mark_delivered` else `request_recycle`; relay redelivers |
| H5 | delivery errors never reach the stale-resume self-heal → session preserved | Confirmed (code) | `stream_claude` only `bail!`s on (None,Some,empty); delivery paths set `delivered`/`reply_text`, never Err; self-heal fires only on genuine claude failure |
| H6 | builds, unit tests pass, dep-direction holds | Confirmed | connector 35 + relay suites pass; agents 6 pass; `check-dep-direction.sh` OK; both `cargo build` clean (no warnings) |
| H7 | rebuilt image rolled fleet-wide, DIDs/memory intact | Confirmed | image `2d823e73a4fa`; 7 containers + host daemon Up; affected `aqua-agent-aqua-consultant-1` DID `z6MkmPz99…` identical post-roll; tim-channel + host `z6MkozJj…` preserved (store_wiped=false) |

## Layer 2: Acceptance Criteria

| # | Criterion | Met? | Evidence |
|---|----------|------|----------|
| AC1 (incident) | a reply sent across token rotation is delivered, not dropped | Met | live durable-redelivery E2E + relay cycle `drain_deliveries` (fresh token) |
| AC2 | a message read but interrupted by restart is answered after restart | Confirmed (unit+code) | journal `Pending` survives reload; `replay_inbox` re-dispatches on startup |
| AC3 | claude failure → user receives an error message | Confirmed (code) | guaranteed-error path in claude-p spawn task |
| AC4 | answer + error delivery confirmed or durably retried | Met | live E2E confirmed delivery + drain; unconfirmed → journal `ToDeliver` → cycle redelivery |
| AC5 | fleet deployed; identities/memory preserved | Met | 7 containers + host daemon on image `2d823e73a4fa`; all DIDs preserved |

## Verdict
All 7 hypotheses Confirmed; all 5 acceptance criteria Met. Shipped (connector `ee7b6e4`,
agents `a790460`) and deployed fleet-wide on image `2d823e73a4fa`, identities/memory preserved.

## Design notes
- Self-heal = redelivery on a fresh-token cycle + `request_recycle` (immediate), NOT an
  interior-mutability client swap (which would orphan the cycle's running sync — rejected as
  fleet-wide risk).
- `owns_completion()` gates the relay's auto-complete so claude-p's detached task owns its item.
- At-least-once: a crash in the produce→mark window may re-answer one message (duplicate, never a drop).
- v1: text fully durable; media replay = caption-only (handle not re-fetchable) — logged.

## Incident answer (for the user)
Tim's 04:01 message WAS read and answered by claude, but the reply's live send hit
`401 M_UNKNOWN_TOKEN` (token rotation at send time); the non-self-healing path retried the
dead token and the prior fix logged-and-dropped it. Now: the reply is journalled `ToDeliver`
and redelivered on the next cycle's fresh token (and an immediate `request_recycle` makes that
happen in seconds), so it can never be silently dropped again.
