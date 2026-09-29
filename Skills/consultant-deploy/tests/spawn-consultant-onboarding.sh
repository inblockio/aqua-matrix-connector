#!/usr/bin/env bash
#
# spawn-consultant-onboarding.sh, offline tests for the --onboard flow of spawn-consultant.sh.
#
# Nothing live is touched: HOME, the config/persist dir and the refs base point into a temp
# dir, the Aqua System bridge is a fake stdio MCP server (onboard-fake-bridge.py semantics
# inline below), Tim's notifier is a recorder (CONSULTANT_NOTIFY), and podman/systemctl are
# shims. No message leaves the machine.
#
# Covers:
#   - --print-onboarding: persona + name, persona pseudonymous ("Hi there"), legacy no-persona
#     wording, voice line on/off (flag, existing config, --keep-config config, template default),
#     MXID from the persisted session vs the placeholder, --onboard-forward-only reason, no side
#     effects (no config written, shims never called)
#   - no rendered text (peer welcome, both Tim notices) contains U+2014 or U+2013
#   - onboard-send.py against the fake bridge: delivered (event id parsed, arguments passed
#     verbatim), allow-list refusal, other refusal, tool error, JSON-RPC error, timeout,
#     garbage output, missing binary
#   - full --onboard spawn (podman shimmed): delivered -> INFO "onboarding sent", not
#     allow-listed (now unexpected) -> WARN "onboarding to forward", bridge error -> WARN,
#     forward-only never calls the bridge; the spawn exits 0 in every case
#   - Owner allow-list step: add when missing (name, note, mode, backup), no-op when present
#     (also by case), label clash -> <label>-owner, both taken / invalid / missing file -> abort
#     before any podman call (also with --replace), print modes write nothing, generic never
#     auto-added, concurrent applies serialize
#
# Usage:  bash Skills/consultant-deploy/tests/spawn-consultant-onboarding.sh
# Exit:   0 when every assertion passes, 1 otherwise (each failure is printed).
#
set -euo pipefail

SKILL_DIR="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
SPAWN="$SKILL_DIR/spawn-consultant.sh"
SENDER="$SKILL_DIR/onboard-send.py"
TEMPLATE="$SKILL_DIR/consultant-config.template.json"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok   $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL $1" >&2; }
check() { local desc="$1"; shift; if "$@"; then ok "$desc"; else bad "$desc"; fi; }
has()    { grep -qF -- "$2" "$1"; }          # has <file> <literal>
hasnt()  { ! grep -qF -- "$2" "$1"; }

SB="$(mktemp -d "${TMPDIR:-/tmp}/spawn-onboard-test.XXXXXX")"
trap 'rm -rf "$SB"' EXIT
FAKE_HOME="$SB/home"; TEST_DIR="$SB/aqua-matrix-test"; REFS_BASE="$SB/refs"; SHIM_BIN="$SB/bin"
SIDE_EFFECTS="$SB/side-effects.log"; NOTIFY_LOG="$SB/notify.jsonl"; BRIDGE_LOG="$SB/bridge.jsonl"
TOKEN_FILE="$FAKE_HOME/.aqua-matrix-heartbeat/claude-oauth-token"
mkdir -p "$FAKE_HOME/.aqua-matrix-heartbeat" "$TEST_DIR" "$REFS_BASE" "$SHIM_BIN" "$SB/out"
echo fake-token > "$TOKEN_FILE"
cp "$TEMPLATE" "$TEST_DIR/consultant-config.template.json"
for r in aqua-rs-sdk aqua-spec aqua-governance-corpus aqua-ecosystem aqua-compliance inblockio.github.io; do
  mkdir -p "$REFS_BASE/$r"
done
TARGET='@did-key-zfaketestpeer:matrix.inblock.io'
AGENT_MXID='@0a1b2c3d4e5f6g7h:matrix.inblock.io'
TIM_MXID='@tim-operator:matrix.inblock.io'
# The allow-list every spawn in this file uses (AQUA_SYSTEM_ALLOWLIST); owner tests swap it.
# Default: Tim plus the peer already present, so the onboarding-flow cases are Owner no-ops.
AL="$SB/allowlist.toml"
cat > "$AL" <<EOF
# sandbox allow-list
[[recipients]]
name = "tim"
mxid = "$TIM_MXID"

[[recipients]]
name = "peer"
mxid = "$TARGET"
EOF
chmod 600 "$AL"

# podman/systemctl: in "print" mode any call is a failure; in "spawn" mode podman pretends
# (container exists -> no, run -> fake id) and systemctl succeeds. Every call is logged.
for tool in podman systemctl; do
  cat > "$SHIM_BIN/$tool" <<EOF
