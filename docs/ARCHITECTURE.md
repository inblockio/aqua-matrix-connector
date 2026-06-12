# Architecture

This document is the single source of truth for how the agent infrastructure on this host is wired. Skills cover *operations*; this doc covers *design*. When something is broken and the obvious answer is not in a skill, start here.

## Components

There are three components running on the host. Each does one thing, has its own failure domain, and can be restarted independently.

```
┌────────────────────────────────────────────────────────────────────────┐
│ Tim's Element (matrix.inblock.io)                                      │
└────────┬───────────────────────────────────────────┬───────────────────┘
         │ DM with heartbeat-identity                │ DM with claude-channel-identity
         │ (ops + #shell commands)                   │ (free-form prose → LLM)
         ▼                                           ▼
┌────────────────────────────────┐       ┌────────────────────────────────┐
│ aqua-matrix-heartbeat          │       │ aqua-matrix-claude-channel     │
│   ident: heartbeat.pem         │       │   ident: claude-channel.pem    │
│   store: ~/.aqua-matrix-       │       │   store: ~/.aqua-matrix-       │
│          heartbeat/            │       │          claude-channel/       │
│                                │       │                                │
│   sends: 10-min status DM      │       │   on incoming DM from target:  │
│   reads (event-driven sync):   │       │     spawn `claude -p <body>`   │
│     #shell <cmd>               │       │     stream stdout back as DM   │
│       → dispatcher             │       │                                │
│       → systemd unit op        │       │   stateless: each msg is a     │
│                                │       │   fresh claude invocation      │
│   #shell respawn ──────────────┼───────┤── targets claude-bridge OR     │
│                                │       │   aqua-matrix-claude-channel   │
└────────────────────────────────┘       └────────────────────────────────┘
                                                       │
                                                       │ (#shell respawn target)
                                                       ▼
                                         ┌────────────────────────────────┐
                                         │ claude-bridge.service          │
                                         │   tmux session `claude-bridge` │
                                         │   running:                     │
                                         │     claude --dangerously-      │
                                         │            skip-permissions    │
                                         │                                │
                                         │   for LOCAL interactive use:   │
                                         │     tmux attach -t claude-     │
                                         │            bridge              │
                                         │                                │
                                         │   NOT connected to Matrix.     │
                                         └────────────────────────────────┘
```

### Component roles

| Component | Purpose | Identity | Failure mode |
|---|---|---|---|
| `aqua-matrix-heartbeat` | Ops channel. Periodic status DMs + `#shell` commands. | `heartbeat.pem` | Heartbeat stops; commands stop. Claude side unaffected. |
| `aqua-matrix-claude-channel` | LLM channel. Free-form prose in, Claude output out. | `claude-channel.pem` | Conversational replies stop. Ops channel unaffected. |
| `claude-bridge.service` | Persistent interactive Claude in tmux, for local human attachment via `tmux attach`. | none (host-local process) | Local interactive Claude unavailable. Both Matrix daemons unaffected. |

## Workspace layout

The repo is a Cargo workspace (virtual root manifest) and a **reference implementation** for any agent backend over Matrix + siwx-oidc — the `claude -p` daemon is just a placeholder backend. After the **physical repo split**, this repo is the **connector substrate only: five crates** under `crates/`. The three backend crates (`aqua-matrix-template`, `aqua-matrix-heartbeat`, `aqua-matrix-claude-p`) plus the agent content (`types/*.json`, `instances/*.toml.example`, `Dockerfile`, `scripts/build-image.sh`) now live in the sibling **`../aqua-agents`** repo, which path-deps back into this connector (the same cross-repo path-dep pattern `siwx-oidc-auth` uses). See "Repo boundary" below.

**Connector half** (this repo — the reusable substrate; these crates know nothing about any specific agent):

