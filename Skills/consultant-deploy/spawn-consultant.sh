#!/usr/bin/env bash
#
# spawn-consultant.sh, generic, parameterized launcher for a DEDICATED, single-target
# Aqua consultant. This REPLACES the hand-cloned recreate-<label>-consultant.sh scripts:
# one script, three required parameters (--label / --target / --persona). It renders the
# per-instance config from ~/.aqua-matrix-test/consultant-config.template.json, launches
# the container with the SAME hardened podman flags as the original scripts (verbatim -
# the security posture must not drift), wires the systemd activity-watcher, DMs Tim
# "channel up", and (with --onboard) waits for the consultant's own welcome to be delivered and
# tells Tim the outcome (see "Onboarding" below). Since the template sets "initiate_dm": true,
# the consultant herself creates the DM room, invites the peer and delivers her
# greeting (the config's `hello`) into it: that greeting IS the welcome. The peer just accepts
# the invite; no need to DM the MXID first, and no second message from anyone else.
# SEQUENCING: a config carrying initiate_dm needs an image whose binary knows the
# field (deny_unknown_fields) — never point an old image at a freshly rendered config.
#
# The relay firewall (crates/aqua-matrix-relay) gates BOTH message dispatch AND invite
# auto-join on authorize(sender==target), so each instance only joins rooms the named
# peer invites and only replies to that one peer.
#
# Identity model (same as the originals): a FRESH persist volume → the agent self-mints
# a brand-new DID on first connect. A `--replace` (rm -f + re-run) reuses the existing
# persist volume, so the DID + memory are PRESERVED, this is the image-roll path.
# `--fresh` forces a brand-new identity by wiping the persist dir first.
#
# Grounding refs: the repos in REFS_REPOS are mounted read-only at /refs inside the
# container. Before EVERY launch the script verifies each repo exists on the host
# (missing = fatal, with a clone hint) and is on the latest upstream commit:
# clean + behind upstream gets a fast-forward pull; dirty / diverged / offline gets
# a loud warning and is mounted as-is (never destructive). Skip the freshness pass
# (presence stays fatal) with --no-refresh-refs.
#
# Host block (per consultant, opt-in): settings for THIS script live in ONE `host` object in the
# consultant JSON. The agent binary accepts the object and ignores its contents; this script
# validates it STRICTLY before any side effect (Owner step, config render, clone, container):
# `host` must be an object, its only keys are `extra_refs` and `refs_follow`, each a list of plain
# repo names ([A-Za-z0-9._-], not ".", no "..", not the reserved "_developments"). Anything else
# exits 2 with nothing changed. It is read from the config the spawn uses (the kept config, or the
# render's base: the existing config, else the template). No `host` = today's argv, byte for byte.
#   "host": { "extra_refs": ["aqua-mail", "siwx-oidc"], "refs_follow": ["aqua-rs-sdk"] }
#
# Extra refs (host.extra_refs): REFS_REPOS is fleet-wide, every consultant mounts it. A consultant
# that needs MORE repos (private ones included) lists them in host.extra_refs. (This replaces the
# former <test-dir>/<key>-extra-refs.list host file, which is no longer read.) Each listed repo is
# mounted read-only at /refs/<repo> into THAT consultant only, from a CLEAN single-branch clone of
# GitHub's default branch at <mirror>/<repo>, never from a working checkout (untracked .env or
# target/ would ride along). <mirror> = ${CONSULTANT_REFS_MIRROR:-$HOME/.local/share/consultant-refs},
# on disk, never /tmp (RAM-backed tmpfs here). A missing mirror is cloned, a present one
# fast-forwarded. FAIL-CLOSED, unlike the fleet pass: a clone/fetch failure, a non-fast-forward,
# or ANY local change in a mirror (modified, untracked or ignored files) aborts the spawn.
# --no-refresh-refs (and so --print-run) mounts the mirrors as-is: no clone, no fetch, but a
# missing or dirty mirror still aborts. A listed repo that is already in REFS_REPOS is skipped
# with a note (the fleet mount wins), and so is one that is followed (host.refs_follow wins).
#
# Followed refs (host.refs_follow): each listed repo is mounted read-only at /refs/<repo> from
# its FOLLOW mirror <follow>/<repo>, IN PLACE of the fleet mount for that repo (a repo outside
# REFS_REPOS is simply added), for THIS consultant only; every other consultant keeps the fleet
# checkout. Its digest dir <follow>/_developments/<repo> is mounted read-only at
# /refs/_developments/<repo>. Directory mounts only (a single-file bind mount goes stale when the
# file is replaced by rename). <follow> = ${CONSULTANT_REFS_FOLLOW_ROOT:-$HOME/.local/share/consultant-refs-follow}.
# The follow mirrors and digests are owned by the host timer consultant-refs-follow.timer
# (aqua-ops), which keeps them on the upstream default branch; this script NEVER clones, fetches
# or updates them. FAIL-CLOSED: a missing mirror, a missing digest dir, or a dirty mirror (same
# rule as extra refs) aborts the spawn before the container is touched, with
# "run: systemctl --user start consultant-refs-follow.service".
#
# Rooms (per consultant, opt-in): when the directory <test-dir>/<key>-rooms/ exists it is
# mounted read-only at /agent/rooms, and <test-dir>/<key>-room-state/ (created when missing,
# never wiped, not even by --fresh) is mounted WRITABLE at /agent/room-state with the same `:U`
# as /agent/memory: the room turns' own Claude config, transcripts and notes, kept apart from
# the owner's DM memory. No rooms dir = neither mount, no warning.
#
# A template prompt change reaches EXISTING consultants via --refresh-prompt: it adopts
# the template's system_prompt/description/ref_mounts into the kept config, preserving
# hello/homeserver customizations and identity. Fleet-wide:
#   roll-consultant-fleet.sh --refresh-prompt
#
# The UN-LABELED generic consultant (container aqua-agent-aqua-consultant-1, config
# aqua-consultant-config.json, persist aqua-consultant-persist, no <label>- prefix,
# bound to the operator's own MXID) is selected with --generic instead of --label.
# It behaves identically except that no activity watcher is wired (its peer IS the
# operator, no self-notification). The registry label `generic` is RESERVED for it,
# so roll-consultant-fleet.sh rolls it with the rest of the fleet.
#
# Persona: every consultant has a warm FEMALE persona. Pass --persona <Name> and the
# script sets the Matrix display alias "<Name> (Aqua Consultant)", the first-contact greeting
# (consultant-persona.py hello_for; the voice line only when the config's final voice.enabled is
# true) that addresses the served person (--name) by name with no DID/MXID noise, and a
# "# Who You Are" preamble in the system prompt so she introduces herself by name. The
# served person's name is hardcoded per config (the hello placeholder layer only ever
# interpolates the agent's OWN id, never the peer's). --display overrides the derived alias;
# omit --name for a pseudonymous peer (she warmly asks their name during onboarding).
#
# Avatar: each consultant also gets a female profile picture. The script bind-mounts
# <test-dir>/<key>-avatar.jpg (key = the label, or "generic" for the un-labeled one) read-only
# at /agent/avatar.png, which the agent uploads as its Matrix avatar. Override the source with
# --avatar PATH. A missing asset is a non-fatal warning, so a consultant without one still runs.
#
# Voice messages (opt-in, per consultant): the agent's voice-note turn (Deepgram STT + TTS)
# is gated by `voice.enabled` in the config, absent/false by default. Two pieces of plumbing:
#   1. The key. If ${AQUA_DEEPGRAM_ENV:-$HOME/.aqua-secrets/deepgram.env} exists and yields a
#      non-empty DEEPGRAM_API_KEY, it is passed to podman as a bare `-e DEEPGRAM_API_KEY`
#      (same by-reference discipline as the OAuth token: never on a command line, never in
#      `podman inspect`). No file = one notice line, voice stays disabled, nothing else changes.
#   2. The switch. --voice on|off patches ONLY `voice.enabled` in the rendered/kept config
#      (idempotent; sibling voice keys preserved; `off` keeps the block). Without --voice the
#      config is left exactly as it is, so `--replace --keep-config` preserves the block and
#      --refresh-prompt never touches it. IMAGE BEFORE CONFIG: `voice` is an unknown field to
#      images older than the voice feature (deny_unknown_fields), so roll the image first and
#      only then `--voice on`; `--voice off` on a config with no block writes nothing.
#      A config with voice on but no key still launches (loud warning; the agent disables
#      voice at runtime and logs it).
#
# Matrix IDs (2026-09-27): NEVER derive an MXID from a DID by string surgery. siwx-oidc gives
# every NEW DID an opaque localpart (16 base36 chars, e.g. @1vo8g4vofiha69ua:matrix.inblock.io)
# and keeps existing accounts on their legacy `did-...` localpart forever, so only the server
# knows which applies.
#   - Peer: --target takes the peer's MXID, OR their DID (`did:key:...` / `did:pkh:...`), which
#     is resolved through siwx-oidc's public `GET /resolve?did=` (grandfathering honoured). If
#     the lookup is unavailable (e.g. a siwx-oidc older than c5ed83b answers 404) the spawn
#     FAILS; it never guesses the legacy form. Pass the MXID from the peer's profile instead.
#   - Agent: a new agent's MXID is unknown until its first login. The script reads it back
#     from the agent's persisted session (`<persist>/store/config.toml`, [session] user_id, the
#     whoami answer) and prints THAT; --print-mxid prints it for an existing consultant.
#   - --siwx-url / --matrix-url point the agent (and the DID lookup) at another deployment,
#     e.g. dev; unset = the image defaults (prod), with no extra -e flags in the argv.
#
# --print-mxid: print the consultant's own MXID from its persisted session and exit (0 = found,
# 1 = no session yet). Reads only `[session] user_id`; no container, token or target needed.
#
# --print-run: assemble everything, print the `podman run` argument vector one arg per line,
# exit 0. Stops BEFORE any container, systemd unit, DM, --replace removal or --fresh wipe,
# and implies --no-refresh-refs (presence of every refs repo is still checked; extra-refs and
# follow mirrors must also be clean). It DOES render/patch the config file and create the persist
# dirs (and a rooms consultant's room-state dir), since those are the run's
# inputs. Secrets print as bare names (`-e DEEPGRAM_API_KEY`), never as values.
#
# Owner rule (Tim, 2026-09-29): every consultant has exactly ONE authoritative Owner, its single
# target peer (--target, or the kept config's target; --generic = Tim). EVERY real spawn (new,
# --replace, --keep-config, and so every fleet roll) first makes sure the Owner is on the Aqua
# System allow-list (owner-allowlist.py: appends name=<label>, else <label>-owner, when no entry
# has that MXID; flock, backup, validated like the bridge parses it, atomic rename). Fail-closed:
# when that cannot be ensured (file missing/invalid, both names taken, generic operator absent)
# the spawn aborts before any config render, --replace removal or launch. Print modes preview only.
#
# Onboarding (--onboard): the welcome is the consultant's OWN first message (the config's `hello`,
# which the relay sends into the DM room it creates; the agent appends its "What's new" list on
# first contact). There is no separate Aqua System DM to the peer. --onboard only confirms the
# outcome to Tim: after launch it polls (up to ONBOARD_WAIT s, default 180) for the agent's
# greeted marker (<persist>/memory/.whats_new_seen, written only after a confirmed send) and
# watches `podman logs` for the relay's hello failure lines. Delivered -> INFO "welcome
# delivered" with the hello quoted; a failure line or the timeout -> WARN "welcome NOT
# confirmed" (retry: podman restart). A marker that already existed before launch (a roll of an
# already-greeted consultant) -> nothing is sent. Never fails the spawn.
#
# --print-onboarding: print the consultant's hello as a spawn with the same flags would render it,
# plus both Tim notices, to stdout and exit 0, before any token, network, config write, container
# or DM (offline copy review), plus a read-only preview of the Owner allow-list step on stderr.
# The voice line follows --voice when given, else the existing config (else the template).
#
# Env overrides (all optional; the defaults are this host's live paths):
#   CONSULTANT_TEST_DIR     dir holding configs/persist/avatars/template (default ~/.aqua-matrix-test)
#   CONSULTANT_TEMPLATE     config template path (default $CONSULTANT_TEST_DIR/consultant-config.template.json)
#   CONSULTANT_REFS_BASE    parent dir of the REFS_REPOS checkouts (default /home/waldknoten-01)
#   CONSULTANT_REFS_MIRROR  root of the extra-refs mirrors (default $HOME/.local/share/consultant-refs; never /tmp)
#   CONSULTANT_REFS_REMOTE  clone base of the extra refs, <base>/<repo>.git (default https://github.com/inblockio)
#   CONSULTANT_REFS_FOLLOW_ROOT  root of the follow mirrors + _developments digests (default
#                           $HOME/.local/share/consultant-refs-follow; written only by the host timer)
#   CONSULTANT_IMAGE        image to run (default localhost/aqua-matrix-agent:poc)
#   AQUA_CLAUDE_TOKEN_FILE  OAuth token file (default ~/.aqua-matrix-heartbeat/claude-oauth-token)
#   AQUA_DEEPGRAM_ENV       Deepgram env file (default $HOME/.aqua-secrets/deepgram.env)
#   ONBOARD_WAIT            seconds --onboard waits for the welcome to be confirmed (default 180)
#   AQUA_SYSTEM_ALLOWLIST   Aqua System allow-list the Owner step ensures (default ~/.aqua-system-bridge/allowlist.toml)
#   CONSULTANT_NOTIFY       notifier used for Tim's DMs (default ~/.aqua-matrix-notify/notify-tim.sh; tests)
#
# Examples:
#   # new consultant (fresh identity) with a female persona; tell Tim once her welcome is delivered:
#   bash ~/spawn-consultant.sh --label gawain \
#        --target '@<peer-localpart>:matrix.inblock.io' \
#        --persona Talia --name Gawain --onboard
#
#   # same, but the peer is known by DID only (resolved via siwx-oidc /resolve, never derived):
#   bash ~/spawn-consultant.sh --label gawain --target 'did:key:z6Mk…' --persona Talia --name Gawain
#
#   # which MXID does an existing consultant have?
#   bash ~/spawn-consultant.sh --print-mxid --label gawain
#
#   # relabel / re-point an EXISTING consultant (DID + memory + bespoke config preserved -
#   # the render MERGES onto the existing config, overriding target + the persona surface):
#   bash ~/spawn-consultant.sh --replace --label zdnaez \
#        --target '@<peer-localpart>:matrix.inblock.io' --persona Coralie --name Aubert
#
#   # image roll (config used VERBATIM, nothing re-rendered), used by roll-consultant-fleet.sh:
#   bash ~/spawn-consultant.sh --replace --keep-config --label zdnaez
#
#   # same image roll for the un-labeled generic consultant (operator-bound):
#   bash ~/spawn-consultant.sh --replace --keep-config --generic
#
#   # enable voice messages on an existing consultant (image already rolled to a voice-aware build):
#   bash ~/spawn-consultant.sh --replace --keep-config --label zdnaez --voice on
#
#   # review the onboarding copy offline (her hello + both Tim notices), nothing started:
#   bash ~/spawn-consultant.sh --print-onboarding --label andreas --target '@…:matrix.inblock.io' \
#        --persona Pelagia --name Andreas --voice on
#
#   # preview the exact podman argument vector, nothing started:
#   bash ~/spawn-consultant.sh --print-run --label zdnaez --target '@…:matrix.inblock.io' --persona Coralie
#
set -euo pipefail

