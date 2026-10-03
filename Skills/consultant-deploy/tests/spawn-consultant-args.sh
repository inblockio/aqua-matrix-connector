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
#   - model pin: the template carries model, a fresh render inherits it, a persona re-render and
#     --refresh-prompt keep an existing config's model and never inject one
#   - MXID resolution (case H): --target did:... resolves through a mock siwx-oidc /resolve
#     (opaque answer used verbatim; no-account warning; 404 old server, 502 and a dead port all
#     FAIL with no legacy-form fallback), --siwx-url/--matrix-url reach the argv only when set,
#     --print-mxid reads [session] user_id and nothing else, and the script holds no DID->MXID
#     string derivation
#   - host block (case I): strict validation before any side effect (unknown key, `host` not an
#     object, a list that is not a list, every bad or reserved name in extra_refs and refs_follow:
#     exit 2, config byte-identical, no persist dir, no argv); host.extra_refs (duplicates, a fleet
#     repo skipped with a note) mounted :ro at /refs/<repo> from the default mirror root; the former
#     <key>-extra-refs.list is ignored (no fallback); the script's own sync_extra_refs (extracted)
#     clones single-branch, fast-forwards, and aborts on untracked / ignored / modified files, a
#     failed clone, a diverged mirror and a missing mirror under no-refresh
#   - rooms (case I): <key>-rooms -> /agent/rooms:ro plus <key>-room-state (created, kept across
#     re-spawns) at /agent/room-state with the same option as /agent/memory; --generic uses the
#     key "generic"; a consultant with neither gets exactly today's argv
#   - host.refs_follow (case J): the follow mirror replaces the fleet mount IN PLACE (exact /refs
#     sequence), a non-fleet followed repo is added, one /refs/_developments/<repo> dir mount each,
#     directory sources only, followed names skipped from extra_refs, an upstream commit is never
#     pulled by the spawn, other consultants keep the fleet mount; missing mirror, missing digest
#     dir, not a clone, untracked / ignored / modified files: exit 1 with the
#     `systemctl --user start consultant-refs-follow.service` hint and nothing changed
#   - preservation (case K): host, invite_policy and rooms keep their exact bytes through a persona
#     render, the --keep-config avatar_path patch, --voice on/off and --refresh-prompt
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
cfg_model() {     # cfg_model <label>: prints the model value, or ABSENT
  python3 - "$TEST_DIR/$1-aqua-consultant-config.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
print(c["model"] if "model" in c else "ABSENT")
PY
}
set_model() {     # set_model <label> <value|ABSENT>: edit the config's model in place
  python3 - "$TEST_DIR/$1-aqua-consultant-config.json" "$2" <<'PY'
import json, sys
p, v = sys.argv[1], sys.argv[2]; c = json.load(open(p))
if v == "ABSENT": c.pop("model", None)
else: c["model"] = v
json.dump(c, open(p, "w"), indent=2, ensure_ascii=True); open(p, "a").write("\n")
PY
}
cfg_without_voice_sha() { # everything except the voice key, canonicalised, hashed; the hello's
  # voice line follows voice.enabled on a render (by design), so it is removed before hashing
  python3 - "$TEST_DIR/$1-aqua-consultant-config.json" <<'PY'
import json, sys, hashlib
c = json.load(open(sys.argv[1])); c.pop("voice", None)
c["hello"] = c.get("hello", "").replace("- You can type, or send me a voice message.\n", "")
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
check "--voice off: no other key changed (hello voice line aside)" [ "$(cfg_without_voice_sha alpha)" = "$before" ]
check "--voice off: the re-rendered hello dropped its voice line" bash -c '! grep -q "send me a voice message" "$1"' _ "$TEST_DIR/alpha-aqua-consultant-config.json"
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

echo "== case G: model pin"
check "template pins model claude-opus-5-5" \
  bash -c 'python3 -c "import json,sys; sys.exit(json.load(open(sys.argv[1])).get(\"model\") != \"claude-opus-5-5\")" "$1"' _ "$TEMPLATE"
check "fresh render from the template carries the model (beta)" [ "$(cfg_model beta)" = "claude-opus-5-5" ]
set_model alpha claude-test-model
rc=0; run g1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "persona re-render exits 0" [ "$rc" -eq 0 ]
check "persona re-render keeps an existing custom model" [ "$(cfg_model alpha)" = "claude-test-model" ]
rc=0; run g2 --replace --keep-config --refresh-prompt --label alpha || rc=$?
check "--refresh-prompt exits 0" [ "$rc" -eq 0 ]
check "--refresh-prompt keeps an existing custom model" [ "$(cfg_model alpha)" = "claude-test-model" ]
set_model alpha ABSENT
rc=0; run g3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "persona re-render never injects a model into a config without one" [ "$(cfg_model alpha)" = "ABSENT" ]
rc=0; run g4 --replace --keep-config --refresh-prompt --label alpha || rc=$?
check "--refresh-prompt never injects a model into a config without one" [ "$(cfg_model alpha)" = "ABSENT" ]
check "voice block survived the model cases" [ "$(cfg_voice alpha)" = '{"enabled": true, "tts_voice": "aura-2-thalia-en"}' ]

echo "== case E: refs mounts"
check "argv mounts inblockio.github.io read-only" argv_has_line a "$REFS_BASE/inblockio.github.io:/refs/inblockio.github.io:ro"
check "all six /refs mounts present and :ro" bash -c '[ "$(grep -c "^$2/[^:]*:/refs/[^:]*:ro$" "$1")" -eq 6 ]' _ "$SB/out/a.argv" "$REFS_BASE"
check "no writable /refs mount" bash -c '! grep -E "^.*:/refs/[^:]*(:rw)?$" "$1" | grep -qv ":ro$"' _ "$SB/out/a.argv"

echo "== case F: --generic"
rc=0; run f --generic --target "$TARGET" --persona Sabrina --name Operator || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "generic container name" argv_has_line f "aqua-agent-aqua-consultant-1"

echo "== case H: MXID resolution (never derived)"
MOCK_PORT_FILE="$SB/mock.port"
python3 - "$MOCK_PORT_FILE" > "$SB/mock.log" 2>&1 <<'MOCK' &
import json, sys, urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer
ANSWERS = {
    "did:key:zNewPeer": {"did": "did:key:zNewPeer", "mxid": "@0a1b2c3d4e5f6g7h:matrix.inblock.io",
                         "exists": True, "attested": True},
    "did:key:zNoAccount": {"did": "did:key:zNoAccount", "mxid": "@hhhhhhhhhhhhhhhh:matrix.inblock.io",
                           "exists": False, "attested": False},
}
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, code, body):
        b = json.dumps(body).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        did = urllib.parse.parse_qs(u.query).get("did", [""])[0]
        if u.path == "/resolve" and did in ANSWERS:
            self.send(200, ANSWERS[did])
        elif u.path == "/resolve":
            self.send(502, {"error": "upstream_error", "message": "homeserver unreachable"})
        else:
            self.send(404, {"error": "not_found"})
