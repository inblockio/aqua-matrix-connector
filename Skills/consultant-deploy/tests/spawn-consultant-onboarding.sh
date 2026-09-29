#!/usr/bin/env bash
#
# spawn-consultant-onboarding.sh, offline tests for the welcome + --onboard flow of
# spawn-consultant.sh and consultant-persona.py, and for the Owner allow-list step.
#
# Nothing live is touched: HOME, the config/persist dir and the refs base point into a temp
# dir, Tim's notifier is a recorder (CONSULTANT_NOTIFY), and podman/systemctl are shims (the
# podman shim can play the agent: write the greeted marker after `run`, or serve canned
# `logs`). No message leaves the machine.
#
# Covers:
#   - hello_for: persona + name, pseudonymous ("Hi there", ends with the name question), voice
#     line on/off, exact copy, no U+2014/U+2013; derive() on the new AND the old hello texts
#   - render/refresh: the hello's voice line follows the config's final voice.enabled (--voice,
#     else the base config); --keep-config leaves the hello byte-identical (also with --voice)
#   - --print-onboarding: hello + both Tim notices for persona+name, pseudonymous, legacy,
#     --keep-config (hello verbatim, persona/person derived for the notices), voice sources; no
#     side effects (no config written, shims never called)
#   - full --onboard spawns (podman shimmed): marker appears -> INFO "welcome delivered" with
#     the hello quoted; relay failure line -> WARN "welcome NOT confirmed" carrying the line;
#     plain timeout -> WARN; marker already there before launch -> no onboarding notice at
#     all; the spawn exits 0 in every case
#   - no code path references the Aqua System bridge MCP / the removed direct send any more
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
HELPER="$SKILL_DIR/consultant-persona.py"
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
SIDE_EFFECTS="$SB/side-effects.log"; NOTIFY_LOG="$SB/notify.jsonl"
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
# (container exists -> no, run -> fake id, logs -> $SHIM_LOGS) and systemctl succeeds. With
# SHIM_HELLO=deliver, `podman run` plays the agent: one second later it writes the greeted
# marker into the host dir mounted at /agent/memory. Every call is logged.
cat > "$SHIM_BIN/podman" <<'EOF'
#!/bin/sh
echo "podman $*" >> "@SIDE@"
[ "${SHIM_MODE:-print}" = spawn ] || { echo "!! shim: podman must not be called" >&2; exit 1; }
case "$1 $2" in
  "container exists") exit 1 ;;
  "run -d")
    if [ "${SHIM_HELLO:-}" = deliver ]; then
      mem=""
      for a in "$@"; do case "$a" in *:/agent/memory:U) mem="${a%:/agent/memory:U}" ;; esac; done
      ( sleep 1; echo 1 > "$mem/.whats_new_seen" ) >/dev/null 2>&1 </dev/null &
    fi
    echo 0123456789abcdef0123; exit 0 ;;
  logs*) [ -z "${SHIM_LOGS:-}" ] || cat "$SHIM_LOGS"; exit 0 ;;
esac
exit 0
EOF
cat > "$SHIM_BIN/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >> "@SIDE@"
[ "${SHIM_MODE:-print}" = spawn ] || { echo "!! shim: systemctl must not be called" >&2; exit 1; }
exit 0
EOF
sed -i "s|@SIDE@|$SIDE_EFFECTS|" "$SHIM_BIN/podman" "$SHIM_BIN/systemctl"
chmod +x "$SHIM_BIN/podman" "$SHIM_BIN/systemctl"

# Recorder for Tim's notifier: one JSON array of its argv per call.
cat > "$SB/notify" <<EOF
#!/usr/bin/env python3
import json, sys
open("$NOTIFY_LOG", "a").write(json.dumps(sys.argv[1:]) + "\n")
EOF
chmod +x "$SB/notify"

EMDASH="$(printf '\xe2\x80\x94')"; ENDASH="$(printf '\xe2\x80\x93')"   # U+2014, U+2013 (locale-independent)
no_dashes() { ! grep -qF -e "$EMDASH" -e "$ENDASH" "$@"; }
cfg_of() { echo "$TEST_DIR/$1-aqua-consultant-config.json"; }
cfg_hello() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("hello") or "")' "$(cfg_of "$1")"; }
export -f cfg_of cfg_hello; export TEST_DIR

