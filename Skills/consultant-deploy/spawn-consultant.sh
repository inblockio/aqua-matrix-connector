#!/usr/bin/env bash
#
# spawn-consultant.sh — generic, parameterized launcher for a DEDICATED, single-target
# Aqua consultant. This REPLACES the hand-cloned recreate-<label>-consultant.sh scripts:
# one script, three required parameters (--label / --target / --display). It renders the
# per-instance config from ~/.aqua-matrix-test/consultant-config.template.json, launches
# the container with the SAME hardened podman flags as the original scripts (verbatim —
# the security posture must not drift), wires the systemd activity-watcher, DMs Tim
# "channel up", and (with --onboard) DMs Tim a ready-to-forward onboarding message that
# carries the agent's self-minted MXID for the peer to DM.
#
# The relay firewall (crates/aqua-matrix-relay) gates BOTH message dispatch AND invite
# auto-join on authorize(sender==target), so each instance only joins rooms the named
# peer invites and only replies to that one peer.
#
# Identity model (same as the originals): a FRESH persist volume → the agent self-mints
# a brand-new DID on first connect. A `--replace` (rm -f + re-run) reuses the existing
# persist volume, so the DID + memory are PRESERVED — this is the image-roll path.
# `--fresh` forces a brand-new identity by wiping the persist dir first.
#
# Grounding refs: the repos in REFS_REPOS are mounted read-only at /refs inside the
# container. Before EVERY launch the script verifies each repo exists on the host
# (missing = fatal, with a clone hint) and is on the latest upstream commit:
# clean + behind upstream gets a fast-forward pull; dirty / diverged / offline gets
# a loud warning and is mounted as-is (never destructive). Skip the freshness pass
# (presence stays fatal) with --no-refresh-refs.
#
# A template prompt change reaches EXISTING consultants via --refresh-prompt: it adopts
# the template's system_prompt/description/ref_mounts into the kept config, preserving
# hello/homeserver customizations and identity. Fleet-wide:
#   roll-consultant-fleet.sh --refresh-prompt
#
# The UN-LABELED generic consultant (container aqua-agent-aqua-consultant-1, config
# aqua-consultant-config.json, persist aqua-consultant-persist — no <label>- prefix,
# bound to the operator's own MXID) is selected with --generic instead of --label.
# It behaves identically except that no activity watcher is wired (its peer IS the
# operator — no self-notification). The registry label `generic` is RESERVED for it,
# so roll-consultant-fleet.sh rolls it with the rest of the fleet.
#
# Examples:
#   # new consultant (fresh identity), and DM Tim an onboarding message to forward:
#   bash ~/spawn-consultant.sh --label gawain \
#        --target '@did-key-…:matrix.inblock.io' \
#        --display 'Aqua Consultant (Gawain)' --onboard --name Gawain
#
#   # with a per-consultant avatar (mounted ro at /agent/avatar.png; set on first connect):
#   bash ~/spawn-consultant.sh --label gawain --target '@did-key-…:matrix.inblock.io' \
#        --display 'Aqua Consultant (Gawain)' --avatar ~/.aqua-matrix-test/gawain-avatar.png
#
#   # relabel / re-point an EXISTING consultant (DID + memory + bespoke config preserved —
#   # the render MERGES onto the existing config, overriding only target/display):
#   bash ~/spawn-consultant.sh --replace --label zdnaez \
#        --target '@did-key-…:matrix.inblock.io' --display 'Aqua Consultant (Aubert)'
#
#   # image roll (config used VERBATIM, nothing re-rendered) — used by roll-consultant-fleet.sh:
#   bash ~/spawn-consultant.sh --replace --keep-config --label zdnaez
#
#   # same image roll for the un-labeled generic consultant (operator-bound):
#   bash ~/spawn-consultant.sh --replace --keep-config --generic
#
set -euo pipefail

