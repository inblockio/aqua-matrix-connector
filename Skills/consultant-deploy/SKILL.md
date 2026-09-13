---
name: consultant-deploy
description: Deploy, relabel, and image-roll dedicated single-target Aqua consultant containers via the generic spawn-consultant.sh + registry + roll-consultant-fleet.sh (replaces per-consultant byte-cloned scripts).
---

# Aqua Consultant Deployment

Deploy and maintain **dedicated, single-target Aqua consultants** — one container per peer, each
running `localhost/aqua-matrix-agent:poc` (the `aqua-matrix-claude-p` backend), bound to exactly
ONE peer MXID by the relay firewall (`authorize(sender == target)` in
[`crates/aqua-matrix-relay`](../../crates/aqua-matrix-relay/src/lib.rs)). Each consultant is a
read-only Aqua-protocol assistant grounded in six repos mounted read-only at `/refs`:
`aqua-rs-sdk` (reference implementation), `aqua-spec` (authoritative spec),
`aqua-ecosystem` (trust-layer map: tiers, products, per-repo registry facts),
`aqua-governance-corpus` (governance principles, ICT kernels, deliberation methods),
`aqua-compliance` (regulatory positioning across jurisdictions), and
`inblockio.github.io` (the published inblock.io website: products, positioning, team).
The system prompt in `consultant-config.template.json` tells the agent how to use each.

This skill replaces the old approach of hand-cloning a `recreate-<label>-consultant.sh` per peer.
One generic launcher does it all; a registry + roller image-upgrades the whole fleet.

## Canonical source vs. host copies

The generic tooling is **canonical in this skill directory** and version-controlled here; the host
runs it via symlinks so there is a single source of truth (no drift):

| Canonical (in repo) | Host path (symlink → canonical) | Role |
|---|---|---|
| `Skills/consultant-deploy/spawn-consultant.sh` | `~/spawn-consultant.sh` | generic launcher |
| `Skills/consultant-deploy/roll-consultant-fleet.sh` | `~/roll-consultant-fleet.sh` | fleet image-roller |
| `Skills/consultant-deploy/restore-agent-fleet.sh` | `~/restore-agent-fleet.sh` | boot-time fleet restore (see below) |
| `Skills/consultant-deploy/consultant-config.template.json` | `~/.aqua-matrix-test/consultant-config.template.json` | config template |
| `Skills/consultant-deploy/consultants.registry.example` | *(host state — not symlinked)* | format reference |

**Host state stays on the host** (not in the repo): the live registry
`~/.aqua-matrix-test/consultants.registry`, and every consultant's `<label>-aqua-consultant-config.json`
+ `<label>-aqua-consultant-persist/{store,memory}` under `~/.aqua-matrix-test/`. Persist holds the
self-minted DID (`store/agent.pem`) + memory — it is the identity, preserved across re-runs.

Edit the scripts **here in the repo**; the host symlinks pick the change up immediately.

## Prerequisites

- Image built: `bash ~/aqua-agents/scripts/build-image.sh` (stages the 4 sibling repos + host `claude`, retags `:poc`).
- Refs checkouts on the host: `~/aqua-rs-sdk`, `~/aqua-spec`, `~/aqua-governance-corpus`,
  `~/aqua-ecosystem`, `~/aqua-compliance`, `~/inblockio.github.io` (all under
  `github.com/inblockio/`). The spawner mounts them ro at
  `/refs/<name>` and **refuses to launch if one is missing** (it prints the exact clone command).
- OAuth token at `~/.aqua-matrix-heartbeat/claude-oauth-token` (passed **by reference** — never on a command line).
- Optional: `~/.aqua-secrets/deepgram.env` (one line, `DEEPGRAM_API_KEY=...`) for voice messages, also
  passed by reference. Missing file = voice stays disabled, nothing else changes. See "Voice messages".
- `~/.aqua-matrix-notify/notify-tim.sh` (operator DMs) and `target/debug/aqua-activity-watch` (built) for the watcher.

## Add a new consultant

One command. Renders the config from the template, launches with the hardened podman flags, wires
a systemd activity-watcher, and (`--onboard`) DMs the operator a forward-ready onboarding message
carrying the agent's self-minted MXID.

**The consultant initiates the connection.** The template sets `"initiate_dm": true`, so on first
connect (when no DM room exists yet) the agent creates the room, invites the peer, and delivers
her greeting into it — the peer just accepts the invite. The onboarding block therefore says
"expect an invite from <MXID>", not "DM this MXID". Delivery is tracked by the greeted-marker:
a failed initiate retries on the next process start. **Sequencing invariant:** `initiate_dm` in a
config requires an image whose binary knows the field (`deny_unknown_fields` hard-rejects unknown
keys) — roll the image BEFORE rendering configs that carry it; never point an old image at a
freshly rendered config. (`--refresh-prompt` and `--keep-config` never inject the field into
existing configs, so already-deployed consultants are unaffected until re-rendered.)

