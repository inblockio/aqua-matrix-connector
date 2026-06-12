# aqua-matrix-agent

Rust library and CLI for Matrix agent communication with E2E encryption.
Agents authenticate via [siwx-oidc](https://github.com/inblockio/siwx-oidc) using decentralized identifiers (DIDs), not passwords.

**Architecture:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — design rationale for the host's three agent components (heartbeat ops channel, claude-channel LLM channel, local claude-bridge tmux), identity/device-id persistence model, stream-sync design, file/identity inventory.

**Recovery / when things break:** [`docs/RECOVERY.md`](docs/RECOVERY.md) — what auto-recovers vs needs human intervention, per-unit restart policies, crash-loop guard explained, manual recovery procedures, and a diagnostic decision tree keyed by symptom. Start here when something is broken.

## If you are an AI agent: how to send and receive messages

This repo is your messaging tool. Use `/matrix-message` for the full skill, or follow the quick start below.

### Send a message

```bash
~/aqua-matrix-agent/target/debug/aqua-matrix-agent --message "your message here"
```

The target comes from `AGENT_TARGET` — set it once in a `.env` file (copy `.env.example`; the repo ships **no** hardcoded target) and `--message`/`--read` need no further flags. On this host a `.env` with the right `AGENT_TARGET` already lives in the repo dir, so the command above works as-is. The agent auto-registers OIDC credentials on first run and caches them in `~/.aqua-matrix-agent/config.toml`. Override the target ad-hoc with `--target`.

### Read messages

```bash
~/aqua-matrix-agent/target/debug/aqua-matrix-agent --read --read-limit 20
```

### Send and read in one call

```bash
~/aqua-matrix-agent/target/debug/aqua-matrix-agent --message "ping" --read
```

### Message a specific user

```bash
~/aqua-matrix-agent/target/debug/aqua-matrix-agent --message "hello" --target "@user:matrix.inblock.io"
```

### Use a different agent identity

```bash
~/aqua-matrix-agent/target/debug/aqua-matrix-agent --key-file other.pem --store-dir ~/.other-agent --message "hi"
```

Each key file produces a unique DID and separate Matrix account. Convention on this host:

- `agent.pem` — chat identity (re-created on first chat run if absent)
- `agent-b.pem` — second test identity for `/e2e-test`
- `heartbeat.pem` — ops identity (heartbeat + `#shell` command channel). Store at `~/.aqua-matrix-heartbeat/`. Managed by `aqua-matrix-heartbeat.service`.
- `claude-channel.pem` — LLM channel identity (forwards prose to `claude -p`). Store at `~/.aqua-matrix-claude-channel/`. Managed by `aqua-matrix-claude-channel.service`.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the rationale (one daemon per surface, separate failure domains, no stateful mode-switching).

### Rich media (files, images, voice, calls)

Beyond text, the connector exposes rich media as **`AgentClient` methods** (async, `anyhow::Result`) for use from inside a `MessageHandler`. The one-shot CLI does not surface these yet — they are for agent backends. matrix-sdk auto-encrypts attachments on send and auto-decrypts on download, so all of this works inside E2E DMs.

**Send** (`target` = peer MXID; `caption` shows as the message body):

- `agent.send_image(target, path, caption).await?` — `m.image` (dimensions read from header bytes, no full decode)
- `agent.send_file(target, path, caption).await?` — `m.file`
- `agent.send_audio(target, path).await?` / `agent.send_video(target, path, caption).await?`
- `agent.send_voice_message(target, path, duration_ms, waveform).await?` — `m.audio` with MSC3245 voice markers so Element X renders a waveform bubble. **You** supply `duration_ms` (known from encoding); `waveform: Option<Vec<f32>>` is synthesised if `None`. The connector does **not** decode audio — it computes neither duration nor waveform from the file.

**Receive:** `InboundMessage` now carries `media: Option<InboundMedia>` (the seam the relay hands your handler). `InboundMedia` holds `kind` (`Image|Audio|Voice|Video|File`), `filename`, `mimetype`, `size`, `duration_ms`, `width`/`height`, `is_voice`, `waveform`, and a `handle`. For a captioned attachment, `msg.body` holds the caption. Fetch bytes on demand:

- `agent.download_media(&media.handle).await?` → `Vec<u8>` (auto-decrypted)
- `agent.download_media_to_temp(&media.handle, dir).await?` → `PathBuf`

**Calls — signaling + detection ONLY, no live media.** `agent.ring_call(target).await?` sends an `m.call.notify` Ring (MSC4075) that makes a peer's Element X show an incoming call. Inbound call events arrive via a new default-no-op `MessageHandler::on_call(&self, agent, target, call: &InboundCall)`; the relay forwards `m.call.invite`/`m.call.notify`/`m.call.hangup` as an `InboundCall { signal: Invite|Ring|Hangup, call_id, sender_mxid, room_id }`. **The agent can ring a peer and detect invites/rings/hangups, but cannot place or carry an actual audio/video stream** — matrix-sdk 0.17 ships no WebRTC/LiveKit stack, so real call-media participation would need a separate WebRTC engine (e.g. webrtc-rs) + the Element Call SFU. Files / images / voice messages are fully functional. **LIVE call media now exists — agents-side, not here.** The sibling `../aqua-agents` `aqua-call-agent` crate provides a real WebRTC media plane (LiveKit/Element Call SFU): an agent can join a call and publish generated video + audio, speak via Deepgram TTS, and transcribe the peer via Deepgram STT — verified live two-agent, both directions. It builds entirely on the two thin connector primitives `AgentClient::request_openid_token()` and `AgentClient::device_id()` (the LiveKit JWT is minted by `lk-jwt` from the OpenID token); the connector itself stays signaling-only and contains no WebRTC. It now also **publishes the `org.matrix.msc3401.call.member` membership state event** (via the connector primitives `AgentClient::set_rtc_member`/`clear_rtc_member`, with `rtc_focus_service_url()` for the LiveKit focus), advertising membership on call join in a real Matrix room and clearing it on leave — using an MSC3757 owned state key `_<user_id>_<device_id>`. **Live-verified: the inblock Synapse accepts the owned state key and another Matrix user reads the membership back (cross-client discoverable as a call participant).** The only thing not yet confirmed is the final visual render in Element X (needs a human to open the room).)

Worked example: [`crates/aqua-matrix-relay/examples/media_agent.rs`](crates/aqua-matrix-relay/examples/media_agent.rs). Design detail and the source-verified feasibility notes: [`docs/plans/2026-06-04-rich-media.md`](docs/plans/2026-06-04-rich-media.md).

### Build if binary is missing

```bash
cd ~/aqua-matrix-agent && cargo build
```

Default to debug builds (release is much slower for iteration). The systemd units intentionally point at `target/debug/`.

## Architecture

This is a Cargo workspace (virtual root manifest) and a reference implementation for any agent backend over Matrix + siwx-oidc — implement `MessageHandler` and call `run_daemon()` from `aqua-matrix-relay`. After the **physical repo split**, this repo is the **connector substrate: six crates** under `crates/`. The three backend crates + agent content now live in the sibling **`../aqua-agents`** repo, which path-deps back here (see "Repo boundary" below and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)):

