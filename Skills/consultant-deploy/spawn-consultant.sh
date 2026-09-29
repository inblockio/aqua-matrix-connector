#!/usr/bin/env bash
#
# spawn-consultant.sh, generic, parameterized launcher for a DEDICATED, single-target
# Aqua consultant. This REPLACES the hand-cloned recreate-<label>-consultant.sh scripts:
# one script, three required parameters (--label / --target / --persona). It renders the
# per-instance config from ~/.aqua-matrix-test/consultant-config.template.json, launches
# the container with the SAME hardened podman flags as the original scripts (verbatim -
# the security posture must not drift), wires the systemd activity-watcher, DMs Tim
# "channel up", and (with --onboard) sends the peer a welcome message through the Aqua System
# bridge (allow-listed peers only), telling Tim either "delivered" or "please forward this"
# (see "Onboarding" below). Since the template sets "initiate_dm": true,
# the consultant herself creates the DM room, invites the peer and delivers her
# greeting (the peer just accepts the invite; no need to DM the MXID first).
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
# script sets the Matrix display alias "<Name> (Aqua Consultant)", a heartfelt first-contact
# greeting that addresses the served person (--name) by name with no DID/MXID noise, and a
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
# and implies --no-refresh-refs (presence of every refs repo is still checked). It DOES
# render/patch the config file and create the persist dirs, since those are the run's
# inputs. Secrets print as bare names (`-e DEEPGRAM_API_KEY`), never as values.
#
# Onboarding (--onboard): once the agent's MXID is known, the script renders the peer welcome
# (persona or legacy wording; the voice line only when the config has voice.enabled == true, read
# from the config file so --keep-config spawns are right too) and tries to send it to --target
# DIRECTLY as the shared "Aqua System" identity, via the stdio MCP server
# aqua-system-bridge-mcp (tool send_message, via onboard-send.py, hard 30 s cap). The bridge only
# messages people on ~/.aqua-system-bridge/allowlist.toml, and that Tim-approved list is what
# authorizes the direct send. Then Tim gets ONE DM: "onboarding sent" (with the quoted text) on
# delivery, or "onboarding to forward" (the text between two lines, plus the reason) when the peer
# is not allow-listed (INFO) or the bridge failed or timed out (WARN). A failed send never fails
# the spawn. --onboard-forward-only (implies --onboard) skips the direct send: the old behaviour.
#
# --print-onboarding: print the peer welcome and both Tim notices to stdout and exit 0, before any
# token, network, config write, container or DM (offline copy review). The MXID comes from the
# persisted session when there is one, else the placeholder @<agent-mxid>:matrix.inblock.io; the
# voice line follows --voice when given, else the existing config (else the template).
#
# Env overrides (all optional; the defaults are this host's live paths):
#   CONSULTANT_TEST_DIR     dir holding configs/persist/avatars/template (default ~/.aqua-matrix-test)
#   CONSULTANT_TEMPLATE     config template path (default $CONSULTANT_TEST_DIR/consultant-config.template.json)
#   CONSULTANT_REFS_BASE    parent dir of the REFS_REPOS checkouts (default /home/waldknoten-01)
#   CONSULTANT_IMAGE        image to run (default localhost/aqua-matrix-agent:poc)
#   AQUA_CLAUDE_TOKEN_FILE  OAuth token file (default ~/.aqua-matrix-heartbeat/claude-oauth-token)
#   AQUA_DEEPGRAM_ENV       Deepgram env file (default $HOME/.aqua-secrets/deepgram.env)
#   AQUA_SYSTEM_BRIDGE_MCP  bridge MCP binary for --onboard (default ~/.local/bin/aqua-system-bridge-mcp)
#   ONBOARD_SEND_TIMEOUT    seconds before a direct onboarding send counts as failed (default 30)
#   CONSULTANT_NOTIFY       notifier used for Tim's DMs (default ~/.aqua-matrix-notify/notify-tim.sh; tests)
#
# Examples:
#   # new consultant (fresh identity) with a female persona; DM Tim a forward-ready intro:
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
#   # review the onboarding copy offline (peer welcome + both Tim notices), nothing started:
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
ONBOARD=0             # after connect, send the peer the onboarding welcome (bridge) and tell Tim
ONBOARD_DIRECT=1      # --onboard-forward-only sets 0: never send directly, DM Tim a forward-ready copy
PRINT_ONBOARDING=0    # --print-onboarding: print the peer welcome + both Tim notices and exit 0
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
# Onboarding direct-send client (Aqua System bridge, stdio MCP), alongside this script too.
ONBOARD_SENDER="$(dirname "$PERSONA_HELPER")/onboard-send.py"
# Grounding repos, mounted ro at /refs/<name>. This ONE list drives the presence check,
# the freshness pass, and the podman -v flags, so the three can never drift apart.
# Keep it in sync with ref_mounts in consultant-config.template.json.
REFS_REPOS=(aqua-rs-sdk aqua-spec aqua-governance-corpus aqua-ecosystem aqua-compliance inblockio.github.io)

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
    --onboard-forward-only) ONBOARD=1; ONBOARD_DIRECT=0; shift ;;
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

