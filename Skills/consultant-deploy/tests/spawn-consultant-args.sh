#!/usr/bin/env bash
#
# spawn-consultant-args.sh, no-side-effect tests for spawn-consultant.sh.
#
# Renders the podman argument vector with `--print-run` inside a throwaway sandbox and
# asserts on it. Nothing live is touched: HOME, the config/persist dir, the refs base,
# the token file and the Deepgram env file all point into a temp dir, and `podman` /
# `systemctl` are replaced by shims that record any call and fail, so a regression that
# reaches a side effect shows up as a failed test rather than a running container.
#
# Covers:
#   - bash -n on spawn-consultant.sh and roll-consultant-fleet.sh
#   - no Deepgram env file       -> no DEEPGRAM_API_KEY in the argv, voice notice printed
#   - fake env file (default path and AQUA_DEEPGRAM_ENV) -> exactly one bare `-e DEEPGRAM_API_KEY`,
#                                   the fake value never on stdout/stderr/argv/disk
#   - voice on in config, no key -> still renders (launches), loud warning on stderr
#   - --voice on / off / on again / off on absent block / bogus value
#   - --replace --keep-config without --voice preserves the voice block
#   - REFS_REPOS lists inblockio.github.io and the argv mounts it :ro (all six refs :ro)
#   - the podman/systemctl shims were never called
#
# Usage:  bash Skills/consultant-deploy/tests/spawn-consultant-args.sh
# Exit:   0 when every assertion passes, 1 otherwise (each failure is printed).
#
set -euo pipefail

SKILL_DIR="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
SPAWN="$SKILL_DIR/spawn-consultant.sh"
ROLL="$SKILL_DIR/roll-consultant-fleet.sh"
TEMPLATE="$SKILL_DIR/consultant-config.template.json"
REGISTRY_EXAMPLE="$SKILL_DIR/consultants.registry.example"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok   $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL $1" >&2; }
check() { # check <description> <command...>   (command's exit status decides)
  local desc="$1"; shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}

# ---------------------------------------------------------------- sandbox
SB="$(mktemp -d "${TMPDIR:-/tmp}/spawn-args-test.XXXXXX")"
trap 'rm -rf "$SB"' EXIT
FAKE_HOME="$SB/home"
TEST_DIR="$SB/aqua-matrix-test"
REFS_BASE="$SB/refs"
SHIM_BIN="$SB/bin"
SIDE_EFFECTS="$SB/side-effects.log"
TOKEN_FILE="$FAKE_HOME/.aqua-matrix-heartbeat/claude-oauth-token"
FAKE_TOKEN="fake-oauth-token-value-never-printed"
FAKE_KEY="FAKE-DEEPGRAM-KEY-MUST-NEVER-LEAK"

mkdir -p "$FAKE_HOME/.aqua-matrix-heartbeat" "$TEST_DIR" "$REFS_BASE" "$SHIM_BIN"
printf '%s\n' "$FAKE_TOKEN" > "$TOKEN_FILE"
cp "$TEMPLATE" "$TEST_DIR/consultant-config.template.json"
cp "$REGISTRY_EXAMPLE" "$TEST_DIR/consultants.registry"
# --print-run implies --no-refresh-refs, so presence is all that is checked: empty dirs suffice.
for r in aqua-rs-sdk aqua-spec aqua-governance-corpus aqua-ecosystem aqua-compliance inblockio.github.io; do
  mkdir -p "$REFS_BASE/$r"
done
# Shims: any call is a test failure (recorded), and they fail loudly.
for tool in podman systemctl; do
  cat > "$SHIM_BIN/$tool" <<EOF
#!/bin/sh
echo "SIDE EFFECT: $tool \$*" >> "$SIDE_EFFECTS"
echo "!! test shim: $tool must never be called under --print-run" >&2
exit 1
EOF
  chmod +x "$SHIM_BIN/$tool"
done

TARGET='@did-key-zfaketestpeer:matrix.inblock.io'