```
aqua-matrix-agent (virtual workspace — CONNECTOR substrate, 6 crates)
  |
  | CONNECTOR half (this repo; reusable substrate; never names an agents-side crate)
  +-- crates/aqua-matrix-agent        -- library (AgentClient, AgentConfig, OIDC, recovery,
  |                                      registry) + one-shot CLI binary `aqua-matrix-agent`
  |                                      (--message / --read / --print-did only)
  +-- crates/aqua-matrix-relay        -- generic daemon: MessageHandler trait + InboundMessage +
  |                                      authorize() seam + run_daemon() lifecycle
  |                                      (connect-rotate-sync-watermark); examples/echo_agent.rs
  +-- crates/aqua-matrix-ask-mcp      -- stdio MCP server: one tool ask_human(q)->answer, bridged
  |                                      over a per-run unix socket; lib half reused by gating
  +-- crates/aqua-matrix-orchestrator -- transport/agent-agnostic Podman engine: ContainerManager
  |                                      driven by the ContainerSpec seam (never sees AgentType)
  +-- crates/aqua-matrix-gating       -- confirmation/gating substrate: PendingMap (ask_user),
  |                                      destructive matcher, AskBridge (ask_human socket)
  +-- crates/aqua-activity-watch      -- host-side activity tracker (binary): tails an agent's
  |                                      inbound.jsonl + DMs the operator on the peer's first message
  |                                      and every Nth; dependency-free (no Matrix/crypto), so it
  |                                      trivially satisfies the one-way rule
  |
  +-- siwx-oidc-auth -- Headless OIDC client (CAIP-122 signature auth)
  +-- matrix-sdk     -- Matrix client with E2E encryption (Megolm/Vodozemac)

../aqua-agents (sibling repo — AGENTS half; path-deps the connector crates above)
  +-- crates/aqua-matrix-template     -- AgentType/InstanceBinding/ResolvedInstance + the
  |                                      build_spawn_spec(agent_type, target, id) -> ContainerSpec factory
  +-- crates/aqua-matrix-heartbeat    -- binary `aqua-matrix-heartbeat` (ops agent: periodic
  |                                      status DM + #shell commands); deps: relay, template, orchestrator
  +-- crates/aqua-matrix-claude-p     -- binary `aqua-matrix-claude-p` (reference Claude backend:
  |                                      forwards DMs to `claude -p`); deps: relay, template, gating
  +-- types/*.json, instances/*.toml.example, Dockerfile, scripts/build-image.sh  -- agent content
```