# ---------------------------------------------------------------- onboarding copy
# One source for the peer welcome and Tim's two notices, used by --onboard and by
# --print-onboarding. No em/en dashes anywhere in the rendered text (tests enforce it).
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

# render_peer_welcome <agent-mxid> <voice 0|1>: the Markdown the peer reads.
render_peer_welcome() {
  local mxid="$1" voice="$2" hi="${HUMAN_NAME:-there}"
  local intro who who_mid she she_lc obj poss
  if [ -n "$PERSONA" ]; then
    intro="You now have your own Aqua Consultant, **${PERSONA}**. She is an AI assistant who knows Aqua inside out, and she is there just for you whenever you have a question."
    who="$PERSONA"; who_mid="$PERSONA"; she="She"; she_lc="she"; obj="her"; poss="her"
  else
    intro="You now have your own **Aqua Consultant**. It is an AI assistant who knows Aqua inside out, and it is there just for you whenever you have a question."
    who="Your consultant"; who_mid="your consultant"; she="It"; she_lc="it"; obj="it"; poss="its"
  fi
  printf '%s\n' \
    "Hi ${hi}! 👋" \
    "" \
    "$intro" \
    "" \
    "Aqua is inblock.io's protocol for trust that travels with your data: every signature, AI action and file carries its own proof, so anyone can check what happened." \
    "" \
    "**How to start**" \
    "${who} has sent you a chat invitation in Element, your Matrix chat app. Accept it, and ${poss} welcome message is already waiting for you. If you ever need to find ${obj}, ${poss} address is \`${mxid}\`." \
    "" \
    "**What you can ask**" \
    "Anything, in your own words. You don't need a technical background, and if you are a developer ${she_lc} will happily go deep. For example:" \
    '- "What is Aqua, and why would I use it?"' \
    '- "How could Aqua help in my work?"' \
    '- "What is the difference between AquaFire, AquaNode and AquaAgents?"' \
    '- "Walk me through signing and verifying a document, step by step."' \
    '- "Show me where the SDK checks a signature."' \
    "" \
    "**Good to know**" \
    "- The chat is one-to-one: ${who_mid} talks only with you."
  if [ "$voice" = 1 ]; then printf '%s\n' "- You can type, or send ${obj} voice messages."; fi
  printf '%s\n' \
    "- ${she} explains and shows you where ${poss} answers come from. ${she} cannot change anything or act on your behalf." \
    "- Like any AI, ${she_lc} can occasionally be wrong. When something matters, ask ${obj} for the source." \
    "" \
    "Enjoy! 🌊"
}

# render_notice_delivered <welcome> <event-id>: Tim's DM after a direct delivery (text quoted).
render_notice_delivered() {
  printf '✅ Onboarding delivered to %s directly (Aqua System DM, event %s). %s has also invited them to a chat. Nothing to forward.\n\nFor reference, this is what they received:\n\n' \
    "${HUMAN_NAME:-the peer}" "$2" "${PERSONA:-The consultant}"
  printf '%s\n' "$1" | sed -e 's/^/> /' -e 's/^> $/>/'
}