# run <name> [spawn args...]: runs the spawner in the sandbox, stores stdout/stderr/rc/argv
# under $SB/out/<name>.*, and returns the spawner's exit status. Extra env can be passed by
# setting RUN_ENV (an array of NAME=value) before the call.
RUN_ENV=()
run() {
  local name="$1"; shift
  mkdir -p "$SB/out"
  local rc=0
  env -i \
    HOME="$FAKE_HOME" \
    PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" \
    CONSULTANT_TEST_DIR="$TEST_DIR" \
    CONSULTANT_REFS_BASE="$REFS_BASE" \
    AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
    ${RUN_ENV[@]+"${RUN_ENV[@]}"} \
    bash "$SPAWN" --print-run "$@" \
    > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  # The argv is everything from the line that is exactly `podman` to the end of stdout.
  awk '/^podman$/{p=1} p' "$SB/out/$name.out" > "$SB/out/$name.argv"
  return "$rc"
}
argv_has_line()   { grep -qxF -- "$2" "$SB/out/$1.argv"; }
count_bare_env()  { # count_bare_env <name> <VAR>: occurrences of the pair "-e" / "<VAR>" in the argv
  awk -v v="$2" 'prev=="-e" && $0==v {n++} {prev=$0} END{print n+0}' "$SB/out/$1.argv"
}
no_leak() {       # no_leak <name> <secret>: the secret appears nowhere in outputs or the test dir
  ! grep -rqF -- "$2" "$SB/out/$1.out" "$SB/out/$1.err" "$SB/out/$1.argv" "$TEST_DIR"
}
cfg_voice() {     # cfg_voice <label>: prints the voice block as compact JSON, or ABSENT
  python3 - "$TEST_DIR/$1-aqua-consultant-config.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
print(json.dumps(c["voice"], sort_keys=True) if "voice" in c else "ABSENT")
PY
}
cfg_without_voice_sha() { # everything except the voice key, canonicalised, hashed
  python3 - "$TEST_DIR/$1-aqua-consultant-config.json" <<'PY'
import json, sys, hashlib
c = json.load(open(sys.argv[1])); c.pop("voice", None)
print(hashlib.sha256(json.dumps(c, sort_keys=True).encode()).hexdigest())
PY
}

echo "== syntax"
check "bash -n spawn-consultant.sh" bash -n "$SPAWN"
check "bash -n roll-consultant-fleet.sh" bash -n "$ROLL"

echo "== static: REFS_REPOS"
check "REFS_REPOS lists inblockio.github.io" \
  grep -qE '^REFS_REPOS=\(.*\binblockio\.github\.io\b.*\)' "$SPAWN"
check "REFS_REPOS lists exactly six repos" \
  bash -c 'n=$(grep -E "^REFS_REPOS=\(" "$1" | sed "s/^REFS_REPOS=(//; s/)$//" | wc -w); [ "$n" -eq 6 ]' _ "$SPAWN"

echo "== case A: no Deepgram env file"
rc=0; run a --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "argv starts with podman run -d" bash -c 'head -n3 "$1" | tr "\n" " " | grep -qx "podman run -d "' _ "$SB/out/a.argv"
check "no DEEPGRAM_API_KEY anywhere in argv" bash -c '! grep -q DEEPGRAM_API_KEY "$1"' _ "$SB/out/a.argv"
check "notice: no Deepgram env file, voice stays disabled" grep -q 'voice: no Deepgram env file' "$SB/out/a.out"
check "OAuth token passed bare (-e CLAUDE_CODE_OAUTH_TOKEN)" [ "$(count_bare_env a CLAUDE_CODE_OAUTH_TOKEN)" -eq 1 ]
check "OAuth token value never printed" no_leak a "$FAKE_TOKEN"
check "config rendered without a voice block" [ "$(cfg_voice alpha)" = "ABSENT" ]
check "no voice warning when voice is absent" bash -c '! grep -q "enabled in config but DEEPGRAM_API_KEY" "$1"' _ "$SB/out/a.err"

echo "== case B: fake Deepgram env file at the default path"
mkdir -p "$FAKE_HOME/.aqua-secrets"
printf 'DEEPGRAM_API_KEY=%s\n' "$FAKE_KEY" > "$FAKE_HOME/.aqua-secrets/deepgram.env"
rc=0; run b --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "exactly one bare -e DEEPGRAM_API_KEY" [ "$(count_bare_env b DEEPGRAM_API_KEY)" -eq 1 ]
check "never -e DEEPGRAM_API_KEY=<value>" bash -c '! grep -q "DEEPGRAM_API_KEY=" "$1"' _ "$SB/out/b.argv"
check "fake key value never leaks (stdout/stderr/argv/test dir)" no_leak b "$FAKE_KEY"
check "notice: key available by reference" grep -q 'voice: DEEPGRAM_API_KEY available' "$SB/out/b.out"
rm -f "$FAKE_HOME/.aqua-secrets/deepgram.env"

echo "== case B2: AQUA_DEEPGRAM_ENV override"
printf 'export DEEPGRAM_API_KEY="%s"\n' "$FAKE_KEY" > "$SB/elsewhere.env"
RUN_ENV=(AQUA_DEEPGRAM_ENV="$SB/elsewhere.env")
rc=0; run b2 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
RUN_ENV=()
check "exit 0" [ "$rc" -eq 0 ]
check "exactly one bare -e DEEPGRAM_API_KEY via override" [ "$(count_bare_env b2 DEEPGRAM_API_KEY)" -eq 1 ]
check "fake key value never leaks via override" no_leak b2 "$FAKE_KEY"

echo "== case B3: env file present but empty key"
printf 'DEEPGRAM_API_KEY=\n' > "$FAKE_HOME/.aqua-secrets/deepgram.env"
rc=0; run b3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "no DEEPGRAM_API_KEY in argv when the file yields nothing" bash -c '! grep -q DEEPGRAM_API_KEY "$1"' _ "$SB/out/b3.argv"
check "warning: file exists but yields no key" grep -q 'yields no DEEPGRAM_API_KEY' "$SB/out/b3.err"
rm -f "$FAKE_HOME/.aqua-secrets/deepgram.env"

