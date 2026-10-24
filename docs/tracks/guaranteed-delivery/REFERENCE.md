# Reference: guaranteed message delivery for our Matrix agents

Status: CONFIRMED by the owner as D1 to D3, 2026-10-24. Workflow: minimal loop (development-conventions
origin/main 022cca1), tier H (persistent state correctness, delivery state machine).

```
Software is for: every person who talks to one of our agents (Scribe, the Aqua Consultants incl. a
  partner's, Paige) over Element.
First feature for that user: "every message I send gets an answer, or an honest 'I could not answer,
  I have passed it to Tim'; never silence, never a duplicate."
Stated goal checked against it: agrees.
```

## The contract

Every accepted peer message opens an obligation; only a delivered answer or a delivered fallback closes
it. One concept, the obligation journal, in the shared client (`aqua-matrix-agent`), so every agent
inherits it:

1. Open: a peer message (text, voice, file; not edits or reactions) is journalled as an obligation in
   the existing durable inbox journal (`durable.rs`) before anything else runs. No second store.
2. Idempotent send: every outbound post carries a deterministic Matrix transaction id derived from the
   obligation (event id, post ordinal) or the outbox item; a retried send is the same event.
3. Close on proof: an obligation closes only when the agent's own post appears in its sync timeline.
4. Watchdog (every 30 s): a running turn past the answer deadline posts one "still working on it"
   (liveness, does not close); with no turn running past the deadline, re-engage: re-run the turn with
   the original message, as a visible reply quoting it; after 2 failed attempts post a fixed fallback
   ("Sorry, I could not answer this one, I have passed it to Tim") and DM the operator. The fallback is
   itself idempotent and closed on proof.
5. Restart-safe: open obligations are re-engaged on start.
6. Measured: each agent exposes open-obligation count and oldest age; a probe alerts beyond 15 min.

Rule (proposal, R1 to R8 run in the design review; global record by PR to development-conventions):
"Every agent SHALL answer every accepted peer message with a delivered answer or a delivered fallback
within 10 minutes." Enforced by the journal, the watchdog and the probe (R8: mechanical).

## Trial script (automated: the qualification runs exactly this)

```
T1  the claude process is killed mid-turn      -> the answer arrives after a re-engage, exactly once
                                                   falsifier: no answer, or two answers
T2  the homeserver answers a send with 502      -> the peer sees exactly one message
                                                   falsifier: zero or two copies
T3  the agent restarts mid-turn                 -> the message is answered after the restart
                                                   falsifier: the obligation is lost
T4  the model is unavailable for every attempt  -> the fallback and an operator DM within 10 min
                                                   falsifier: silence past the deadline
T5  a Paige-style broker is down at forward     -> no silent drop (answer after recovery, or fallback)
                                                   falsifier: the message vanishes
T6  normal traffic, no faults                   -> no "still working" noise, no duplicate, every
                                                   obligation closed, open count 0
                                                   falsifier: any extra post or an open obligation
```

## Not-goals

```
N1  No deploy of its own: it lands on main and ships with the next regular fleet roll (D2).
N2  No "seen" guarantee: delivered means in the room timeline, not read or decrypted on the peer device.
N3  No change to the single-peer firewall or to who may talk to an agent.
N4  Paige's broker outbox moves onto the journal in the Paige track (after rebasing its branch), not here.
N5  Scribe's call-room broadcasts are not obligations (they answer no peer message); Scribe's replies to
    peers who message it are.
```

## Hypotheses

```
H0  (kill-switch) Scribe and every consultant send and receive through the same aqua-matrix-agent client
    path; falsified when one agent bypasses it (then the fix is not one place).
H1  matrix-sdk lets the agent set the transaction id, and a client rotation keeps it meaningful;
    falsified when rotation creates a new device or token scope (then the journal's posted event id is
    the first duplicate check and the txn id the second).
H2  the agent's own posts reliably come back in its sync (E2EE rooms included); falsified when an echo
    can be missing, which would leave obligations open (then close on the send response + a fetch).
H3  the faults of T1 to T5 can be injected in automated tests (mock homeserver or the repo's e2e rig);
    falsified when a fault class cannot be reproduced without a live server.
```

## Decision rows

```
D1  (Tim, 2026-10-24) Design and rule as reflected back: "start a separate track which implements the
    message behavior upgrade".
D2  (Tim, 2026-10-24) "merge it after successful testing so it will land with future deploys (make it
    not its own deploy)": merge to main on green qualification and a clear audit; no fleet roll.
D3  (Tim, 2026-10-24) "ensure you're doing the automated tests required to qualify it": the trial
    script above runs as automated tests, plus each repo's CI and its integration harness where one
    exists.
```

## Spike S0 (2026-10-24, connector 4ee7c96, aqua-agents bee500f)

H0 FALSIFIED: the consultants share `run_daemon` + relay; Scribe (`aqua_transcript_agent.rs`, crate
aqua-call-agent) runs its own `sync_once` poll loop with no handler and no journal, and many `.ok()` sends.
Design adjustment (loop decision, within D1): the obligation journal, txn ids and own-event closing live
in `aqua-matrix-agent` (`WorkItem`/`WorkJournal`, `AgentClient`), callable from `run_daemon` and from
Scribe's loop. H1 partly confirmed: matrix-sdk 0.17 `Room::send(..).with_transaction_id(..)` exists and
is unused; `retry_finalize` (lib.rs:2574) mints a fresh id per retry, so a lost response duplicates today;
rotation keeps the device except on the store-wipe path. H2 confirmed with gaps (backfill cap of 4 pages,
undecryptable pre-wipe events): close on the echo OR on the send response confirmed by an event fetch.
H3 partly confirmed: the raw-TCP fake homeserver (lib.rs:4133) and a fake `claude` make T1 to T5 hermetic.
Silent-drop paths on main to close: watermark advanced before enqueue (relay lib.rs:1672); ts <= watermark
drops out-of-order events (:1636); unknown msgtypes vanish (:1660); replay once per process (:677);
`reply_text == None` marked delivered (claude-p main.rs:787); IDLE_TIMEOUT error text; Scribe `.ok()`.
Qualification: no connector CI; aqua-agents CI runs only aqua-call-agent and aqua-agent-refs: add claude-p
and template to ci.yml; quote clean-checkout runs in each PR.

## Plan (tier H, budget 60M, checkpoint per issue)

| ID | What | Repo | Depends |
|---|---|---|---|
| GD-1 | Obligation journal, deterministic txn ids on every reliable send (fixes retry_finalize), close on echo or confirmed send, watchdog sweep API with injected clock, the relay silent-drop fixes, fake homeserver 502 and lost-response modes; tests T2, T3, T6 | connector | S0 |
| GD-2 | claude-p on the obligation API: re-engage re-runs the turn, IDLE_TIMEOUT -> liveness + re-engage, no-reply -> re-engage/fallback, fallback + operator DM, status file (open count, oldest age); fake claude for T1, T4; CI job adds claude-p + template | aqua-agents | GD-1 |
| GD-3 | Scribe: peer-message replies through the journal and reliable sends (no `.ok()` on peer replies) | aqua-agents | GD-1 |
| GD-A | Fresh Opus audit + grok on GD-1..3, fix wave, re-audit of the fixed findings | both | GD-2, GD-3 |
| GD-M | PRs with clean-checkout evidence; merge connector first, then aqua-agents; no fleet roll (D2) | both | GD-A |