echo "== hello_for / derive (consultant-persona.py)"
check "hello copy: persona + name + voice, exact" python3 - "$HELPER" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("cp", sys.argv[1]); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
want = """Hi Andreas! \U0001F30A I'm Pelagia, your own Aqua Consultant. I'm an AI assistant, and I'm here just for you whenever you have a question about Aqua.

In one sentence: Aqua is inblock.io's protocol for trust that travels with your data. Every signature, AI action and file carries its own proof, so anyone can check what happened.

**Ask me anything, in your own words.** You don't need a technical background, and if you're a developer I'm happy to go deep. For example:
- "What is Aqua, and why would I use it?"
- "How could Aqua help in my work?"
- "What's the difference between AquaFire, AquaNode and AquaAgents?"
- "Walk me through signing and verifying a document, step by step."
- "Show me where the SDK checks a signature."

**Good to know**
- I only talk with you; this chat is one-to-one.
- You can type, or send me a voice message.
- I explain things and show you where my answers come from. I can't change anything or act on your behalf.
- Like any AI, I can occasionally be wrong. When something matters, ask me for the source.

So, what would you like to explore first?"""
got = m.hello_for("Pelagia", "Andreas", True)
assert got == want, got
PY
check "hello: voice off drops exactly the voice line" python3 - "$HELPER" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("cp", sys.argv[1]); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
on, off = m.hello_for("Pelagia", "Andreas", True), m.hello_for("Pelagia", "Andreas", False)
assert "voice" not in off.lower(), off
assert on.replace("- You can type, or send me a voice message.\n", "") == off
assert m.hello_for("Pelagia", "Andreas") == off   # default: voice off
PY
check "hello: pseudonymous greets 'there' and ends with the name question" python3 - "$HELPER" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("cp", sys.argv[1]); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
h = m.hello_for("Pelagia", "", True)
assert h.startswith("Hi there! \U0001F30A I'm Pelagia, your own Aqua Consultant."), h
assert h.splitlines()[-1] == "Before we start, what should I call you?", h
assert "explore first" not in h
PY
check "hello: no em/en dash in any variant" python3 - "$HELPER" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("cp", sys.argv[1]); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
for p in ("Andreas", ""):
    for v in (True, False):
        h = m.hello_for("Pelagia", p, v)
        assert "—" not in h and "–" not in h, h
PY
check "derive(): new and old hello texts, named and pseudonymous" python3 - "$HELPER" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("cp", sys.argv[1]); m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
d = "Pelagia (Aqua Consultant)"
old_named = "Hi Andreas! \U0001F30A I'm Pelagia, your very own Aqua Consultant, and I'm genuinely happy you're here. So, whenever you're ready, what would you like to explore first?"
old_pseudo = "Hi there! \U0001F30A I'm Pelagia, your very own Aqua Consultant, and I'm genuinely happy you're here. First, though, what should I call you?"
cases = [
    (m.hello_for("Pelagia", "Andreas", True), ("Pelagia", "Andreas")),
    (m.hello_for("Pelagia", "Andreas", False), ("Pelagia", "Andreas")),
    (m.hello_for("Pelagia", "", True), ("Pelagia", "")),
    (old_named, ("Pelagia", "Andreas")),
    (old_pseudo, ("Pelagia", "")),
]
for h, want in cases:
    got = m.derive({"display_name": d, "hello": h})
    assert got == want, (h[:40], got)
assert m.derive({"display_name": "Aqua Consultant", "hello": "Hello {user_id}, I am"}) == ("", "")
PY