# ---------------------------------------------------------------- defaults / args
LABEL=""
TARGET=""
DISPLAY_NAME=""
AVATAR=""             # optional avatar image; mounted ro at /agent/avatar.png + rendered into avatar_path
ID=""                 # defaults to <label>-aqua-consultant-1
HUMAN_NAME=""         # friendly name for the onboarding message (defaults to display)
GENERIC=0             # target the UN-LABELED generic consultant instead of a --label one
REPLACE=0             # rm -f an existing container first (image roll; DID preserved)
FRESH=0               # wipe persist dir first (force a brand-new identity)
ONBOARD=0             # after connect, DM Tim a forward-ready onboarding message
KEEP_CONFIG=0         # reuse the existing config verbatim (image-roll; never clobber customizations)
REFRESH_REFS=1        # fast-forward the /refs repos before launch (--no-refresh-refs to skip)
REFRESH_PROMPT=0      # adopt the template's system_prompt/description/ref_mounts into the config
TEMPLATE="${CONSULTANT_TEMPLATE:-/home/waldknoten-01/.aqua-matrix-test/consultant-config.template.json}"
IMAGE="${CONSULTANT_IMAGE:-localhost/aqua-matrix-agent:poc}"
REFS_BASE="${CONSULTANT_REFS_BASE:-/home/waldknoten-01}"
# Grounding repos, mounted ro at /refs/<name>. This ONE list drives the presence check,
# the freshness pass, and the podman -v flags, so the three can never drift apart.
# Keep it in sync with ref_mounts in consultant-config.template.json.
REFS_REPOS=(aqua-rs-sdk aqua-spec aqua-governance-corpus aqua-ecosystem)

usage() { sed -n '2,56p' "$0"; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --label)   LABEL="$2"; shift 2 ;;
    --target)  TARGET="$2"; shift 2 ;;
    --display) DISPLAY_NAME="$2"; shift 2 ;;
    --avatar)  AVATAR="$2"; shift 2 ;;
    --id)      ID="$2"; shift 2 ;;
    --name)    HUMAN_NAME="$2"; shift 2 ;;
    --template) TEMPLATE="$2"; shift 2 ;;
    --image)   IMAGE="$2"; shift 2 ;;
    --generic) GENERIC=1; shift ;;
    --replace) REPLACE=1; shift ;;
    --fresh)   FRESH=1; shift ;;
    --onboard) ONBOARD=1; shift ;;
    --keep-config) KEEP_CONFIG=1; shift ;;
    --no-refresh-refs) REFRESH_REFS=0; shift ;;
    --refresh-prompt) REFRESH_PROMPT=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "!! unknown arg: $1" >&2; usage 1 ;;
  esac
done

# Contradictory combo: --fresh wipes the DID + memory that --keep-config means to preserve.
if [ "$FRESH" -eq 1 ] && [ "$KEEP_CONFIG" -eq 1 ]; then
  echo "!! --fresh and --keep-config are contradictory (--fresh wipes the identity/memory --keep-config preserves)." >&2
  exit 2
fi

# ---------------------------------------------------------------- validate label + paths
# STEM is the shared name fragment: container aqua-agent-<STEM>-1, config <STEM>-config.json,
# persist <STEM>-persist, default id <STEM>-1. Labeled consultants use <label>-aqua-consultant;
# the un-labeled generic one (--generic) uses plain aqua-consultant — no other difference.
if [ "$GENERIC" -eq 1 ]; then
  [ -z "$LABEL" ] || { echo "!! --generic and --label are mutually exclusive" >&2; exit 2; }
  STEM="aqua-consultant"
else
  [ -n "$LABEL" ] || { echo "!! --label is required (e.g. gawain), or --generic for the un-labeled consultant" >&2; exit 2; }
  # label must be a safe slug (used in container name + paths + systemd unit)
  case "$LABEL" in
    *[!a-z0-9-]*|"") echo "!! --label must be lowercase [a-z0-9-]: '$LABEL'" >&2; exit 2 ;;
    generic) echo "!! the label 'generic' is reserved for the un-labeled consultant — use --generic" >&2; exit 2 ;;
  esac
  STEM="${LABEL}-aqua-consultant"
fi

NAME="aqua-agent-${STEM}-1"
CFG="/home/waldknoten-01/.aqua-matrix-test/${STEM}-config.json"
PERSIST="/home/waldknoten-01/.aqua-matrix-test/${STEM}-persist"
STORE="$PERSIST/store"
MEM="$PERSIST/memory"

# ---------------------------------------------------------------- resolve id/target/display
if [ "$KEEP_CONFIG" -eq 1 ]; then
  # Image-roll path: the EXISTING config is authoritative. Derive id/target/display from it
  # so we never clobber a hand-customized config (e.g. carlotta's hello, gary's homeserver).
  [ -f "$CFG" ] || { echo "!! --keep-config but no existing config at $CFG" >&2; exit 2; }
  eval "$(python3 - "$CFG" <<'PY'