**Repo boundary (the one-way rule):** dependencies flow **agents → connector, NEVER the reverse.** The two halves now live in **two repos**: this connector and the sibling `../aqua-agents` (which must be checked out alongside this repo for its path-deps to resolve). No connector crate (`agent`, `relay`, `ask-mcp`, `orchestrator`, `gating`, `activity-watch`) may name a backend crate (`template`, `heartbeat`, `claude-p`) in its `Cargo.toml`. The connector reaches agents only through two seams: the `ContainerSpec` seam (`aqua_matrix_template::build_spawn_spec` maps a rich `AgentType` down to the agent-agnostic spec the orchestrator consumes) and the `MessageHandler`/`InboundMessage`/`authorize` seam in the relay. The guard `scripts/check-dep-direction.sh` enforces this mechanically (anchored grep of connector `Cargo.toml` dependency lines) — **run it as part of the gate** alongside `cargo build` / `cargo test`. Full rationale: [`docs/plans/repo-split-execution-handover.md`](docs/plans/repo-split-execution-handover.md).

**Deployment glue stays here (for now):** `Skills/` and `systemd/` remain in this connector repo pending a later deployment-migration pass — the agent systemd units still `ExecStart` from `%h/aqua-matrix-agent/target/debug/...`. (The `Dockerfile`/`build-image.sh` half of that pass is **done as of 2026-06-03**: both — now in `../aqua-agents` — stage a cross-repo two-workspace context, building `claude-p` from the agents workspace and `ask-mcp` from the connector, and rebuild `localhost/aqua-matrix-agent:poc` correctly.)

**Authentication flow:** Ed25519 key -> derive DID -> CAIP-122 challenge-response against siwx-oidc -> siwx-oidc verifies signature, provisions user in Synapse via MSC3861 endpoints, issues opaque `mat_*`/`mcr_*` tokens -> Synapse validates tokens via `/oauth2/introspect` (RFC 7662) -> Matrix session restored.