```bash
bash ~/spawn-consultant.sh \
  --label gawain \
  --target '@did-key-…:matrix.inblock.io' \
  --display 'Aqua Consultant (Gawain)' \
  --name Gawain \
  --onboard
```

**Before spawning, ALWAYS diff the new `--target` against existing configs** — a did:key Tim gives
with a fresh human name may already have a consultant (this is exactly how "Aubert" turned out to be
the existing `zdnaez` peer):

```bash
grep -l 'THE_DID_KEY_LOCALPART' ~/.aqua-matrix-test/*-config.json   # any hit = already deployed
```

Then add the consultant to the live registry so the fleet roller includes it:
```bash
printf 'gawain\t@did-key-…:matrix.inblock.io\tAqua Consultant (Gawain)\n' >> ~/.aqua-matrix-test/consultants.registry
```

## Relabel / re-point an existing consultant

Change the Matrix display name (or rebind the target) while **preserving the DID, memory, and any
hand-customized config** (the render *merges* onto the existing config, overriding only
id/target/display). Uses `--replace` (rm -f + re-run; DID survives via the persist volume):

```bash
bash ~/spawn-consultant.sh --replace \
  --label zdnaez \
  --target '@did-key-…:matrix.inblock.io' \
  --display 'Aqua Consultant (Aubert)'
```

For a display-only tweak with zero container churn, edit `<label>-...-config.json` `.display_name`
and `podman restart <container>` (the daemon re-applies it idempotently on reconnect).

## Image-roll the whole fleet

After rebuilding the image, roll every registry consultant onto it. DIDs + memory preserved (persist
reused); configs used **verbatim** (`--keep-config`, never re-rendered) so custom hello/homeserver
survive. The reserved registry label `generic` covers the **un-labeled operator-bound consultant**
(`aqua-agent-aqua-consultant-1`, config `aqua-consultant-config.json`, persist
`aqua-consultant-persist` — no `<label>-` prefix): the roller translates it to
`spawn-consultant.sh --generic`, so one roll covers the whole consultant fleet. Only
`aqua-agent-tim-channel` stays excluded (different backend; `SEED=0 bash ~/recreate-tim-channel.sh`).

```bash
bash ~/roll-consultant-fleet.sh --dry-run     # preview
bash ~/roll-consultant-fleet.sh --build        # rebuild image, then roll all
```

After a **template prompt change** (a new grounding repo, a new behavioural rule), plain
`--keep-config` would leave existing consultants on the old prompt. Add `--refresh-prompt`
so each kept config adopts the template's current `system_prompt`/`description`/`ref_mounts`
while everything else (hello, homeserver, DID, memory) is preserved:

```bash
bash ~/roll-consultant-fleet.sh --refresh-prompt
```

## Refs grounding and freshness (automatic)

The agent's knowledge is the live host checkouts, bind-mounted read-only; a host-side
`git pull` is visible to running consultants immediately. On **every** launch (spawn,
relabel, and each consultant of a fleet roll) the spawner runs a refs check over the
`REFS_REPOS` list, so a rebuild can never ship stale or missing grounding:

| Repo state | Behaviour |
|---|---|
| missing on host | **FATAL**: refuses to launch, prints the `git clone` command |
| clean + behind upstream | fast-forward pull, logs old/new state |
| dirty + behind | loud warning, mounted as-is (local work never touched) |
| diverged from upstream | loud warning, mounted as-is (resolve manually) |
| fetch fails (offline) / no upstream / not a git checkout | warning, mounted as-is |

`--no-refresh-refs` skips the freshness pass (presence stays fatal). The mount flags,
the presence check, and the freshness pass are all driven by the single `REFS_REPOS`
list in `spawn-consultant.sh`; keep that list in sync with `ref_mounts` in
`consultant-config.template.json` (which the system prompt mirrors).

## Identity & lifecycle flags