srv = HTTPServer(("127.0.0.1", 0), H)
open(sys.argv[1], "w").write(str(srv.server_address[1]))
srv.serve_forever()
MOCK
MOCK_PID=$!
trap 'kill "$MOCK_PID" 2>/dev/null; rm -rf "$SB"' EXIT
for _ in $(seq 1 50); do [ -s "$MOCK_PORT_FILE" ] && break; sleep 0.1; done
MOCK="http://127.0.0.1:$(cat "$MOCK_PORT_FILE")"
OPAQUE='@0a1b2c3d4e5f6g7h:matrix.inblock.io'
cfg_target() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["target"])' "$TEST_DIR/$1-aqua-consultant-config.json"; }

RUN_ENV=( CONSULTANT_SIWX_URL="$MOCK" )
rc=0; run h1 --label gamma --target 'did:key:zNewPeer' --persona Thalia --name Tester || rc=$?
check "DID target: exit 0" [ "$rc" -eq 0 ]
check "DID target: AGENT_TARGET is the server's opaque answer" argv_has_line h1 "AGENT_TARGET=$OPAQUE"
check "DID target: config target is the server's opaque answer" [ "$(cfg_target gamma)" = "$OPAQUE" ]
check "DID target: resolution reported on stderr" grep -qF "resolved via $MOCK/resolve -> $OPAQUE" "$SB/out/h1.err"
check "DID target: the legacy form appears nowhere" bash -c '! grep -rqi "did-key-znewpeer" "$1" "$2"' _ "$SB/out" "$TEST_DIR"

rc=0; run h2 --label delta --target 'did:key:zNoAccount' --persona Thalia || rc=$?
check "DID with no account: exit 0" [ "$rc" -eq 0 ]
check "DID with no account: loud warning" grep -qF "has NO account yet" "$SB/out/h2.err"
check "DID with no account: uses the server's answer" argv_has_line h2 "AGENT_TARGET=@hhhhhhhhhhhhhhhh:matrix.inblock.io"

RUN_ENV=( CONSULTANT_SIWX_URL="$MOCK/old-server" )
rc=0; run h3 --label epsilon --target 'did:key:zNewPeer' --persona Thalia || rc=$?
check "404 (siwx-oidc without /resolve): refused" [ "$rc" -ne 0 ]
check "404: names the missing route" grep -qF "no /resolve route" "$SB/out/h3.err"
check "404: says it will not fall back" grep -qF "NOT falling back" "$SB/out/h3.err"
check "404: no argv rendered" [ ! -s "$SB/out/h3.argv" ]
check "404: no config rendered" [ ! -e "$TEST_DIR/epsilon-aqua-consultant-config.json" ]

RUN_ENV=( CONSULTANT_SIWX_URL="$MOCK" )
rc=0; run h4 --label epsilon --target 'did:key:zUnknownUpstream' --persona Thalia || rc=$?
check "502 from the lookup: refused" [ "$rc" -ne 0 ]
check "502: reported" grep -qF "HTTP 502 homeserver unreachable" "$SB/out/h4.err"

RUN_ENV=( CONSULTANT_SIWX_URL="http://127.0.0.1:9" )
rc=0; run h5 --label epsilon --target 'did:pkh:eip155:1:0x4b23da593596d94035c57adf6c2454216449b1b2' --persona Thalia || rc=$?
check "lookup down: refused" [ "$rc" -ne 0 ]
check "lookup down: no legacy did-pkh guess anywhere" bash -c '! grep -rq "@did-pkh-eip155-1-0x4b23" "$1"' _ "$SB/out/h5.argv"

RUN_ENV=()
rc=0; run h6 --label gamma --target "$TARGET" --persona Thalia --siwx-url "$MOCK" --matrix-url https://dev.matrix.example || rc=$?
check "--siwx-url/--matrix-url: exit 0" [ "$rc" -eq 0 ]
check "--siwx-url reaches the argv" argv_has_line h6 "SIWX_URL=$MOCK"
check "--matrix-url reaches the argv" argv_has_line h6 "MATRIX_URL=https://dev.matrix.example"
check "no URL overrides in the default argv" bash -c '! grep -qE "^(SIWX|MATRIX)_URL=" "$1"' _ "$SB/out/a.argv"