# ---------------------------------------------------------------- defaults / args
LABEL=""
TARGET=""
DISPLAY_NAME=""
ID=""                 # defaults to <label>-aqua-consultant-1
HUMAN_NAME=""         # the SERVED person's name (greeting + onboarding); empty => pseudonymous
PERSONA=""            # the consultant's FEMALE persona name (e.g. Talia); drives the display
                      # alias "<Name> (Aqua Consultant)" + heartfelt greeting + system-prompt persona
GENERIC=0             # target the UN-LABELED generic consultant instead of a --label one
REPLACE=0             # rm -f an existing container first (image roll; DID preserved)
FRESH=0               # wipe persist dir first (force a brand-new identity)
ONBOARD=0             # after launch, wait for the consultant's welcome to be delivered and tell Tim
PRINT_ONBOARDING=0    # --print-onboarding: print the consultant's hello + both Tim notices and exit 0
KEEP_CONFIG=0         # reuse the existing config verbatim (image-roll; never clobber customizations)
REFRESH_REFS=1        # fast-forward the /refs repos before launch (--no-refresh-refs to skip)
REFRESH_PROMPT=0      # adopt the template's system_prompt/description/ref_mounts into the config
AVATAR=""             # explicit avatar image path; default = <test-dir>/<key>-avatar.jpg (key = label, or "generic")
VOICE=""              # --voice on|off: patch voice.enabled in the config; empty = leave the config untouched
PRINT_RUN=0           # --print-run: print the podman run argument vector and exit 0 before any side effect
PRINT_MXID=0          # --print-mxid: print the consultant's own MXID from its persisted session, exit
SIWX_URL_ARG=""       # --siwx-url: siwx-oidc base URL for the agent + DID lookup; empty = image default (prod)
MATRIX_URL_ARG=""     # --matrix-url: homeserver base URL for the agent; empty = image default (prod)
# Host state dir: per-instance configs, persist volumes, avatars, and the config template.
# Overridable so the arg-rendering tests can run in a sandbox without touching live state.
TEST_DIR="${CONSULTANT_TEST_DIR:-/home/waldknoten-01/.aqua-matrix-test}"
TEMPLATE="${CONSULTANT_TEMPLATE:-$TEST_DIR/consultant-config.template.json}"
IMAGE="${CONSULTANT_IMAGE:-localhost/aqua-matrix-agent:poc}"
REFS_BASE="${CONSULTANT_REFS_BASE:-/home/waldknoten-01}"
# Persona rendering helper, alongside this script (resolve through the ~/ symlink).
PERSONA_HELPER="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)/consultant-persona.py"
# Grounding repos, mounted ro at /refs/<name>. This ONE list drives the presence check,
# the freshness pass, and the podman -v flags, so the three can never drift apart.
# Keep it in sync with ref_mounts in consultant-config.template.json.
REFS_REPOS=(aqua-rs-sdk aqua-spec aqua-governance-corpus aqua-ecosystem aqua-compliance inblockio.github.io)
# Per-consultant extra refs (host.extra_refs): clean clones live under REFS_MIRROR, one
# dir per repo, cloned from REFS_REMOTE/<repo>.git. Disk only: /tmp is RAM-backed on this host.
REFS_MIRROR="${CONSULTANT_REFS_MIRROR:-$HOME/.local/share/consultant-refs}"
REFS_REMOTE="${CONSULTANT_REFS_REMOTE:-https://github.com/inblockio}"
# Followed refs (host.refs_follow): mirrors + _developments digests under FOLLOW_ROOT, kept current
# by the host timer consultant-refs-follow.timer (aqua-ops). This script only reads them.
FOLLOW_ROOT="${CONSULTANT_REFS_FOLLOW_ROOT:-$HOME/.local/share/consultant-refs-follow}"