echo "== render / refresh: voice line follows the final voice.enabled; --keep-config verbatim"
# run_print <name> [spawn args...]: --print-run (renders the config, starts nothing)
run_print() {
  local name="$1"; shift; local rc=0
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" \
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
    AQUA_SYSTEM_ALLOWLIST="$AL" \
    bash "$SPAWN" --print-run "$@" > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
}
run_print r1 --label rv --target "$TARGET" --persona Pelagia --name Andreas --voice on
check "fresh render --voice on: voice.enabled true and hello has the voice line" bash -c '[ "$(cat "$1")" = 0 ] && cfg_hello rv | grep -qxF -- "- You can type, or send me a voice message." && python3 -c "import json,sys; assert json.load(open(sys.argv[1]))[\"voice\"][\"enabled\"] is True" "$(cfg_of rv)"' _ "$SB/out/r1.rc"
check "fresh render: hello starts with the new copy" bash -c 'cfg_hello rv | head -n1 | grep -qF "Hi Andreas! 🌊 I'"'"'m Pelagia, your own Aqua Consultant."'
run_print r2 --label rv --target "$TARGET" --persona Pelagia --name Andreas
check "re-render without --voice: voice read from the existing config, line kept" bash -c 'cfg_hello rv | grep -qF "send me a voice message"'
run_print r3 --label rv --target "$TARGET" --persona Pelagia --name Andreas --voice off
check "re-render --voice off: line gone" bash -c '! cfg_hello rv | grep -q "voice message"'
run_print r4 --label rvt --target "$TARGET" --persona Pelagia
check "template render (no voice block): no voice line, pseudonymous ending" bash -c '! cfg_hello rvt | grep -q "voice message" && [ "$(cfg_hello rvt | tail -n1)" = "Before we start, what should I call you?" ]'
# --keep-config: an existing consultant's hello (old copy) stays byte-identical, even with --voice on
python3 - "$(cfg_of kc)" "$TARGET" <<'PY'
import json, sys
json.dump({"id": "kc-aqua-consultant-1", "target": sys.argv[2], "display_name": "Pelagia (Aqua Consultant)",
           "hello": "Hi Andreas! \U0001F30A I'm Pelagia, your very own Aqua Consultant, and I'm genuinely happy you're here.",
           "memory": {"config_dir": "/agent/memory"}}, open(sys.argv[1], "w"), indent=2, ensure_ascii=True)
PY
KC_BEFORE="$(cfg_hello kc)"
run_print k1 --label kc --keep-config
check "--keep-config: hello byte-identical" [ "$(cfg_hello kc)" = "$KC_BEFORE" ]
run_print k2 --label kc --keep-config --voice on
check "--keep-config --voice on: voice patched, hello still byte-identical" bash -c '[ "$(cfg_hello kc)" = "$1" ] && grep -q "\"enabled\": true" "$(cfg_of kc)"' _ "$KC_BEFORE"
cp "$(cfg_of kc)" "$SB/kc.json"
run_print k3 --label kc --keep-config --refresh-prompt
check "--refresh-prompt: canonical hello, voice line from the config (on)" bash -c 'cfg_hello kc | head -n1 | grep -qF "your own Aqua Consultant." && cfg_hello kc | grep -qF "send me a voice message"'
cp "$SB/kc.json" "$(cfg_of kc)"   # back to the old hello for the print tests below