mkdir -p "$TEST_DIR/zeta-aqua-consultant-persist/store"
cat > "$TEST_DIR/zeta-aqua-consultant-persist/store/config.toml" <<'TOML'
[oidc]
client_id = "client-FAKE-SECRET-1"
redirect_uri = "http://localhost:0/callback"

[session]
access_token = "mat_FAKE-SECRET-2"
user_id = "@0a1b2c3d4e5f6g7h:dev.matrix.inblock.io"
device_id = "AQUA_x"
expires_at_unix = 1
refresh_token = "mcr_FAKE-SECRET-3"
did = "did:key:z6MkFake"
TOML
rc=0; run h7 --print-mxid --label zeta || rc=$?
check "--print-mxid: exit 0" [ "$rc" -eq 0 ]
check "--print-mxid: prints exactly the session user_id" [ "$(cat "$SB/out/h7.out")" = "@0a1b2c3d4e5f6g7h:dev.matrix.inblock.io" ]
check "--print-mxid: no token material on stdout/stderr" bash -c '! grep -q "FAKE-SECRET" "$1" "$2"' _ "$SB/out/h7.out" "$SB/out/h7.err"
rc=0; run h8 --print-mxid --label nosession || rc=$?
check "--print-mxid with no session: exit 1" [ "$rc" -eq 1 ]

check "spawn-consultant.sh holds no DID->MXID string derivation" \
  bash -c '! grep -nE "did-key-%s|did-pkh-%s|tr .\[:upper:\]. .\[:lower:\].|tr .:. .-." "$1"' _ "$SPAWN"

echo "== case I: host block (strict validation), host.extra_refs, rooms"
# Mirrors go to the DEFAULT root under the sandbox HOME; the "GitHub" remote is a local dir of
# bare repos (CONSULTANT_REFS_REMOTE). --print-run never clones, so the clone/fetch half is
# exercised by running the script's own sync_extra_refs (extracted verbatim) in a subshell.
MIRROR="$FAKE_HOME/.local/share/consultant-refs"
REMOTE="$SB/remote"
GITC=(git -c user.name=test -c user.email=test@example.invalid)
mk_remote() {     # mk_remote <repo>: bare repo at $REMOTE/<repo>.git, main + a side branch
  local w="$SB/work/$1"
  git init -q -b main "$w"
  printf 'hello %s\n' "$1" > "$w/README.md"
  "${GITC[@]}" -C "$w" add README.md
  "${GITC[@]}" -C "$w" commit -q -m init
  "${GITC[@]}" -C "$w" branch side
  git clone -q --bare "$w" "$REMOTE/$1.git"
}
upstream_commit() { # upstream_commit <repo>: one more commit on main, pushed to the bare remote
  local w="$SB/work/$1"
  date +%s%N >> "$w/README.md"
  "${GITC[@]}" -C "$w" commit -q -am more
  git -C "$w" push -q "$REMOTE/$1.git" main
}
mkdir -p "$REMOTE"
mk_remote extra-one; mk_remote extra-two
SYNC_FN="$(sed -n '/^sync_extra_refs() {$/,/^}$/p' "$SPAWN")"
check "sync_extra_refs extracted from the script" grep -q 'git clone --quiet --single-branch' <(printf '%s\n' "$SYNC_FN")
sync() {          # sync <name> <refresh 0|1> <repo...>: the script's sync_extra_refs, sandboxed
  local name="$1" refresh="$2" rc=0; shift 2
  ( set -euo pipefail; eval "$SYNC_FN"
    REFS_MIRROR="$MIRROR"; REFS_REMOTE="$REMOTE"; REFRESH_REFS="$refresh"; EXTRA_REFS=("$@")
    sync_extra_refs ) > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  return "$rc"
}
rc=0; sync s1 1 extra-one extra-two || rc=$?
check "sync: missing mirrors are cloned (exit 0)" [ "$rc" -eq 0 ]
check "sync: clone reported" grep -q '>> extra refs: extra-one cloned (main at ' "$SB/out/s1.out"
check "sync: clone is single-branch (no side branch fetched)" \
  bash -c '[ "$(git -C "$1" for-each-ref --format="%(refname)" refs/remotes | grep -c side)" -eq 0 ]' _ "$MIRROR/extra-one"
upstream_commit extra-one
rc=0; sync s2 1 extra-one extra-two || rc=$?
check "sync: upstream ahead -> fast-forward (exit 0)" [ "$rc" -eq 0 ]
check "sync: fast-forward reported for extra-one only" \
  bash -c 'grep -q "extra-one fast-forwarded to" "$1" && ! grep -q extra-two "$1"' _ "$SB/out/s2.out"
check "sync: mirror HEAD == upstream main" \
  [ "$(git -C "$MIRROR/extra-one" rev-parse HEAD)" = "$(git -C "$REMOTE/extra-one.git" rev-parse main)" ]