# Print the leading comment block (line 2 through the line before `set -euo pipefail`),
# so this stays correct as the header grows.
usage() { sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --label)   LABEL="$2"; shift 2 ;;
    --target)  TARGET="$2"; shift 2 ;;
    --display) DISPLAY_NAME="$2"; shift 2 ;;
    --id)      ID="$2"; shift 2 ;;
    --name)    HUMAN_NAME="$2"; shift 2 ;;
    --persona) PERSONA="$2"; shift 2 ;;
    --avatar)  AVATAR="$2"; shift 2 ;;
    --template) TEMPLATE="$2"; shift 2 ;;
    --image)   IMAGE="$2"; shift 2 ;;
    --generic) GENERIC=1; shift ;;
    --replace) REPLACE=1; shift ;;
    --fresh)   FRESH=1; shift ;;
    --onboard) ONBOARD=1; shift ;;
    --print-onboarding) PRINT_ONBOARDING=1; shift ;;
    --keep-config) KEEP_CONFIG=1; shift ;;
    --no-refresh-refs) REFRESH_REFS=0; shift ;;
    --refresh-prompt) REFRESH_PROMPT=1; shift ;;
    --voice)   VOICE="$2"; shift 2 ;;
    --print-run) PRINT_RUN=1; shift ;;
    --print-mxid) PRINT_MXID=1; shift ;;
    --siwx-url) SIWX_URL_ARG="$2"; shift 2 ;;
    --matrix-url) MATRIX_URL_ARG="$2"; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "!! unknown arg: $1" >&2; usage 1 ;;
  esac
done

# Contradictory combo: --fresh wipes the DID + memory that --keep-config means to preserve.
if [ "$FRESH" -eq 1 ] && [ "$KEEP_CONFIG" -eq 1 ]; then
  echo "!! --fresh and --keep-config are contradictory (--fresh wipes the identity/memory --keep-config preserves)." >&2
  exit 2
fi
case "$VOICE" in
  ""|on|off) : ;;
  *) echo "!! --voice takes exactly 'on' or 'off' (got '$VOICE')" >&2; exit 2 ;;
esac
# The siwx-oidc the DID lookup asks: the one the agent will sign in with. Default = the image's
# own default (prod). CONSULTANT_SIWX_URL overrides for tests.
LOOKUP_SIWX_URL="${SIWX_URL_ARG:-${CONSULTANT_SIWX_URL:-https://siwx-oidc.inblock.io}}"
LOOKUP_SIWX_URL="${LOOKUP_SIWX_URL%/}"
# --print-run must not fetch/pull anything; presence of the refs repos is still enforced.
[ "$PRINT_RUN" -eq 1 ] && REFRESH_REFS=0

# ---------------------------------------------------------------- validate label + paths
# STEM is the shared name fragment: container aqua-agent-<STEM>-1, config <STEM>-config.json,
# persist <STEM>-persist, default id <STEM>-1. Labeled consultants use <label>-aqua-consultant;
# the un-labeled generic one (--generic) uses plain aqua-consultant, no other difference.
if [ "$GENERIC" -eq 1 ]; then
  [ -z "$LABEL" ] || { echo "!! --generic and --label are mutually exclusive" >&2; exit 2; }
  STEM="aqua-consultant"
else
  [ -n "$LABEL" ] || { echo "!! --label is required (e.g. gawain), or --generic for the un-labeled consultant" >&2; exit 2; }
  # label must be a safe slug (used in container name + paths + systemd unit)
  case "$LABEL" in
    *[!a-z0-9-]*|"") echo "!! --label must be lowercase [a-z0-9-]: '$LABEL'" >&2; exit 2 ;;
    generic) echo "!! the label 'generic' is reserved for the un-labeled consultant, use --generic" >&2; exit 2 ;;
  esac
  STEM="${LABEL}-aqua-consultant"
fi
# KEY names the per-consultant host assets in TEST_DIR (<key>-avatar.jpg, <key>-rooms/): the
# label, or "generic" for the un-labeled consultant.
if [ "$GENERIC" -eq 1 ]; then KEY="generic"; else KEY="$LABEL"; fi

NAME="aqua-agent-${STEM}-1"
CFG="$TEST_DIR/${STEM}-config.json"
PERSIST="$TEST_DIR/${STEM}-persist"
STORE="$PERSIST/store"
MEM="$PERSIST/memory"

# ---------------------------------------------------------------- agent MXID (read back, never derived)
# The agent's MXID is whatever siwx-oidc assigned at its first login (opaque for a new DID,
# legacy for a grandfathered one); the agent persists the whoami answer as [session] user_id
# in store/config.toml. Only that one key is read: the same file holds live tokens, so it is
# never printed, copied or grepped wholesale.
agent_mxid_from_store() {
  python3 - "$STORE/config.toml" <<'PY'
import sys, tomllib
try:
    with open(sys.argv[1], "rb") as f:
        d = tomllib.load(f)
except (OSError, tomllib.TOMLDecodeError):
    sys.exit(1)
u = (d.get("session") or {}).get("user_id") or ""
if not (u.startswith("@") and ":" in u):
    sys.exit(1)
print(u)
PY
}

if [ "$PRINT_MXID" -eq 1 ]; then
  if MXID_NOW="$(agent_mxid_from_store)"; then
    printf '%s\n' "$MXID_NOW"; exit 0
  fi
  echo "!! no persisted session in $STORE/config.toml yet (agent never logged in?)" >&2
  exit 1
fi

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
  [ -n "$TARGET" ] && [ "$TARGET" != "$CFG_TARGET" ] && echo "!! warn: --target '$TARGET' != config '$CFG_TARGET', using config" >&2
  [ -n "$DISPLAY_NAME" ] && [ "$DISPLAY_NAME" != "$CFG_DISPLAY" ] && echo "!! warn: --display '$DISPLAY_NAME' != config '$CFG_DISPLAY', using config" >&2
  ID="$CFG_ID"; TARGET="$CFG_TARGET"; DISPLAY_NAME="$CFG_DISPLAY"
else
  [ -n "$TARGET" ] || { echo "!! --target is required (peer MXID, or peer DID to resolve)" >&2; exit 2; }
  # Persona-aware: --persona <Name> derives the Matrix display alias "<Name> (Aqua
  # Consultant)" when --display is not given explicitly (--display still overrides).
  if [ -z "$DISPLAY_NAME" ] && [ -n "$PERSONA" ]; then
    DISPLAY_NAME="${PERSONA} (Aqua Consultant)"
  fi
  [ -n "$DISPLAY_NAME" ] || { echo "!! provide --persona <Name> (recommended), or --display <text>" >&2; exit 2; }
  [ -f "$TEMPLATE" ] || { echo "!! missing config template $TEMPLATE" >&2; exit 2; }
  if [ -n "$PERSONA" ]; then
    [ -f "$PERSONA_HELPER" ] || { echo "!! --persona needs the helper, missing: $PERSONA_HELPER" >&2; exit 2; }
  fi