echo "== --print-onboarding"
# print_onb <name> [spawn args...]: --print-onboarding in the sandbox (shims in print mode).
print_onb() {
  local name="$1"; shift; local rc=0
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" \
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_SYSTEM_ALLOWLIST="$AL" \
    bash "$SPAWN" --print-onboarding "$@" > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  # Split the three sections for targeted assertions.
  awk -v d="$SB/out/$name" '/^==== consultant hello/{f=d".hello";next} /^==== Tim notice, delivered/{f=d".delivered";next} /^==== Tim notice, not confirmed/{f=d".unconfirmed";next} f{print > f}' "$SB/out/$name.out"
  return "$rc"
}
rm -f "$SIDE_EFFECTS"
print_onb a --label andreas --target "$TARGET" --persona Pelagia --name Andreas --voice on
check "exit 0" [ "$(cat "$SB/out/a.rc")" = 0 ]
check "hello = hello_for(Pelagia, Andreas, voice on)" bash -c 'diff <(sed "\$d" "$1") <(python3 -c "import importlib.util,sys; s=importlib.util.spec_from_file_location(\"cp\",sys.argv[1]); m=importlib.util.module_from_spec(s); s.loader.exec_module(m); print(m.hello_for(\"Pelagia\",\"Andreas\",True))" "$2") >/dev/null' _ "$SB/out/a.hello" "$HELPER"
check "section title names the voice line" has "$SB/out/a.out" "voice line on;"
check "delivered notice head" bash -c '[ "$(head -n1 "$1")" = "✅ Pelagia invited Andreas and posted her welcome." ]' _ "$SB/out/a.delivered"
check "delivered notice: 'This is what they see:' then the quote" bash -c 'sed -n 3p "$1" | grep -qxF "This is what they see:" && sed -n 5p "$1" | grep -qF "> Hi Andreas! 🌊 I'"'"'m Pelagia"' _ "$SB/out/a.delivered"
check "delivered notice: quoted blank lines are bare >" bash -c 'grep -qx ">" "$1" && ! grep -q "^> $" "$1"' _ "$SB/out/a.delivered"
check "delivered notice: voice line quoted" has "$SB/out/a.delivered" "> - You can type, or send me a voice message."
check "delivered notice: ends with the What's new marker" bash -c '[ "$(grep -v "^$" "$1" | tail -n1)" = "> *(followed by the \"What'"'"'s new\" list)*" ]' _ "$SB/out/a.delivered"
check "unconfirmed notice head" has "$SB/out/a.unconfirmed" "⚠️ Pelagia's welcome to Andreas was not confirmed within 180 s."
check "unconfirmed notice: retry hint" has "$SB/out/a.unconfirmed" "It retries on the next process start: podman restart aqua-agent-andreas-aqua-consultant-1."
check "section titles" bash -c 'grep -qF "INFO \"welcome delivered: Andreas (aqua-agent-andreas-aqua-consultant-1)\"" "$1" && grep -qF "WARN \"welcome NOT confirmed: Andreas (aqua-agent-andreas-aqua-consultant-1)\"" "$1"' _ "$SB/out/a.out"
check "print-onboarding previews the Owner step (already present)" has "$SB/out/a.err" "owner allow-list: Owner $TARGET already present as 'peer'"
check "no em/en dash" no_dashes "$SB/out/a.out"

print_onb b --label pseudo --target "$TARGET" --persona Pelagia --voice off
check "pseudonymous: exit 0, Hi there, name question, no voice line" bash -c '[ "$(cat "$1.rc")" = 0 ] && head -n1 "$1.hello" | grep -qF "Hi there! 🌊" && grep -qxF "Before we start, what should I call you?" "$1.hello" && ! grep -q "voice message" "$1.hello"' _ "$SB/out/b"
check "pseudonymous: notices fall back to the peer" bash -c 'head -n1 "$1.delivered" | grep -qxF "✅ Pelagia invited the peer and posted her welcome." && grep -qF "Pelagia'"'"'s welcome to the peer was not" "$1.unconfirmed"' _ "$SB/out/b"
check "pseudonymous: title falls back to the persona" has "$SB/out/b.out" '"welcome delivered: Pelagia (aqua-agent-pseudo-aqua-consultant-1)"'
check "no em/en dash" no_dashes "$SB/out/b.out"

print_onb c --label legacy --target "$TARGET" --display "Aqua Consultant" --name Bob
check "legacy: exit 0, the template hello (unchanged path)" bash -c '[ "$(cat "$1.rc")" = 0 ] && head -n1 "$1.hello" | grep -qF "Hello {user_id}, I am the Aqua Consultant"' _ "$SB/out/c"
check "legacy: neutral notices" bash -c 'head -n1 "$1.delivered" | grep -qxF "✅ The consultant invited Bob and posted its welcome." && grep -qF "The consultant'"'"'s welcome to Bob" "$1.unconfirmed"' _ "$SB/out/c"

print_onb d --label kc --keep-config
check "--keep-config: hello verbatim from the config" bash -c '[ "$(sed "\$d" "$1.hello")" = "$2" ]' _ "$SB/out/d" "$KC_BEFORE"
check "--keep-config: persona/person derived for the notices" bash -c 'head -n1 "$1.delivered" | grep -qxF "✅ Pelagia invited Andreas and posted her welcome." && grep -qF "\"welcome delivered: Andreas (aqua-agent-kc-aqua-consultant-1)\"" "$1.out"' _ "$SB/out/d"
check "--keep-config: section title says verbatim" has "$SB/out/d.out" "kept config, hello verbatim"
print_onb d2 --label rv --target "$TARGET" --persona Pelagia --name Andreas --voice on
print_onb d3 --label rv --target "$TARGET" --persona Pelagia --name Andreas
check "voice from the existing config (rv has it off now)" bash -c '! grep -q "voice message" "$1"' _ "$SB/out/d3.hello"
check "--voice on overrides the existing config" has "$SB/out/d2.hello" "- You can type, or send me a voice message."
check "print-onboarding wrote no config" bash -c '! ls "$1"/andreas-* "$1"/pseudo-* "$1"/legacy-* >/dev/null 2>&1' _ "$TEST_DIR"
check "print-onboarding called no podman/systemctl" [ ! -e "$SIDE_EFFECTS" ]