**siwx-oidc is NOT a fork of MAS.** It is a fully independent Rust OIDC provider (Axum + Redis) that implements MSC3861 compatibility so Synapse can delegate authentication to it. The CAIP-122 signature verification happens server-side in siwx-core. A shared `MAS_SHARED_SECRET` secures the introspection channel between Synapse and siwx-oidc.

**Encryption:** All DMs are E2E encrypted (Megolm via Vodozemac). The agent bootstraps cross-signing on first connect so its device appears as "verified" in Element. Crypto state persists in SQLite at the store directory.

**OIDC auto-registration:** If no `--client-id` is provided, the agent registers a new client via the siwx-oidc `/register` endpoint and caches credentials in `{store_dir}/config.toml`.

## Building and testing

```bash
cargo build                                  # debug build for iteration (DEFAULT — fast)
cargo build --release                        # only when shipping; do not use during dev
cargo test                                   # run unit tests (config roundtrip, partial loading)
cargo test --test e2e --features e2e         # run E2E test (requires live matrix.inblock.io)
```

A `cargo build` in this connector repo builds the six connector crates, producing `target/debug/aqua-matrix-agent` (the one-shot CLI) and `target/debug/aqua-activity-watch` (the host-side activity tracker). The `aqua-matrix-heartbeat` and `aqua-matrix-claude-p` binaries now build from the **sibling `../aqua-agents` repo** (`cd ../aqua-agents && cargo build`, which path-deps this connector). The systemd units still point at `%h/aqua-matrix-agent/target/debug/...` — reconciling that build/output location is part of the deferred deployment-migration pass.

## Configuration (`.env`, per-instance)

Every binary loads a `.env` file at startup (before parsing flags), so config —
especially `AGENT_TARGET` — lives in a file, not baked into the code. **The repo
ships no hardcoded target or secrets**; copy `.env.example` to `.env` and fill in
your own. Precedence, high → low: explicit CLI flag > process env (e.g. systemd
`Environment=`) > `.env` file > built-in default.

- **Which file:** `AGENT_ENV_FILE=/path/to/file` selects an explicit file; unset,
  it loads a conventional `.env` from the working dir. This is how **multiple
  agents** coexist — each instance points `AGENT_ENV_FILE` at its own file. The
  systemd units do exactly this (`%h/.aqua-matrix-<role>/agent.env`).
- **On this host:** machine-local `.env` files already hold the real
  `AGENT_TARGET` (repo dir for the CLI; each store dir for the daemons). They are
  gitignored — never commit real values.
- **URLs** (`MATRIX_URL`, `SIWX_URL`) default to the inblock endpoints and are
  overridable via `.env`/flag for a different deployment.

## CLI flags

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--key-file` | `AGENT_KEY_FILE` | `agent.pem` | Ed25519 PEM key (created if missing) |
| `--siwx-url` | `SIWX_URL` | `https://siwx-oidc.inblock.io` | siwx-oidc provider URL |
| `--matrix-url` | `MATRIX_URL` | `https://matrix.inblock.io` | Matrix homeserver URL |
| `--client-id` | `OIDC_CLIENT_ID` | auto-registered | OIDC client ID |
| `--redirect-uri` | `OIDC_REDIRECT_URI` | `http://localhost:0/callback` | OIDC redirect URI |
| `--target` | `AGENT_TARGET` | none (required for `--message`/`--read`) | Matrix user ID to message; set via `.env` (see `.env.example`) |
| `--store-dir` | `AGENT_STORE_DIR` | `~/.aqua-matrix-agent` | SQLite + config directory |
| `--device-id` | `AGENT_DEVICE_ID` | derived `AQUA_<sha256(did)[..12]>` | Pin a stable Matrix device_id; omit to derive one from the DID |
| `--message` | | | Message text to send |
| `--read` | | | Read recent messages |
| `--read-limit` | | `20` | Number of messages to fetch |
| `--print-did` | | | Print agent DID and exit |