#!/bin/sh
echo "$tool \$*" >> "$SIDE_EFFECTS"
[ "\${SHIM_MODE:-print}" = spawn ] || { echo "!! shim: $tool must not be called" >&2; exit 1; }
case "$tool \$1 \$2" in
  "podman container exists") exit 1 ;;
  "podman run -d") echo 0123456789abcdef0123; exit 0 ;;
esac
exit 0
EOF
  chmod +x "$SHIM_BIN/$tool"
done

# Recorder for Tim's notifier: one JSON array of its argv per call.
cat > "$SB/notify" <<EOF
#!/usr/bin/env python3
import json, sys
open("$NOTIFY_LOG", "a").write(json.dumps(sys.argv[1:]) + "\n")
EOF
chmod +x "$SB/notify"

# Fake bridge MCP server. Mode from FAKE_BRIDGE_MODE; logs every request it reads.
cat > "$SB/fake-bridge" <<EOF
#!/usr/bin/env python3
import json, os, sys, time
mode = os.environ.get("FAKE_BRIDGE_MODE", "delivered")
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def result(i, text, err): out({"jsonrpc": "2.0", "id": i, "result": {"content": [{"type": "text", "text": text}], "isError": err}})
for line in sys.stdin:
    req = json.loads(line)
    open("$BRIDGE_LOG", "a").write(json.dumps(req) + "\n")
    if req.get("method") == "initialize":
        out({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}}})
    elif req.get("method") == "tools/call":
        i, to = req["id"], req["params"]["arguments"]["to"]
        if mode == "delivered":
            result(i, f"Delivered to peer ({to}) as Matrix event \$AbC-123_xyz. inbox_seq=7 (pass as after_seq to wait_for_reply to wait for an answer to this message).", False)
        elif mode == "refused":
            result(i, f'REFUSED: "{to}" is not on the Aqua System allow-list. Allowed recipients: [tim]; allowed rooms: []. To add someone, append a [[recipients]] entry.', True)
        elif mode == "refused-load":
            result(i, "REFUSED: the allow-list failed to load (parse error); no one can be messaged until /x/allowlist.toml is fixed", True)
        elif mode == "refused-other":
            result(i, f"REFUSED: {to} is a room the bridge never sends to.", True)
        elif mode == "tool-error":
            result(i, "bridge daemon not reachable at /run/user/1000/bridge.sock: connection refused", True)
        elif mode == "rpc-error":
            out({"jsonrpc": "2.0", "id": i, "error": {"code": -32602, "message": "unknown tool"}})
        elif mode == "hang":
            time.sleep(60)
        elif mode == "garbage":
            print("this is not json"); sys.exit(2)
EOF
chmod +x "$SB/fake-bridge"

# print_onb <name> [spawn args...]: --print-onboarding in the sandbox (shims in print mode).
print_onb() {
  local name="$1"; shift; local rc=0
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" \
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_SYSTEM_ALLOWLIST="$AL" \
    bash "$SPAWN" --print-onboarding "$@" > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  # Split the three sections for targeted assertions.
  awk -v d="$SB/out/$name" '/^==== peer welcome/{f=d".peer";next} /^==== Tim notice, delivered/{f=d".delivered";next} /^==== Tim notice, not delivered/{f=d".forward";next} f{print > f}' "$SB/out/$name.out"
  return "$rc"
}
EMDASH="$(printf '\xe2\x80\x94')"; ENDASH="$(printf '\xe2\x80\x93')"   # U+2014, U+2013 (locale-independent)
no_dashes() { ! grep -qF -e "$EMDASH" -e "$ENDASH" "$@"; }