rc=0; sync s3 1 extra-one extra-two || rc=$?
check "sync: up to date -> exit 0, silent" bash -c '[ "$1" -eq 0 ] && [ ! -s "$2" ]' _ "$rc" "$SB/out/s3.out"
touch "$MIRROR/extra-one/untracked.txt"
rc=0; sync s4 1 extra-one || rc=$?
check "sync: untracked file -> abort" bash -c '[ "$1" -ne 0 ] && grep -q "has LOCAL CHANGES" "$2"' _ "$rc" "$SB/out/s4.err"
rm -f "$MIRROR/extra-one/untracked.txt"
echo 'secret.env' >> "$MIRROR/extra-one/.git/info/exclude"; touch "$MIRROR/extra-one/secret.env"
rc=0; sync s5 0 extra-one || rc=$?
check "sync: IGNORED file -> abort (also under --no-refresh-refs)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "has LOCAL CHANGES" "$2"' _ "$rc" "$SB/out/s5.err"
rm -f "$MIRROR/extra-one/secret.env"
echo changed >> "$MIRROR/extra-one/README.md"
rc=0; sync s6 1 extra-one || rc=$?
check "sync: modified tracked file -> abort" [ "$rc" -ne 0 ]
git -C "$MIRROR/extra-one" checkout -q -- README.md
rc=0; sync s7 1 no-such-repo || rc=$?
check "sync: clone failure -> abort, reported" bash -c '[ "$1" -ne 0 ] && grep -q "clone FAILED for no-such-repo" "$2"' _ "$rc" "$SB/out/s7.err"
check "sync: failed clone leaves no dir behind" [ ! -e "$MIRROR/no-such-repo" ]
rc=0; sync s8 0 extra-three || rc=$?
check "sync: missing mirror under --no-refresh-refs -> abort, nothing cloned" \
  bash -c '[ "$1" -ne 0 ] && grep -q "mirror MISSING" "$2" && [ ! -e "$3" ]' _ "$rc" "$SB/out/s8.err" "$MIRROR/extra-three"
"${GITC[@]}" -C "$MIRROR/extra-two" commit -q --allow-empty -m local-only
upstream_commit extra-two
rc=0; sync s9 1 extra-two || rc=$?
check "sync: diverged mirror (non-fast-forward) -> abort" \
  bash -c '[ "$1" -ne 0 ] && grep -q "fetch / fast-forward FAILED for extra-two" "$2"' _ "$rc" "$SB/out/s9.err"
rm -rf "$MIRROR/extra-two"; sync s10 1 extra-two || true
check "sync: a deleted mirror is simply re-cloned" [ -d "$MIRROR/extra-two/.git" ]

cfg_of() { if [ "$1" = generic ]; then echo "$TEST_DIR/aqua-consultant-config.json"; else echo "$TEST_DIR/$1-aqua-consultant-config.json"; fi; }
put_host() {      # put_host <label|generic> <json|ABSENT>: set (or drop) the config's host block,
  # creating the config from the template first when missing, so the next run renders onto it
  local cfg; cfg="$(cfg_of "$1")"
  [ -f "$cfg" ] || cp "$TEST_DIR/consultant-config.template.json" "$cfg"
  python3 - "$cfg" "$2" <<'PY'
import json, sys
p, v = sys.argv[1], sys.argv[2]
c = json.load(open(p))
if v == "ABSENT": c.pop("host", None)
else: c["host"] = json.loads(v)
with open(p, "w") as f:
    json.dump(c, f, indent=2, ensure_ascii=True); f.write("\n")
PY
}
refs_seq() { awk 'prev=="-v" && /:\/refs\// {print} {prev=$0}' "$SB/out/$1.argv"; }   # /refs mounts, argv order
sha_of() { sha256sum < "$1" | cut -d' ' -f1; }

put_host iota '{"extra_refs": ["extra-one", "extra-two", "aqua-rs-sdk", "extra-one", "extra-two"]}'
rc=0; run i1 --label iota --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "host.extra_refs: exit 0" [ "$rc" -eq 0 ]
check "host.extra_refs: extra-one mounted :ro from the default mirror root" argv_has_line i1 "$MIRROR/extra-one:/refs/extra-one:ro"
check "host.extra_refs: extra-two mounted :ro from the default mirror root" argv_has_line i1 "$MIRROR/extra-two:/refs/extra-two:ro"
check "host.extra_refs: 6 fleet + 2 extra /refs mounts, each right after -v" \
  bash -c '[ "$(awk "prev==\"-v\" && /:\/refs\/[^:]*:ro\$/ {n++} {prev=\$0} END{print n+0}" "$1")" -eq 8 ]' _ "$SB/out/i1.argv"
check "host.extra_refs: fleet repo skipped with a note, mounted once from the fleet checkout" \
  bash -c 'grep -q "aqua-rs-sdk is already fleet-mounted (REFS_REPOS), skipped" "$1" && [ "$(grep -c ":/refs/aqua-rs-sdk:ro$" "$2")" -eq 1 ] && grep -qx "$3/aqua-rs-sdk:/refs/aqua-rs-sdk:ro" "$2"' \
  _ "$SB/out/i1.out" "$SB/out/i1.argv" "$REFS_BASE"
check "host.extra_refs: one summary line (duplicates dropped)" \
  grep -qxF ">> extra refs: 2 repo(s) from $MIRROR (as-is, freshness pass skipped), ro at /refs: extra-one extra-two" "$SB/out/i1.out"
check "host.extra_refs: no rooms mounts without a rooms dir" bash -c '! grep -q "/agent/room" "$1"' _ "$SB/out/i1.argv"
check "host block carried through the persona render unchanged" \
  python3 -c 'import json,sys; sys.exit(json.load(open(sys.argv[1]))["host"] != {"extra_refs": ["extra-one", "extra-two", "aqua-rs-sdk", "extra-one", "extra-two"]})' "$(cfg_of iota)"