import json, sys, shlex
c = json.load(open(sys.argv[1]))
print("CFG_ID=" + shlex.quote(c.get("id", "")))
print("CFG_TARGET=" + shlex.quote(c.get("target", "")))
print("CFG_DISPLAY=" + shlex.quote(c.get("display_name", "")))
PY
)"
  # The config wins, but a mismatch against an explicitly-passed value is worth a shout.
  [ -n "$TARGET" ] && [ "$TARGET" != "$CFG_TARGET" ] && echo "!! warn: --target '$TARGET' != config '$CFG_TARGET' — using config" >&2
  [ -n "$DISPLAY_NAME" ] && [ "$DISPLAY_NAME" != "$CFG_DISPLAY" ] && echo "!! warn: --display '$DISPLAY_NAME' != config '$CFG_DISPLAY' — using config" >&2
  ID="$CFG_ID"; TARGET="$CFG_TARGET"; DISPLAY_NAME="$CFG_DISPLAY"
else
  [ -n "$TARGET" ] || { echo "!! --target is required (peer MXID)" >&2; exit 2; }
  [ -n "$DISPLAY_NAME" ] || { echo "!! --display is required (e.g. 'Aqua Consultant (Gawain)')" >&2; exit 2; }
  [ -f "$TEMPLATE" ] || { echo "!! missing config template $TEMPLATE" >&2; exit 2; }
fi
: "${ID:=${STEM}-1}"
: "${HUMAN_NAME:=$DISPLAY_NAME}"

# GUARD: refuse a misbound instance. Anchored to the template sentinel (not an uppercase
# substring blocklist) so legit human localparts are never false-rejected.
case "$TARGET" in
  *__TARGET__*|'')
    echo "!! TARGET is still the unfilled template placeholder ($TARGET)." >&2; exit 1 ;;
esac
case "$TARGET" in
  @*:*) : ;;  # looks like @localpart:homeserver
  *) echo "!! TARGET '$TARGET' does not look like a Matrix MXID (@local:server)" >&2; exit 1 ;;
esac
# DISPLAY_NAME is interpolated into the systemd unit (Description + --display-label); a quote or
# newline would mis-tokenize ExecStart / inject unit directives. Reject them (no legit name needs them).
case "$DISPLAY_NAME" in
  *'"'*|*$'\n'*)
    echo "!! --display must not contain a double-quote or newline (would break the systemd unit)." >&2; exit 2 ;;
esac

# ---------------------------------------------------------------- token (by reference)
TOKEN_FILE="${AQUA_CLAUDE_TOKEN_FILE:-/home/waldknoten-01/.aqua-matrix-heartbeat/claude-oauth-token}"
if [ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ] && [ -s "$TOKEN_FILE" ]; then
  CLAUDE_CODE_OAUTH_TOKEN="$(tr -d '[:space:]' < "$TOKEN_FILE")"
  export CLAUDE_CODE_OAUTH_TOKEN
fi
: "${CLAUDE_CODE_OAUTH_TOKEN:?set CLAUDE_CODE_OAUTH_TOKEN, or populate $TOKEN_FILE via:  claude setup-token}"

export XDG_RUNTIME_DIR="/run/user/$(id -u)"