| Flag | Effect |
|---|---|
| *(none)* | New container; refuses if one already exists. Fresh DID self-minted on first connect. |
| `--generic` | Select the **un-labeled** operator-bound consultant instead of a `--label` one (container `aqua-agent-aqua-consultant-1`, config/persist without the `<label>-` prefix). Identical behaviour except no activity watcher is wired (the peer IS the operator). Mutually exclusive with `--label`; the registry label `generic` is reserved to map here. |
| `--replace` | `podman rm -f` + re-run, **reusing persist** → DID + memory PRESERVED (the image-roll path). |
| `--keep-config` | Use the existing config verbatim (no re-render); derives id/target/display from it. |
| `--fresh` | Wipe the persist dir first → brand-new identity + empty memory. (Rejected with `--keep-config`.) |
| `--onboard` | After connect, derive the agent MXID from logs and DM the operator a forward-ready onboarding message. |
| `--no-refresh-refs` | Skip the refs freshness pass (fetch/ff-pull). Presence of every refs repo is still enforced. |
| `--refresh-prompt` | Adopt the template's current `system_prompt`/`description`/`ref_mounts` into the config; hello/homeserver customizations, DID, and memory preserved. The sanctioned way to push a prompt update to existing consultants. |
| `--voice on\|off` | Patch only `voice.enabled` in the rendered/kept config (idempotent, sibling voice keys preserved, `off` keeps the block). Without it the config's voice block is left exactly as it is. See "Voice messages". |
| `--print-run` | Print the assembled `podman run` argument vector one arg per line and exit 0 **before** any container, systemd unit, DM, `--replace` removal or `--fresh` wipe. Implies `--no-refresh-refs` (presence still enforced). Still renders/patches the config and creates the persist dirs (they are the run's inputs). Secrets print as bare names only. |

## Invariants the tooling enforces (verified by adversarial audit, 2026-06-08)

- **Security posture is byte-identical** to the proven original `recreate-zdnaez-consultant.sh`
  for the resource/caps/token flags: `--restart on-failure`, `--memory 2048m --cpus 2 --pids-limit 512`,
  `--cap-drop ALL`, `--security-opt no-new-privileges`, `--tmpfs /tmp`, OAuth token passed
  **by reference** (`-e CLAUDE_CODE_OAUTH_TOKEN`, no value). The optional Deepgram key follows the
  same rule (`-e DEEPGRAM_API_KEY`, bare, only when the env file yields one). Since 2026-09-13 the
  argument vector is assembled once (`RUN_ARGS`) and feeds both `--print-run` and the real run.
  Keep it that way — verify after any edit:
  ```bash
  diff <(grep -E '^\s*(--restart|--memory|--cpus|--pids-limit|--cap-drop|--security-opt|--tmpfs|-e )' ~/recreate-zdnaez-consultant.sh) \
       <(grep -E '^\s*(--restart|--memory|--cpus|--pids-limit|--cap-drop|--security-opt|--tmpfs|-e )' ~/spawn-consultant.sh)
  ```
- **Mounts**: config mounted `:ro`, persist store/memory mounted `:U`, every `REFS_REPOS` repo
  mounted `:ro`. Since 2026-06-12 the refs mounts are list-driven and the legacy two-mount
  baseline was deliberately extended with `aqua-governance-corpus` + `aqua-ecosystem`, so the
  `-v` lines no longer byte-match the legacy script (the flag diff above intentionally excludes
  them). Verify nothing mounts writable:
  ```bash
  grep -n 'REF_MOUNT_ARGS+' ~/spawn-consultant.sh        # the one mount template; must end :ro
  podman inspect aqua-agent-<label>-aqua-consultant-1 \
    --format '{{range .Mounts}}{{.Destination}} rw={{.RW}}{{"\n"}}{{end}}'   # every /refs/* rw=false
  ```
- **Single-target binding** — the relay matches one exact target; never widen a consultant to >1 peer.
- **Guards**: placeholder/non-MXID target rejected; `--display` rejects quote/newline (systemd-unit
  injection); registry parser skips indented comments and rejects non-slug labels; `--fresh` asserts
  the persist-path prefix before any `rm -rf`.

## Voice messages (opt-in, per consultant)

The agent's voice-note turn (Deepgram STT for the peer's note, Deepgram TTS for the answer) is
gated by `voice.enabled` in the per-instance config and is **absent/false by default**. Two
pieces of plumbing in `spawn-consultant.sh`, both no-ops until you opt in:

| Piece | What it does |
|---|---|
| Key, by reference | If `${AQUA_DEEPGRAM_ENV:-$HOME/.aqua-secrets/deepgram.env}` is readable and yields a non-empty `DEEPGRAM_API_KEY`, the spawner passes it to podman as a bare `-e DEEPGRAM_API_KEY` (value never on a command line, never in `podman inspect`). The file is sourced in a subshell and only that one variable is captured. No file: one notice line, voice stays disabled. |
| Switch | `--voice on` sets `voice.enabled = true` (creating `{"enabled": true}` when the block is absent, otherwise flipping only that key and keeping siblings such as `tts_voice`). `--voice off` flips it to `false` and **keeps** the block; on a config with no block it writes nothing. Anything but `on`/`off` is rejected. |