echo "== --print-onboarding: persona + name, voice on"
print_onb a --label andreas --target "$TARGET" --persona Pelagia --name Andreas --voice on
check "exit 0" [ "$(cat "$SB/out/a.rc")" = 0 ]
check "greets by name" has "$SB/out/a.peer" "Hi Andreas! 👋"
check "persona intro" has "$SB/out/a.peer" "You now have your own Aqua Consultant, **Pelagia**. She is an AI assistant who knows Aqua inside out, and she is there just for you whenever you have a question."
check "how to start (persona)" bash -c 'grep -A1 -xF "**How to start**" "$1" | tail -n1 | grep -qxF "Open your chat with Pelagia and say hi. If it still shows as an invitation, accept it first; her welcome is already waiting there."' _ "$SB/out/a.peer"
check "peer text: no MXID, no app explanation" bash -c '! grep -qE "@[^ ]*:matrix\.inblock\.io|agent-mxid|Matrix chat app|address is" "$1"' _ "$SB/out/a.peer"
check "voice line present" has "$SB/out/a.peer" "- You can type, or send her voice messages."
check "one-to-one line" has "$SB/out/a.peer" "- The chat is one-to-one: Pelagia talks only with you."
check "ends with Enjoy" bash -c '[ "$(grep -v "^$" "$1" | tail -n1)" = "Enjoy! 🌊" ]' _ "$SB/out/a.peer"
check "delivered notice head" has "$SB/out/a.delivered" "✅ Onboarding delivered to Andreas directly (Aqua System DM, event \$<event-id>). Pelagia has also invited them to a chat."
check "delivered notice: no 'Nothing to forward'" hasnt "$SB/out/a.delivered" "Nothing to forward"
check "delivered notice quotes the welcome" has "$SB/out/a.delivered" "> Hi Andreas! 👋"
check "quoted blank lines are bare >" bash -c 'grep -qx ">" "$1" && ! grep -q "^> $" "$1"' _ "$SB/out/a.delivered"
check "forward notice head (example reason)" has "$SB/out/a.forward" "📋 Onboarding for Andreas: please forward the text between the lines. It was not sent directly: the Aqua System bridge failed (<short error>)."
check "print-onboarding previews the Owner step (already present)" has "$SB/out/a.err" "owner allow-list: Owner $TARGET already present as 'peer'"
check "forward notice has the text between two rules" bash -c '[ "$(grep -cx -- "----------" "$1")" = 2 ]' _ "$SB/out/a.forward"
check "section titles" bash -c 'grep -qF "\"onboarding sent: Andreas (aqua-agent-andreas-aqua-consultant-1)\"" "$1" && grep -qF "\"onboarding to forward: Andreas (aqua-agent-andreas-aqua-consultant-1)\"" "$1"' _ "$SB/out/a.out"
check "no em/en dash" no_dashes "$SB/out/a.out"

echo "== --print-onboarding: persona, pseudonymous peer, voice off"
print_onb b --label pseudo --target "$TARGET" --persona Pelagia --voice off
check "exit 0" [ "$(cat "$SB/out/b.rc")" = 0 ]
check "Hi there" has "$SB/out/b.peer" "Hi there! 👋"
check "no voice line" hasnt "$SB/out/b.peer" "voice messages"
check "delivered notice falls back to the peer" has "$SB/out/b.delivered" "✅ Onboarding delivered to the peer directly"
check "forward notice falls back to your contact" has "$SB/out/b.forward" "📋 Onboarding for your contact: please forward"
check "title falls back to the persona" has "$SB/out/b.out" '"onboarding sent: Pelagia (aqua-agent-pseudo-aqua-consultant-1)"'
check "no em/en dash" no_dashes "$SB/out/b.out"

echo "== --print-onboarding: legacy (no persona)"
print_onb c --label legacy --target "$TARGET" --display "Aqua Consultant" --name Bob
check "exit 0" [ "$(cat "$SB/out/c.rc")" = 0 ]
check "legacy intro" has "$SB/out/c.peer" "You now have your own **Aqua Consultant**. It is an AI assistant who knows Aqua inside out, and it is there just for you whenever you have a question."
check "legacy how to start" bash -c 'grep -A1 -xF "**How to start**" "$1" | tail -n1 | grep -qxF "Open your chat with your consultant and say hi. If it still shows as an invitation, accept it first; its welcome is already waiting there."' _ "$SB/out/c.peer"
check "legacy: no MXID, no app explanation" bash -c '! grep -qE "@[^ ]*:matrix\.inblock\.io|agent-mxid|Matrix chat app|address is" "$1"' _ "$SB/out/c.peer"
check "legacy go deep" has "$SB/out/c.peer" "if you are a developer it will happily go deep"
check "legacy one-to-one" has "$SB/out/c.peer" "- The chat is one-to-one: your consultant talks only with you."
check "legacy explains" has "$SB/out/c.peer" "- It explains and shows you where its answers come from. It cannot change anything or act on your behalf."
check "legacy source" has "$SB/out/c.peer" "- Like any AI, it can occasionally be wrong. When something matters, ask it for the source."
check "legacy: no she/her" bash -c '! grep -qwE "She|she|her" "$1"' _ "$SB/out/c.peer"
check "legacy: template has no voice -> no voice line" hasnt "$SB/out/c.peer" "voice messages"
check "legacy delivered notice" has "$SB/out/c.delivered" "The consultant has also invited them to a chat."
check "no em/en dash" no_dashes "$SB/out/c.out"