# ---------------------------------------------------------------- refs grounding
# The consultant's knowledge is the host checkouts in REFS_REPOS, bind-mounted ro at
# /refs. A missing repo is FATAL (the mount would fail, or silently ground the agent
# in nothing). The freshness pass brings each repo to the latest upstream commit when
# that is safe (clean tree, fast-forward only) and warns loudly otherwise; it never
# rewrites local work. Network failure is non-fatal: an offline host mounts as-is.
refresh_refs() {
  local rc=0 repo dir behind ahead
  for repo in "${REFS_REPOS[@]}"; do
    dir="$REFS_BASE/$repo"
    if [ ! -d "$dir" ]; then
      echo "!! refs repo MISSING: $dir" >&2
      echo "   clone it first:  git clone https://github.com/inblockio/${repo}.git $dir" >&2
      rc=1; continue
    fi
    if [ "$REFRESH_REFS" -eq 0 ]; then
      echo ">> refs: $repo present (freshness pass skipped via --no-refresh-refs)"
      continue
    fi
    if ! git -C "$dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
      echo "!! refs: $dir is not a git checkout; cannot verify freshness, mounting as-is" >&2
      continue
    fi
    if ! git -C "$dir" fetch --quiet 2>/dev/null; then
      echo "!! refs: fetch failed for $repo (offline?); mounting the checkout as-is" >&2
      continue
    fi
    if ! git -C "$dir" rev-parse --abbrev-ref '@{upstream}' >/dev/null 2>&1; then
      echo "!! refs: $repo has no upstream configured; cannot verify freshness, mounting as-is" >&2
      continue
    fi
    behind="$(git -C "$dir" rev-list --count 'HEAD..@{upstream}')"
    ahead="$(git -C "$dir" rev-list --count '@{upstream}..HEAD')"
    if [ "$behind" -eq 0 ]; then
      echo ">> refs: $repo up to date ($(git -C "$dir" rev-parse --short HEAD))"
    elif [ "$ahead" -gt 0 ]; then
      echo "!! refs: $repo DIVERGED from upstream ($ahead ahead / $behind behind); resolve manually, mounting as-is" >&2
    elif [ -n "$(git -C "$dir" status --porcelain)" ]; then
      echo "!! refs: $repo is $behind commit(s) behind upstream but the tree is DIRTY; not pulling, mounting as-is" >&2
    elif git -C "$dir" pull --ff-only --quiet 2>/dev/null; then
      echo ">> refs: $repo fast-forwarded $behind commit(s) to $(git -C "$dir" rev-parse --short HEAD)"
    else
      echo "!! refs: fast-forward pull failed for $repo; mounting the checkout as-is" >&2
    fi
  done
  return "$rc"
}
refresh_refs || { echo "!! aborting: missing refs repo(s), see clone hints above" >&2; exit 1; }

REF_MOUNT_ARGS=()
for repo in "${REFS_REPOS[@]}"; do
  REF_MOUNT_ARGS+=( -v "$REFS_BASE/$repo:/refs/$repo:ro" )
done

# Optional avatar: mounted READ-ONLY at a fixed path the agent reads on first
# connect. The image stays a ro mount (never embedded in config); only its
# container path is recorded as avatar_path during the render below.
AVATAR_CONTAINER_PATH="/agent/avatar.png"
AVATAR_MOUNT_ARGS=()
if [ -n "$AVATAR" ]; then
  [ -f "$AVATAR" ] || { echo "!! --avatar file not found: $AVATAR" >&2; exit 2; }
  AVATAR_MOUNT_ARGS+=( -v "$AVATAR:$AVATAR_CONTAINER_PATH:ro" )
fi

# ---------------------------------------------------------------- notify (best effort)
# DM Tim from the host CLI identity (independent of any container), never fatal.
NOTIFY=/home/waldknoten-01/.aqua-matrix-notify/notify-tim.sh
notify() {
  [ -x "$NOTIFY" ] || { echo "notify: $NOTIFY missing/not executable; skipping DM" >&2; return 0; }
  "$NOTIFY" "$@" || echo "notify: DM failed (non-fatal) — see notify.log" >&2
}

# ---------------------------------------------------------------- activity watcher
# Host-level systemd user service: tails this instance's inbound.jsonl and DMs Tim on
# the peer's FIRST message + every 10th. Idempotent; never fatal.
ensure_activity_watch() {
  if [ "$GENERIC" -eq 1 ]; then
    # The generic consultant's peer IS the operator — a watcher would DM Tim about
    # Tim's own messages. Deliberately never wired (also: the unit name is label-derived).
    echo ">> --generic: no activity watcher (peer is the operator; no self-notification)"
    return 0
  fi
  command -v systemctl >/dev/null 2>&1 || { echo "watch: systemctl absent; skipping watcher" >&2; return 0; }
  local activity_log="$MEM/activity/inbound.jsonl"
  local unit="aqua-activity-watch-${LABEL}.service"
  local unit_path="$HOME/.config/systemd/user/$unit"
  mkdir -p "$HOME/.config/systemd/user"
  cat > "$unit_path" <<EOF
[Unit]
Description=Aqua activity watcher — ${DISPLAY_NAME} (DMs Tim on first message + every 10th)
Documentation=https://github.com/inblockio/aqua-matrix-agent
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=10

[Service]
Type=simple
WorkingDirectory=%h/.aqua-matrix-notify
Environment=PATH=%h/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=%h/aqua-matrix-agent/target/debug/aqua-activity-watch \\
  --activity-log ${activity_log} \\
  --label ${LABEL} \\
  --display-label "${DISPLAY_NAME}" \\
  --milestone 10
Restart=always
RestartSec=10s
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF
  systemctl --user daemon-reload 2>/dev/null || true
  systemctl --user enable "$unit" 2>/dev/null || true
  if systemctl --user restart "$unit" 2>/dev/null; then
    echo ">> activity watcher up: $unit (tailing $activity_log)"
  else
    echo "watch: failed to enable/start $unit (non-fatal)" >&2
  fi
}