fi
: "${ID:=${STEM}-1}"
# HUMAN_NAME = the SERVED person (greeting + onboarding). Legacy (no --persona) falls back
# to the display name as before; in persona mode it stays exactly what --name gave, empty
# means a pseudonymous peer, which the persona render greets without a name.
[ -n "$PERSONA" ] || : "${HUMAN_NAME:=$DISPLAY_NAME}"

# ---------------------------------------------------------------- owner allow-list
# Every consultant has exactly ONE authoritative Owner: its single target peer (--target, or the
# kept config's target; for --generic that is Tim). Tim's standing rule (2026-09-29): the Owner
# is always on the Aqua System allow-list (independent of onboarding). owner-allowlist.py appends a [[recipients]] entry (name = label, else
# <label>-owner) only when no entry has that MXID yet, under flock, with a backup, validated
# against the bridge's own rules before an atomic rename. FAIL-CLOSED: a real spawn aborts
# before any container change when the Owner cannot be ensured. Print modes only preview.
OWNER_HELPER="$(dirname "$PERSONA_HELPER")/owner-allowlist.py"
ALLOWLIST_FILE="${AQUA_SYSTEM_ALLOWLIST:-$HOME/.aqua-system-bridge/allowlist.toml}"
ensure_owner_allowlisted() {  # ensure_owner_allowlisted check|apply
  local sel=(--generic)
  [ "$GENERIC" -eq 1 ] || sel=(--label "$LABEL")
  if [ ! -f "$OWNER_HELPER" ]; then
    echo "!! owner allow-list: helper missing: $OWNER_HELPER" >&2
    [ "$1" = check ]; return
  fi
  python3 "$OWNER_HELPER" "$1" --path "$ALLOWLIST_FILE" --mxid "$TARGET" \
    --container "$NAME" --who "${PERSONA:-$DISPLAY_NAME}" "${sel[@]}"
}

# ---------------------------------------------------------------- onboarding copy
# The welcome is the consultant's own hello (consultant-persona.py); this section only renders
# Tim's two notices around it, for --onboard and --print-onboarding alike. No em/en dashes
# anywhere in the rendered text (tests enforce it).
# config_voice_enabled <config.json>: exit 0 iff the config has voice.enabled == true.
config_voice_enabled() {
  [ -f "$1" ] || return 1
  python3 - "$1" <<'PY'
import json, sys
try:
    v = json.load(open(sys.argv[1])).get("voice")
except Exception:
    sys.exit(1)
sys.exit(0 if isinstance(v, dict) and v.get("enabled") is True else 1)
PY
}

# The config's FINAL voice.enabled for a render: --voice wins, else the base the render starts
# from (the existing config, else the template). The --voice step below patches the file to the
# same value, so a freshly rendered hello and voice.enabled always agree.
if [ -f "$CFG" ]; then VOICE_BASE="$CFG"; else VOICE_BASE="$TEMPLATE"; fi
case "$VOICE" in
  on)  EFFECTIVE_VOICE=on ;;
  off) EFFECTIVE_VOICE=off ;;
  *)   if config_voice_enabled "$VOICE_BASE"; then EFFECTIVE_VOICE=on; else EFFECTIVE_VOICE=off; fi ;;
esac

# Who the notices name. Explicit --persona/--name win; a --keep-config spawn without them reads
# the persona and person back from the kept config (display alias + "Hi <name>! " prefix).
NOTICE_PERSONA="$PERSONA"; NOTICE_PERSON="$HUMAN_NAME"
if [ -z "$PERSONA" ] && [ "$KEEP_CONFIG" -eq 1 ] && [ -f "$PERSONA_HELPER" ]; then
  eval "$(python3 "$PERSONA_HELPER" derive "$CFG")"
  NOTICE_PERSONA="$D_PERSONA"; NOTICE_PERSON="$D_PERSON"
fi
ONBOARD_WHO="${NOTICE_PERSON:-$NOTICE_PERSONA}"   # title fragment: the served person, else the persona

# render_notice_delivered <hello>: Tim's DM once the consultant's welcome is confirmed, with the
# hello quoted line by line (blank lines as a bare ">") and the What's-new tail marked.
render_notice_delivered() {
  local poss=its
  [ -n "$NOTICE_PERSONA" ] && poss=her
  printf '✅ %s invited %s and posted %s welcome.\n\nThis is what they see:\n\n' \
    "${NOTICE_PERSONA:-The consultant}" "${NOTICE_PERSON:-the peer}" "$poss"
  printf '%s\n' "$1" | sed -e 's/^/> /' -e 's/^> $/>/'
  printf '>\n> *(followed by the "What'"'"'s new" list)*\n'
}

# render_notice_unconfirmed <seconds> [<relay log line or other detail>]: Tim's DM when the
# welcome was not confirmed.
render_notice_unconfirmed() {
  local who="The consultant's"
  [ -n "$NOTICE_PERSONA" ] && who="${NOTICE_PERSONA}'s"
  printf '⚠️ %s welcome to %s was not confirmed within %s s.\n\n' "$who" "${NOTICE_PERSON:-the peer}" "$1"
  [ -z "${2:-}" ] || printf '%s\n\n' "$2"
  printf 'It retries on the next process start: podman restart %s.\n' "$NAME"
}

if [ "$PRINT_ONBOARDING" -eq 1 ]; then
  # Offline copy review: no token, no network, no config write, no container, no DM.
  # The Owner step a real spawn runs first, previewed read-only (stderr, so stdout stays copy).
  case "$TARGET" in
    @*:*) ensure_owner_allowlisted check ;;
    *) echo ">> owner allow-list: --target is not an MXID yet (a DID is resolved only on a real spawn); not previewed" >&2 ;;
  esac
  [ -f "$PERSONA_HELPER" ] || { echo "!! --print-onboarding needs the helper, missing: $PERSONA_HELPER" >&2; exit 2; }
  if [ "$KEEP_CONFIG" -eq 1 ]; then PO_BASE="$CFG"; else PO_BASE="$VOICE_BASE"; fi
  PO_MXID="$(agent_mxid_from_store 2>/dev/null)" || PO_MXID=""
  PO_HELLO="$(python3 "$PERSONA_HELPER" preview "$PO_BASE" "$KEEP_CONFIG" "$REFRESH_PROMPT" \
    "$DISPLAY_NAME" "$PERSONA" "$HUMAN_NAME" "$EFFECTIVE_VOICE" "$PO_MXID")"
  if [ "$KEEP_CONFIG" -eq 1 ] && [ "$REFRESH_PROMPT" -eq 0 ]; then
    PO_VOICE_NOTE="kept config, hello verbatim"
  else
    PO_VOICE_NOTE="voice line $EFFECTIVE_VOICE"
  fi
  printf '==== consultant hello (her first message to %s; %s; What'"'"'s new list appended on first contact) ====\n' "$TARGET" "$PO_VOICE_NOTE"
  printf '%s\n' "$PO_HELLO"
  printf '\n==== Tim notice, delivered: INFO "welcome delivered: %s (%s)" ====\n' "$ONBOARD_WHO" "$NAME"
  render_notice_delivered "$PO_HELLO"
  printf '\n==== Tim notice, not confirmed: WARN "welcome NOT confirmed: %s (%s)" (example log line) ====\n' "$ONBOARD_WHO" "$NAME"
  render_notice_unconfirmed "${ONBOARD_WAIT:-180}" 'Last relay log line: `… initiate-DM hello failed (retries next process start): <error>`'
  exit 0
fi

