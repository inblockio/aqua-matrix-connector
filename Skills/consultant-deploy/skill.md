---
name: consultant-deploy
description: Deploy, relabel, and image-roll dedicated single-target Aqua consultant containers via the generic spawn-consultant.sh + registry + roll-consultant-fleet.sh (replaces per-consultant byte-cloned scripts).
---

# Aqua Consultant Deployment

Deploy and maintain **dedicated, single-target Aqua consultants** — one container per peer, each
running `localhost/aqua-matrix-agent:poc` (the `aqua-matrix-claude-p` backend), bound to exactly
ONE peer MXID by the relay firewall (`authorize(sender == target)` in
[`crates/aqua-matrix-relay`](../../crates/aqua-matrix-relay/src/lib.rs)). Each consultant is a
read-only Aqua-protocol assistant grounded in the `/refs` repos.

This skill replaces the old approach of hand-cloning a `recreate-<label>-consultant.sh` per peer.
One generic launcher does it all; a registry + roller image-upgrades the whole fleet.

## Canonical source vs. host copies

The generic tooling is **canonical in this skill directory** and version-controlled here; the host
runs it via symlinks so there is a single source of truth (no drift):

| Canonical (in repo) | Host path (symlink → canonical) | Role |
|---|---|---|
| `Skills/consultant-deploy/spawn-consultant.sh` | `~/spawn-consultant.sh` | generic launcher |
| `Skills/consultant-deploy/roll-consultant-fleet.sh` | `~/roll-consultant-fleet.sh` | fleet image-roller |
| `Skills/consultant-deploy/consultant-config.template.json` | `~/.aqua-matrix-test/consultant-config.template.json` | config template |
| `Skills/consultant-deploy/consultants.registry.example` | *(host state — not symlinked)* | format reference |

**Host state stays on the host** (not in the repo): the live registry
`~/.aqua-matrix-test/consultants.registry`, and every consultant's `<label>-aqua-consultant-config.json`
+ `<label>-aqua-consultant-persist/{store,memory}` under `~/.aqua-matrix-test/`. Persist holds the
self-minted DID (`store/agent.pem`) + memory — it is the identity, preserved across re-runs.

Edit the scripts **here in the repo**; the host symlinks pick the change up immediately.

## Prerequisites

- Image built: `bash ~/aqua-agents/scripts/build-image.sh` (stages the 4 sibling repos + host `claude`, retags `:poc`).
- OAuth token at `~/.aqua-matrix-heartbeat/claude-oauth-token` (passed **by reference** — never on a command line).
- `~/.aqua-matrix-notify/notify-tim.sh` (operator DMs) and `target/debug/aqua-activity-watch` (built) for the watcher.

## Add a new consultant

One command. Renders the config from the template, launches with the hardened podman flags, wires
a systemd activity-watcher, and (`--onboard`) DMs the operator a forward-ready onboarding message
carrying the agent's self-minted MXID for the peer to DM:

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
survive. The two Tim-bound containers are intentionally excluded.

```bash
bash ~/roll-consultant-fleet.sh --dry-run     # preview
bash ~/roll-consultant-fleet.sh --build        # rebuild image, then roll all
```

## Identity & lifecycle flags

| Flag | Effect |
|---|---|
| *(none)* | New container; refuses if one already exists. Fresh DID self-minted on first connect. |
| `--replace` | `podman rm -f` + re-run, **reusing persist** → DID + memory PRESERVED (the image-roll path). |
| `--keep-config` | Use the existing config verbatim (no re-render); derives id/target/display from it. |
| `--fresh` | Wipe the persist dir first → brand-new identity + empty memory. (Rejected with `--keep-config`.) |
| `--onboard` | After connect, derive the agent MXID from logs and DM the operator a forward-ready onboarding message. |

## Invariants the tooling enforces (verified by adversarial audit, 2026-06-08)

- **Security posture is byte-identical** to the proven original `recreate-zdnaez-consultant.sh`:
  `--restart on-failure`, `--memory 2048m --cpus 2 --pids-limit 512`, `--cap-drop ALL`,
  `--security-opt no-new-privileges`, `--tmpfs /tmp`, config + `/refs` mounted `:ro`, persist
  store/memory mounted `:U`, OAuth token passed **by reference** (`-e CLAUDE_CODE_OAUTH_TOKEN`, no value).
  Keep it that way — verify after any edit:
  ```bash
  diff <(grep -E '^\s*(--restart|--memory|--cpus|--pids-limit|--cap-drop|--security-opt|--tmpfs|-e |-v )' ~/recreate-zdnaez-consultant.sh) \
       <(grep -E '^\s*(--restart|--memory|--cpus|--pids-limit|--cap-drop|--security-opt|--tmpfs|-e |-v )' ~/spawn-consultant.sh)
  ```
- **Single-target binding** — the relay matches one exact target; never widen a consultant to >1 peer.
- **Guards**: placeholder/non-MXID target rejected; `--display` rejects quote/newline (systemd-unit
  injection); registry parser skips indented comments and rejects non-slug labels; `--fresh` asserts
  the persist-path prefix before any `rm -rf`.

## Verify a deployment

```bash
podman ps --filter name=aqua-agent-<label>            # Up, restarts 0
podman logs aqua-agent-<label>-aqua-consultant-1 | grep -E 'agent DID|connected|display name set|daemon starting'
systemctl --user is-active aqua-activity-watch-<label>.service
```

Healthy = `daemon starting (target: <the one peer>)`, `connected store_wiped=false` (for `--replace`),
and `display name set to "<your display>"`. Then the peer DMs the agent's `@did-key-…:matrix.inblock.io`
MXID (shown in the onboarding message / derivable from the `agent DID:` log line, lowercased).

## Related

- Memory: `consultant-fleet-recreate` (procedure + legacy per-script notes), `gawain-aqua-consultant`,
  `zdnaez-aqua-consultant` (= "Aubert"), `notify-tim-channel`, `relay-mxid-auth-case`.
- Skills: [`claude-channel`](../claude-channel/skill.md) (the backend daemon), [`heartbeat`](../heartbeat/skill.md).