echo "== voice from the config, session MXID, --keep-config, forward-only"
mkdir -p "$TEST_DIR/kept-aqua-consultant-persist/store"
cat > "$TEST_DIR/kept-aqua-consultant-config.json" <<EOF
{"id": "kept-aqua-consultant-1", "target": "$TARGET", "display_name": "Pelagia (Aqua Consultant)", "voice": {"enabled": true}}
EOF
printf '[session]\nuser_id = "%s"\naccess_token = "mat_FAKE-SECRET"\n' "$AGENT_MXID" > "$TEST_DIR/kept-aqua-consultant-persist/store/config.toml"
print_onb d --label kept --keep-config --persona Pelagia --name Andreas
check "keep-config: exit 0" [ "$(cat "$SB/out/d.rc")" = 0 ]
check "keep-config: voice line from the config" has "$SB/out/d.peer" "- You can type, or send her voice messages."
check "keep-config: the session MXID stays out of the peer text" hasnt "$SB/out/d.peer" "$AGENT_MXID"
check "keep-config: no token material printed" hasnt "$SB/out/d.out" "FAKE-SECRET"
print_onb d2 --label kept --target "$TARGET" --persona Pelagia --name Andreas
check "re-render: voice line from the existing config" has "$SB/out/d2.peer" "send her voice messages"
print_onb d3 --label kept --target "$TARGET" --persona Pelagia --name Andreas --voice off
check "--voice off overrides the existing config" hasnt "$SB/out/d3.peer" "voice messages"
print_onb e --label fwd --target "$TARGET" --persona Pelagia --name Andreas --onboard-forward-only
check "forward-only reason" has "$SB/out/e.forward" "It was not sent directly: direct send disabled (--onboard-forward-only)."
check "no em/en dash" no_dashes "$SB/out/d.out" "$SB/out/e.out"
check "print-onboarding wrote no config" bash -c '! ls "$1"/andreas-* "$1"/pseudo-* "$1"/legacy-* "$1"/fwd-* >/dev/null 2>&1' _ "$TEST_DIR"
check "print-onboarding called no podman/systemctl" [ ! -e "$SIDE_EFFECTS" ]

echo "== onboard-send.py against the fake bridge"
send() { # send <mode> [env...]: runs the sender, prints "<rc> <line>"
  local mode="$1"; shift; local rc=0 out
  out="$(printf 'Hi Andreas! 👋\n\n"quoted" \\ back`tick`\n' | env AQUA_SYSTEM_BRIDGE_MCP="$SB/fake-bridge" FAKE_BRIDGE_MODE="$mode" "$@" python3 "$SENDER" "$TARGET")" || rc=$?
  printf '%s %s\n' "$rc" "$out"
}
rm -f "$BRIDGE_LOG"
check "delivered: rc 0 + event id" [ "$(send delivered)" = '0 DELIVERED $AbC-123_xyz' ]
check "delivered: send_message args verbatim" python3 - "$BRIDGE_LOG" "$TARGET" <<'PY'
import json, sys
reqs = [json.loads(l) for l in open(sys.argv[1])]
assert [r.get("method") for r in reqs] == ["initialize", "notifications/initialized", "tools/call"], reqs
p = reqs[2]["params"]
assert p["name"] == "send_message"
assert p["arguments"] == {"to": sys.argv[2], "markdown": 'Hi Andreas! 👋\n\n"quoted" \\ back`tick`\n', "from_label": "consultant onboarding"}, p
PY
check "allow-list refusal: rc 3 REFUSED" bash -c '[[ "$1" == "3 REFUSED \""*"is not on the Aqua System allow-list"* ]]' _ "$(send refused)"
check "other refusal: rc 3" bash -c '[[ "$1" == "3 REFUSED "* ]]' _ "$(send refused-other)"
check "tool error: rc 4 ERROR" bash -c '[[ "$1" == "4 ERROR bridge daemon not reachable"* ]]' _ "$(send tool-error)"
check "JSON-RPC error: rc 4" bash -c '[[ "$1" == "4 ERROR unknown tool" ]]' _ "$(send rpc-error)"
check "timeout: rc 4, says it may still arrive" bash -c '[[ "$1" == "4 ERROR no answer within 2s, it may still arrive"* ]]' _ "$(send hang ONBOARD_SEND_TIMEOUT=2)"
check "garbage: rc 4" bash -c '[[ "$1" == "4 ERROR no answer to send_message"* ]]' _ "$(send garbage)"
check "missing binary: rc 4" bash -c '[[ "$1" == "4 ERROR bridge client"*"missing"* ]]' _ "$(send delivered AQUA_SYSTEM_BRIDGE_MCP="$SB/nope")"

