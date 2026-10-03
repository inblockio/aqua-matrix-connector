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
| `Skills/consultant-deploy/consultant-persona.py`, `owner-allowlist.py` | *(none: found next to the symlink target)* | persona + welcome (hello) render, Owner allow-list step |

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
a systemd activity-watcher, and (`--onboard`) tells the operator once the consultant's own welcome
has reached the peer (see "Onboarding" below).

**The consultant initiates the connection.** The template sets `"initiate_dm": true`, so on first
connect (when no DM room exists yet) the agent creates the room, invites the peer, and delivers
her greeting (the config's `hello`) into it; the peer just accepts the invite. That greeting IS
the welcome: nobody else messages the peer. Delivery is tracked by the greeted marker: a failed
initiate retries on the next process start. **Sequencing invariant:** `initiate_dm` in a
config requires an image whose binary knows the field (`deny_unknown_fields` hard-rejects unknown
keys) — roll the image BEFORE rendering configs that carry it; never point an old image at a
freshly rendered config. (`--refresh-prompt` and `--keep-config` never inject the field into
existing configs, so already-deployed consultants are unaffected until re-rendered.)

```bash
bash ~/spawn-consultant.sh \
  --label gawain \
  --target 'did:key:z6Mk…' \
  --persona Talia \
  --name Gawain \
  --onboard
```

**Owner rule (Tim, standing, 2026-09-29).** Every consultant has exactly ONE authoritative
Owner: its single target peer (`--target`, or the kept config's `target`; for `--generic`, Tim).
The Owner is **always on the Aqua System allow-list**: every real spawn (new, `--replace`,
`--keep-config`, and therefore every `roll-consultant-fleet.sh` roll, which backfills existing
Owners) runs `owner-allowlist.py` before anything else changes (before the config render, the
`--replace` removal and the launch). It is a no-op when any `[[recipients]]` entry already has
that MXID (ASCII case-insensitive, like the bridge); otherwise it appends

```toml
[[recipients]]
name = "<label>"            # "<label>-owner" if <label> is taken (by a person OR a room)
mxid = "<Owner MXID>"
note = "Owner of <container> (<persona or display>), auto-added by spawn-consultant.sh YYYY-MM-DD"
```

right after the last existing `[[recipients]]` entry, under `flock` on `allowlist.toml.lock`,
with a backup `allowlist.toml.bak-<label>-<ts>` (only on change), via a mode-600 temp file that
is parsed with `tomllib` and checked against the bridge's own rules (names unique across
recipients and rooms, valid MXIDs/room ids, existing entries unchanged) before an atomic rename.
The daemon hot-reloads it. Tim's standing approval covers Owners only; everyone else still needs
his explicit go. The generic consultant's Owner (Tim) is never auto-added: if he is missing, the
spawn fails. Path override: `AQUA_SYSTEM_ALLOWLIST` (default `~/.aqua-system-bridge/allowlist.toml`).

**New failure mode (fail-closed):** when the Owner cannot be ensured (file missing, unreadable or
already invalid, `<label>` and `<label>-owner` both taken, generic operator absent, lock not
obtained in 60 s, result would not validate) the spawn prints `!! owner allow-list: …` and
`!! aborting: could not put the Owner … on the Aqua System allow-list` and exits 1 with nothing
changed: no config render, no `--replace` removal, no launch. A fleet roll reports that label as
failed and moves on. Fix the file (a bad file also silences the bridge itself) or free a name, then
re-run. `--print-run` and `--print-onboarding` only preview (`would add …` / `already present`) and
never write.

**The welcome (Tim, 2026-09-29).** The peer's first contact is the consultant's OWN first message:
the config's `hello`, which the relay sends into the DM room it creates (`initiate_dm`), rendered as
Markdown. On first contact the agent appends its "What's new" list (`"\n\n" + WHATS_NEW`, in
aqua-agents claude-p). There is **no** separate Aqua System onboarding DM to the peer (a second
invite from a second unknown sender had no job left). With `--persona`, spawn renders the hello
from `consultant-persona.py hello_for(persona, person, voice)`: who she is (an AI assistant, just
for you), Aqua in one sentence, "Ask me anything" with five example questions, "Good to know"
(one-to-one, the voice line, explains and cites but cannot act, can be wrong), and a closing
question. Pseudonymous peers (no `--name`) get `Hi there!` and the closing
`Before we start, what should I call you?`. The line "You can type, or send me a voice message."
appears only when the config's FINAL `voice.enabled` is true: a render resolves `--voice` first,
else the base config (existing config, else template), and the `--voice` step leaves the same value
in the file. `--refresh-prompt` re-renders the hello to the canonical text with the config's voice
state. `--keep-config` keeps the hello byte-identical (also with `--voice`), so existing
consultants keep their old text; `derive()` reads persona and person from the old and the new text
alike (`Hi <name>! ` / `Hi there! ` prefix, which must stay). Without `--persona` (legacy) the
template's hello is used unchanged.

**Onboarding (`--onboard`) confirms delivery to the operator.** After launch, spawn waits up to
`ONBOARD_WAIT` s (default 180) for the agent's greeted marker: `.whats_new_seen` in the config's
`memory.config_dir` (`/agent/memory`, bind-mounted from
`~/.aqua-matrix-test/<label>-aqua-consultant-persist/memory/`), which claude-p writes only after the
relay confirmed the hello send (`hello_delivered`). It also watches `podman logs` for the relay's
failure lines (`initiate-DM hello failed (retries next process start)`, `no DM room yet; deferring
hello`, `hello send failed`); one is printed on the terminal at once, but the wait continues, since
a crash restart retries. The operator then gets ONE DM:

| Outcome | Level | Title | Body |
|---|---|---|---|
| marker appeared | INFO | `welcome delivered: <name or persona> (<container>)` | "✅ <persona> invited <name> and posted her welcome." + "This is what they see:" + the config's hello quoted line by line + `> *(followed by the "What's new" list)*` |
| not confirmed within the wait | WARN | `welcome NOT confirmed: …` | "⚠️ <persona>'s welcome to <name> was not confirmed within <N> s." + the last relay failure line, if any + "It retries on the next process start: podman restart <container>." |
| marker existed before launch (roll of an already-greeted consultant) | none | (no DM) | spawn prints `>> onboarding: <name> was already greeted earlier; nothing to send` |

A config with no `hello`, or a `memory.config_dir` outside `/agent/memory`, gets the WARN right
away with that reason. Nothing here ever fails the spawn, and the "channel up" DM is unchanged.
On a `--keep-config` spawn without `--persona`/`--name`, the notices take both from the kept config.
Review the copy offline first (no token, network, config write, container or DM involved):

```bash
bash ~/spawn-consultant.sh --print-onboarding --label gawain --target '@…:matrix.inblock.io' \
  --persona Talia --name Gawain --voice on
```

It prints the hello exactly as this spawn would leave it in the config (`--keep-config`: verbatim)
plus both operator notices, and previews the Owner step on stderr. The voice line follows `--voice`
when given, else the existing config, else the template.

**MXIDs are never derived from DIDs (2026-09-27).** siwx-oidc gives every NEW DID an opaque
localpart (16 base36 chars, e.g. `@1vo8g4vofiha69ua:matrix.inblock.io`) and keeps existing
accounts on their legacy `@did-key-…` / `@did-pkh-…` localpart forever, so only the server knows
which applies. `--target` therefore takes either the peer's **MXID** (from their profile) or
their **DID**, which spawn resolves via siwx-oidc `GET /resolve?did=` and prints
(`>> --target did:… resolved via …/resolve -> @…`). If the lookup is unavailable (prod siwx-oidc
older than c5ed83b answers 404) the spawn **fails** rather than guessing the legacy form: pass the
MXID instead. The agent's OWN MXID is read back from its persisted session after first login
(`>> agent MXID (from its persisted session): @…`), or later with
`bash ~/spawn-consultant.sh --print-mxid --label <label>`. For a dev deployment add
`--siwx-url https://dev.siwx.inblock.io --matrix-url https://dev.matrix.inblock.io`.

**Before spawning, ALWAYS diff the peer's MXID against existing configs** — a DID Tim gives
with a fresh human name may already have a consultant (this is exactly how "Aubert" turned out to be
the existing `zdnaez` peer). Diff the resolved MXID, not a DID fragment: an opaque MXID contains
nothing of the DID.

```bash
grep -l 'THE_PEER_MXID_LOCALPART' ~/.aqua-matrix-test/*-config.json   # any hit = already deployed
```

Then add the consultant to the live registry so the fleet roller includes it (the peer MXID
exactly as spawn printed it):
```bash
printf 'gawain\t@<peer-localpart>:matrix.inblock.io\tAqua Consultant (Gawain)\n' >> ~/.aqua-matrix-test/consultants.registry
```

## Relabel / re-point an existing consultant

Change the Matrix display name (or rebind the target) while **preserving the DID, memory, and any
hand-customized config** (the render *merges* onto the existing config, overriding only
id/target/display). Uses `--replace` (rm -f + re-run; DID survives via the persist volume):

```bash
bash ~/spawn-consultant.sh --replace \
  --label zdnaez \
  --target '@<peer-localpart>:matrix.inblock.io' \
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

**The `host` block (per consultant, opt-in).** Settings that only this spawn script uses live
in ONE `host` object in the consultant JSON:

```json
"host": { "extra_refs": ["aqua-mail", "siwx-oidc"], "refs_follow": ["aqua-rs-sdk"] }
```

The agent binary accepts the object and ignores its contents (IMAGE BEFORE CONFIG: only an image
that knows the `host` field may run a config that carries it, `deny_unknown_fields`). The script
reads it from the config the spawn uses (the kept config, or the render's base: the existing
config, else the template) and validates it strictly before any side effect (Owner step, config
render, clone, container): `host` must be an object, its only keys are `extra_refs` and
`refs_follow`, each a list of plain repo names (`[A-Za-z0-9._-]`, not `.`, no `..`, not the
reserved `_developments`). Anything else exits 2 with nothing changed. Persona render,
`--keep-config` (including its `avatar_path` patch and `--voice`) and `--refresh-prompt` carry
`host`, `invite_policy` and `rooms` through byte for byte. No `host` = today's argv, byte for byte.

**Extra refs (`host.extra_refs`).** `REFS_REPOS` reaches every consultant. A repo only ONE
consultant may see (private ones included) goes in its `host.extra_refs`: it is mounted `:ro` at
`/refs/<repo>` into that consultant only, from a clean single-branch clone of the default branch
under `${CONSULTANT_REFS_MIRROR:-~/.local/share/consultant-refs}` (never a working checkout,
never /tmp). Unlike the fleet pass this is fail-closed: a failed clone/fetch, a non-fast-forward
or any local change in a mirror (ignored files included) aborts the spawn before the container is
touched. A fleet repo or a followed repo in the list is skipped with a note. The former host file
`<test-dir>/<key>-extra-refs.list` is NOT read any more (no fallback): move its names into
`host.extra_refs` before the next spawn of that consultant, or they silently drop out of its
`/refs`. These mounts are NOT in `ref_mounts` and NOT in the system prompt; the agent reads them
through its unscoped `Read`/`Glob`/`Grep`, so tell it about them in its own config if it should
use them.

**Followed refs (`host.refs_follow`).** A followed repo is mounted `:ro` at `/refs/<repo>` from
its FOLLOW mirror `<follow>/<repo>`, IN PLACE of the fleet mount for that repo (a repo outside
`REFS_REPOS` is simply added), for that consultant only; everyone else keeps the fleet checkout.
Its digest dir `<follow>/_developments/<repo>` (holding `DEVELOPMENTS.md`: latest tags, main of
the last 30 days, open PRs) is mounted `:ro` at `/refs/_developments/<repo>`. Both are directory
mounts, never a single-file bind mount (a file replaced by rename would go stale in the
container). `<follow>` = `${CONSULTANT_REFS_FOLLOW_ROOT:-~/.local/share/consultant-refs-follow}`.
The follow mirrors and digests come from the host timer `consultant-refs-follow.timer`
(inblockio/aqua-ops, every 30 min, `Persistent=true`), which reads every live config's
`host.refs_follow`, fast-forwards each mirror to the upstream default branch and refuses a dirty
one. The spawn script NEVER clones, fetches or updates them; it only checks (read-only, with
`GIT_OPTIONAL_LOCKS=0` so it never races the timer for the index lock) that each mirror is a
clean git clone and its digest dir exists. Missing or dirty: exit 1 before anything changes, with
`run: systemctl --user start consultant-refs-follow.service`.

**Rooms.** A `<test-dir>/<key>-rooms/` dir is mounted `:ro` at `/agent/rooms`, together with a
durable, writable `<test-dir>/<key>-room-state/` (created if missing, never wiped) at
`/agent/room-state` with the same `:U` as `/agent/memory`. No rooms dir = no rooms mounts.

## Identity & lifecycle flags

| Flag | Effect |
|---|---|
| *(none)* | New container; refuses if one already exists. Fresh DID self-minted on first connect. |
| `--generic` | Select the **un-labeled** operator-bound consultant instead of a `--label` one (container `aqua-agent-aqua-consultant-1`, config/persist without the `<label>-` prefix). Identical behaviour except no activity watcher is wired (the peer IS the operator). Mutually exclusive with `--label`; the registry label `generic` is reserved to map here. |
| `--replace` | `podman rm -f` + re-run, **reusing persist** → DID + memory PRESERVED (the image-roll path). |
| `--keep-config` | Use the existing config verbatim (no re-render); derives id/target/display from it. |
| `--fresh` | Wipe the persist dir first → brand-new identity + empty memory. (Rejected with `--keep-config`.) |
| `--onboard` | After launch, wait (`ONBOARD_WAIT`, default 180 s) for the consultant's own welcome to be delivered (greeted marker) and DM the operator "welcome delivered" (INFO, hello quoted) or "welcome NOT confirmed" (WARN, last relay failure line). Nothing when the peer was greeted before. Never fails the spawn. See "Onboarding". |
| `--print-onboarding` | Print the consultant's hello as this spawn would render it and both operator notices to stdout and exit 0 before any token, network, config write, container or DM; previews the Owner allow-list step on stderr. |
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
`REFS_REPOS` and mounted `:ro`; the template pins `model`, a fresh render carries it, and a
persona re-render or `--refresh-prompt` keeps an existing config's `model` (and never injects one);
the `host` block is validated strictly (unknown key, non-object `host`, non-list value, every bad or
reserved name: exit 2, config byte-identical, no persist dir, no argv), `host.extra_refs` mounts
from the mirrors and the old `<key>-extra-refs.list` is ignored, `host.refs_follow` puts the follow
mirror IN PLACE of the fleet mount (exact `/refs` sequence) plus one `/refs/_developments/<repo>`
directory mount, and a missing, non-clone or dirty follow mirror or a missing digest dir exits 1
with the service hint and nothing changed; `host`, `invite_policy` and `rooms` keep their exact bytes
through a persona render, the `avatar_path` patch, `--voice` and `--refresh-prompt`; the shims were
never called.

`tests/spawn-consultant-onboarding.sh` covers the welcome and `--onboard`: `hello_for` copy
(persona + name exact, pseudonymous "Hi there" ending with the name question, voice line on/off,
no U+2014/U+2013 dash), `derive()` on the new and the old hello texts, the hello's voice line
following the final `voice.enabled` on a render, `--keep-config` leaving the hello byte-identical
(also with `--voice on`), `--refresh-prompt` re-rendering it; `--print-onboarding` for persona +
name, pseudonymous, legacy and `--keep-config` (hello verbatim, persona/person derived for the
notices), with no side effects; whole `--onboard` spawns with podman shimmed (the shim writes the
greeted marker or serves canned relay logs) and a recording notifier: marker appears = INFO
"welcome delivered" quoting the config's hello, relay failure line = WARN carrying the line
(ANSI stripped), plain timeout = WARN, marker already there = no onboarding DM, exit 0 throughout;
and that no script references the bridge MCP or the removed direct send. The Owner step: added
when missing (name, note, mode 600, one backup), no-op when present (also by case),
`<label>-owner` on a clash, abort before any podman call (also with `--replace`) when both names
are taken or the file is invalid or missing, print modes write nothing, generic never auto-added,
concurrent applies serialize. No message leaves the machine and the real allow-list is never read
or written. `spawn-consultant-args.sh` hashes configs with the hello's voice line removed, since
that line now legitimately follows `--voice`.

```bash
bash Skills/consultant-deploy/tests/spawn-consultant-args.sh
bash Skills/consultant-deploy/tests/spawn-consultant-onboarding.sh
```

The sandbox relies on the spawner's env overrides: `CONSULTANT_TEST_DIR` (configs, persist,
avatars, template; default `~/.aqua-matrix-test`), `CONSULTANT_TEMPLATE`, `CONSULTANT_REFS_BASE`,
`CONSULTANT_REFS_MIRROR`, `CONSULTANT_REFS_REMOTE`, `CONSULTANT_REFS_FOLLOW_ROOT`, `CONSULTANT_IMAGE`, `AQUA_CLAUDE_TOKEN_FILE`, `AQUA_DEEPGRAM_ENV`, plus for onboarding
`CONSULTANT_NOTIFY`, `ONBOARD_WAIT` and `AQUA_SYSTEM_ALLOWLIST`.

## Model pin (`model`)

The template sets `"model": "claude-opus-5-5"`, so every NEW spawn is pinned: the relay passes
`--model <value>` to every `claude -p` run (conversational, plan, resume). Absent means no flag and
the CLI default applies. Existing configs are pinned by an explicit in-place JSON edit (back up
first, never `mv` over the bind-mounted file), then a `--replace --keep-config` roll. A persona
re-render and `--refresh-prompt` both keep whatever `model` a config already has and never inject
one; `--keep-config` uses the config verbatim.

**Image before config.** `model` is an unknown field to images built before aqua-agents PR #29
(`deny_unknown_fields`), and a consultant whose config carries it on such an image crash-loops.
Order: build `:poc` from a main that includes the field, then add `model` to configs, then roll.
Rollback is the reverse: remove `model` from the configs FIRST, then roll back to the older image.
`CONSULTANT_IMAGE` (default `localhost/aqua-matrix-agent:poc`) selects the image for a canary;
because the template now carries `model`, never render a fresh config with `CONSULTANT_IMAGE`
pointing at a pre-#29 image.

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
and `display name set to "<your display>"`. The consultant invites the peer itself
(`initiate_dm`); its own MXID is printed by spawn and by
`bash ~/spawn-consultant.sh --print-mxid --label <label>` (read from `[session] user_id` in
`<persist>/store/config.toml`, never derived from the `agent DID:` log line: a new agent's
localpart is opaque).

## Related

- Memory: `consultant-fleet-recreate` (procedure + legacy per-script notes), `gawain-aqua-consultant`,
  `zdnaez-aqua-consultant` (= "Aubert"), `notify-tim-channel`, `relay-mxid-auth-case`.
- Skills: [`claude-channel`](../claude-channel/skill.md) (the backend daemon), [`heartbeat`](../heartbeat/skill.md).