# ---------------------------------------------------------------- host block: read + validate
# The config's `host` object (spawn-only settings, ignored by the agent) is parsed and validated
# HERE, before the Owner step, the config render or any clone, so a bad block aborts with exit 2
# and nothing changed. Source: the config this spawn uses (kept config, or the render's base).
# Names are plain GitHub repo names: [A-Za-z0-9._-] only, so no slash and no ".." can steer a
# mirror path or a /refs mount; "_developments" is reserved for the follow digests' mount point.
host_block() {  # host_block <config.json>: prints HOST_EXTRA_REFS=(...) HOST_REFS_FOLLOW=(...)
  python3 - "$1" <<'PY'
import json, re, shlex, sys
path = sys.argv[1]
def die(msg):
    print(f"!! host: {path}: {msg}", file=sys.stderr)
    sys.exit(2)
def jtype(v):
    return {dict: "object", list: "array", str: "string", bool: "boolean", type(None): "null"}.get(type(v), "number")
try:
    with open(path) as f:
        cfg = json.load(f)
except (OSError, ValueError) as e:
    die(f"cannot read the config as JSON: {e}")
if not isinstance(cfg, dict):
    die("the config is not a JSON object")
host = cfg.get("host", {})
if not isinstance(host, dict):
    die(f"`host` must be a JSON object, got {jtype(host)}")
unknown = sorted(k for k in host if k not in ("extra_refs", "refs_follow"))
if unknown:
    die(f"unknown key(s) in `host`: {', '.join(unknown)} (allowed: extra_refs, refs_follow)")
name_ok = re.compile(r"[A-Za-z0-9._-]+")
for key, var in (("extra_refs", "HOST_EXTRA_REFS"), ("refs_follow", "HOST_REFS_FOLLOW")):
    names = host.get(key, [])
    if not isinstance(names, list):
        die(f"host.{key} must be a list of repo names, got {jtype(names)}")
    for i, n in enumerate(names):
        if not (isinstance(n, str) and name_ok.fullmatch(n)) or n == "." or ".." in n:
            die(f"host.{key}[{i}]: invalid repo name {n!r}: plain repo names only, [A-Za-z0-9._-], no slash, no '..'")
        if n == "_developments":
            die(f"host.{key}[{i}]: '_developments' is reserved (mount point of the follow digests)")
    print(var + "=(" + " ".join(shlex.quote(n) for n in names) + ")")
PY
}
HOST_SRC="$CFG"; [ -f "$HOST_SRC" ] || HOST_SRC="$TEMPLATE"
HOST_EXTRA_REFS=(); HOST_REFS_FOLLOW=()
if ! HOST_ASSIGN="$(host_block "$HOST_SRC")"; then
  echo "!! aborting: fix the host block in $HOST_SRC (nothing was changed)" >&2
  exit 2
fi
eval "$HOST_ASSIGN"

# REFS_FOLLOW: host.refs_follow, a repeated name dropped.
REFS_FOLLOW=()
for name in "${HOST_REFS_FOLLOW[@]}"; do
  for r in "${REFS_FOLLOW[@]}"; do [ "$r" != "$name" ] || continue 2; done
  REFS_FOLLOW+=( "$name" )
done
is_followed() { local r; for r in "${REFS_FOLLOW[@]}"; do [ "$r" != "$1" ] || return 0; done; return 1; }
is_fleet_repo() { local r; for r in "${REFS_REPOS[@]}"; do [ "$r" != "$1" ] || return 0; done; return 1; }

# EXTRA_REFS: host.extra_refs, minus followed repos (the follow mount wins) and REFS_REPOS (the
# fleet mount wins), a repeated name dropped.
EXTRA_REFS=()
for name in "${HOST_EXTRA_REFS[@]}"; do
  if is_followed "$name"; then
    echo ">> extra refs: $name is followed (host.refs_follow), skipped"
    continue
  fi
  if is_fleet_repo "$name"; then
    echo ">> extra refs: $name is already fleet-mounted (REFS_REPOS), skipped"
    continue
  fi
  for r in "${EXTRA_REFS[@]}"; do [ "$r" != "$name" ] || continue 2; done
  EXTRA_REFS+=( "$name" )
done

# Followed refs: the host timer consultant-refs-follow.timer owns FOLLOW_ROOT; this script never
# clones, fetches or updates it, it only checks (read-only) that each followed mirror is a clean
# git clone and that its digest dir exists. GIT_OPTIONAL_LOCKS=0: the status must not take the
# index lock, which would race the timer's fast-forward. Checked here, before the Owner step, so
# a missing or dirty mirror aborts with nothing changed.
check_follow_refs() {
  local repo dir dev st
  for repo in "${REFS_FOLLOW[@]}"; do
    dir="$FOLLOW_ROOT/$repo"; dev="$FOLLOW_ROOT/_developments/$repo"
    if [ ! -e "$dir" ]; then
      echo "!! refs follow: mirror MISSING: $dir" >&2
      return 1
    fi
    if [ ! -d "$dir/.git" ]; then
      echo "!! refs follow: $dir is not a git clone; refusing to mount it" >&2
      return 1
    fi
    if ! st="$(GIT_OPTIONAL_LOCKS=0 git -C "$dir" status --porcelain --ignored 2>&1)"; then
      echo "!! refs follow: cannot read the status of $dir: $st" >&2
      return 1
    fi
    if [ -n "$st" ]; then
      echo "!! refs follow: mirror $dir has LOCAL CHANGES (modified, untracked or ignored files); refusing to mount it" >&2
      echo "   inspect: git -C $dir status --ignored   (or delete the dir; the service re-clones it)" >&2
      return 1
    fi
    if [ ! -d "$dev" ]; then
      echo "!! refs follow: digest dir MISSING: $dev" >&2
      return 1
    fi
  done
}
if [ "${#REFS_FOLLOW[@]}" -gt 0 ]; then
  if ! check_follow_refs; then
    echo "   this script never clones or updates follow mirrors; the host timer does." >&2
    echo "   run: systemctl --user start consultant-refs-follow.service" >&2
    echo "!! aborting: follow mirror not usable (nothing was changed)" >&2
    exit 1
  fi
  echo ">> refs follow: ${#REFS_FOLLOW[@]} repo(s) from $FOLLOW_ROOT (kept current by consultant-refs-follow.timer), ro at /refs, digests ro at /refs/_developments: ${REFS_FOLLOW[*]}"
fi

# ---------------------------------------------------------------- peer DID -> MXID (lookup, never derived)
# `--target did:...` is resolved through siwx-oidc's public GET /resolve?did=, which applies
# the server's own grandfathering rule. Any failure is FATAL: a guessed legacy MXID for a DID
# that is actually new would bind the consultant to an account that is not the peer's.
resolve_did_to_mxid() {  # resolve_did_to_mxid <did> -> prints the MXID; diagnostics on stderr
  python3 - "$LOOKUP_SIWX_URL" "$1" <<'PY'
import json, sys, urllib.error, urllib.parse, urllib.request
base, did = sys.argv[1], sys.argv[2]
url = f"{base}/resolve?" + urllib.parse.urlencode({"did": did})
def die(msg):
    print(f"!! cannot resolve {did} via {url}: {msg}", file=sys.stderr)
    print("   NOT falling back to the legacy did-... form (a new DID gets an opaque MXID).", file=sys.stderr)
    print("   Pass the peer's MXID directly (--target '@localpart:server', from their profile).", file=sys.stderr)
    sys.exit(1)
try:
    with urllib.request.urlopen(url, timeout=20) as r:
        body = json.load(r)
except urllib.error.HTTPError as e:
    if e.code == 404:
        die("HTTP 404, this siwx-oidc has no /resolve route (older than c5ed83b)")
    try:
        detail = json.load(e).get("message", "")
    except Exception:
        detail = ""
    die(f"HTTP {e.code} {detail}".strip())
except Exception as e:
    die(f"{type(e).__name__}: {e}")
mxid = body.get("mxid") if isinstance(body, dict) else None
if not (isinstance(mxid, str) and mxid.startswith("@") and ":" in mxid):
    die(f"no mxid in the answer: {body!r}")
if not body.get("exists"):
    print(f"!! warn: {did} has NO account yet; {mxid} is the MXID siwx-oidc will assign on their "
          "first sign-in. The consultant cannot invite them until then.", file=sys.stderr)
print(f">> --target {did} resolved via {base}/resolve -> {mxid} "
      f"(exists={str(bool(body.get('exists'))).lower()}, attested={str(bool(body.get('attested'))).lower()})",
      file=sys.stderr)
print(mxid)
PY
}
case "$TARGET" in
  did:*)
    PEER_DID="$TARGET"
    TARGET="$(resolve_did_to_mxid "$PEER_DID")" || { echo "!! aborting: peer DID could not be resolved" >&2; exit 1; }
    ;;
esac

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

# The Owner (= TARGET, now a resolved MXID) must be allow-listed BEFORE anything else changes:
# before the config render, the --replace removal and the launch. --print-run only previews.
if [ "$PRINT_RUN" -eq 1 ]; then
  ensure_owner_allowlisted check
elif ! ensure_owner_allowlisted apply; then
  echo "!! aborting: could not put the Owner $TARGET on the Aqua System allow-list ($ALLOWLIST_FILE)." >&2
  echo "   Nothing was changed: no config render, no container removal, no launch. Fix the file (or" >&2
  echo "   the name clash) and re-run; see SKILL.md \"Owner rule\"." >&2
  exit 1