echo "== case C: --voice on / off"
rc=0; run c1 --label alpha --target "$TARGET" --persona Thalia --name Tester --voice on || rc=$?
check "--voice on exits 0" [ "$rc" -eq 0 ]
check "--voice on: voice.enabled == true" [ "$(cfg_voice alpha)" = '{"enabled": true}' ]
check "--voice on without key: loud warning on stderr" grep -q '!! voice: enabled in config but DEEPGRAM_API_KEY is not available' "$SB/out/c1.err"
check "--voice on without key: still renders the argv (launch not blocked)" argv_has_line c1 'localhost/aqua-matrix-agent:poc'
before="$(cfg_without_voice_sha alpha)"
# Seed a sibling voice key by hand to prove --voice off preserves it.
python3 - "$TEST_DIR/alpha-aqua-consultant-config.json" <<'PY'
import json, sys
p = sys.argv[1]; c = json.load(open(p)); c["voice"]["tts_voice"] = "aura-2-thalia-en"
json.dump(c, open(p, "w"), indent=2, ensure_ascii=True); open(p, "a").write("\n")
PY
rc=0; run c2 --label alpha --target "$TARGET" --persona Thalia --name Tester --voice off || rc=$?
check "--voice off exits 0" [ "$rc" -eq 0 ]
check "--voice off: enabled false, sibling key kept" [ "$(cfg_voice alpha)" = '{"enabled": false, "tts_voice": "aura-2-thalia-en"}' ]
check "--voice off: no other key changed" [ "$(cfg_without_voice_sha alpha)" = "$before" ]
check "--voice off: no warning when disabled" bash -c '! grep -q "enabled in config but DEEPGRAM_API_KEY" "$1"' _ "$SB/out/c2.err"
rc=0; run c3 --label alpha --target "$TARGET" --persona Thalia --name Tester --voice off || rc=$?
check "--voice off again is idempotent (nothing written)" grep -q 'voice: already off' "$SB/out/c3.out"
rc=0; run c4 --label alpha --target "$TARGET" --persona Thalia --name Tester --voice on || rc=$?
check "--voice on flips back, sibling key kept" [ "$(cfg_voice alpha)" = '{"enabled": true, "tts_voice": "aura-2-thalia-en"}' ]
rc=0; run c5 --label alpha --target "$TARGET" --persona Thalia --name Tester --voice bogus || rc=$?
check "--voice bogus is rejected with exit 2" [ "$rc" -eq 2 ]
check "--voice bogus prints no argv" [ ! -s "$SB/out/c5.argv" ]

echo "== case C6: --voice off on a config with no voice block writes nothing (image-before-config)"
rc=0; run c6 --label beta --target "$TARGET" --persona Galene --name Other --voice off || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "voice key NOT injected" [ "$(cfg_voice beta)" = "ABSENT" ]
check "notice: already disabled, nothing written" grep -q 'no voice block' "$SB/out/c6.out"

echo "== case D: --replace --keep-config without --voice preserves the block"
rc=0; run d --replace --keep-config --label alpha || rc=$?
check "exit 0 (keep-config derives target/display from the config)" [ "$rc" -eq 0 ]
check "voice block untouched" [ "$(cfg_voice alpha)" = '{"enabled": true, "tts_voice": "aura-2-thalia-en"}' ]
check "argv still names the same config path" argv_has_line d "$TEST_DIR/alpha-aqua-consultant-config.json:/agent/config.json:ro"

echo "== case E: refs mounts"
check "argv mounts inblockio.github.io read-only" argv_has_line a "$REFS_BASE/inblockio.github.io:/refs/inblockio.github.io:ro"
check "all six /refs mounts present and :ro" bash -c '[ "$(grep -c "^$2/[^:]*:/refs/[^:]*:ro$" "$1")" -eq 6 ]' _ "$SB/out/a.argv" "$REFS_BASE"
check "no writable /refs mount" bash -c '! grep -E "^.*:/refs/[^:]*(:rw)?$" "$1" | grep -qv ":ro$"' _ "$SB/out/a.argv"

echo "== case F: --generic"
rc=0; run f --generic --target "$TARGET" --persona Sabrina --name Operator || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "generic container name" argv_has_line f "aqua-agent-aqua-consultant-1"

echo "== side effects"
check "podman/systemctl shims were never called" [ ! -e "$SIDE_EFFECTS" ]
check "no systemd unit written into the sandbox HOME" [ ! -d "$FAKE_HOME/.config/systemd" ]
check "nothing written outside the sandbox test dir (live ~/.aqua-matrix-test untouched)" \
  bash -c '! ls "$HOME/.aqua-matrix-test/alpha-aqua-consultant-config.json" "$HOME/.aqua-matrix-test/beta-aqua-consultant-config.json" >/dev/null 2>&1'

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
