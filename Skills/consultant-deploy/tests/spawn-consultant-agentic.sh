#!/usr/bin/env bash
# Tests for spawn-consultant.sh --host agentic (the default since 2026-10-30) and agentic-manifest.py, hermetic:
# ssh, rsync, systemctl, podman and skh-run are shims that record every call, so nothing leaves the sandbox.
#   - agentic refuses --replace / --fresh / --generic before any side effect; --host takes agentic|nuc10 only
#   - a full new spawn: the shape and env file (secret only in the 0600 env file, never in the shape or the output),
#     the order rsync sources -> rsync shape -> refs push -> agents.py place -> run, no podman run, Tim's notices
#     name agentic, the MXID and the greeted marker are read on agentic, nuc10's activity watcher is wired
#   - agents.py run failing: CRITICAL notice, non-zero exit
#   - preflight refusals: nuc10 state in the persist dir; agentic holding a container, persist trees, or no docker;
#     state appearing before the launch; empty nuc10 persist dirs (a retry) are not state
#   - --host nuc10 refuses an agent whose moved copy nuc10 holds (a2a-moved-<rest>), no podman run
#   - agentic-manifest.py: owners by mode, carry.py's key set, sources outside HOME refused
set -uo pipefail
SKILL_DIR="$(cd "$(dirname "$0")/.." && pwd)"
SPAWN="$SKILL_DIR/spawn-consultant.sh"
MANIFEST="$SKILL_DIR/agentic-manifest.py"
TEMPLATE="$SKILL_DIR/consultant-config.template.json"
PASS=0; FAIL=0
ok()  { PASS=$((PASS + 1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }
check() { local desc="$1"; shift; if "$@"; then ok "$desc"; else bad "$desc"; fi; }
has() { grep -qF -- "$2" "$1"; }

SB="$(mktemp -d "${TMPDIR:-/tmp}/spawn-agentic-test.XXXXXX")"
trap 'rm -rf "$SB"' EXIT
FAKE_HOME="$SB/home"; TEST_DIR="$FAKE_HOME/.aqua-matrix-test"; SHIM_BIN="$SB/bin"
STUB="$SB/stub/aqua-agent-refs"; REFS_ROOT="$FAKE_HOME/.local/share/agent-refs"
SIDE="$SB/side-effects.log"; NOTIFY_LOG="$SB/notify.jsonl"
OPS="$SB/aqua-ops"; CARRY="$FAKE_HOME/.cache/agents-to-agentic/carry/meta"
TOKEN_FILE="$FAKE_HOME/.aqua-matrix-heartbeat/claude-oauth-token"
mkdir -p "$FAKE_HOME/.aqua-matrix-heartbeat" "$TEST_DIR" "$SHIM_BIN" "$SB/out" "$SB/stub" "$REFS_ROOT" \
  "$OPS/tools/key-rotation" "$OPS/tools/host-move"
echo fake-token-sekrit > "$TOKEN_FILE"
cp "$TEMPLATE" "$TEST_DIR/consultant-config.template.json"
TARGET='@peerpeerpeerpeer:matrix.inblock.io'
AGENT_MXID='@0a1b2c3d4e5f6g7h:matrix.inblock.io'
AL="$SB/allowlist.toml"
printf '[[recipients]]\nname = "tim"\nmxid = "@tim-operator:matrix.inblock.io"\n\n[[recipients]]\nname = "peer"\nmxid = "%s"\n' \
  "$TARGET" > "$AL"; chmod 600 "$AL"

cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
agent=""; for a in "$@"; do case "$a" in --agent=*) agent="${a#--agent=}" ;; esac; done
case "${1:-}" in
  path) mkdir -p "$AGENT_REFS_ROOT/views/$agent"; printf '%s\n' "$AGENT_REFS_ROOT/views/$agent" ;;
  add|sync|check) : ;;
  *) exit 64 ;;
esac
EOF
# ssh: the last argument is the remote command (rsync never reaches this shim: rsync is a shim too).
cat > "$SHIM_BIN/ssh" <<'EOF'
#!/bin/bash
cmd="${!#}"; echo "ssh $cmd" >> "@SIDE@"
case "$cmd" in
  true) exit 0 ;;
  "if ! docker info"*)   # agentic_probe; SHIM_PROBE="<1st> <2nd>" answers per call, default absent
    n=$(( $(cat "@SB@/probes" 2>/dev/null || echo 0) + 1 )); echo "$n" > "@SB@/probes"
    set -- ${SHIM_PROBE:-absent absent}; eval "echo \${$n:-absent}" ;;
  "test -d "*) exit 0 ;;
  "docker exec "*" grep "*) echo 'user_id = "@AGENT@"' ;;
  "docker exec "*" sh -c "*) exit 0 ;;
  "docker logs "*) exit 0 ;;
  "docker inspect -f "*) echo 0123456789abcdef0123456789abcdef ;;
  *) exit 99 ;;