fi

# ---------------------------------------------------------------- token (by reference)
TOKEN_FILE="${AQUA_CLAUDE_TOKEN_FILE:-/home/waldknoten-01/.aqua-matrix-heartbeat/claude-oauth-token}"
if [ -z "${CLAUDE_CODE_OAUTH_TOKEN:-}" ] && [ -s "$TOKEN_FILE" ]; then
  CLAUDE_CODE_OAUTH_TOKEN="$(tr -d '[:space:]' < "$TOKEN_FILE")"
  export CLAUDE_CODE_OAUTH_TOKEN
fi
: "${CLAUDE_CODE_OAUTH_TOKEN:?set CLAUDE_CODE_OAUTH_TOKEN, or populate $TOKEN_FILE via:  claude setup-token}"

# ---------------------------------------------------------------- Deepgram key (optional, by reference)
# Voice messages need DEEPGRAM_API_KEY inside the container. Same discipline as the OAuth
# token: the value is taken from a file and handed to podman as a bare `-e NAME` (inherited
# from this process's environment), so it never appears on a command line, in `podman
# inspect`, or in this script's output. The file is sourced in a SUBSHELL and only the one
# variable is captured, so nothing else in it (other vars, shell options) leaks in here.
# An already-exported DEEPGRAM_API_KEY wins over the file. No file = silent skip (one notice
# line): the container starts unchanged and voice stays disabled.
DEEPGRAM_ENV_FILE="${AQUA_DEEPGRAM_ENV:-$HOME/.aqua-secrets/deepgram.env}"
if [ -z "${DEEPGRAM_API_KEY:-}" ] && [ -r "$DEEPGRAM_ENV_FILE" ]; then
  # shellcheck disable=SC1090
  # A malformed file must not abort the spawn: it degrades to "no key" plus the notice below.
  DEEPGRAM_API_KEY="$(set -a; . "$DEEPGRAM_ENV_FILE" >/dev/null 2>&1; set +a; printf '%s' "${DEEPGRAM_API_KEY:-}")" || DEEPGRAM_API_KEY=""
fi
DEEPGRAM_ENV_ARGS=()
if [ -n "${DEEPGRAM_API_KEY:-}" ]; then
  export DEEPGRAM_API_KEY
  DEEPGRAM_ENV_ARGS+=( -e DEEPGRAM_API_KEY )
  echo ">> voice: DEEPGRAM_API_KEY available (by reference), passed into the container as a bare -e"
elif [ -e "$DEEPGRAM_ENV_FILE" ]; then
  echo "!! voice: $DEEPGRAM_ENV_FILE exists but yields no DEEPGRAM_API_KEY (unreadable, or empty); voice stays disabled" >&2
else
  echo ">> voice: no Deepgram env file at $DEEPGRAM_ENV_FILE, voice stays disabled"
fi

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

# A followed fleet repo is mounted from its follow mirror IN PLACE of the fleet checkout (same
# position in the argv); a followed repo outside REFS_REPOS is added after the fleet mounts, then
# one digest dir per followed repo. No host.refs_follow = exactly the fleet mounts, as before.
REF_MOUNT_ARGS=()
for repo in "${REFS_REPOS[@]}"; do
  if is_followed "$repo"; then
    REF_MOUNT_ARGS+=( -v "$FOLLOW_ROOT/$repo:/refs/$repo:ro" )
  else
    REF_MOUNT_ARGS+=( -v "$REFS_BASE/$repo:/refs/$repo:ro" )
  fi
done
for repo in "${REFS_FOLLOW[@]}"; do
  is_fleet_repo "$repo" || REF_MOUNT_ARGS+=( -v "$FOLLOW_ROOT/$repo:/refs/$repo:ro" )
done
for repo in "${REFS_FOLLOW[@]}"; do
  REF_MOUNT_ARGS+=( -v "$FOLLOW_ROOT/_developments/$repo:/refs/_developments/$repo:ro" )
done

# Extra refs (EXTRA_REFS, from host.extra_refs): clean single-branch clones of each
# repo's default branch under REFS_MIRROR. The whole tree is readable in the container, so the
# mirror must be exactly what upstream has: FAIL-CLOSED on a missing mirror under
# --no-refresh-refs, a failed clone or fetch, a non-fast-forward, and ANY local change
# (modified, untracked OR ignored files). Never prompts for credentials (GIT_TERMINAL_PROMPT=0);
# private repos authenticate through the host's git credential helper.
sync_extra_refs() {
  local repo dir st before
  for repo in "${EXTRA_REFS[@]}"; do
    dir="$REFS_MIRROR/$repo"
    if [ ! -e "$dir" ]; then
      if [ "$REFRESH_REFS" -eq 0 ]; then
        echo "!! extra refs: mirror MISSING: $dir (nothing is cloned under --no-refresh-refs / --print-run)" >&2
        echo "   run once without --no-refresh-refs, or:  git clone --single-branch $REFS_REMOTE/${repo}.git $dir" >&2
        return 1
      fi
      mkdir -p "$REFS_MIRROR"
      if ! GIT_TERMINAL_PROMPT=0 git clone --quiet --single-branch "$REFS_REMOTE/${repo}.git" "$dir"; then
        echo "!! extra refs: clone FAILED for $repo ($REFS_REMOTE/${repo}.git)" >&2
        return 1
      fi
      echo ">> extra refs: $repo cloned ($(git -C "$dir" rev-parse --abbrev-ref HEAD) at $(git -C "$dir" rev-parse --short HEAD))"
      continue
    fi
    if [ ! -d "$dir/.git" ]; then
      echo "!! extra refs: $dir is not a git clone; refusing to mount it" >&2
      return 1
    fi
    if ! st="$(git -C "$dir" status --porcelain --ignored 2>&1)"; then
      echo "!! extra refs: cannot read the status of $dir: $st" >&2
      return 1
    fi
    if [ -n "$st" ]; then
      echo "!! extra refs: mirror $dir has LOCAL CHANGES (modified, untracked or ignored files); refusing to mount it" >&2
      echo "   inspect: git -C $dir status --ignored   (or delete the dir; the next spawn re-clones it)" >&2
      return 1
    fi
    [ "$REFRESH_REFS" -eq 1 ] || continue
    before="$(git -C "$dir" rev-parse HEAD)"
    if ! GIT_TERMINAL_PROMPT=0 git -C "$dir" pull --ff-only --quiet; then
      echo "!! extra refs: fetch / fast-forward FAILED for $repo (offline, or the mirror diverged from upstream)" >&2
      return 1
    fi
    if [ "$(git -C "$dir" rev-parse HEAD)" != "$before" ]; then
      echo ">> extra refs: $repo fast-forwarded to $(git -C "$dir" rev-parse --short HEAD)"
    fi
  done
}
if [ "${#EXTRA_REFS[@]}" -gt 0 ]; then
  sync_extra_refs || { echo "!! aborting: extra refs mirror not usable, see above (nothing was launched)" >&2; exit 1; }
  for repo in "${EXTRA_REFS[@]}"; do
    REF_MOUNT_ARGS+=( -v "$REFS_MIRROR/$repo:/refs/$repo:ro" )
  done
  if [ "$REFRESH_REFS" -eq 1 ]; then EXTRA_MODE="up to date with upstream"; else EXTRA_MODE="as-is, freshness pass skipped"; fi
  echo ">> extra refs: ${#EXTRA_REFS[@]} repo(s) from $REFS_MIRROR ($EXTRA_MODE), ro at /refs: ${EXTRA_REFS[*]}"
fi

# ---------------------------------------------------------------- avatar mount
# Each consultant has a female profile picture at <test-dir>/<key>-avatar.jpg, bind-mounted
# read-only at /agent/avatar.png (the fixed path the agent reads to set its Matrix avatar;
# the agent re-uploads only when the file fingerprint changes). key = the label, or "generic"
# for the un-labeled consultant. Override the source with --avatar PATH. A missing asset is a
# non-fatal warning so a consultant without one still launches (just with no avatar).
AVATAR_SRC="${AVATAR:-$TEST_DIR/${KEY}-avatar.jpg}"
AVATAR_MOUNT_ARGS=()
if [ -f "$AVATAR_SRC" ]; then
  AVATAR_MOUNT_ARGS+=( -v "$AVATAR_SRC:/agent/avatar.png:ro" )
  echo ">> avatar: mounting $AVATAR_SRC at /agent/avatar.png"
else
  echo "!! avatar: no asset at $AVATAR_SRC; launching without an avatar (drop a ${KEY}-avatar.jpg there, or pass --avatar PATH)" >&2