echo "== full --onboard spawn (podman shimmed, notifier recorded)"
# spawn_onb <name> [VAR=value...] -- [spawn args...]: a whole spawn; the agent session pre-exists.
spawn_onb() {
  local name="$1"; shift; local rc=0 label="s-$name" envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done; shift
  mkdir -p "$TEST_DIR/$label-aqua-consultant-persist/store"
  printf '[session]\nuser_id = "%s"\n' "$AGENT_MXID" > "$TEST_DIR/$label-aqua-consultant-persist/store/config.toml"
  rm -f "$NOTIFY_LOG"
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" SHIM_MODE=spawn \
    CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
    CONSULTANT_NOTIFY="$SB/notify" AQUA_SYSTEM_ALLOWLIST="$AL" ONBOARD_WAIT=10 "${envs[@]}" \
    bash "$SPAWN" --label "$label" --target "$TARGET" --no-refresh-refs "$@" \
    > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  # The onboarding notice = the last recorded notify call (the first is "channel up").
  tail -n1 "$NOTIFY_LOG" > "$SB/out/$name.notify" 2>/dev/null || true
  wc -l < "$NOTIFY_LOG" > "$SB/out/$name.count" 2>/dev/null || echo 0 > "$SB/out/$name.count"
}
onb_field() { python3 -c 'import json,sys; a=json.load(open(sys.argv[1])); print(a[int(sys.argv[2])])' "$SB/out/$1.notify" "$2"; }
export -f onb_field; export SB

T0=$SECONDS
spawn_onb f1 SHIM_HELLO=deliver -- --persona Pelagia --name Andreas --onboard --voice on
check "delivered: spawn exit 0, 2 notices (channel up + welcome), well before the wait cap" bash -c '[ "$(cat "$SB/out/f1.rc")" = 0 ] && [ "$(cat "$SB/out/f1.count")" = 2 ] && [ "$1" -lt 8 ]' _ "$((SECONDS - T0))"
check "delivered: INFO + title" bash -c '[ "$(onb_field f1 1)" = INFO ] && [ "$(onb_field f1 3)" = "welcome delivered: Andreas (aqua-agent-s-f1-aqua-consultant-1)" ]'
check "delivered: body head" bash -c '[ "$(onb_field f1 4 | head -n1)" = "✅ Pelagia invited Andreas and posted her welcome." ]'
check "delivered: quotes the config hello line by line (voice on) + What's new marker" python3 - "$SB/out/f1.notify" "$(cfg_of s-f1)" <<'PY'
import json, sys
body = json.load(open(sys.argv[1]))[4]
hello = json.load(open(sys.argv[2]))["hello"]
quoted = "\n".join(">" if l == "" else "> " + l for l in hello.split("\n"))
assert body == ("✅ Pelagia invited Andreas and posted her welcome.\n\nThis is what they see:\n\n"
                + quoted + "\n>\n> *(followed by the \"What's new\" list)*"), body
assert "> - You can type, or send me a voice message." in body
assert "—" not in body and "–" not in body
PY
check "delivered: stdout says so" has "$SB/out/f1.out" ">> onboarding: welcome delivered after"

