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
#     allow-listed -> INFO "onboarding to forward", bridge error -> WARN, forward-only never
#     calls the bridge; the spawn exits 0 in every case
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
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" \
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
check "how to start names the persona" has "$SB/out/a.peer" "Pelagia has sent you a chat invitation in Element, your Matrix chat app. Accept it, and her welcome message is already waiting for you."
check "placeholder MXID when no session" has "$SB/out/a.peer" 'her address is `@<agent-mxid>:matrix.inblock.io`.'
check "voice line present" has "$SB/out/a.peer" "- You can type, or send her voice messages."
check "one-to-one line" has "$SB/out/a.peer" "- The chat is one-to-one: Pelagia talks only with you."
check "ends with Enjoy" bash -c '[ "$(grep -v "^$" "$1" | tail -n1)" = "Enjoy! 🌊" ]' _ "$SB/out/a.peer"
check "delivered notice head" has "$SB/out/a.delivered" "✅ Onboarding delivered to Andreas directly (Aqua System DM, event \$<event-id>). Pelagia has also invited them to a chat. Nothing to forward."
check "delivered notice quotes the welcome" has "$SB/out/a.delivered" "> Hi Andreas! 👋"
check "quoted blank lines are bare >" bash -c 'grep -qx ">" "$1" && ! grep -q "^> $" "$1"' _ "$SB/out/a.delivered"
check "forward notice head (allow-list example reason)" has "$SB/out/a.forward" "📋 Onboarding for Andreas: please forward the text between the lines. It was not sent directly: $TARGET is not on the Aqua System allow-list."
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
check "legacy how to start" has "$SB/out/c.peer" "Your consultant has sent you a chat invitation in Element, your Matrix chat app. Accept it, and its welcome message is already waiting for you. If you ever need to find it, its address is"
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
check "keep-config: MXID from the session" has "$SB/out/d.peer" "her address is \`$AGENT_MXID\`."
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
    ONBOARD_SEND_TIMEOUT=5 \
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
check "delivered: body names the event" bash -c 'onb_field f1 4 | head -n1 | grep -qF "✅ Onboarding delivered to Andreas directly (Aqua System DM, event \$AbC-123_xyz). Pelagia has also invited them to a chat. Nothing to forward."'
check "delivered: welcome carries the real MXID and the voice line (config voice on)" \
  bash -c 'b="$(onb_field f1 4)"; [[ "$b" == *"> Pelagia has sent you"*"$0"* && "$b" == *"> - You can type, or send her voice messages."* ]]' "$AGENT_MXID"
check "delivered: the bridge got the same welcome" python3 - "$BRIDGE_LOG" "$AGENT_MXID" <<'PY'
import json, sys
md = [json.loads(l) for l in open(sys.argv[1])][2]["params"]["arguments"]["markdown"]
assert md.startswith("Hi Andreas! 👋\n") and f"`{sys.argv[2]}`" in md and "send her voice messages" in md, md
assert "\u2014" not in md and "\u2013" not in md
PY
check "delivered notice body extracted" bash -c 'onb_field f1 4 > "$SB/out/f1.body"'
check "delivered notice body has no em/en dash" no_dashes "$SB/out/f1.body"

spawn_onb f2 refused --onboard
check "not allow-listed: spawn exit 0" [ "$(cat "$SB/out/f2.rc")" = 0 ]
check "not allow-listed: INFO + forward title" bash -c '[ "$(onb_field f2 1)" = INFO ] && [ "$(onb_field f2 3)" = "onboarding to forward: Andreas (aqua-agent-s-f2-aqua-consultant-1)" ]'
check "not allow-listed: reason" bash -c 'onb_field f2 4 | head -n1 | grep -qF "It was not sent directly: $0 is not on the Aqua System allow-list."' "$TARGET"
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

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