fi

# ---------------------------------------------------------------- rooms mounts (optional)
# A consultant with a <test-dir>/<key>-rooms/ directory gets it read-only at /agent/rooms, plus
# its own WRITABLE <test-dir>/<key>-room-state/ at /agent/room-state: the room turns' Claude
# config dir, transcripts and notes, kept physically apart from /agent/memory (the owner's DM
# transcripts, which room turns must never read). room-state is durable like the persist dir:
# created when missing, never wiped (not even by --fresh), and mounted exactly like
# /agent/memory (`mkdir -p` here, `:U` so podman chowns it to the container's agent user).
# No rooms dir = neither mount and no warning, so every other consultant's argv is unchanged.
ROOMS_SRC="$TEST_DIR/${KEY}-rooms"
ROOM_STATE="$TEST_DIR/${KEY}-room-state"
ROOMS_MOUNT_ARGS=()
if [ -d "$ROOMS_SRC" ]; then
  mkdir -p "$ROOM_STATE"
  ROOMS_MOUNT_ARGS+=( -v "$ROOMS_SRC:/agent/rooms:ro" -v "$ROOM_STATE:/agent/room-state:U" )
  echo ">> rooms: mounting $ROOMS_SRC read-only at /agent/rooms, room state $ROOM_STATE writable at /agent/room-state"
fi

# ---------------------------------------------------------------- notify (best effort)
# DM Tim from the host CLI identity (independent of any container), never fatal.
NOTIFY="${CONSULTANT_NOTIFY:-/home/waldknoten-01/.aqua-matrix-notify/notify-tim.sh}"
notify() {
  [ -x "$NOTIFY" ] || { echo "notify: $NOTIFY missing/not executable; skipping DM" >&2; return 0; }
  "$NOTIFY" "$@" || echo "notify: DM failed (non-fatal), see notify.log" >&2
}