**Image before config.** `voice` is an unknown field to every image older than the voice
feature, and the template structs are `deny_unknown_fields`, so a consultant carrying a `voice`
block on an old image crash-loops. Roll the image first, then `--voice on`. The spawner never
injects the block on its own: `--replace --keep-config` (the fleet roll) carries an existing
block through verbatim, `--refresh-prompt` only touches `system_prompt`/`description`/`ref_mounts`,
and the template itself has no `voice` key.

A config with `voice.enabled=true` but no key still launches (the agent logs the missing key and
disables voice at runtime); the spawner prints `!! voice: enabled in config but DEEPGRAM_API_KEY
is not available` so the gap is visible at deploy time.

```bash
# enable on one consultant whose container already runs the voice-aware image:
bash ~/spawn-consultant.sh --replace --keep-config --label zdnaez --voice on
# preview the exact argument vector first, nothing started (secrets as bare names):
bash ~/spawn-consultant.sh --print-run --replace --keep-config --label zdnaez --voice on
```

Fleet-wide enablement is a separate decision (third-party processing disclosure to peers).

### Tests (no live side effects)

`tests/spawn-consultant-args.sh` renders the argument vector with `--print-run` in a sandbox
(temp HOME, test dir, refs base, token and env files; `podman`/`systemctl` replaced by shims
that fail and record any call) and asserts: no key file means no `DEEPGRAM_API_KEY`; a fake
key file means exactly one bare `-e DEEPGRAM_API_KEY` and the value nowhere; `--voice on|off`
flips only that key; `--replace --keep-config` preserves the block; `inblockio.github.io` is in
`REFS_REPOS` and mounted `:ro`; the shims were never called.

```bash
bash Skills/consultant-deploy/tests/spawn-consultant-args.sh
```

The sandbox relies on the spawner's env overrides: `CONSULTANT_TEST_DIR` (configs, persist,
avatars, template; default `~/.aqua-matrix-test`), `CONSULTANT_TEMPLATE`, `CONSULTANT_REFS_BASE`,
`CONSULTANT_IMAGE`, `AQUA_CLAUDE_TOKEN_FILE`, `AQUA_DEEPGRAM_ENV`.

## Boot-time restore (reboot survival)

Podman restart policies don't survive a WSL/VM reboot, and the fleet's `--restart on-failure`
never fires on the clean SIGTERM exit a shutdown produces — without help, **every** container
(consultants AND the Tim-bound pair) sits in `exited` after a reboot until someone starts it
(this silenced the whole fleet for 9h on 2026-06-10). Two pieces close that hole:

- **`aqua-agent-fleet-restore.service`** (systemd user oneshot, enabled, runs at boot; unit
  canonical at [`systemd/aqua-agent-fleet-restore.service`](../../systemd/aqua-agent-fleet-restore.service)) runs
  `~/restore-agent-fleet.sh`, which `podman start`s every exited `aqua-agent-*` container.
  Identity/memory live in the persist volumes, so agents come back `store_wiped=false`.
  Caveat: it restarts intentionally-stopped containers too — `podman rm` (or rename away from
  the `aqua-agent-` prefix) anything that must stay down across reboots.
- **`crashloop-watch.sh`** (host-canonical at `~/.aqua-matrix-notify/`) now re-alerts every
  hour while a container stays down (`--realert 3600`) and only counts an alert as sent when
  the DM actually delivered (failed sends retry every 15s poll). The watcher unit is ordered
  `After=aqua-agent-fleet-restore.service` so a normal reboot stays quiet.

Recovery matrix: [`docs/RECOVERY.md`](../../docs/RECOVERY.md).

## Verify a deployment

```bash
podman ps --filter name=aqua-agent-<label>            # Up, restarts 0
podman logs aqua-agent-<label>-aqua-consultant-1 | grep -E 'agent DID|connected|display name set|daemon starting'
systemctl --user is-active aqua-activity-watch-<label>.service   # n/a for --generic (no watcher by design)
podman inspect aqua-agent-<label>-aqua-consultant-1 \
  --format '{{range .Mounts}}{{.Destination}} {{end}}'   # expect all four /refs/* + config + store + memory
```

For the generic consultant the container name is plain `aqua-agent-aqua-consultant-1` (no label).

Healthy = `daemon starting (target: <the one peer>)`, `connected store_wiped=false` (for `--replace`),
and `display name set to "<your display>"`. Then the peer DMs the agent's `@did-key-…:matrix.inblock.io`
MXID (shown in the onboarding message / derivable from the `agent DID:` log line, lowercased).

## Related

- Memory: `consultant-fleet-recreate` (procedure + legacy per-script notes), `gawain-aqua-consultant`,
  `zdnaez-aqua-consultant` (= "Aubert"), `notify-tim-channel`, `relay-mxid-auth-case`.
- Skills: [`claude-channel`](../claude-channel/skill.md) (the backend daemon), [`heartbeat`](../heartbeat/skill.md).