# render_notice_forward <welcome> <reason>: Tim's DM when the text was NOT sent directly. The
# blank lines around the rules keep Markdown from reading "Enjoy! 🌊" + "----" as a heading.
render_notice_forward() {
  printf '📋 Onboarding for %s: please forward the text between the lines. It was not sent directly: %s.\n\n----------\n\n%s\n\n----------\n' \
    "${HUMAN_NAME:-your contact}" "$2" "$1"
}

# Title fragment for Tim's notices: the served person, else the persona.
ONBOARD_WHO="${HUMAN_NAME:-$PERSONA}"

if [ "$PRINT_ONBOARDING" -eq 1 ]; then
  # Offline copy review: no token, no network, no config write, no container, no DM.
  PO_MXID="$(agent_mxid_from_store)" || PO_MXID="@<agent-mxid>:matrix.inblock.io"
  case "$VOICE" in
    on)  PO_VOICE=1 ;;
    off) PO_VOICE=0 ;;
    *)   PO_BASE="$TEMPLATE"; [ -f "$CFG" ] && PO_BASE="$CFG"
         if config_voice_enabled "$PO_BASE"; then PO_VOICE=1; else PO_VOICE=0; fi ;;
  esac
  if [ "$ONBOARD_DIRECT" -eq 1 ]; then
    PO_REASON="${TARGET} is not on the Aqua System allow-list"
  else
    PO_REASON="direct send disabled (--onboard-forward-only)"
  fi
  PO_WELCOME="$(render_peer_welcome "$PO_MXID" "$PO_VOICE")"
  if [ "$PO_VOICE" = 1 ]; then PO_VOICE_WORD=on; else PO_VOICE_WORD=off; fi
  printf '==== peer welcome (Aqua System DM to %s; voice line %s) ====\n' "$TARGET" "$PO_VOICE_WORD"
  printf '%s\n' "$PO_WELCOME"
  printf '\n==== Tim notice, delivered: INFO "onboarding sent: %s (%s)" ====\n' "$ONBOARD_WHO" "$NAME"
  render_notice_delivered "$PO_WELCOME" '$<event-id>'
  printf '\n==== Tim notice, not delivered: INFO "onboarding to forward: %s (%s)" (WARN if the bridge errored) ====\n' "$ONBOARD_WHO" "$NAME"
  render_notice_forward "$PO_WELCOME" "$PO_REASON"
  exit 0
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

REF_MOUNT_ARGS=()
for repo in "${REFS_REPOS[@]}"; do
  REF_MOUNT_ARGS+=( -v "$REFS_BASE/$repo:/refs/$repo:ro" )
done

# ---------------------------------------------------------------- avatar mount
# Each consultant has a female profile picture at <test-dir>/<key>-avatar.jpg, bind-mounted
# read-only at /agent/avatar.png (the fixed path the agent reads to set its Matrix avatar;
# the agent re-uploads only when the file fingerprint changes). key = the label, or "generic"
# for the un-labeled consultant. Override the source with --avatar PATH. A missing asset is a
# non-fatal warning so a consultant without one still launches (just with no avatar).
if [ "$GENERIC" -eq 1 ]; then AVATAR_KEY="generic"; else AVATAR_KEY="$LABEL"; fi
AVATAR_SRC="${AVATAR:-$TEST_DIR/${AVATAR_KEY}-avatar.jpg}"
AVATAR_MOUNT_ARGS=()
if [ -f "$AVATAR_SRC" ]; then
  AVATAR_MOUNT_ARGS+=( -v "$AVATAR_SRC:/agent/avatar.png:ro" )
  echo ">> avatar: mounting $AVATAR_SRC at /agent/avatar.png"
else
  echo "!! avatar: no asset at $AVATAR_SRC; launching without an avatar (drop a ${AVATAR_KEY}-avatar.jpg there, or pass --avatar PATH)" >&2
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
  # the heartfelt hello and the "# Who You Are" preamble (stripping any prior one first, so a
  # changed persona name updates cleanly). To force a clean template render, delete $CFG first.
  BASE="$TEMPLATE"; [ -f "$CFG" ] && BASE="$CFG"
  python3 "$PERSONA_HELPER" render "$BASE" "$CFG" "$ID" "$TARGET" "$DISPLAY_NAME" "$PERSONA" "$HUMAN_NAME"
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