# ---------------------------------------------------------------- derive agent MXID
# The agent's Matrix localpart is `did-key-<multibase>` lowercased (Synapse lowercases
# localparts). We read the self-minted DID from the container logs and derive the MXID
# the peer must DM. Verified against zdnaez: did:key:z6MkswJK… → @did-key-z6mkswjk…
agent_mxid_from_logs() {
  local did
  did="$(podman logs "$NAME" 2>&1 | grep -oE 'agent DID: did:key:[1-9A-HJ-NP-Za-km-z]+' | tail -n1 | sed 's/^agent DID: did:key://')"
  [ -n "$did" ] || return 1
  printf '@did-key-%s:matrix.inblock.io' "$(printf '%s' "$did" | tr '[:upper:]' '[:lower:]')"
}

# ---------------------------------------------------------------- render config
mkdir -p "$STORE" "$MEM"
if [ "$KEEP_CONFIG" -eq 1 ]; then
  echo ">> --keep-config: using existing $CFG verbatim (id=$ID, display=$DISPLAY_NAME)"
else
  # Base = the EXISTING per-instance config when one is already present (so a re-run / relabel
  # preserves hand-customizations like a bespoke hello), otherwise the generic template. Only
  # id/target/display_name are overridden. To force a clean template render, delete $CFG first.
  BASE="$TEMPLATE"; [ -f "$CFG" ] && BASE="$CFG"
  python3 - "$BASE" "$CFG" "$ID" "$TARGET" "$DISPLAY_NAME" "${AVATAR:+$AVATAR_CONTAINER_PATH}" <<'PY'
import json, sys
base, out, _id, target, display = sys.argv[1:6]
avatar = sys.argv[6] if len(sys.argv) > 6 else ""
cfg = json.load(open(base))
cfg['id'] = _id
cfg['target'] = target
cfg['display_name'] = display
if avatar:
    cfg['avatar_path'] = avatar
with open(out, 'w') as f:
    json.dump(cfg, f, indent=2, ensure_ascii=True)
    f.write('\n')
print(f">> rendered config {out} from base {base}  (id={_id}, display={display!r}, avatar={avatar!r})")
PY
fi

# --refresh-prompt: adopt the template's prompt surface into the (kept or re-rendered)
# config, so a template prompt update can reach existing consultants without clobbering
# their customizations (hello, homeserver, ...). Identity fields are untouched.
if [ "$REFRESH_PROMPT" -eq 1 ]; then
  [ -f "$TEMPLATE" ] || { echo "!! --refresh-prompt: missing template $TEMPLATE" >&2; exit 2; }
  python3 - "$TEMPLATE" "$CFG" <<'PY'
import json, sys
tpl_path, cfg_path = sys.argv[1:3]
tpl = json.load(open(tpl_path))
cfg = json.load(open(cfg_path))
fields = ("system_prompt", "description", "ref_mounts")
changed = [k for k in fields if cfg.get(k) != tpl[k]]
for k in fields:
    cfg[k] = tpl[k]
with open(cfg_path, "w") as f:
    json.dump(cfg, f, indent=2, ensure_ascii=True)
    f.write("\n")
print(">> --refresh-prompt: adopted template " + (", ".join(changed) if changed else "fields (already current)") + f" into {cfg_path}")
PY
fi

# ---------------------------------------------------------------- container lifecycle
if podman container exists "$NAME"; then
  if [ "$REPLACE" -eq 1 ]; then
    echo ">> --replace: removing existing $NAME (DID preserved via persist volume)"
    podman rm -f "$NAME" >/dev/null
  else
    echo "!! $NAME already exists. Use --replace to roll it (DID preserved)," >&2
    echo "   or 'podman start $NAME' to resume. Refusing to clobber." >&2
    exit 1
  fi
fi