echo "== full --onboard spawn (podman shimmed, fake bridge, notifier recorded)"
# spawn_onb <name> <bridge-mode> [spawn args...]: a whole spawn; the agent session pre-exists.
spawn_onb() {
  local name="$1" mode="$2"; shift 2; local rc=0 label="s-$name"
  mkdir -p "$TEST_DIR/$label-aqua-consultant-persist/store"
  printf '[session]\nuser_id = "%s"\n' "$AGENT_MXID" > "$TEST_DIR/$label-aqua-consultant-persist/store/config.toml"
  rm -f "$NOTIFY_LOG" "$BRIDGE_LOG"
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" SHIM_MODE=spawn \
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
    CONSULTANT_NOTIFY="$SB/notify" AQUA_SYSTEM_BRIDGE_MCP="$SB/fake-bridge" FAKE_BRIDGE_MODE="$mode" \
    ONBOARD_SEND_TIMEOUT=5 AQUA_SYSTEM_ALLOWLIST="$AL" \
    bash "$SPAWN" --label "$label" --target "$TARGET" --persona Pelagia --name Andreas --no-refresh-refs "$@" \
    > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  # The onboarding notice = the last recorded notify call.
  tail -n1 "$NOTIFY_LOG" > "$SB/out/$name.notify" 2>/dev/null || true
}
onb_field() { python3 -c 'import json,sys; a=json.load(open(sys.argv[1])); print(a[int(sys.argv[2])])' "$SB/out/$1.notify" "$2"; }
export -f onb_field; export SB

spawn_onb f1 delivered --onboard --voice on
check "delivered: spawn exit 0" [ "$(cat "$SB/out/f1.rc")" = 0 ]
check "delivered: INFO + sent title" bash -c '[ "$(onb_field f1 1)" = INFO ] && [ "$(onb_field f1 3)" = "onboarding sent: Andreas (aqua-agent-s-f1-aqua-consultant-1)" ]'
check "delivered: body names the event" bash -c 'onb_field f1 4 | head -n1 | grep -qF "✅ Onboarding delivered to Andreas directly (Aqua System DM, event \$AbC-123_xyz). Pelagia has also invited them to a chat."'
check "delivered: quoted welcome has the new how-to-start and the voice line (config voice on), no MXID" \
  bash -c 'b="$(onb_field f1 4)"; [[ "$b" == *"> Open your chat with Pelagia and say hi."* && "$b" == *"> - You can type, or send her voice messages."* && "$b" != *"$0"* && "$b" != *"Nothing to forward"* ]]' "$AGENT_MXID"
check "delivered: the bridge got the same welcome" python3 - "$BRIDGE_LOG" "$AGENT_MXID" <<'PY'
import json, sys
md = [json.loads(l) for l in open(sys.argv[1])][2]["params"]["arguments"]["markdown"]
assert md.startswith("Hi Andreas! 👋\n") and sys.argv[2] not in md and "send her voice messages" in md, md
assert "Matrix chat app" not in md and "Open your chat with Pelagia and say hi." in md, md
assert "\u2014" not in md and "\u2013" not in md
PY
check "delivered notice body extracted" bash -c 'onb_field f1 4 > "$SB/out/f1.body"'
check "delivered notice body has no em/en dash" no_dashes "$SB/out/f1.body"

spawn_onb f2 refused --onboard
check "not allow-listed (unexpected): spawn exit 0" [ "$(cat "$SB/out/f2.rc")" = 0 ]
check "not allow-listed (unexpected): WARN + forward title" bash -c '[ "$(onb_field f2 1)" = WARN ] && [ "$(onb_field f2 3)" = "onboarding to forward: Andreas (aqua-agent-s-f2-aqua-consultant-1)" ]'
check "not allow-listed (unexpected): reason" bash -c 'onb_field f2 4 | head -n1 | grep -qF "It was not sent directly: $0 is not on the Aqua System allow-list (unexpected: the owner is added automatically at spawn; check allowlist.toml)."' "$TARGET"
check "not allow-listed: no voice line (template has none)" bash -c '! onb_field f2 4 | grep -q "voice messages"'

