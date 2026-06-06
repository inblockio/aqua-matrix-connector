---
name: e2e-test
description: Run and verify the E2EE integration test between two aqua-matrix-agent identities
---

# E2E Encryption Integration Test

Runs a bidirectional end-to-end-encrypted Matrix exchange between two distinct agent identities (Agent A and Agent B) against the live `matrix.inblock.io` homeserver. Used to verify that CAIP-122 auth, OIDC auto-registration, device key upload, and Megolm key exchange all work together.

The test lives at `tests/e2e.rs` and is gated behind the `e2e` Cargo feature so it doesn't run in `cargo test`.

## Run it

```bash
cd ~/aqua-matrix-agent && cargo test --test e2e --features e2e -- --nocapture
```

`--nocapture` is recommended — the test prints the user IDs, DIDs, and message tags it exchanged. Without it you only see pass/fail.

Each identity keeps ONE persistent crypto store at `.e2e-store/<key-stem>` (e.g. `.e2e-store/agent`, `.e2e-store/agent-b`), reused across runs and never wiped (the dir is gitignored). The device_id is deterministic per DID, so the old "wipe on every run" behaviour re-uploaded fresh identity keys under the same device_id, recycling the device and breaking Megolm key sharing on repeat runs (`[unable to decrypt]`). Persisting the store keeps each device's keys stable, exactly like a real client.

All e2e tests share the two identities (and therefore their stores), so they are serialized internally by a lock. Run with `--test-threads=1` for clean, non-interleaved output.

### Poisoned accounts (fresh-identity override)

If an account's server-side device keys are already poisoned (you'll see `One time key ... already exists`, or persistent `[unable to decrypt]` even with a clean store), the deterministic device_id cannot be reset in place. Switch to a fresh identity via env override:

```bash
openssl genpkey -algorithm Ed25519 -out /tmp/fresh-a.pem
openssl genpkey -algorithm Ed25519 -out /tmp/fresh-b.pem
SIWX_E2E_KEY_A=/tmp/fresh-a.pem SIWX_E2E_KEY_B=/tmp/fresh-b.pem \
  cargo test --test e2e --features e2e e2ee_bidirectional_messaging -- --nocapture --test-threads=1
```

`SIWX_E2E_KEY_A` / `SIWX_E2E_KEY_B` default to the committed `agent.pem` / `agent-b.pem`.

## What it asserts

1. Both agents authenticate via CAIP-122 against `siwx-oidc.inblock.io` and end up with distinct Matrix user IDs.
2. Agent A creates a DM room and invites Agent B.
3. Agent B joins, then sends a tagged message to Agent A.
4. Agent A syncs, finds the message, and asserts the body is **not** `[unable to decrypt]`.
5. Agent A replies; Agent B syncs and asserts decryptability in the reverse direction.

The unique tag per run (UUID) means a passing test proves the message round-tripped in this run, not from a previous one.

## Key ordering rule (do not break this)

Agent B must send **first**, not Agent A. The reason is buried in Megolm session creation:

- Agent A creates the room and the *first* outbound Megolm session before Agent B exists in the room.
- If A then sends, A's session key has no recipient for B and the message is undecryptable on B's side.
- When B joins and sends, B creates *its* outbound session with A already present, so A receives a usable key.
- After B's send completes a successful round-trip, A's next send rotates/shares correctly and B can decrypt.

If this ordering is changed, expect `[unable to decrypt]` on one side.

## Required state

- `~/aqua-matrix-agent/agent.pem` (Agent A key) — pre-existing in the repo
- `~/aqua-matrix-agent/agent-b.pem` (Agent B key) — pre-existing in the repo
- Network access to `https://siwx-oidc.inblock.io` and `https://matrix.inblock.io`
- The corresponding Matrix accounts already provisioned (they were on first run)

Don't delete the `.pem` files — the DIDs and Matrix accounts are derived from those keys. Losing them strands the accounts.

## Failure modes

| Symptom | Likely cause |
|---|---|
| `[unable to decrypt]` on either side | Poisoned server-side device keys (switch to a fresh identity, see above), or key-ordering rule above was violated |
| `One time key ... already exists` during cross-signing bootstrap | Account's device keys are poisoned from earlier device recycling — use a fresh identity via `SIWX_E2E_KEY_A/B` |
| `Agent A failed to connect` | `siwx-oidc.inblock.io` down, or network issue |
| `no DM room found between agents` | Invite was sent but sync didn't pick it up — usually a flaky sync, re-run once |
| `agents must have different identities` | Both agents loaded the same key file — check `agent.pem` vs `agent-b.pem` paths |

## Logging

The test installs `tracing_subscriber` at `warn,aqua_matrix_agent=info`. Bump it by exporting `RUST_LOG` before the test, e.g. `RUST_LOG=debug,matrix_sdk_crypto=trace`.