# Strict validation: every failure exits 2 before ANY side effect (config byte-identical, no
# persist dir, no argv). kappa's config is created from the template and never rendered.
host_rejects() {  # host_rejects <description> <host json> <expected stderr fragment>
  rm -f "$(cfg_of kappa)"; put_host kappa "$2"
  local sha rc=0; sha="$(sha_of "$(cfg_of kappa)")"
  run i2 --label kappa --target "$TARGET" --persona Thalia || rc=$?
  check "$1: exit 2, named, no argv, config untouched, no persist dir" \
    bash -c '[ "$1" -eq 2 ] && grep -qF -- "$2" "$3" && grep -qF "nothing was changed" "$3" && [ ! -s "$4" ] && [ "$(sha256sum < "$5" | cut -d" " -f1)" = "$6" ] && [ ! -e "$7" ]' \
    _ "$rc" "$3" "$SB/out/i2.err" "$SB/out/i2.argv" "$(cfg_of kappa)" "$sha" "$TEST_DIR/kappa-aqua-consultant-persist"
}
for key in extra_refs refs_follow; do
  for bad in '"../etc"' '"a/b"' '".."' '"."' '"a..b"' '"has space"' '"x;y"' '"$(id)"' '""' '7' 'null'; do
    host_rejects "host.$key bad name $bad" "{\"$key\": [\"extra-one\", $bad]}" "host.$key[1]: invalid repo name"
  done
  host_rejects "host.$key reserved name _developments" "{\"$key\": [\"_developments\"]}" "host.$key[0]: '_developments' is reserved"
  host_rejects "host.$key not a list" "{\"$key\": \"extra-one\"}" "host.$key must be a list of repo names, got string"
done
host_rejects "unknown host key" '{"extra_refs": [], "refs_folow": ["aqua-rs-sdk"]}' 'unknown key(s) in `host`: refs_folow'
host_rejects "unknown host key (old design's interval)" '{"interval_minutes": 30}' 'unknown key(s) in `host`: interval_minutes'
for shape in '[]:array' '"x":string' 'null:null' '5:number' 'true:boolean'; do
  host_rejects "host not an object (${shape%:*})" "${shape%:*}" "\`host\` must be a JSON object, got ${shape##*:}"
done
rm -f "$(cfg_of kappa)"

# The former <key>-extra-refs.list host file is not read at all any more (no fallback).
printf 'extra-three\n' > "$TEST_DIR/nu-extra-refs.list"    # extra-three has no mirror: reading it would abort
rc=0; run i3 --label nu --target "$TARGET" --persona Thalia || rc=$?
check "list file ignored: exit 0, exactly the six fleet /refs mounts, no extra-refs note" \
  bash -c '[ "$1" -eq 0 ] && [ "$(grep -c ":/refs/" "$2")" -eq 6 ] && ! grep -q "extra refs" "$3" "$4"' \
  _ "$rc" "$SB/out/i3.argv" "$SB/out/i3.out" "$SB/out/i3.err"
put_host nu '{"extra_refs": ["extra-one"]}'
rc=0; run i3b --label nu --target "$TARGET" --persona Thalia || rc=$?
check "list file next to host.extra_refs: only the host names are mounted" \
  bash -c '[ "$1" -eq 0 ] && grep -qx "$2/extra-one:/refs/extra-one:ro" "$3" && ! grep -q extra-three "$3" && [ "$(grep -c ":/refs/" "$3")" -eq 7 ]' \
  _ "$rc" "$MIRROR" "$SB/out/i3b.argv"
check "spawn-consultant.sh has no list-file reader left" bash -c '! grep -nE "extra-refs\.list\"|EXTRA_REFS_LIST|read_extra_refs" "$1"' _ "$SPAWN"
rm -f "$TEST_DIR/nu-extra-refs.list"

put_host lambda '{"extra_refs": ["extra-three"]}'
rc=0; run i3c --label lambda --target "$TARGET" --persona Thalia || rc=$?
check "missing mirror under --print-run: abort, nothing cloned, no argv" \
  bash -c '[ "$1" -ne 0 ] && grep -q "mirror MISSING" "$2" && [ ! -e "$3" ] && [ ! -s "$4" ]' \
  _ "$rc" "$SB/out/i3c.err" "$MIRROR/extra-three" "$SB/out/i3c.argv"
touch "$MIRROR/extra-two/notes.txt"
rc=0; run i4 --label iota --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "dirty mirror under --print-run: abort, no argv" \
  bash -c '[ "$1" -ne 0 ] && grep -q "has LOCAL CHANGES" "$2" && [ ! -s "$3" ]' _ "$rc" "$SB/out/i4.err" "$SB/out/i4.argv"
rm -f "$MIRROR/extra-two/notes.txt"

mkdir -p "$TEST_DIR/iota-rooms"
rc=0; run i5 --label iota --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "rooms: exit 0" [ "$rc" -eq 0 ]
check "rooms: <key>-rooms mounted read-only at /agent/rooms" argv_has_line i5 "$TEST_DIR/iota-rooms:/agent/rooms:ro"
check "rooms: <key>-room-state created" [ -d "$TEST_DIR/iota-room-state" ]
check "rooms: room-state mounted at /agent/room-state with the SAME option as /agent/memory" \
  bash -c 'm="$(grep -E "^[^:]+:/agent/memory:" "$1" | sed "s/.*:\/agent\/memory//")"; [ -n "$m" ] && grep -qxF "$2:/agent/room-state$m" "$1"' \
  _ "$SB/out/i5.argv" "$TEST_DIR/iota-room-state"