esac
EOF
cat > "$SHIM_BIN/rsync" <<'EOF'
#!/bin/sh
echo "rsync $*" >> "@SIDE@"
EOF
cat > "$SHIM_BIN/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >> "@SIDE@"
EOF
cat > "$SHIM_BIN/podman" <<'EOF'
#!/bin/sh
echo "podman $*" >> "@SIDE@"
case "$1 $2" in
  "container exists") exit 1 ;;
  "ps -a") [ -z "${SHIM_MOVED:-}" ] || echo "$SHIM_MOVED"; exit 0 ;;
  "run -d") echo 0123456789abcdef0123; exit 0 ;;
esac
exit 0
EOF
cat > "$OPS/tools/key-rotation/skh-run" <<'EOF'
#!/bin/sh
echo "skh-run $*" >> "@SIDE@"
verb="$6"; name="$7"
case "$verb" in
  place) echo "SKH agents placed name=$name trees=4 env_names=3" ;;
  run) if [ -n "${SHIM_RUN_FAIL:-}" ]; then echo "SKH agents refused reason=no_image"; exit 2; fi
       echo "SKH agents started name=$name state=running restarts=0 secs=5 did=did:key:z6Mkfake connected=1" ;;
esac
EOF
touch "$OPS/tools/host-move/agents.py"
sed -i "s|@SIDE@|$SIDE|; s|@AGENT@|$AGENT_MXID|; s|@SB@|$SB|g" "$SHIM_BIN"/* "$OPS/tools/key-rotation/skh-run"
chmod +x "$STUB" "$SHIM_BIN"/* "$OPS/tools/key-rotation/skh-run"
cat > "$SB/notify" <<EOF
#!/usr/bin/env python3
import json, sys
open("$NOTIFY_LOG", "a").write(json.dumps(sys.argv[1:]) + "\n")
EOF
chmod +x "$SB/notify"

# spawn <name> [VAR=value...] -- [spawn args...]: a real spawn; no CONSULTANT_HOST, so the default host applies.
spawn() {
  local name="$1"; shift; local rc=0 envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done; shift
  rm -f "$SIDE" "$NOTIFY_LOG" "$SB/probes"
  env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" AGENT_REFS_BIN="$STUB" \
    AGENT_REFS_ROOT="$REFS_ROOT" CONSULTANT_TEST_DIR="$TEST_DIR" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" \
    CONSULTANT_NOTIFY="$SB/notify" AQUA_SYSTEM_ALLOWLIST="$AL" AQUA_OPS_DIR="$OPS" ONBOARD_WAIT=10 \
    ${envs[@]+"${envs[@]}"} bash "$SPAWN" --target "$TARGET" --no-refresh-refs "$@" \
    > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  touch "$SIDE" "$NOTIFY_LOG"
}
rc_is() { [ "$(cat "$SB/out/$1.rc")" = "$2" ]; }
quiet() { ! grep -qE '^(ssh|rsync|skh-run|podman run)' "$SIDE"; }   # nothing reached agentic or a container

echo "== agentic refuses what it does not do"
for f in --replace --fresh --generic; do
  spawn "r$f" -- --label refused --persona Pelagia "$f"
  check "default host, $f: exit 2, named, nothing sent, no config" bash -c \
    '[ "$(cat "$1")" = 2 ] && grep -q "deploys NEW consultants only" "$2" && ! grep -qE "^(ssh|rsync|skh-run|podman)" "$3" && [ ! -e "$4" ]' \
    _ "$SB/out/r$f.rc" "$SB/out/r$f.err" "$SIDE" "$TEST_DIR/refused-aqua-consultant-config.json"
done
spawn hostbad -- --label hb --persona Pelagia --host elsewhere
check "--host elsewhere: exit 2" bash -c '[ "$(cat "$1")" = 2 ] && grep -q "takes exactly .agentic. or .nuc10." "$2"' _ "$SB/out/hostbad.rc" "$SB/out/hostbad.err"

echo "== a new consultant on agentic"
L=alpha; NAME="aqua-agent-$L-aqua-consultant-1"
printf 'jpg' > "$TEST_DIR/$L-avatar.jpg"
spawn full -- --label "$L" --persona Pelagia --name Andreas --onboard
check "exit 0" rc_is full 0
J="$CARRY/$NAME.json"; E="$CARRY/$NAME.env"
check "shape: image, 5 mounts with owners by mode, env names, no value" python3 - "$J" "$TARGET" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
m = {x["dst"]: (x["src"], x["ro"], x["owner"]) for x in d["mounts"]}
assert d["image"] == "localhost/aqua-matrix-agent:poc", d["image"]
assert m == {"/agent/config.json": (".aqua-matrix-test/alpha-aqua-consultant-config.json", True, 1000),
             "/agent/store": (".aqua-matrix-test/alpha-aqua-consultant-persist/store", False, 1001),
             "/agent/memory": (".aqua-matrix-test/alpha-aqua-consultant-persist/memory", False, 1001),
             "/refs": (".local/share/agent-refs/views/alpha-aqua-consultant", True, 1000),
             "/agent/avatar.png": (".aqua-matrix-test/alpha-avatar.jpg", True, 1000)}, m
assert d["env_names"] == ["AGENT_TARGET", "AGENT_CONFIG_FILE", "CLAUDE_CODE_OAUTH_TOKEN"], d["env_names"]
assert len(d["cap_drop"]) == 11 and d["security_opt"] == ["no-new-privileges"] and d["pids"] == 512
assert sys.argv[2] not in open(sys.argv[1]).read()
PY
check "env file: 0600, target, config path and token" bash -c \
  '[ "$(stat -c %a "$1")" = 600 ] && grep -qxF "AGENT_TARGET=$2" "$1" && grep -qxF "AGENT_CONFIG_FILE=/agent/config.json" "$1" && grep -qxF "CLAUDE_CODE_OAUTH_TOKEN=fake-token-sekrit" "$1"' \
  _ "$E" "$TARGET"
check "the token appears nowhere but the env file" bash -c '! grep -rqF fake-token-sekrit "$1" "$2" "$3" "$4" "$5"' \
  _ "$SB/out/full.out" "$SB/out/full.err" "$SIDE" "$NOTIFY_LOG" "$J"
check "order: sources, shape, refs push, place, run; then the MXID read" python3 - "$SIDE" "$NAME" <<'PY'
import sys
lines, name = open(sys.argv[1]).read().splitlines(), sys.argv[2]
def at(pred):
    return next(i for i, l in enumerate(lines) if pred(l))
a = at(lambda l: l.startswith("rsync -aL --relative") and "/./.aqua-matrix-test/alpha-aqua-consultant-config.json" in l)
b = at(lambda l: l.startswith("rsync -a ") and f"{name}.env" in l and f"{name}.json" in l)
c = at(lambda l: l == "systemctl --user start agentic-refs-push.service")
d = at(lambda l: l.startswith("skh-run --sudo --dir / prod") and l.endswith(f"place {name}"))
e = at(lambda l: l.startswith("skh-run --sudo --dir / prod") and l.endswith(f"run {name}"))
f = max(i for i, l in enumerate(lines) if l.startswith(f"ssh docker exec {name} grep"))  # the first is the identity probe
assert a < b < c < d < e < f, (a, b, c, d, e, f)
src = lines[a]
assert "/persist/store" not in src and "-persist/store" in src and "-persist/memory" in src and "alpha-avatar.jpg" in src
assert "agent-refs" not in src, src
assert not any(l.startswith("podman run") or l.startswith("systemd-run") for l in lines)
PY
check "MXID read back from agentic" has "$SB/out/full.out" "agent MXID (from its persisted session): $AGENT_MXID"
check "channel up names agentic" python3 - "$NOTIFY_LOG" <<'PY'
import json, sys
calls = [json.loads(l) for l in open(sys.argv[1])]
up = [c for c in calls if "channel up: aqua-agent-alpha-aqua-consultant-1" in c]
assert up and up[0][-1].endswith("on agentic.inblock.io."), calls
assert any(any(str(x).startswith("welcome delivered") for x in c) for c in calls), calls
PY
check "greeted marker read inside the container" has "$SIDE" "ssh docker exec $NAME sh -c '[ -e /agent/memory/.whats_new_seen ]'"
check "nuc10 activity watcher wired" bash -c 'grep -q "^systemctl --user restart aqua-activity-watch-alpha" "$1"' _ "$SIDE"
check "verify hint is the agentic one" has "$SB/out/full.out" "ssh agentic.inblock.io docker logs -f $NAME"

echo "== failures and refusals"
spawn runfail SHIM_RUN_FAIL=1 -- --label beta --persona Pelagia
check "agents.py run refused: exit non-zero, reason named, CRITICAL to Tim" bash -c \
  '[ "$(cat "$1")" != 0 ] && grep -q "agents.py run: SKH agents refused reason=no_image" "$2" && grep -q "spawn FAILED: aqua-agent-beta-aqua-consultant-1" "$3"' \
  _ "$SB/out/runfail.rc" "$SB/out/runfail.err" "$NOTIFY_LOG"
mkdir -p "$TEST_DIR/gamma-aqua-consultant-persist/store"; echo pem > "$TEST_DIR/gamma-aqua-consultant-persist/store/agent.pem"
spawn local -- --label gamma --persona Pelagia
check "nuc10 identity in the persist dir: exit 2, nothing sent" bash -c \
  '[ "$(cat "$1")" = 2 ] && grep -q "holds an identity or state on nuc10" "$2"' _ "$SB/out/local.rc" "$SB/out/local.err"
check "  ... and nothing reached agentic" quiet
for p in "container:already exists on agentic" "state:its identity lives there" "docker_down:cannot tell what agentic holds"; do
  spawn "probe-${p%%:*}" "SHIM_PROBE=${p%%:*}" -- --label delta --persona Pelagia
  check "agentic holds ${p%%:*}: exit 2, named" bash -c '[ "$(cat "$1")" = 2 ] && grep -q "$3" "$2"' \
    _ "$SB/out/probe-${p%%:*}.rc" "$SB/out/probe-${p%%:*}.err" "${p#*:}"
  check "  ... no rsync, no agents.py" bash -c '! grep -qE "^(rsync|skh-run)" "$1"' _ "$SIDE"
done
spawn reprobe "SHIM_PROBE=absent state" -- --label zeta --persona Pelagia
check "state appearing between the preflight and the launch: refused, nothing sent" bash -c \
  '[ "$(cat "$1")" != 0 ] && grep -q "it held nothing at the preflight" "$2" && ! grep -qE "^(rsync|skh-run)" "$3"' \
  _ "$SB/out/reprobe.rc" "$SB/out/reprobe.err" "$SIDE"
mkdir -p "$TEST_DIR/eta-aqua-consultant-persist/store" "$TEST_DIR/eta-aqua-consultant-persist/memory"
spawn retry -- --label eta --persona Pelagia
check "empty persist dirs on nuc10 (a retry): not refused, exit 0" rc_is retry 0
spawn moved SHIM_MOVED=a2a-moved-eps-aqua-consultant-1 CONSULTANT_HOST=nuc10 -- --label eps --persona Pelagia
check "--host nuc10 with a moved copy: exit 2, no podman run" bash -c \
  '[ "$(cat "$1")" = 2 ] && grep -q "runs on agentic" "$2" && ! grep -q "^podman run" "$3"' _ "$SB/out/moved.rc" "$SB/out/moved.err" "$SIDE"

echo "== agentic-manifest.py"
mkdir -p "$FAKE_HOME/d" "$SB/outside"; touch "$FAKE_HOME/f"
check "a source outside HOME is refused" bash -c \
  '! python3 "$1" --out "$2/m" --name aqua-agent-x-1 --image i --home "$3" --mount "$4:/x:ro" 2>/dev/null' _ "$MANIFEST" "$SB" "$FAKE_HOME" "$SB/outside"
check "a symlink below HOME pointing outside is refused" bash -c \
  'ln -s "$4" "$3/l" && ! python3 "$1" --out "$2/m" --name aqua-agent-x-1 --image i --home "$3" --mount "$3/l:/x:ro" 2>/dev/null' _ "$MANIFEST" "$SB" "$FAKE_HOME" "$SB/outside"
check "the shape has carry.py's keys, an unset env name is left out" bash -c \
  'X=1 python3 "$1" --out "$2/m" --name aqua-agent-x-1 --image i --home "$3" --mount "$3/d:/d:rw" --mount "$3/f:/f:ro" --env X --env UNSET_Y >/dev/null &&
   python3 -c "import json,sys; d=json.load(open(sys.argv[1])); assert sorted(d)==sorted([\"cap_add\",\"cap_drop\",\"cmd\",\"entrypoint\",\"env_names\",\"image\",\"memory\",\"mounts\",\"name\",\"nano_cpus\",\"networks\",\"pids\",\"security_opt\",\"tmpfs\",\"user\",\"userns_keep_id\",\"workdir\"]) and d[\"env_names\"]==[\"X\"] and [m[\"owner\"] for m in d[\"mounts\"]]==[1001,1000]" "$2/m/aqua-agent-x-1.json"' \
  _ "$MANIFEST" "$SB" "$FAKE_HOME"

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