FAILLOG="$SB/relay-fail.log"
printf '2026-09-29T12:00:00Z  INFO aqua_matrix_relay: claude-p: client cycle starting\n\033[33m2026-09-29T12:00:01Z  WARN\033[0m aqua_matrix_relay: claude-p: initiate-DM hello failed (retries next process start): M_FORBIDDEN `invite` refused\n' > "$FAILLOG"
spawn_onb f2 SHIM_LOGS="$FAILLOG" ONBOARD_WAIT=3 -- --persona Pelagia --name Andreas --onboard
check "relay failure line: spawn exit 0, reported on the terminal once" bash -c '[ "$(cat "$SB/out/f2.rc")" = 0 ] && [ "$(grep -c "relay reports a failed hello" "$1")" = 1 ]' _ "$SB/out/f2.err"
check "relay failure line: WARN + title" bash -c '[ "$(onb_field f2 1)" = WARN ] && [ "$(onb_field f2 3)" = "welcome NOT confirmed: Andreas (aqua-agent-s-f2-aqua-consultant-1)" ]'
check "relay failure line: body head, the line (ANSI stripped, backticks neutralised), retry hint" python3 - "$SB/out/f2.notify" <<'PY'
import json, sys
b = json.load(open(sys.argv[1]))[4].split("\n")
assert b[0] == "\u26a0\ufe0f Pelagia's welcome to Andreas was not confirmed within 3 s.", b[0]
assert b[2] == "Last relay log line: `2026-09-29T12:00:01Z  WARN aqua_matrix_relay: claude-p: initiate-DM hello failed (retries next process start): M_FORBIDDEN 'invite' refused`", b[2]
assert b[-1] == "It retries on the next process start: podman restart aqua-agent-s-f2-aqua-consultant-1.", b[-1]
assert "\x1b" not in "\n".join(b)
PY

spawn_onb f3 ONBOARD_WAIT=2 -- --persona Pelagia --name Andreas --onboard
check "timeout, no failure line: spawn exit 0, WARN, 'within 2 s', no log line" bash -c '[ "$(cat "$SB/out/f3.rc")" = 0 ] && [ "$(onb_field f3 1)" = WARN ] && b="$(onb_field f3 4)" && [ "$(printf "%s\n" "$b" | head -n1)" = "⚠️ Pelagia'"'"'s welcome to Andreas was not confirmed within 2 s." ] && [[ "$b" != *"Last relay log line"* ]]'

# already greeted: the marker exists before launch (roll of a greeted consultant) -> no onboarding notice
spawn_onb f4 -- --persona Pelagia --name Andreas
echo 1 > "$TEST_DIR/s-f4-aqua-consultant-persist/memory/.whats_new_seen"
H_BEFORE="$(cfg_hello s-f4)"
spawn_onb f4 SHIM_HELLO=deliver -- --replace --keep-config --onboard
check "already greeted: exit 0, only the channel-up notice" bash -c '[ "$(cat "$SB/out/f4.rc")" = 0 ] && [ "$(cat "$SB/out/f4.count")" = 1 ] && [[ "$(onb_field f4 3)" == "channel up: "* ]]'
check "already greeted: says nothing to send" has "$SB/out/f4.out" ">> onboarding: Andreas was already greeted earlier; nothing to send"
check "already greeted: --keep-config left the hello byte-identical" [ "$(cfg_hello s-f4)" = "$H_BEFORE" ]

spawn_onb f5 SHIM_HELLO=deliver -- --persona Pelagia --name Andreas
check "no --onboard: only channel-up notified" [ "$(cat "$SB/out/f5.count")" = 1 ]
check "no em/en dash in any spawn output or notice" no_dashes "$SB"/out/f*.out "$SB"/out/f*.err "$NOTIFY_LOG"

echo "== the Aqua System peer send is gone"
check "onboard-send.py deleted" [ ! -e "$SKILL_DIR/onboard-send.py" ]
check "no script references the bridge MCP or the direct send" bash -c '! grep -nE "aqua-system-bridge-mcp|AQUA_SYSTEM_BRIDGE_MCP|onboard-send|ONBOARD_SEND_TIMEOUT|onboard-forward-only|send_message|ONBOARD_DIRECT" "$1"/*.sh "$1"/*.py' _ "$SKILL_DIR"
check "--onboard-forward-only is now an unknown flag" bash -c '! env -i HOME="$1" PATH=/usr/bin:/bin bash "$2" --print-onboarding --label x --target "$3" --persona P --onboard-forward-only >/dev/null 2>&1' _ "$FAKE_HOME" "$SPAWN" "$TARGET"

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
    CONSULTANT_NOTIFY="$SB/notify" AQUA_SYSTEM_ALLOWLIST="$al" \
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