spawn_onb f3 tool-error --onboard
check "bridge error: spawn exit 0" [ "$(cat "$SB/out/f3.rc")" = 0 ]
check "bridge error: WARN + forward title" bash -c '[ "$(onb_field f3 1)" = WARN ] && [[ "$(onb_field f3 3)" == "onboarding to forward: "* ]]'
check "bridge error: reason carries the short error" bash -c 'onb_field f3 4 | head -n1 | grep -qF "It was not sent directly: the Aqua System bridge failed (bridge daemon not reachable at /run/user/1000/bridge.sock: connection refused)."'

spawn_onb f4 hang --onboard
check "bridge timeout: spawn exit 0, WARN" bash -c '[ "$(cat "$SB/out/f4.rc")" = 0 ] && [ "$(onb_field f4 1)" = WARN ]'

spawn_onb f5 refused-load --onboard
check "allow-list load failure: WARN" bash -c '[ "$(onb_field f5 1)" = WARN ] && onb_field f5 4 | head -n1 | grep -qF "the Aqua System bridge failed (the allow-list failed to load"'

spawn_onb f6 delivered --onboard-forward-only
check "forward-only: spawn exit 0" [ "$(cat "$SB/out/f6.rc")" = 0 ]
check "forward-only: bridge never called" [ ! -e "$BRIDGE_LOG" ]
check "forward-only: INFO + reason" bash -c '[ "$(onb_field f6 1)" = INFO ] && onb_field f6 4 | head -n1 | grep -qF "It was not sent directly: direct send disabled (--onboard-forward-only)."'

spawn_onb f7 delivered
check "no --onboard: bridge never called, only channel-up notified" bash -c '[ ! -e "$1" ] && [ "$(wc -l < "$2")" = 1 ]' _ "$BRIDGE_LOG" "$NOTIFY_LOG"

echo "== Owner allow-list (owner-allowlist.py; real spawns with podman shimmed)"
OWNER_HELPER="$SKILL_DIR/owner-allowlist.py"
OWNER='@1vo8g4vofiha69ua:matrix.inblock.io'
OTHER='@someoneelse00000:matrix.inblock.io'
TODAY="$(date +%F)"
mk_al() { # mk_al <file> [extra toml lines...]: Tim plus the extras, mode 600
  local f="$1"; shift
  { printf '# sandbox allow-list, comments must survive\n[[recipients]]\nname = "tim"\nmxid = "%s"\n' "$TIM_MXID"
    [ "$#" -eq 0 ] || printf '%s\n' "$@"; } > "$f"
  chmod 600 "$f"
}
# spawn_owner <name> <label> <allowlist> [spawn args...]: real spawn (no --onboard), Owner = $OWNER
spawn_owner() {
  local name="$1" label="$2" al="$3"; shift 3; local rc=0
  mkdir -p "$TEST_DIR/$label-aqua-consultant-persist/store"
  printf '[session]\nuser_id = "%s"\n' "$AGENT_MXID" > "$TEST_DIR/$label-aqua-consultant-persist/store/config.toml"
  rm -f "$SIDE_EFFECTS"
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" SHIM_MODE=spawn \
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
    CONSULTANT_NOTIFY="$SB/notify" AQUA_SYSTEM_BRIDGE_MCP="$SB/fake-bridge" AQUA_SYSTEM_ALLOWLIST="$al" \
    bash "$SPAWN" --label "$label" --target "$OWNER" --persona Pelagia --name Andreas --no-refresh-refs "$@" \
    > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
}
al_entry() { # al_entry <file> <mxid>: prints "name|note" of the entry with that MXID (exact), or NONE
  python3 - "$1" "$2" <<'PY'
import sys, tomllib
d = tomllib.load(open(sys.argv[1], "rb"))
hits = [r for r in d.get("recipients", []) if r["mxid"] == sys.argv[2]]
print("NONE" if not hits else "|".join([hits[0]["name"], hits[0].get("note", "")]) if len(hits) == 1 else "MULTIPLE")
PY
}
backups() { ls "$1".bak-* 2>/dev/null | wc -l; }
podman_ran() { grep -q '^podman run' "$SIDE_EFFECTS" 2>/dev/null; }
export -f al_entry backups
no_podman() { [ ! -e "$SIDE_EFFECTS" ] || ! grep -q '^podman' "$SIDE_EFFECTS"; }

