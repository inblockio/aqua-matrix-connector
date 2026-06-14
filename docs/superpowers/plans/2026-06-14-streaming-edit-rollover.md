# Streaming-edit 413 M_TOO_LARGE → multi-message rollover

**Date:** 2026-06-14
**Trigger:** agent `did:key:z6MkmPz99…htzivkfu` (container `aqua-agent-aqua-consultant-1`) emitted
`[claude-channel error] failed to send streaming edit: [413 / M_TOO_LARGE] event too large`,
then lost its session memory.

## Root cause (confirmed from logs)

Matrix caps a single event at **65,536 bytes**. A streamed reply is delivered as an
`m.replace` **edit** of **markdown** that is **E2E-encrypted**. The 16,000-byte body
(`STREAM_MAX_BYTES`) inflates past 64 KiB: markdown→HTML `formatted_body`, **doubled** by
the embedded `m.new_content`, then **+33 %** Megolm/base64 → ~70–130 KB event → **413**.

Cascade (container log `2026-06-14T15:49:55Z`):
1. `s.push(text).await?` (claude-p `main.rs:833`) propagates the 413 → `stream_claude` returns `Err`.
2. `child` dropped with `kill_on_drop(true)` → **claude killed mid-turn**.
3. spawn task sees `Err`+prior session → **misfires stale-session self-heal → `sessions.clear()`** (`main.rs:383-390`) → **memory wiped**.
4. cold retry races token rotation → 401, then 413 again.

## Hypothesis Register

| ID | If | Then | Assumptions | Verification |
|----|-----|------|-------------|--------------|
| H1 | a streamed reply's live buffer exceeds a safe markdown budget (~6 KB) | rolling to a new message before each edit keeps every encrypted edit event < 65,536 B | HTML×2×base64 inflation ≤ ~10× | E2E: drive a >20 KB reply; assert zero `413/M_TOO_LARGE` in container logs |
| H2 | the buffer is split at the last paragraph/sentence/word boundary ≤ budget | continuation messages never start mid-sentence | boundary exists within window, else word/hard fallback | unit test `split_for_matrix` on prose, code-fence, no-boundary inputs |
| H3 | `push()` swallows edit failures (best-effort, logs) instead of `?`-propagating | a delivery hiccup can never abort `stream_claude` or kill the claude child | only `start()`/finalize may surface errors | unit/inspection: `push` returns `Ok` on simulated edit error; code review |
| H4 | a Matrix send error no longer returns `Err` from the streamed run | the stale-session self-heal does not fire and `sessions.clear()` is not called on delivery failure | session id captured at `init` is returned in `RunOutcome` | code review + E2E: after a forced large reply, next DM resumes same session id |
| H5 | `finish()` is rollover-aware (renders committed-tail + continuation, no re-render of frozen msgs) | the final authoritative text lands across N messages without duplication | streamed buf == full output (every delta pushed) | unit test: finish after rollover produces no duplicated prefix |
| H6 | the connector + agents build, unit tests pass, dep-direction holds | the fix is shippable | toolchain present | `cargo build`, `cargo test`, `scripts/check-dep-direction.sh` |
| H7 | rebuilt image rolled across the fleet preserving stores | all agents run the fix, DIDs/memory intact | recreate scripts preserve `/agent/store` | `podman logs` show same DIDs post-roll; agents reconnect |

## Tasks

### Task 1: Connector — splitter + rollover + best-effort edits
**Hypotheses:** H1, H2, H3, H5
**Files:** `~/aqua-matrix-agent/crates/aqua-matrix-agent/src/lib.rs`
- [ ] `split_for_matrix(text, budget) -> Vec<String>` with paragraph→sentence→word→hard fallback + code-fence continuity
- [ ] `STREAM_ROLLOVER_BYTES = 6_000`; document the 64 KiB budget math
- [ ] `ReplyStream`: track `committed`; `push` rolls over (freeze head, new placeholder, continue) and is best-effort
- [ ] rollover-aware `finish` (render committed-tail across current + continuation messages)
- [ ] unit tests for `split_for_matrix`

### Task 2: Backend — continuity + non-fatal delivery
**Hypotheses:** H3, H4
**Files:** `~/aqua-agents/crates/aqua-matrix-claude-p/src/main.rs`
- [ ] don't let a finalize/delivery error return `Err` from `stream_claude` (keep `RunOutcome`)
- [ ] route the error note through the stream (`push`) instead of via `final_text` replacement
- [ ] gate the stale-session self-heal so it only fires on genuine claude/resume failure, not delivery
- [ ] `(None, None)` path sends long output as multiple messages

### Task 3: Build + gate (both repos)  — **H6**
### Task 4: E2E with deployed test agents — **H1, H2, H4**
### Task 5: Build image, deploy, roll fleet — **H7**
### Task 6: Audit + finish branches

## Boundary conditions
- One-way dep rule (connector never names an agents crate) — `check-dep-direction.sh` in the gate.
- Preserve DIDs/memory on roll. Debug builds. No changes to token rotation or call/transcript agents.
- Risk: split inside code fence → fence continuity in splitter. Risk: tiny messages → min-head floor. Risk: duplication → committed-prefix tracking.