echo keep > "$TEST_DIR/iota-room-state/note.md"
rc=0; run i6 --replace --keep-config --label iota || rc=$?
check "rooms: existing room-state kept across a re-spawn" bash -c '[ "$1" -eq 0 ] && [ "$(cat "$2")" = keep ]' _ "$rc" "$TEST_DIR/iota-room-state/note.md"
check "rooms: --fresh never touches room-state (its rm -rf names only STORE/MEM)" \
  bash -c '! grep -nE "rm -rf.*(ROOM|room)" "$1"' _ "$SPAWN"

put_host generic '{"extra_refs": ["extra-two"]}'; mkdir -p "$TEST_DIR/generic-rooms"
rc=0; run i7 --generic --target "$TARGET" --persona Sabrina --name Operator || rc=$?
check "generic: the generic config's host block and generic-rooms are used" \
  bash -c '[ "$1" -eq 0 ] && grep -qx "$2/extra-two:/refs/extra-two:ro" "$3" && grep -qx "$4/generic-rooms:/agent/rooms:ro" "$3"' \
  _ "$rc" "$MIRROR" "$SB/out/i7.argv" "$TEST_DIR"
put_host generic ABSENT; rm -rf "$TEST_DIR/generic-rooms" "$TEST_DIR/generic-room-state"

rc=0; run i8 --label mu --target "$TARGET" --persona Thalia || rc=$?
check "default (no host, no rooms dir): exactly the six fleet /refs mounts, no rooms, no notes" \
  bash -c '[ "$1" -eq 0 ] && [ "$(grep -c ":/refs/" "$2")" -eq 6 ] && ! grep -q "/agent/room" "$2" && ! grep -q "extra refs\|rooms:" "$3" "$4" && [ ! -e "$5" ]' \
  _ "$rc" "$SB/out/i8.argv" "$SB/out/i8.out" "$SB/out/i8.err" "$TEST_DIR/mu-room-state"
check "mirror root default is on disk (~/.local/share), never /tmp" \
  grep -qF 'REFS_MIRROR="${CONSULTANT_REFS_MIRROR:-$HOME/.local/share/consultant-refs}"' "$SPAWN"

echo "== case J: host.refs_follow (follow mirror IN PLACE of the fleet mount, digests, fail-closed)"
# The follow root is the DEFAULT under the sandbox HOME. Mirrors are clones of local bare remotes,
# standing in for what consultant-refs-follow.timer (aqua-ops) maintains; the spawn only reads them.
FOLLOW="$FAKE_HOME/.local/share/consultant-refs-follow"
mk_remote aqua-rs-sdk; mk_remote extra-four
mkdir -p "$FOLLOW/_developments/aqua-rs-sdk" "$FOLLOW/_developments/extra-four"
git clone -q --single-branch "$REMOTE/aqua-rs-sdk.git" "$FOLLOW/aqua-rs-sdk"
git clone -q --single-branch "$REMOTE/extra-four.git" "$FOLLOW/extra-four"
printf 'digest\n' > "$FOLLOW/_developments/aqua-rs-sdk/DEVELOPMENTS.md"
FHEAD="$(git -C "$FOLLOW/aqua-rs-sdk" rev-parse HEAD)"
put_host xi '{"refs_follow": ["aqua-rs-sdk", "extra-four", "aqua-rs-sdk"], "extra_refs": ["extra-one", "aqua-rs-sdk", "extra-four"]}'
rc=0; run j1 --label xi --target "$TARGET" --persona Thalia || rc=$?
check "follow: exit 0" [ "$rc" -eq 0 ]
J1_WANT="$(printf '%s\n' \
  "$FOLLOW/aqua-rs-sdk:/refs/aqua-rs-sdk:ro" \
  "$REFS_BASE/aqua-spec:/refs/aqua-spec:ro" \
  "$REFS_BASE/aqua-governance-corpus:/refs/aqua-governance-corpus:ro" \
  "$REFS_BASE/aqua-ecosystem:/refs/aqua-ecosystem:ro" \
  "$REFS_BASE/aqua-compliance:/refs/aqua-compliance:ro" \
  "$REFS_BASE/inblockio.github.io:/refs/inblockio.github.io:ro" \
  "$FOLLOW/extra-four:/refs/extra-four:ro" \
  "$FOLLOW/_developments/aqua-rs-sdk:/refs/_developments/aqua-rs-sdk:ro" \
  "$FOLLOW/_developments/extra-four:/refs/_developments/extra-four:ro" \
  "$MIRROR/extra-one:/refs/extra-one:ro")"
check "follow: exact /refs mount sequence; follow mirror IN PLACE of the fleet aqua-rs-sdk mount" [ "$(refs_seq j1)" = "$J1_WANT" ]
check "follow: the fleet checkout of the followed repo is not mounted" \
  bash -c '! grep -qx "$1/aqua-rs-sdk:/refs/aqua-rs-sdk:ro" "$2"' _ "$REFS_BASE" "$SB/out/j1.argv"
check "follow: every follow mount source is a directory (never a single-file bind mount)" \
  bash -c 'n=0; for s in $(grep "^$1/" "$2" | cut -d: -f1); do [ -d "$s" ] || exit 1; n=$((n+1)); done; [ "$n" -eq 4 ]' _ "$FOLLOW" "$SB/out/j1.argv"