if [ "$FRESH" -eq 1 ]; then
  # PERSIST is STEM-derived and the label half is slug-validated, but assert the expected
  # shape before any rm -rf so a future refactor can never point this at a stray path.
  case "$PERSIST" in
    /home/waldknoten-01/.aqua-matrix-test/*-aqua-consultant-persist) : ;;
    /home/waldknoten-01/.aqua-matrix-test/aqua-consultant-persist) : ;;   # --generic
    *) echo "!! refusing --fresh: unexpected persist path '$PERSIST'" >&2; exit 2 ;;
  esac
  echo ">> --fresh: wiping persist dir for a brand-new identity ($PERSIST)"
  rm -rf -- "$STORE" "$MEM"
  mkdir -p "$STORE" "$MEM"
fi

if [ -s "$STORE/agent.pem" ]; then
  IDENTITY_NOTE="identity PRESERVED (existing DID in persist volume)"
else
  IDENTITY_NOTE="identity FRESH (self-minted DID on first connect)"
fi
echo ">> persist volume ready — $IDENTITY_NOTE"

echo ">> launching $NAME bound single-target to $TARGET"
set +e
CID="$(podman run -d \
  --name "$NAME" \
  --restart on-failure \
  --memory 2048m --cpus 2 --pids-limit 512 \
  --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp \
  -e AGENT_TARGET="$TARGET" \
  -e AGENT_CONFIG_FILE=/agent/config.json \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  -v "$CFG:/agent/config.json:ro" \
  -v "$STORE:/agent/store:U" \
  -v "$MEM:/agent/memory:U" \
  "${REF_MOUNT_ARGS[@]}" \
  "${AVATAR_MOUNT_ARGS[@]}" \
  "$IMAGE" 2>&1)"
run_rc=$?
set -e

if [ "$run_rc" -ne 0 ]; then
  echo "!! podman run failed (rc=$run_rc):" >&2
  printf '%s\n' "$CID" >&2
  notify -s CRITICAL -t "spawn FAILED: $NAME" \
    "podman run rc=$run_rc launching '$NAME' (peer $TARGET) on $(hostname): $(printf '%s' "$CID" | tail -n1)"
  exit "$run_rc"
fi

CID_SHORT="$(printf '%s' "$CID" | tail -n1 | cut -c1-12)"
notify -s INFO -t "channel up: $NAME" \
  "agent '$NAME' launched — single-target peer $TARGET, $IDENTITY_NOTE, container $CID_SHORT, on $(hostname)."

ensure_activity_watch || true

# ---------------------------------------------------------------- onboarding DM
if [ "$ONBOARD" -eq 1 ]; then
  echo ">> --onboard: waiting for agent to self-mint its DID (for the forward-ready MXID)…"
  MXID=""
  for _ in $(seq 1 60); do
    if MXID="$(agent_mxid_from_logs)" && [ -n "$MXID" ]; then break; fi
    MXID=""
    sleep 2
  done
  if [ -n "$MXID" ]; then
    echo ">> agent MXID: $MXID"
    ONBOARD_MSG="$(cat <<EOF
📋 Onboarding for ${HUMAN_NAME} — forward the block below to them.

———
Hi ${HUMAN_NAME}! You now have your own dedicated Aqua Consultant on Matrix.

To start, send a direct message to:
  ${MXID}

It's a read-only assistant that answers questions about the Aqua protocol, spec, and Rust SDK, plus the repository ecosystem around them and the governance principles behind them, grounded in the latest local Aqua sources. On first contact it'll greet you and ask a couple of quick questions (your name, background, what brings you to Aqua, and how deep you want to go), then tailor everything to you. It only explains and cites — it never modifies anything.
———
EOF
)"
    notify -s INFO -t "onboarding: ${HUMAN_NAME} (${NAME})" "$ONBOARD_MSG"
    echo ">> onboarding message DM'd to Tim (forward-ready, carries $MXID)"
  else
    echo "!! could not read agent MXID from logs within timeout — onboarding DM skipped." >&2
    notify -s WARN -t "onboarding pending: ${NAME}" \
      "Spawned '${NAME}' for ${HUMAN_NAME} but could not read its self-minted MXID from logs yet. Re-derive with: podman logs ${NAME} | grep 'agent DID'"
  fi
fi

echo
echo ">> container started: $CID_SHORT"
echo ">> done. verify with:"
echo "   podman logs -f $NAME    # watch it connect + self-mint its DID + set display name"
echo "   # then have the peer DM the agent's @did-key-…:matrix.inblock.io MXID (shown above / in the logs)"
