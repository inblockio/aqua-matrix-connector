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
#   - extra refs (case I): <key>-extra-refs.list parsing (comments, blanks, CR, duplicates, a
#     fleet repo skipped with a note), every bad name aborts with exit 2 before any render, mounts
#     land :ro at /refs/<repo> from the default mirror root; the script's own sync_extra_refs
#     (extracted) clones single-branch, fast-forwards, and aborts on untracked / ignored /
#     modified files, a failed clone, a diverged mirror and a missing mirror under no-refresh
#   - rooms (case I): <key>-rooms -> /agent/rooms:ro plus <key>-room-state (created, kept across
#     re-spawns) at /agent/room-state with the same option as /agent/memory; --generic uses the
#     key "generic"; a consultant with neither file gets exactly today's argv
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

echo "== case I: per-consultant extra refs (<key>-extra-refs.list) + rooms"
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

printf '%s\n' '# extra refs for iota' 'extra-one' '  extra-two   # trailing comment' 'aqua-rs-sdk' '' 'extra-one' > "$TEST_DIR/iota-extra-refs.list"
printf 'extra-two\r\n' >> "$TEST_DIR/iota-extra-refs.list"
rc=0; run i1 --label iota --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "list: exit 0" [ "$rc" -eq 0 ]
check "list: extra-one mounted :ro from the default mirror root" argv_has_line i1 "$MIRROR/extra-one:/refs/extra-one:ro"
check "list: extra-two mounted :ro from the default mirror root" argv_has_line i1 "$MIRROR/extra-two:/refs/extra-two:ro"
check "list: 6 fleet + 2 extra /refs mounts, each right after -v" \
  bash -c '[ "$(awk "prev==\"-v\" && /:\/refs\/[^:]*:ro\$/ {n++} {prev=\$0} END{print n+0}" "$1")" -eq 8 ]' _ "$SB/out/i1.argv"
check "list: fleet repo skipped with a note, mounted once from the fleet checkout" \
  bash -c 'grep -q "aqua-rs-sdk is already fleet-mounted (REFS_REPOS), skipped" "$1" && [ "$(grep -c ":/refs/aqua-rs-sdk:ro$" "$2")" -eq 1 ] && grep -qx "$3/aqua-rs-sdk:/refs/aqua-rs-sdk:ro" "$2"' \
  _ "$SB/out/i1.out" "$SB/out/i1.argv" "$REFS_BASE"
check "list: one summary line (duplicates, comments, blanks, CR dropped)" \
  grep -qxF ">> extra refs: 2 repo(s) from $MIRROR (as-is, freshness pass skipped), ro at /refs: extra-one extra-two" "$SB/out/i1.out"
check "list: no rooms mounts without a rooms dir" bash -c '! grep -q "/agent/room" "$1"' _ "$SB/out/i1.argv"

for bad in '../etc' 'a/b' '..' '.' 'a..b' 'has space' 'x;y' '$(id)'; do
  printf '%s\n' extra-one "$bad" > "$TEST_DIR/kappa-extra-refs.list"
  rc=0; run i2 --label kappa --target "$TARGET" --persona Thalia || rc=$?
  check "bad name '$bad': exit 2, named, no argv, no config rendered" \
    bash -c '[ "$1" -eq 2 ] && grep -qF "line 2: invalid repo name" "$2" && [ ! -s "$3" ] && [ ! -e "$4" ]' \
    _ "$rc" "$SB/out/i2.err" "$SB/out/i2.argv" "$TEST_DIR/kappa-aqua-consultant-config.json"
done
rm -f "$TEST_DIR/kappa-extra-refs.list"

printf 'extra-three\n' > "$TEST_DIR/lambda-extra-refs.list"
rc=0; run i3 --label lambda --target "$TARGET" --persona Thalia || rc=$?
check "missing mirror under --print-run: abort, nothing cloned, no argv" \
  bash -c '[ "$1" -ne 0 ] && grep -q "mirror MISSING" "$2" && [ ! -e "$3" ] && [ ! -s "$4" ]' \
  _ "$rc" "$SB/out/i3.err" "$MIRROR/extra-three" "$SB/out/i3.argv"
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

printf 'extra-two\n' > "$TEST_DIR/generic-extra-refs.list"; mkdir -p "$TEST_DIR/generic-rooms"
rc=0; run i7 --generic --target "$TARGET" --persona Sabrina --name Operator || rc=$?
check "generic: key 'generic' selects generic-extra-refs.list and generic-rooms" \
  bash -c '[ "$1" -eq 0 ] && grep -qx "$2/extra-two:/refs/extra-two:ro" "$3" && grep -qx "$4/generic-rooms:/agent/rooms:ro" "$3"' \
  _ "$rc" "$MIRROR" "$SB/out/i7.argv" "$TEST_DIR"
rm -f "$TEST_DIR/generic-extra-refs.list"; rm -rf "$TEST_DIR/generic-rooms" "$TEST_DIR/generic-room-state"

rc=0; run i8 --label mu --target "$TARGET" --persona Thalia || rc=$?
check "default (no list, no rooms dir): exactly the six fleet /refs mounts, no rooms, no notes" \
  bash -c '[ "$1" -eq 0 ] && [ "$(grep -c ":/refs/" "$2")" -eq 6 ] && ! grep -q "/agent/room" "$2" && ! grep -q "extra refs\|rooms:" "$3" "$4" && [ ! -e "$5" ]' \
  _ "$rc" "$SB/out/i8.argv" "$SB/out/i8.out" "$SB/out/i8.err" "$TEST_DIR/mu-room-state"
check "mirror root default is on disk (~/.local/share), never /tmp" \
  grep -qF 'REFS_MIRROR="${CONSULTANT_REFS_MIRROR:-$HOME/.local/share/consultant-refs}"' "$SPAWN"

echo "== side effects"
check "podman/systemctl shims were never called" [ ! -e "$SIDE_EFFECTS" ]
check "no systemd unit written into the sandbox HOME" [ ! -d "$FAKE_HOME/.config/systemd" ]
check "nothing written outside the sandbox test dir (live ~/.aqua-matrix-test untouched)" \
  bash -c '! ls "$HOME/.aqua-matrix-test/alpha-aqua-consultant-config.json" "$HOME/.aqua-matrix-test/beta-aqua-consultant-config.json" >/dev/null 2>&1'

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