These are the only flags on the `aqua-matrix-agent` binary — it is one-shot. The long-running daemons now live in the **sibling `../aqua-agents` repo** (built with `cargo build` there): `aqua-matrix-heartbeat` (status-DM + `#shell`; interval via `--interval <secs>`, see `/heartbeat` skill) and `aqua-matrix-claude-p` (forwards inbound DMs from `--target` through `claude -p`, see `/claude-channel` skill). See [`docs/REFERENCE.md`](docs/REFERENCE.md).

## Wrapped-harness configuration

This repo runs under `claude-ws` (alias for `claude --dangerously-skip-permissions`, defined at `~/.bashrc:122`). `~/.claude/settings.json` is configured to:

- pin `"model": "claude-opus-4-7"` so every session uses the most capable model
- expose `CONTEXT_WINDOW=1000000` so the Stop hook + heartbeat report the right context % for the 1M variant
- register a `Stop` hook at `~/.claude/hooks/compact-at-50.py` that blocks stop with `decision: "block"` and instructs Claude to run `/compact` whenever context usage crosses 50% (`COMPACT_THRESHOLD` env var to tune)

The hook reads the latest `usage.input_tokens` from the active transcript, so token accounting matches whatever the model itself reported.

## Skills

| Skill | Purpose |
|---|---|
| `/matrix-message` | Full reference for sending and receiving E2E encrypted messages |
| `/e2e-test` | Run and verify E2EE integration tests between two agent identities |
| `/heartbeat` | Run the `aqua-matrix-heartbeat` binary — DMs status every 10min AND honors `#shell help`, `#shell status`, `#shell ping`, `#shell uptime`, `#shell restart`, `#shell respawn`, `#shell respawn-channel`, `#shell logs`, `#shell import <message>` commands sent from `--target` |
| `/claude-bridge` | Persistent `claude --dangerously-skip-permissions` in tmux, supervised by systemd; respawnable via `#shell respawn`. Messages can be injected into it via `#shell import` → `~/import` inbox → `aqua-import-watch` (see `/heartbeat`). |
| `/claude-channel` | Matrix LLM channel daemon (`aqua-matrix-claude-p` binary) — separate identity, forwards DMs from `--target` to `claude -p` and replies with stdout; respawnable via `#shell respawn-channel` |

**Skill layout.** Skill source-of-truth lives at the repo root in `Skills/<name>/skill.md`. The Claude Code discovery directory `.claude/skills/<name>` is a symlink into `Skills/`:

```
Skills/
  matrix-message/skill.md   <-- canonical content (edit here)
  e2e-test/skill.md
.claude/skills/
  matrix-message -> ../../Skills/matrix-message
  e2e-test      -> ../../Skills/e2e-test
```

Edit skills in `Skills/`. Do not duplicate content into `.claude/skills/`. When adding a new skill: create `Skills/<name>/skill.md`, then `ln -s ../../Skills/<name> .claude/skills/<name>`. If `.claude/skills/<name>/skill.md` ever resolves to a regular file instead of a symlink target, the layout has drifted — re-create the symlink.

## Companion repos

| Repo | Purpose |
|---|---|
| `../siwx-oidc/` | CAIP-122 OIDC provider. Path-dep source of `siwx-oidc-auth` (see Cargo.toml). |
| `../aqua-auth/` | Workspace member of `../siwx-oidc/`. Must be checked out as a sibling for `cargo build` to resolve `siwx-oidc-auth`'s `path = "../../aqua-auth"` dep. |
| `../siwx-oidc-matrix-server/` | Docker Compose stack (Synapse + siwx-oidc + Element Web) |
| `../aqua-agents/` | The **agents half** (backends `aqua-matrix-template`/`-heartbeat`/`-claude-p` + `types`/`instances`/`Dockerfile`/`build-image.sh`). Path-deps this connector's crates (`../aqua-matrix-agent/crates/...`); build with `cargo build` from there. |

Fresh dev setup needs `git clone https://github.com/inblockio/siwx-oidc.git` and `git clone https://github.com/inblockio/aqua-auth.git` alongside this repo.