check "follow: followed names in extra_refs are skipped with a note" \
  bash -c 'grep -qx ">> extra refs: aqua-rs-sdk is followed (host.refs_follow), skipped" "$1" && grep -qx ">> extra refs: extra-four is followed (host.refs_follow), skipped" "$1"' _ "$SB/out/j1.out"
check "follow: summary line names the follow root and the timer" \
  grep -qxF ">> refs follow: 2 repo(s) from $FOLLOW (kept current by consultant-refs-follow.timer), ro at /refs, digests ro at /refs/_developments: aqua-rs-sdk extra-four" "$SB/out/j1.out"
check "follow: mirror untouched (same HEAD, still clean)" \
  bash -c '[ "$(git -C "$1" rev-parse HEAD)" = "$2" ] && [ -z "$(git -C "$1" status --porcelain --ignored)" ]' _ "$FOLLOW/aqua-rs-sdk" "$FHEAD"
upstream_commit aqua-rs-sdk
rc=0; run j1b --label xi --target "$TARGET" --persona Thalia || rc=$?
check "follow: an upstream commit is not pulled (print-run; the static checks below cover real spawns)" \
  bash -c '[ "$1" -eq 0 ] && [ "$(git -C "$2" rev-parse HEAD)" = "$3" ]' _ "$rc" "$FOLLOW/aqua-rs-sdk" "$FHEAD"
rc=0; run j2 --label omicron --target "$TARGET" --persona Thalia || rc=$?
check "follow: a consultant without host.refs_follow keeps the six fleet mounts, no digests" \
  bash -c '[ "$1" -eq 0 ] && [ "$(grep -c ":/refs/" "$2")" -eq 6 ] && grep -qx "$3/aqua-rs-sdk:/refs/aqua-rs-sdk:ro" "$2" && ! grep -q "_developments\|$4" "$2"' \
  _ "$rc" "$SB/out/j2.argv" "$REFS_BASE" "$FOLLOW"
FOLLOW_FN="$(sed -n '/^check_follow_refs() {$/,/^}$/p' "$SPAWN")"
check "check_follow_refs extracted from the script" grep -q 'status --porcelain --ignored' <(printf '%s\n' "$FOLLOW_FN")
check "check_follow_refs never clones, fetches, updates or creates (message lines aside)" \
  bash -c '! printf "%s\n" "$1" | grep -vE "^[[:space:]]*echo " | grep -qE "(clone|fetch|pull|checkout|reset|clean|mkdir|rm |touch)"' _ "$FOLLOW_FN"
check "the follow status never takes the index lock (GIT_OPTIONAL_LOCKS=0)" \
  grep -qF 'GIT_OPTIONAL_LOCKS=0 git -C "$dir" status --porcelain --ignored' <(printf '%s\n' "$FOLLOW_FN")
check "follow root default is on disk (~/.local/share), never /tmp" \
  grep -qF 'FOLLOW_ROOT="${CONSULTANT_REFS_FOLLOW_ROOT:-$HOME/.local/share/consultant-refs-follow}"' "$SPAWN"

# Fail-closed: missing mirror, missing digest dir, not a clone, dirty (untracked / ignored /
# modified). Each exits 1 before any side effect with the service hint; the mirror is left as is.
follow_rejects() {  # follow_rejects <description> <host json> <expected stderr fragment>
  rm -rf "$(cfg_of pi)" "$TEST_DIR/pi-aqua-consultant-persist"; put_host pi "$2"
  local sha rc=0; sha="$(sha_of "$(cfg_of pi)")"
  run j3 --label pi --target "$TARGET" --persona Thalia || rc=$?
  check "$1: exit 1, service hint, no argv, config untouched, no persist dir" \
    bash -c '[ "$1" -eq 1 ] && grep -qF -- "$2" "$3" && grep -qxF "   run: systemctl --user start consultant-refs-follow.service" "$3" && [ ! -s "$4" ] && [ "$(sha256sum < "$5" | cut -d" " -f1)" = "$6" ] && [ ! -e "$7" ]' \
    _ "$rc" "$3" "$SB/out/j3.err" "$SB/out/j3.argv" "$(cfg_of pi)" "$sha" "$TEST_DIR/pi-aqua-consultant-persist"
}
mkdir -p "$FOLLOW/_developments/extra-five"
follow_rejects "follow: missing mirror" '{"refs_follow": ["extra-five"]}' "refs follow: mirror MISSING: $FOLLOW/extra-five"
check "follow: a missing mirror is never cloned by the spawn" [ ! -e "$FOLLOW/extra-five" ]
git clone -q --single-branch "$REMOTE/extra-four.git" "$FOLLOW/extra-six"
follow_rejects "follow: missing digest dir" '{"refs_follow": ["extra-six"]}' "refs follow: digest dir MISSING: $FOLLOW/_developments/extra-six"
check "follow: a missing digest dir is never created by the spawn" [ ! -e "$FOLLOW/_developments/extra-six" ]
mkdir -p "$FOLLOW/extra-seven" "$FOLLOW/_developments/extra-seven"
follow_rejects "follow: not a git clone" '{"refs_follow": ["extra-seven"]}' "refs follow: $FOLLOW/extra-seven is not a git clone"
touch "$FOLLOW/aqua-rs-sdk/untracked.txt"
follow_rejects "follow: untracked file" '{"refs_follow": ["aqua-rs-sdk"]}' "refs follow: mirror $FOLLOW/aqua-rs-sdk has LOCAL CHANGES"
check "follow: the untracked file is left in place" [ -e "$FOLLOW/aqua-rs-sdk/untracked.txt" ]
rm -f "$FOLLOW/aqua-rs-sdk/untracked.txt"
echo 'local.env' >> "$FOLLOW/aqua-rs-sdk/.git/info/exclude"; touch "$FOLLOW/aqua-rs-sdk/local.env"
follow_rejects "follow: IGNORED file" '{"refs_follow": ["aqua-rs-sdk"]}' "refs follow: mirror $FOLLOW/aqua-rs-sdk has LOCAL CHANGES"
rm -f "$FOLLOW/aqua-rs-sdk/local.env"
echo changed >> "$FOLLOW/aqua-rs-sdk/README.md"
follow_rejects "follow: modified tracked file" '{"refs_follow": ["aqua-rs-sdk"]}' "refs follow: mirror $FOLLOW/aqua-rs-sdk has LOCAL CHANGES"
git -C "$FOLLOW/aqua-rs-sdk" checkout -q -- README.md
rm -rf "$(cfg_of pi)"; put_host pi '{"refs_follow": ["aqua-rs-sdk"]}'
rc=0; run j4 --label pi --target "$TARGET" --persona Thalia || rc=$?
check "follow: clean again -> exit 0" [ "$rc" -eq 0 ]