# O1: add when missing
O1="$SB/o1.toml"; mk_al "$O1"
spawn_owner o1 andreas "$O1"
check "add: spawn exit 0 and launched" bash -c '[ "$(cat "$1")" = 0 ]' _ "$SB/out/o1.rc"
check "add: podman run happened after the Owner step" podman_ran
check "add: entry name=label, exact note" [ "$(al_entry "$O1" "$OWNER")" = "andreas|Owner of aqua-agent-andreas-aqua-consultant-1 (Pelagia), auto-added by spawn-consultant.sh $TODAY" ]
check "add: comments and Tim preserved" bash -c 'grep -q "comments must survive" "$1" && grep -q "name = \"tim\"" "$1"' _ "$O1"
check "add: file stays mode 600" [ "$(stat -c %a "$O1")" = 600 ]
check "add: exactly one backup, mode 600, equal to the old file" bash -c '[ "$(ls "$1".bak-andreas-* | wc -l)" = 1 ] && [ "$(stat -c %a "$1".bak-andreas-*)" = 600 ] && ! grep -q "$2" "$1".bak-andreas-*' _ "$O1" "$OWNER"
check "add: no temp file left behind" bash -c '! ls "$(dirname "$1")"/.allowlist.*.tmp >/dev/null 2>&1' _ "$O1"
check "add: stderr reports it" has "$SB/out/o1.err" "added Owner $OWNER as 'andreas'"
cp "$O1" "$SB/o1.after"
spawn_owner o1b andreas "$O1" --replace --keep-config
check "re-roll (--replace --keep-config): no-op, file byte-identical, no new backup" bash -c '[ "$(cat "$1")" = 0 ] && cmp -s "$2" "$3" && [ "$(ls "$2".bak-* | wc -l)" = 1 ]' _ "$SB/out/o1b.rc" "$O1" "$SB/o1.after"

# O2: present under a case difference -> no-op
O2="$SB/o2.toml"; mk_al "$O2" '[[recipients]]' 'name = "andreas-x"' "mxid = \"$(printf '%s' "$OWNER" | tr a-z A-Z | sed 's/:MATRIX.INBLOCK.IO/:matrix.inblock.io/')\""
cp "$O2" "$SB/o2.before"
spawn_owner o2 andreas "$O2"
check "case-insensitive present: exit 0, file untouched, no backup" bash -c '[ "$(cat "$1")" = 0 ] && cmp -s "$2" "$3" && [ "$(backups "$2")" = 0 ]' _ "$SB/out/o2.rc" "$O2" "$SB/o2.before"
check "case-insensitive present: says already present" has "$SB/out/o2.err" "already present as 'andreas-x'"

# O3: label taken (by a person with another MXID; rooms follow) -> <label>-owner, inserted contiguously
O3="$SB/o3.toml"; mk_al "$O3" '[[recipients]]' 'name = "Andreas"' "mxid = \"$OTHER\"" '' '# the rooms' '[[rooms]]' 'name = "daily"' 'room_id = "!abc:matrix.inblock.io"'
spawn_owner o3 andreas "$O3"
check "collision: exit 0, added as andreas-owner" bash -c '[ "$(cat "$1")" = 0 ] && [ "$(al_entry "$2" "$3" | cut -d"|" -f1)" = andreas-owner ]' _ "$SB/out/o3.rc" "$O3" "$OWNER"
check "collision: the other Andreas untouched" bash -c '[ "$(al_entry "$1" "$2" | cut -d"|" -f1)" = Andreas ]' _ "$O3" "$OTHER"
check "collision: new entry sits before the rooms comment and [[rooms]]" bash -c 'a=$(grep -n "andreas-owner" "$1" | cut -d: -f1); c=$(grep -n "^# the rooms" "$1" | cut -d: -f1); [ "$a" -lt "$c" ]' _ "$O3"

# O4: label and <label>-owner both taken (a room counts) -> abort before launch
O4="$SB/o4.toml"; mk_al "$O4" '[[recipients]]' 'name = "clash"' "mxid = \"$OTHER\"" '[[rooms]]' 'name = "CLASH-owner"' 'room_id = "!r:matrix.inblock.io"'
cp "$O4" "$SB/o4.before"
spawn_owner o4 clash "$O4"
check "both names taken: spawn exits non-zero" [ "$(cat "$SB/out/o4.rc")" != 0 ]
check "both names taken: no podman call at all" no_podman
check "both names taken: allow-list untouched, no backup" bash -c 'cmp -s "$1" "$2" && [ "$(backups "$1")" = 0 ]' _ "$O4" "$SB/o4.before"
check "both names taken: no config rendered" [ ! -e "$TEST_DIR/clash-aqua-consultant-config.json" ]
check "both names taken: clear error" bash -c 'grep -q "both taken" "$1" && grep -q "aborting: could not put the Owner" "$1"' _ "$SB/out/o4.err"