# --voice on|off: patch voice.enabled in the (rendered or kept) config, idempotently and
# without touching any other key. `on` creates {"enabled": true} when the block is absent
# and otherwise flips only `enabled`, preserving sibling voice keys (tts_voice, ...). `off`
# flips `enabled` to false and KEEPS the block. `off` on a config with no voice block writes
# nothing: absent already means disabled, and injecting the key would trip deny_unknown_fields
# on an image older than the voice feature (image-before-config). No --voice = no write at
# all, which is what lets --replace --keep-config carry an existing block through unchanged.
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

# ---------------------------------------------------------------- onboarding
# Direct first: send the peer the welcome as "Aqua System" (allow-listed peers only), then tell
# Tim what happened. Every failure here is non-fatal: it degrades to a forward-ready DM to Tim.
if [ "$ONBOARD" -eq 1 ]; then
  if [ -n "$MXID" ]; then
    if config_voice_enabled "$CFG"; then ONB_VOICE=1; else ONB_VOICE=0; fi
    ONB_WELCOME="$(render_peer_welcome "$MXID" "$ONB_VOICE")"
    ONB_LEVEL=INFO; ONB_EVENT=""; ONB_REASON=""
    if [ "$ONBOARD_DIRECT" -eq 0 ]; then
      ONB_REASON="direct send disabled (--onboard-forward-only)"
    else
      echo ">> onboarding: sending the welcome to $TARGET via the Aqua System bridge"
      if [ -f "$ONBOARD_SENDER" ]; then
        set +e
        ONB_OUT="$(python3 "$ONBOARD_SENDER" "$TARGET" <<<"$ONB_WELCOME" 2>&1)"
        onb_rc=$?
        set -e
        ONB_OUT="${ONB_OUT##*$'\n'}"   # the sender's verdict is its one (last) line
      else
        onb_rc=4; ONB_OUT="ERROR onboarding sender $ONBOARD_SENDER is missing"
      fi
      ONB_DETAIL="${ONB_OUT#* }"
      case "$onb_rc:$ONB_OUT" in
        0:DELIVERED\ *) ONB_EVENT="$ONB_DETAIL" ;;
        3:REFUSED\ *is\ not\ on\ the\ Aqua\ System\ allow-list*)
          ONB_REASON="${TARGET} is not on the Aqua System allow-list" ;;
        3:REFUSED\ *allow-list\ failed\ to\ load*)
          ONB_LEVEL=WARN; ONB_REASON="the Aqua System bridge failed (${ONB_DETAIL})" ;;
        3:REFUSED\ *)
          ONB_REASON="the Aqua System bridge refused ${TARGET} (${ONB_DETAIL})" ;;
        *)
          ONB_LEVEL=WARN; ONB_REASON="the Aqua System bridge failed (${ONB_DETAIL:-exit $onb_rc})" ;;
      esac
    fi
    if [ -n "$ONB_EVENT" ]; then
      echo ">> onboarding: delivered to $TARGET directly (event $ONB_EVENT)"
      notify -s INFO -t "onboarding sent: ${ONBOARD_WHO} (${NAME})" \
        "$(render_notice_delivered "$ONB_WELCOME" "$ONB_EVENT")"
      echo ">> onboarding: told Tim (delivered, nothing to forward)"
    else
      if [ "$ONB_LEVEL" = WARN ]; then
        echo "!! onboarding: direct send failed: $ONB_REASON" >&2
      else
        echo ">> onboarding: not sent directly: $ONB_REASON"
      fi
      notify -s "$ONB_LEVEL" -t "onboarding to forward: ${ONBOARD_WHO} (${NAME})" \
        "$(render_notice_forward "$ONB_WELCOME" "$ONB_REASON")"
      echo ">> onboarding: forward-ready copy DM'd to Tim (carries $MXID)"
    fi
  else
    echo "!! could not read the agent's MXID from its session within the timeout, onboarding skipped." >&2
    notify -s WARN -t "onboarding pending: ${NAME}" \
      "Spawned '${NAME}' for ${ONBOARD_WHO:-its peer} but its first login has not completed yet, so its MXID is unknown and nothing was sent. Once it has logged in, get the text to forward with the same spawn flags plus --print-onboarding (it reads the MXID from the session)."
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