# ---------------------------------------------------------------- activity watcher
# Host-level systemd user service: tails this instance's inbound.jsonl and DMs Tim on
# the peer's FIRST message + every 10th. Idempotent; never fatal.
ensure_activity_watch() {
  if [ "$GENERIC" -eq 1 ]; then
    # The generic consultant's peer IS the operator, a watcher would DM Tim about
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
Description=Aqua activity watcher, ${DISPLAY_NAME} (DMs Tim on first message + every 10th)
Documentation=https://github.com/inblockio/aqua-matrix-agent
After=network-online.target
Wants=network-online.target
# Start limit DISABLED (2026-09-27). The old 10-starts-in-300s limit was a
# trap: ten 10s restarts fit inside 300s, so any network outage longer than
# ~2 min (three nightly ISP outages of 1.5-3.5 h in one week) left the watcher
# dead for good. The crash-loop guard is the progressive backoff in [Service].
StartLimitIntervalSec=0

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
# Progressive backoff (systemd >= 254): 10s -> 120s over 6 steps (~10, 15, 23,
# 35, 52, 79, then 120s), retrying forever instead of hitting a start limit.
RestartSec=10s
RestartSteps=6
RestartMaxDelaySec=120s
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

# ---------------------------------------------------------------- render config
mkdir -p "$STORE" "$MEM"
if [ "$KEEP_CONFIG" -eq 1 ]; then
  echo ">> --keep-config: using existing $CFG verbatim (id=$ID, display=$DISPLAY_NAME)"
else
  # Base = the EXISTING per-instance config when one is already present (so a re-run / relabel
  # preserves hand-customizations like a bespoke hello), otherwise the generic template.
  # The helper always sets id/target/display_name; with a non-empty persona it also (re)writes
  # the hello (the welcome; its voice line follows EFFECTIVE_VOICE, the value the --voice step
  # below leaves in the file) and the "# Who You Are" preamble (stripping any prior one first,
  # so a changed persona name updates cleanly). To force a clean template render, delete $CFG first.
  BASE="$TEMPLATE"; [ -f "$CFG" ] && BASE="$CFG"
  python3 "$PERSONA_HELPER" render "$BASE" "$CFG" "$ID" "$TARGET" "$DISPLAY_NAME" "$PERSONA" "$HUMAN_NAME" "$EFFECTIVE_VOICE"
fi

# --voice on|off: patch voice.enabled in the (rendered or kept) config, idempotently and
# without touching any other key. `on` creates {"enabled": true} when the block is absent
# and otherwise flips only `enabled`, preserving sibling voice keys (tts_voice, ...). `off`
# flips `enabled` to false and KEEPS the block. `off` on a config with no voice block writes
# nothing: absent already means disabled, and injecting the key would trip deny_unknown_fields
# on an image older than the voice feature (image-before-config). No --voice = no write at
# all, which is what lets --replace --keep-config carry an existing block through unchanged.
# It runs BEFORE --refresh-prompt, whose canonical hello takes its voice line from this value.
# A kept config's hello is never touched here (--keep-config = hello verbatim).
if [ -n "$VOICE" ]; then
python3 - "$CFG" "$VOICE" <<'PY'
import json, sys
path, mode = sys.argv[1], sys.argv[2]
cfg = json.load(open(path))
want = mode == "on"
voice = cfg.get("voice")
if not isinstance(voice, dict):
    if not want:
        print(f">> voice: no voice block in {path}; already disabled, nothing written")
        sys.exit(0)
    voice = {}
    cfg["voice"] = voice
if voice.get("enabled") is want:
    print(f">> voice: already {mode} in {path}, nothing written")
    sys.exit(0)
voice["enabled"] = want
with open(path, "w") as f:
    json.dump(cfg, f, indent=2, ensure_ascii=True); f.write("\n")
print(f">> voice: set voice.enabled={'true' if want else 'false'} in {path}")
PY
fi


# --refresh-prompt: adopt the template's prompt surface into the (kept or re-rendered)
# config, so a template prompt update can reach existing consultants without clobbering
# their customizations (hello, homeserver, ...). Identity fields are untouched.
if [ "$REFRESH_PROMPT" -eq 1 ]; then
  [ -f "$TEMPLATE" ] || { echo "!! --refresh-prompt: missing template $TEMPLATE" >&2; exit 2; }
  [ -f "$PERSONA_HELPER" ] || { echo "!! --refresh-prompt needs the helper, missing: $PERSONA_HELPER" >&2; exit 2; }
  # Adopt the template's system_prompt/description/ref_mounts, then RE-APPLY the persona
  # derived from this config's own display alias + hello, so a prompt refresh never
  # silently strips the "# Who You Are" preamble (identity/hello otherwise preserved).
  python3 "$PERSONA_HELPER" refresh "$CFG" "$TEMPLATE"
fi

# Keep the config's avatar_path consistent with whether we mounted an avatar, so an
# avatar_path-aware image actually sets the picture. Mounted -> avatar_path=/agent/avatar.png;
# no asset -> remove the key. Idempotent (only rewrites on change). Safe on the current image,
# which tolerates avatar_path; do not roll a consultant carrying it onto a pre-d78ce77 image.
if [ "${#AVATAR_MOUNT_ARGS[@]}" -gt 0 ]; then AVATAR_WANT=/agent/avatar.png; else AVATAR_WANT=""; fi
python3 - "$CFG" "$AVATAR_WANT" <<'PY'
import json, sys
path, want = sys.argv[1], sys.argv[2]
cfg = json.load(open(path))
cur = cfg.get("avatar_path")
if want:
    changed = cur != want
    cfg["avatar_path"] = want
elif "avatar_path" in cfg:
    changed = True
    del cfg["avatar_path"]
else:
    changed = False
if changed:
    with open(path, "w") as f:
        json.dump(cfg, f, indent=2, ensure_ascii=True); f.write("\n")
    print(f">> avatar_path {'set ' + want if want else 'removed'} in {path}")
PY

# A config with voice on but no key still launches (the agent logs the missing key and
# disables voice at runtime), but say so loudly: this is almost always a missing env file.
if [ "${#DEEPGRAM_ENV_ARGS[@]}" -eq 0 ] && config_voice_enabled "$CFG"; then
  echo "!! voice: enabled in config but DEEPGRAM_API_KEY is not available (no readable $DEEPGRAM_ENV_FILE?); launching anyway, the agent will disable voice at runtime" >&2
fi

# Deployment URLs, only when overridden, so the fleet's argv is unchanged. Not secrets.
URL_ENV_ARGS=()
[ -n "$SIWX_URL_ARG" ] && URL_ENV_ARGS+=( -e "SIWX_URL=$SIWX_URL_ARG" )
[ -n "$MATRIX_URL_ARG" ] && URL_ENV_ARGS+=( -e "MATRIX_URL=$MATRIX_URL_ARG" )

# ---------------------------------------------------------------- assemble the run
# ONE argument vector feeds both --print-run and the real `podman run`, so what is printed
# is exactly what runs. Secrets are bare `-e NAME` (inherited), never `-e NAME=value`. The
# resource/caps/token lines are byte-identical to the legacy recreate-*.sh (see SKILL.md,
# "Invariants"); keep them that way.
RUN_ARGS=( \
  --name "$NAME" \
  --restart on-failure \
  --memory 2048m --cpus 2 --pids-limit 512 \
  --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp \
  -e AGENT_TARGET="$TARGET" \
  -e AGENT_CONFIG_FILE=/agent/config.json \
  "${URL_ENV_ARGS[@]}" \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  "${DEEPGRAM_ENV_ARGS[@]}" \
  -v "$CFG:/agent/config.json:ro" \
  -v "$STORE:/agent/store:U" \
  -v "$MEM:/agent/memory:U" \
  "${REF_MOUNT_ARGS[@]}" \
  "${AVATAR_MOUNT_ARGS[@]}" \
  "${ROOMS_MOUNT_ARGS[@]}" \
  "$IMAGE" \
)

if [ "$PRINT_RUN" -eq 1 ]; then
  echo ">> --print-run: podman run argument vector follows (one per line); no container, no systemd unit, no DM, no --replace/--fresh" >&2
  printf '%s\n' podman run -d "${RUN_ARGS[@]}"
  exit 0
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
    "$TEST_DIR"/*-aqua-consultant-persist) : ;;
    "$TEST_DIR"/aqua-consultant-persist) : ;;   # --generic
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
echo ">> persist volume ready, $IDENTITY_NOTE"

# ---------------------------------------------------------------- greeted marker (for --onboard)
# The agent commits <memory.config_dir>/.whats_new_seen only after the relay CONFIRMED the hello
# send (claude-p hello_delivered). memory.config_dir is /agent/memory, bind-mounted from $MEM, so
# the host sees it at $MEM/.whats_new_seen. Recorded here, after any --replace removal and --fresh
# wipe and before the launch: a marker that already exists means this peer was greeted earlier.
greeted_marker_path() {
  python3 - "$CFG" "$MEM" <<'PY'
import json, posixpath, sys
cfg, mem = sys.argv[1], sys.argv[2]
d = posixpath.normpath((json.load(open(cfg)).get("memory") or {}).get("config_dir") or "")
if d == "/agent/memory":
    print(f"{mem}/.whats_new_seen")
elif d.startswith("/agent/memory/"):
    print(f"{mem}/{d[len('/agent/memory/'):]}/.whats_new_seen")
else:
    sys.exit(1)   # not under the host-mounted memory dir: the marker is invisible from here
PY
}
ONB_MARKER=""; ONB_ALREADY=0
if [ "$ONBOARD" -eq 1 ]; then
  ONB_MARKER="$(greeted_marker_path)" || ONB_MARKER=""
  if [ -n "$ONB_MARKER" ] && [ -e "$ONB_MARKER" ]; then ONB_ALREADY=1; fi
fi

echo ">> launching $NAME bound single-target to $TARGET"
set +e
CID="$(podman run -d "${RUN_ARGS[@]}" 2>&1)"
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
  "agent '$NAME' launched, single-target peer $TARGET, $IDENTITY_NOTE, container $CID_SHORT, on $(hostname)."

ensure_activity_watch || true

# ---------------------------------------------------------------- read back the agent's MXID
# A preserved identity already has a session; a fresh one gets it on first login, so wait
# (bounded) for the agent to persist it. Never derived from the DID.
if [ "$GENERIC" -eq 1 ]; then MXID_SEL="--generic"; else MXID_SEL="--label $LABEL"; fi
MXID=""
for _ in $(seq 1 60); do
  if MXID="$(agent_mxid_from_store)" && [ -n "$MXID" ]; then break; fi
  MXID=""
  sleep 2
done
if [ -n "$MXID" ]; then
  echo ">> agent MXID (from its persisted session): $MXID"
else
  echo "!! agent has not completed its first login within 120s; its MXID is not known yet." >&2
  echo "   read it later with: bash ~/spawn-consultant.sh --print-mxid $MXID_SEL" >&2
fi

# ---------------------------------------------------------------- onboarding: confirm the welcome
# The consultant sends her own welcome (the config's hello) into the DM room she creates. Here we
# only confirm the outcome to Tim: wait (bounded, ONBOARD_WAIT) for the greeted marker, and watch
# the relay log for its hello failure lines. A failure line is reported on the terminal at once,
# but the wait continues (a crash restart under --restart on-failure retries the hello); on the
# timeout Tim's WARN carries the last such line. Every outcome is non-fatal for the spawn.
ONB_FAIL_RE='initiate-DM hello failed|no DM room yet; deferring hello|hello send failed'
relay_hello_failure() {  # last relay hello-failure line of this container, ANSI-stripped, capped
  podman logs "$NAME" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -E "$ONB_FAIL_RE" | tail -n1 \
    | cut -c1-300 | tr '`' "'" || true
}
if [ "$ONBOARD" -eq 1 ]; then
  ONB_WAIT="${ONBOARD_WAIT:-180}"
  ONB_HELLO="$(python3 "$PERSONA_HELPER" hello-of "$CFG" "$MXID" 2>/dev/null)" || ONB_HELLO=""
  if [ -n "$ONBOARD_WHO" ]; then ONB_TITLE="${ONBOARD_WHO} (${NAME})"; else ONB_TITLE="$NAME"; fi
  if [ "$ONB_ALREADY" -eq 1 ]; then
    echo ">> onboarding: ${NOTICE_PERSON:-the peer} was already greeted earlier; nothing to send"
  elif [ -z "$ONB_HELLO" ]; then
    echo "!! onboarding: $CFG has no hello, so the consultant sends no welcome" >&2
    notify -s WARN -t "welcome NOT confirmed: ${ONB_TITLE}" \
      "$(render_notice_unconfirmed 0 'The config has no `hello`, so the consultant sends no welcome at all.')"
  elif [ -z "$ONB_MARKER" ]; then
    echo "!! onboarding: memory.config_dir in $CFG is not under /agent/memory; delivery cannot be observed from the host" >&2
    notify -s WARN -t "welcome NOT confirmed: ${ONB_TITLE}" \
      "$(render_notice_unconfirmed 0 'Its memory.config_dir is not under /agent/memory, so the host cannot see the greeted marker.')"
  else
    echo ">> onboarding: waiting up to ${ONB_WAIT}s for ${NOTICE_PERSONA:-the consultant}'s welcome to be delivered (marker $ONB_MARKER)"
    ONB_START=$SECONDS; ONB_DONE=""; ONB_LOGLINE=""; ONB_SEEN=""
    while :; do
      if [ -e "$ONB_MARKER" ]; then ONB_DONE=delivered; break; fi
      ONB_LOGLINE="$(relay_hello_failure)"
      if [ -n "$ONB_LOGLINE" ] && [ "$ONB_LOGLINE" != "$ONB_SEEN" ]; then
        echo "!! onboarding: the relay reports a failed hello (still waiting, a restart retries): $ONB_LOGLINE" >&2
        ONB_SEEN="$ONB_LOGLINE"
      fi
      [ $((SECONDS - ONB_START)) -lt "$ONB_WAIT" ] || break
      sleep 1
    done
    if [ "$ONB_DONE" = delivered ]; then
      echo ">> onboarding: welcome delivered after $((SECONDS - ONB_START))s; telling Tim"
      notify -s INFO -t "welcome delivered: ${ONB_TITLE}" "$(render_notice_delivered "$ONB_HELLO")"
    else
      ONB_DETAIL=""
      [ -z "$ONB_LOGLINE" ] || ONB_DETAIL="Last relay log line: \`${ONB_LOGLINE}\`"
      echo "!! onboarding: welcome not confirmed within ${ONB_WAIT}s; it retries on the next process start: podman restart $NAME" >&2
      notify -s WARN -t "welcome NOT confirmed: ${ONB_TITLE}" \
        "$(render_notice_unconfirmed "$ONB_WAIT" "$ONB_DETAIL")"
    fi
  fi
fi

echo
echo ">> container started: $CID_SHORT"
echo ">> done. verify with:"
echo "   podman logs -f $NAME    # watch it connect + self-mint its DID + set display name"
if [ -n "$MXID" ]; then
  echo "   # the consultant invites the peer itself; its MXID is $MXID"
else
  echo "   # its MXID, once logged in:  bash ~/spawn-consultant.sh --print-mxid $MXID_SEL"
fi