# O5: invalid existing file (duplicate name, like the bridge rejects) -> abort; missing file -> abort
O5="$SB/o5.toml"; mk_al "$O5" '[[recipients]]' 'name = "TIM"' "mxid = \"$OTHER\""
cp "$O5" "$SB/o5.before"
spawn_owner o5 invalid "$O5"
check "invalid file: exit non-zero, untouched, no podman" bash -c '[ "$(cat "$1")" != 0 ] && cmp -s "$2" "$3" && [ "$(backups "$2")" = 0 ]' _ "$SB/out/o5.rc" "$O5" "$SB/o5.before"
check "invalid file: no podman call" no_podman
check "invalid file: names the problem" has "$SB/out/o5.err" "duplicate name"
spawn_owner o5b missing "$SB/does-not-exist.toml"
check "missing file: exit non-zero, no podman, not created" bash -c '[ "$(cat "$1")" != 0 ] && [ ! -e "$2" ]' _ "$SB/out/o5b.rc" "$SB/does-not-exist.toml"
check "missing file: no podman call" no_podman

# O6: --replace aborts BEFORE the rm when the Owner cannot be ensured
spawn_owner o6 invalid "$O5" --replace --keep-config
check "--replace + bad allow-list: exit non-zero" [ "$(cat "$SB/out/o6.rc")" != 0 ]
check "--replace + bad allow-list: no podman rm (no podman at all)" no_podman

# O7: print modes write nothing, but preview
O7="$SB/o7.toml"; mk_al "$O7"; cp "$O7" "$SB/o7.before"
rc=0; env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" \
  CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
  AQUA_SYSTEM_ALLOWLIST="$O7" bash "$SPAWN" --print-run --label printrun --target "$OWNER" --persona Pelagia \
  > "$SB/out/o7.out" 2> "$SB/out/o7.err" || rc=$?
check "--print-run: exit 0, allow-list untouched, no backup, no lock file" bash -c '[ "$1" = 0 ] && cmp -s "$2" "$3" && [ "$(backups "$2")" = 0 ] && [ ! -e "$2.lock" ]' _ "$rc" "$O7" "$SB/o7.before"
check "--print-run: previews the entry" bash -c 'grep -q "would add to" "$1" && grep -qF "name = \"printrun\"" "$1"' _ "$SB/out/o7.err"
AL_SAVE="$AL"; AL="$O7"
print_onb o7b --label printonb --target "$OWNER" --persona Pelagia --name Andreas
AL="$AL_SAVE"
check "--print-onboarding: exit 0, allow-list untouched" bash -c '[ "$(cat "$1")" = 0 ] && cmp -s "$2" "$3" && [ "$(backups "$2")" = 0 ]' _ "$SB/out/o7b.rc" "$O7" "$SB/o7.before"
check "--print-onboarding: previews the entry on stderr, not in the copy" bash -c 'grep -qF "name = \"printonb\"" "$1" && ! grep -q "would add" "$2"' _ "$SB/out/o7b.err" "$SB/out/o7b.out"
check "print modes: podman never called" [ ! -e "$SIDE_EFFECTS" ] || ! grep -q '^podman run' "$SIDE_EFFECTS"

# O8: generic = Tim; never auto-added
O8="$SB/o8.toml"; mk_al "$O8"
check "generic: operator present -> exit 0" python3 "$OWNER_HELPER" apply --path "$O8" --mxid "$(printf '%s' "$TIM_MXID" | tr a-z A-Z | sed 's/:MATRIX.INBLOCK.IO/:matrix.inblock.io/')" --container aqua-agent-aqua-consultant-1 --who x --generic
check "generic: operator absent -> exit 1, nothing written" bash -c '! python3 "$1" apply --path "$2" --mxid "$3" --container c --who x --generic 2>/dev/null && [ "$(ls "$2".bak-* 2>/dev/null | wc -l)" = 0 ]' _ "$OWNER_HELPER" "$O8" "$OWNER"

# O9: concurrent applies (fleet roll) serialize on the lock
O9="$SB/o9.toml"; mk_al "$O9"
for i in 1 2 3 4; do
  python3 "$OWNER_HELPER" apply --path "$O9" --mxid "@conc$i:matrix.inblock.io" --container "c$i" --who x --label "conc$i" 2>/dev/null &
done
wait
check "concurrent: all four Owners present, file valid" python3 - "$O9" <<'PY'
import sys, tomllib
d = tomllib.load(open(sys.argv[1], "rb"))
names = sorted(r["name"] for r in d["recipients"])
assert names == ["conc1", "conc2", "conc3", "conc4", "tim"], names
PY
check "no em/en dash in any owner output" no_dashes "$SB"/out/o*.err

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