echo "== case K: host, invite_policy and rooms survive every config rewrite"
# A config carrying all three (synthetic values) goes through each path that rewrites a config:
# persona render onto the existing config, --keep-config with the avatar_path patch firing,
# --voice on / off, --refresh-prompt. Each block's serialized bytes must be unchanged.
rm -rf "$(cfg_of rho)"; put_host rho '{"extra_refs": ["extra-one"], "refs_follow": ["aqua-rs-sdk"]}'
python3 - "$(cfg_of rho)" <<'PY'
import json, sys
p = sys.argv[1]; c = json.load(open(p))
c["invite_policy"] = "owner_only"
c["rooms"] = [{"name": "test-room", "room_id": "!fake:example.invalid", "dir": "/agent/rooms/test-room"}]
with open(p, "w") as f:
    json.dump(c, f, indent=2, ensure_ascii=True); f.write("\n")
PY
cp "$(cfg_of rho)" "$SB/rho.orig.json"
blocks_same() {   # exit 0 iff host/invite_policy/rooms serialize to the same bytes in both files
  python3 - "$SB/rho.orig.json" "$(cfg_of rho)" <<'PY'
import json, sys
a_raw, b_raw = open(sys.argv[1]).read(), open(sys.argv[2]).read()
a, b = json.loads(a_raw), json.loads(b_raw)
for k in ("host", "invite_policy", "rooms"):
    seg = f'  "{k}": ' + json.dumps(a[k], indent=2, ensure_ascii=True).replace("\n", "\n  ")
    if seg not in a_raw or seg not in b_raw or json.dumps(a[k]) != json.dumps(b.get(k)):
        sys.exit(1)
PY
}
rc_and_same() { [ "$1" -eq 0 ] && blocks_same; }
rc=0; run k1 --label rho --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "persona render onto the existing config: blocks byte-identical" rc_and_same "$rc"
python3 - "$(cfg_of rho)" <<'PY2'
import json, sys
p = sys.argv[1]; c = json.load(open(p)); c["avatar_path"] = "/agent/avatar.png"
with open(p, "w") as f:
    json.dump(c, f, indent=2, ensure_ascii=True); f.write("\n")
PY2
rc=0; run k2 --replace --keep-config --label rho || rc=$?
check "--keep-config, avatar_path patch rewrote the file" \
  bash -c 'grep -q "avatar_path removed" "$1"' _ "$SB/out/k2.out"
check "  (k2) blocks byte-identical" rc_and_same "$rc"
rc=0; run k3 --replace --keep-config --label rho --voice on || rc=$?
check "--keep-config --voice on rewrote the file" \
  bash -c 'grep -q "set voice.enabled=true" "$1"' _ "$SB/out/k3.out"
check "  (k3) blocks byte-identical" rc_and_same "$rc"
rc=0; run k4 --replace --keep-config --label rho --voice off || rc=$?
check "--keep-config --voice off rewrote the file" bash -c 'grep -q "set voice.enabled=false" "$1"' _ "$SB/out/k4.out"
check "  (k4) blocks byte-identical" rc_and_same "$rc"
rc=0; run k5 --replace --keep-config --refresh-prompt --label rho || rc=$?
check "--keep-config --refresh-prompt rewrote the file" bash -c 'grep -q "refresh-prompt: adopted" "$1"' _ "$SB/out/k5.out"
check "  (k5) blocks byte-identical" rc_and_same "$rc"
cp "$(cfg_of rho)" "$SB/rho.before-k6.json"
rc=0; run k6 --replace --keep-config --label rho || rc=$?
check "plain --keep-config: file byte-identical (no write at all)" \
  bash -c '[ "$1" -eq 0 ] && cmp -s "$2" "$3"' _ "$rc" "$SB/rho.before-k6.json" "$(cfg_of rho)"

echo "== side effects"
check "podman/systemctl shims were never called" [ ! -e "$SIDE_EFFECTS" ]
check "no systemd unit written into the sandbox HOME" [ ! -d "$FAKE_HOME/.config/systemd" ]
check "nothing written outside the sandbox test dir (live ~/.aqua-matrix-test untouched)" \
  bash -c '! ls "$HOME/.aqua-matrix-test/alpha-aqua-consultant-config.json" "$HOME/.aqua-matrix-test/beta-aqua-consultant-config.json" >/dev/null 2>&1'

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