| Crate | Role |
|---|---|
| `aqua-matrix-agent` | The library (`AgentClient`, `AgentConfig`, OIDC, recovery, registry) plus the one-shot `aqua-matrix-agent` CLI binary (`--message` / `--read` / `--print-did`). |
| `aqua-matrix-relay` | Generic daemon framework: the `MessageHandler` trait + `InboundMessage` + `authorize()` seam + `run_daemon()` lifecycle (connect-rotate-sync-watermark). Ships `examples/echo_agent.rs`. |
| `aqua-matrix-ask-mcp` | Tiny stdio MCP server exposing one tool, `ask_human(question) -> answer`, bridged over a per-run unix socket to the daemon. The library half (`ipc`, `jsonrpc`) is reused by the gating crate. |
| `aqua-matrix-orchestrator` | Transport/agent-agnostic Podman engine: `ContainerManager` (spawn/replace/stop/tail) driven by the `ContainerSpec` seam. **Never sees `AgentType`.** |
| `aqua-matrix-gating` | Confirmation/gating substrate: `PendingMap` (`ask_user` router), the `destructive` matcher, and `AskBridge` (per-run `ask_human` socket). Keyed on `AgentClient`; mentions no specific LLM. |

**Agents half** (the backends — depend on the connector, never the reverse; **now live in the sibling `../aqua-agents` repo**, path-dep'd back here):

| Crate | Role |
|---|---|
| `aqua-matrix-template` | The agent capability schema: `AgentType` / `InstanceBinding` / `ResolvedInstance`, plus the `build_spawn_spec(agent_type, target, id) -> ContainerSpec` factory that produces a connector `ContainerSpec` (the only place that maps a rich `AgentType` down to the agent-agnostic spawn input). |
| `aqua-matrix-heartbeat` | Binary `aqua-matrix-heartbeat` — the ops/heartbeat agent (periodic status DM + `#shell` commands). Deps: relay, template, orchestrator. |
| `aqua-matrix-claude-p` | Binary `aqua-matrix-claude-p` — the reference Claude backend that forwards DMs to `claude -p`. Deps: relay, template, gating. |

These three crates, plus the agent content (`types/*.json`, `instances/*.toml.example`, `Dockerfile`, `scripts/build-image.sh`), have been **moved out of this repo** into `../aqua-agents`; their `[dependencies]` resolve the connector crates by path back into this repo. To build them, run `cargo build` from `../aqua-agents` (this connector must be checked out alongside as a sibling). A `cargo build` in *this* repo builds only the five connector crates.

## Repo boundary

The two halves now live in **two repos** with a **strictly one-way dependency rule**, so the connector substrate can evolve independently of any agent that rides on it. This is the invariant the whole split exists to create; see [`docs/plans/repo-split-execution-handover.md`](plans/repo-split-execution-handover.md) (§1 locked decisions, §4 target architecture) for the full rationale.

- **Connector half (this repo, `aqua-matrix-agent`):** `aqua-matrix-agent`, `aqua-matrix-relay`, `aqua-matrix-ask-mcp`, `aqua-matrix-orchestrator`, `aqua-matrix-gating`. A reusable "run any agent over Matrix + Podman" substrate.
- **Agents half (sibling repo, `../aqua-agents`):** `aqua-matrix-template`, `aqua-matrix-heartbeat`, `aqua-matrix-claude-p`. The concrete backends + the capability schema + the agent content. Its `[workspace.dependencies]` pull the connector crates in by path (`../aqua-matrix-agent/crates/...`), the same cross-repo path-dep pattern `siwx-oidc-auth` uses.

**The one-way rule:** dependencies flow **agents → connector, NEVER the reverse.** No connector crate may name a backend crate (`aqua-matrix-template`, `aqua-matrix-heartbeat`, `aqua-matrix-claude-p`) in its `Cargo.toml`. With the split now physical, an inverted edge is a cross-repo dependency-cycle problem; the in-place refactor that preceded the split proved the boundary holds as a compile-time check first.

> **Deployment glue still here pending a migration pass:** `Skills/` and `systemd/` remain in this connector repo for now (the agent systemd units' `ExecStart` still point at `%h/aqua-matrix-agent/target/debug/...`, and the `Dockerfile`/`build-image.sh` in `../aqua-agents` need a cross-repo build context with both repos present — flagged as a known follow-up in the aqua-agents README). A later deployment-migration pass reconciles the unit paths, image build context, and skill homes. The existing `localhost/aqua-matrix-agent:poc` image remains usable meanwhile.

Two seams keep the boundary clean:

- **The `ContainerSpec` seam (orchestrator).** The connector's `ContainerManager::spawn` consumes a minimal, agent-agnostic `ContainerSpec`, so the connector **never sees `AgentType`**. The mapping from a rich `AgentType` down to a `ContainerSpec` lives entirely agents-side, in `aqua_matrix_template::build_spawn_spec(agent_type, target, id)` (it resolves ref-mount host paths absolute and serializes the `ResolvedInstance` into `config_blob` mounted RO at `/agent/config.json`). `aqua-matrix-template` therefore gains an `aqua-matrix-orchestrator` dep — an *allowed* agents → connector edge.
- **The `MessageHandler` / `InboundMessage` / `authorize` seam (relay).** Backends implement `MessageHandler::handle_message(agent, target, &InboundMessage) -> anyhow::Result<()>`. The relay owns the connect-rotate-sync-watermark lifecycle and dispatch, calling `authorize(sender_mxid, target)` (default = case-insensitive equality with the single target) as the allow/deny gate for both message dispatch and invite auto-join. This is the aqua-security allow/deny hook-point (keyed on DID in a future phase), scaffolded inert today.

**The guard:** `scripts/check-dep-direction.sh` enforces the one-way rule mechanically. It greps each connector crate's `Cargo.toml` with an *anchored* dependency-line regex (`^[[:space:]]*aqua-matrix-(template|heartbeat|claude-p)\b`) so it inspects dependency entries only, never `description` metadata; it exits non-zero and names the offending crate/line on any forbidden edge, prints `OK` otherwise. Run it as part of the gate alongside `cargo build` / `cargo test`.

## Why two Matrix identities (not one)

See the comparison in the chat history that led to this design. Summary:

- **State**: each daemon is stateless w.r.t. the other. No "mode" to track, no command/prose dispatcher.
- **Restart safety**: restarting one does not perturb the other. `#shell restart` of the heartbeat does not disrupt an in-flight `claude -p` invocation.
- **Heartbeat noise stays out of the LLM conversation**: 10-minute status payloads do not interleave with Claude replies.
- **Misroute risk**: zero. A DM to identity-A cannot be misread as a command for identity-B.
- **Element UX**: two distinct DM rooms in Tim's Element — mute, star, or notify each independently.
- **Cost**: one extra `.pem`, one extra OIDC client, one extra systemd unit. ~20 minutes of setup, paid once.

## Synchronization model (stream sync, not polling)

Both Matrix daemons use **matrix-sdk's continuous sync stream** rather than polling:

```rust
// at startup:
client.add_event_handler(|ev: OriginalSyncRoomMessageEvent, room: Room| async move { ... });
tokio::spawn(async move { client.sync(SyncSettings::default()).await });
```

Implications:

- Incoming DMs arrive within ~1 second (matrix-sdk's sync long-poll cadence), not after a 30s tick.
- Event handlers run on the sync task; long-running work (e.g. `claude -p`) is spawned as a separate tokio task so it does not block sync.
- The heartbeat timer is a third tokio task — independent of sync state.

Tradeoff: the daemon holds an open HTTP connection to the homeserver continuously. If the homeserver is unreachable, sync reconnects internally; the rest of the daemon stays alive.

## Identity and device-id persistence

Matrix `device_id` was historically server-assigned on every fresh OAuth login (a new `SIWX_<uuid>` each time). matrix-sdk's SQLite crypto store binds to the `device_id` it was created with, so a `device_id` change against an existing store errors with `account in the store doesn't match`. We now pin a **stable `device_id`** on every login, so the agent reuses ONE Synapse device across logins and restarts, and the refresh-token grant remains the fast path that also preserves it.

**Stable device_id (pinned on every login).** `acquire_session` (the single token-acquisition path, shared by `AgentClient::connect` and the on-401 / near-expiry self-heal `reauth_inner`) resolves an *effective* `device_id` and passes it to `siwx_oidc_auth::authenticate_with_device(..., Some(id))`. The siwx-oidc server then provisions (idempotent upsert, never deletes) that exact device instead of minting a fresh `SIWX_<uuid>`, so the device's E2EE keys survive each login. Resolution precedence:

- **(a) Explicit** `AgentConfig.device_id` / `AGENT_DEVICE_ID` — lets ops give a role-named device (e.g. `heartbeat`).
- **(b) Derived** (default, zero config) — `stable_device_id(did)` = `AQUA_` + the first 12 lowercase-hex chars of `SHA-256(did)`. A pure function of the DID, so the same agent re-derives the SAME device every time. Existing deployments get device stability with no new config.

**Three-tier session resolution in `acquire_session`** (`<store_dir>/config.toml` holds the cache):

1. **Cached access token still within TTL** → validate via `/whoami`, reuse. No network beyond the cheap whoami call.
2. **Access token expired but refresh_token present** → call `siwx_oidc_auth::refresh(server, client_id, refresh_token, did)` to mint a new access token. `device_id` is preserved across this exchange because the refresh grant is bound to the original login session (no device scope needed — the server carries it). Persist the new access token (and rotated refresh token, if the server rotated) back to config.toml.
3. **No cached session, refresh failed, or refresh_token missing** → fresh `authenticate_with_device(..., Some(effective_id))`, which provisions the stable device. The fresh AuthTokens come with a new refresh_token (24h TTL on siwx-oidc standalone) which is persisted for next time.

**Cache divergence guard (the `SIWX_` → `AQUA_` transition).** Before honouring tiers 1–2, `connect` checks the cached session's `device_id` against the effective id. A session cached *before* stable device_ids existed holds an old `SIWX_<uuid>`; reusing or refreshing it would keep the agent on the old device for up to the refresh-token TTL. So a mismatched cache is **discarded**, forcing tier 3, which provisions the stable device. The store, still bound to the old device, then fails `restore_session` with "account in the store doesn't match", and the existing wipe-and-retry rebuilds it. This makes the migration deterministic and converge on the **first** login after upgrade, not eventually. The same guard handles an operator changing `AGENT_DEVICE_ID`.

If tier 3 returns a `device_id` the existing SQLite store rejects with "account in the store doesn't match", the connect code wipes `<store_dir>/matrix-sdk-*.sqlite3*` (NOT `config.toml`) and retries `restore_session` once, then cross-signing re-bootstraps. The previous `ExecStartPre=rm -f ...sqlite3*` workaround in the systemd unit is removed; the in-code path replaces it.

**Resulting behavior:**

| Time since last activity | `device_id` after restart? | Network calls |
|---|---|---|
| Within access-token TTL (~5 min) | Same (stable) | 1 `/whoami` |
| Within refresh-token TTL (24h) | Same (stable) | 1 `/token` (refresh grant) |
| Beyond refresh-token TTL | Same (stable) — re-provisioned by the pinned id; store survives, no wipe | Full OAuth code flow |
| First login after `SIWX_`→`AQUA_` upgrade, or `AGENT_DEVICE_ID` change | Changes to the stable id (crypto store wiped + rebuilt once) | Full OAuth code flow + cross-signing rebootstrap |

So practical operation preserves identity completely, and even multi-day downtime now keeps the SAME device because the id is re-derived and re-provisioned rather than freshly minted. A crypto-store wipe is now a one-time migration event (or a deliberate device rename), not a routine consequence of token expiry. The daemon recovers automatically from any of these cases without manual intervention.

**Persisted fields in `SessionCache`** (`<store_dir>/config.toml` under `[session]`):

| Field | Purpose |
|---|---|
| `access_token` | Bearer token for the Matrix homeserver. Validated via `/whoami`. |
| `expires_at_unix` | When the access token expires. We trigger refresh 30s early. |
| `refresh_token` | siwx-oidc refresh-grant token (opaque `mcr_*`). 24h TTL on standalone. |
| `did` | Required by the refresh-grant call (siwx-oidc signature: `refresh(server, client_id, refresh_token, did)`). |
| `user_id` / `device_id` | `user_id` is server-assigned; `device_id` is the stable id we pin (`AQUA_…` or an explicit `AGENT_DEVICE_ID`). Both are checked against `/whoami` to detect drift and against the effective id to drive the migration guard; required for `restore_session`. |

**Upstream dependency:** this design relies on `siwx-oidc-auth` shipping `AuthTokens.refresh_token` and `siwx_oidc_auth::refresh(...)`. Since commit `ab3ad3f` of the siwx-oidc repo these are available; we depend via `path = "../siwx-oidc/siwx-oidc-auth"` (sibling checkout) rather than a pinned git rev, because the newer commit's workspace also requires the `aqua-auth` crate at `../../aqua-auth` which cargo cannot fetch through a single git dep. To set up a fresh dev host: `git clone https://github.com/inblockio/siwx-oidc.git && git clone https://github.com/inblockio/aqua-auth.git` alongside this repo.

## DM rooms as server-side state (`m.direct`)

When the agent sends a DM it also marks the room as direct in the `m.direct`
global account data map (`{ user_id: [room_id, ...] }`). This means the DM
relationship lives in server-side account data, not only in local crypto/sync
state:

- The DM room resolves from the homeserver on a cold start — even after a full
  crypto-store wipe (tier-3 fresh auth, see "Identity and device-id
  persistence") — instead of the agent creating a fresh, duplicate DM room.
- Element shows the room under "People" / direct chats rather than as a generic
  room.

`m.direct` is written best-effort and idempotently (the existing entry is read,
the room id added if absent, the map written back); it does not gate message
sending.

## Agent self-registry (`io.inblock.aqua.registry`)

Each daemon advertises itself in its own global account data under the custom
type `io.inblock.aqua.registry`, so the fleet can be enumerated by reading
account data rather than scraping logs or systemd. The payload:

```jsonc
{
  "did":          "did:...",                    // the agent's decentralized identifier
  "user_id":      "@...:matrix.inblock.io",      // server-assigned Matrix user id
  "role":         "heartbeat" | "claude-channel",// which surface this identity serves
  "systemd_unit": "aqua-matrix-heartbeat",       // managing unit (None for ad-hoc runs)
  "last_online":  1700000000,                    // unix seconds, refreshed on each upsert
  "version":      "..."                          // crate version
}
```

`AgentClient::update_registry(role, systemd_unit)` performs the upsert. Refresh
cadence:

- **heartbeat**: every tick (and on each client-cycle start), so `last_online`
  tracks the heartbeat interval.
- **claude-channel**: on each client-cycle start. The channel has no periodic
  tick of its own, so `last_online` is refreshed at the token-rotation cadence
  (≈270s).

The upsert is always best-effort at the call site — a registry write failure is
logged at `warn` and never crashes or skips the daemon's primary job.

## Benign `404 Account data not found` log lines on a fresh account

On a brand-new account the matrix-sdk `http_client` logs `ERROR` lines like
`404 ... Account data not found` for `m.direct` and
`m.secret_storage.default_key` during initial sync. These are harmless: the SDK
is probing for account data that has not been written yet.

- The `m.direct` 404 stops once the agent marks its first DM room (see "DM rooms
  as server-side state").
- The `m.secret_storage.default_key` 404 stops once SSSS is enabled (see
  RECOVERY.md § "Secure Secret Storage").

We intentionally do **not** filter the SDK's `http_client` logging to suppress
these, because that same logger surfaces genuine errors — silencing it would
hide real problems to cosmetically remove two expected first-run probes.

## Auto-respawn

All three units are designed to be "always alive" via `Restart=always` (Matrix daemons) or a bash supervisor loop (claude-bridge). Per-unit mechanism, recovery times, manual procedures, and the diagnostic decision tree all live in [`RECOVERY.md`](RECOVERY.md) — start there when something is broken.

## Auto-start chain (WSL boot → daemons up)

```
WSL distro starts
    └─ /etc/wsl.conf [boot] systemd=true
            └─ systemd PID 1 boots
                    └─ user@1000.service starts (linger=yes in loginctl)
                            └─ user systemd manager spawns
                                    ├─ aqua-matrix-heartbeat.service (enabled)
                                    ├─ aqua-matrix-claude-channel.service (enabled)
                                    └─ claude-bridge.service (enabled)
```

Verify on a fresh boot:

```bash
cat /etc/wsl.conf | grep -A1 boot           # systemd=true
loginctl show-user "$USER" | grep Linger    # Linger=yes
systemctl --user is-enabled aqua-matrix-heartbeat aqua-matrix-claude-channel claude-bridge
```

WSL itself does **not** start on Windows boot unless triggered (open a WSL terminal, or set up a Windows scheduled task running `wsl --distribution <name> --exec true` at logon). After WSL starts, everything above is automatic.

## File and identity inventory

| Path | Purpose | Created by |
|---|---|---|
| `agent.pem` | Free chat identity for ad-hoc `aqua-matrix-agent` runs. | Binary, on first chat run. |
| `agent-b.pem` | Second identity for `/e2e-test`. | E2E test setup. |
| `heartbeat.pem` | Heartbeat daemon identity. | Renamed from auto-generated `agent.pem` during initial setup. |
| `claude-channel.pem` | Claude-channel daemon identity. | Auto-generated on first `aqua-matrix-claude-p` run. |
| `~/.aqua-matrix-heartbeat/` | Heartbeat sync state + config + session cache. | Heartbeat daemon. |
| `~/.aqua-matrix-claude-channel/` | Claude-channel sync state + config + session cache. | Claude-channel daemon. |
| `~/.config/systemd/user/aqua-matrix-heartbeat.service` | Heartbeat unit. | Operator install (`cp` from `systemd/`). |
| `~/.config/systemd/user/aqua-matrix-claude-channel.service` | Claude-channel unit. | Operator install. |
| `~/.config/systemd/user/claude-bridge.service` | Interactive Claude tmux unit. | Operator install. |

All `.pem` files are gitignored (`*.pem` in `.gitignore`) — they ARE the identities, must not be checked in.

## Troubleshooting

Moved to [`RECOVERY.md`](RECOVERY.md) — its "Diagnostic decision tree" section covers the canonical symptoms (silent ops channel, claude channel not responding, device_id rotating, `#shell` commands ignored, bridge tmux missing). RECOVERY.md is the runbook; this document is the design rationale.

## Adding a fourth surface

To add another Matrix-visible agent surface (e.g. a calendar bot, a code-review bot): implement the `MessageHandler` trait in a new crate (or as an example) and call `run_daemon()` from `aqua-matrix-relay` — that gives you the connect-rotate-sync-watermark lifecycle for free, so you only write the per-message logic. Then generate a fresh `.pem`, register a fresh OIDC client (auto on first run), ship a new systemd unit, and enable it. See [`docs/REFERENCE.md`](REFERENCE.md) and `crates/aqua-matrix-relay/examples/echo_agent.rs` for a minimal working handler. Do NOT multiplex onto an existing identity — the whole point of this architecture is that each surface owns its channel.
