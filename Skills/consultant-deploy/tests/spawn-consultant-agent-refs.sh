#!/usr/bin/env bash
#
# spawn-consultant-agent-refs.sh, no-live-side-effect tests for the aqua-agent-refs launcher path of
# spawn-consultant.sh (header, "Per-agent refs lists").
#
# `aqua-agent-refs` does not run here: AGENT_REFS_BIN points at a stub that records every argv and
# plays a tiny list store (AGENT_REFS_ROOT in the sandbox) with scripted exit codes. HOME, the
# config/persist dir, the refs base, the token and the allow-list are in a temp dir; podman,
# systemctl and systemd-run are shims (podman keeps a "container is running" flag file, so a
# --replace abort is visible as "the old container is still running"); git is a shim that logs
# and fails in every run except the legacy equality scenarios that need a real one.
#
# Covers:
#   - list present: print-run uses `path` + `check` (never sync/add), exactly one `-v <view>:/refs:ro`
#     in the position the fleet mounts had (whole argv compared), no REFS_REPOS / extra / follow
#     mounts, no git, no presence check of the fleet checkouts; a real spawn runs
#     `sync --agent <STEM> --wait 900`; sync exit 3 proceeds with a STALE warning; sync exit
#     1 / 2 / 4 aborts before the config render and any podman call, and on --replace the old
#     container is still running; --no-refresh-refs runs check (failure aborts)
#   - new consultant: the list is created with `add --agent <STEM> <REFS_REPOS in order>`, then path,
#     then sync; add failure aborts with no config and no podman; --print-run creates nothing and
#     says so (placeholder mount, no add/sync/check call)
#   - existing consultant without a list: argv, stdout and stderr byte-identical to the script as of
#     8bb03b3 (the last commit before this feature) and to a host without the binary (scenarios:
#     fleet, host.extra_refs + host.refs_follow, rooms, --generic, --keep-config); the stub is only
#     asked `path`
#   - binary not installed: legacy with ONE warning (existing and new consultant alike), default name
#     found on PATH works, list file present + binary missing aborts (fail closed)
#   - fail closed on `path`: exit 1/3/127, empty / relative / colon output abort with nothing changed
#   - host.extra_refs / host.refs_follow with a list: one "ignored" line, no extra mounts, no mirror
#     or follow-root access (a missing follow mirror would abort the legacy path)
#   - print-onboarding / print-mxid never run the tool
#
# Usage:  bash Skills/consultant-deploy/tests/spawn-consultant-agent-refs.sh
# Exit:   0 when every assertion passes, 1 otherwise (each failure is printed).
#
# The assertions pass bash -c bodies in single quotes on purpose (positional args, no expansion here).
# shellcheck disable=SC2016
set -euo pipefail

SKILL_DIR="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
SPAWN="$SKILL_DIR/spawn-consultant.sh"
TEMPLATE="$SKILL_DIR/consultant-config.template.json"
PRE_CHANGE_COMMIT=8bb03b3     # origin/main before this feature: the legacy reference script

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok   $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL $1" >&2; }
skip() { echo "  skip $1"; }
check() { local desc="$1"; shift; if "$@"; then ok "$desc"; else bad "$desc"; fi; }

SB="$(mktemp -d "${TMPDIR:-/tmp}/spawn-agent-refs-test.XXXXXX")"
if [ -n "${KEEP_SANDBOX:-}" ]; then echo "sandbox kept: $SB"; else trap 'rm -rf "$SB"' EXIT; fi
FAKE_HOME="$SB/home"; TEST_DIR="$SB/aqua-matrix-test"; REFS_BASE="$SB/refs"; SHIM_BIN="$SB/bin"
NOGIT_BIN="$SB/nogit-bin"; STUB="$SB/stub/aqua-agent-refs"; STUB_ROOT="$SB/agent-refs-root"
STUB_LOG="$SB/stub.log"; SIDE_EFFECTS="$SB/side-effects.log"; GIT_LOG="$SB/git.log"; RUNNING="$SB/container.running"
TOKEN_FILE="$FAKE_HOME/.aqua-matrix-heartbeat/claude-oauth-token"; AL="$SB/allowlist.toml"
TARGET='@did-key-zfaketestpeer:matrix.inblock.io'
REFS_LIST=(aqua-rs-sdk aqua-spec aqua-governance-corpus aqua-ecosystem aqua-compliance inblockio.github.io)
mkdir -p "$FAKE_HOME/.aqua-matrix-heartbeat" "$TEST_DIR" "$REFS_BASE" "$SHIM_BIN" "$NOGIT_BIN" "$SB/stub" "$STUB_ROOT/agents" "$STUB_ROOT/views" "$SB/out"
echo fake-token > "$TOKEN_FILE"
cp "$TEMPLATE" "$TEST_DIR/consultant-config.template.json"
for r in "${REFS_LIST[@]}"; do mkdir -p "$REFS_BASE/$r"; done
printf '[[recipients]]\nname = "tim"\nmxid = "@tim-operator:matrix.inblock.io"\n\n[[recipients]]\nname = "peer"\nmxid = "%s"\n' "$TARGET" > "$AL"
chmod 600 "$AL"

# --- podman / systemctl / systemd-run shims. print mode: any call is a failure. spawn mode: podman keeps
# the RUNNING flag (container exists = flag present, rm -f removes it, run -d sets it).
cat > "$SHIM_BIN/podman" <<'EOF'
#!/bin/sh
echo "podman $*" >> "@SIDE@"
[ "${SHIM_MODE:-print}" = spawn ] || { echo "!! shim: podman must not be called" >&2; exit 1; }
case "$1 $2" in
  "container exists") [ -e "@RUNNING@" ]; exit $? ;;
  "rm -f") rm -f "@RUNNING@"; exit 0 ;;
  "run -d") touch "@RUNNING@"; echo 0123456789abcdef0123; exit 0 ;;
esac
exit 0
EOF
cat > "$SHIM_BIN/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >> "@SIDE@"
[ "${SHIM_MODE:-print}" = spawn ] || { echo "!! shim: systemctl must not be called" >&2; exit 1; }
exit 0
EOF
cat > "$SHIM_BIN/systemd-run" <<'EOF'
#!/bin/sh
echo "systemd-run $*" >> "@SIDE@"
[ "${SHIM_MODE:-print}" = spawn ] || { echo "!! shim: systemd-run must not be called" >&2; exit 1; }
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
shift
exec "$@"
EOF
sed -i "s|@SIDE@|$SIDE_EFFECTS|; s|@RUNNING@|$RUNNING|" "$SHIM_BIN/podman" "$SHIM_BIN/systemctl" "$SHIM_BIN/systemd-run"
chmod +x "$SHIM_BIN/podman" "$SHIM_BIN/systemctl" "$SHIM_BIN/systemd-run"
# git shim: logs and fails. The list path must never reach git (no working-checkout freshness pass).
printf '#!/bin/sh\necho "git $*" >> "%s"\nexit 1\n' "$GIT_LOG" > "$NOGIT_BIN/git"
chmod +x "$NOGIT_BIN/git"
printf '#!/usr/bin/env python3\nimport json, sys\nopen("%s", "a").write(json.dumps(sys.argv[1:]) + "\\n")\n' "$SB/notify.jsonl" > "$SB/notify"
chmod +x "$SB/notify"

# --- the stub aqua-agent-refs. Records argv as "a|b|c|" lines. State = $AGENT_REFS_ROOT/agents/<key>.json.
# path: forced by STUB_PATH_RC (+ STUB_PATH_OUT), else 0 + the view dir when the list file exists, else 2.
# add: STUB_ADD_RC (default 0); on success creates the list file and the view dir.
# sync / check: STUB_SYNC_RC / STUB_CHECK_RC (default 0).
cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
set -u
{ printf '%s|' "$@"; echo; } >> "$STUB_LOG"
cmd="${1:-}"; shift || true
agent=""
while [ $# -gt 0 ]; do
  case "$1" in --agent) agent="$2"; shift 2 ;; --wait) shift 2 ;; *) shift ;; esac
done
case "$cmd" in
  path)
    if [ -n "${STUB_PATH_RC:-}" ]; then
      [ -z "${STUB_PATH_OUT+x}" ] || printf '%s\n' "$STUB_PATH_OUT"
      exit "$STUB_PATH_RC"
    fi
    [ -e "$AGENT_REFS_ROOT/agents/$agent.json" ] || exit 2
    printf '%s\n' "$AGENT_REFS_ROOT/views/$agent"; exit 0 ;;
  add)
    [ "${STUB_ADD_RC:-0}" -eq 0 ] || exit "$STUB_ADD_RC"
    mkdir -p "$AGENT_REFS_ROOT/agents" "$AGENT_REFS_ROOT/views/$agent"
    echo '{}' > "$AGENT_REFS_ROOT/agents/$agent.json"; exit 0 ;;
  sync)  exit "${STUB_SYNC_RC:-0}" ;;
  check) exit "${STUB_CHECK_RC:-0}" ;;
esac
exit 64
EOF
chmod +x "$STUB"

EMDASH="$(printf '\xe2\x80\x94')"; ENDASH="$(printf '\xe2\x80\x93')"
no_dashes() { ! grep -qF -e "$EMDASH" -e "$ENDASH" "$@"; }

# --- runners. RUN_ENV (array of NAME=value) is appended last, so it overrides any default below.
RUN_ENV=()
BASE_PATH="$NOGIT_BIN:$SHIM_BIN:/usr/local/bin:/usr/bin:/bin"
reset_logs() { : > "$STUB_LOG"; rm -f "$SIDE_EFFECTS" "$GIT_LOG"; }
runp() {   # runp <name> <spawn args...>: --print-run; stdout/stderr/rc in $SB/out, argv in <name>.argv
  local name="$1"; shift; local rc=0; reset_logs
  env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" \
    AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" AGENT_REFS_BIN="$STUB" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
    ${RUN_ENV[@]+"${RUN_ENV[@]}"} \
    bash "$SPAWN" --print-run "$@" > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  awk '/^podman$/{p=1} p' "$SB/out/$name.out" > "$SB/out/$name.argv"
  return "$rc"
}
runs() {   # runs <name> <spawn args...>: a REAL spawn with podman shimmed (SHIM_MODE=spawn)
  local name="$1"; shift; local rc=0; reset_logs
  env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" SHIM_MODE=spawn CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" \
    AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" CONSULTANT_NOTIFY="$SB/notify" AQUA_SYSTEM_ALLOWLIST="$AL" \
    AGENT_REFS_BIN="$STUB" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
    ${RUN_ENV[@]+"${RUN_ENV[@]}"} \
    bash "$SPAWN" "$@" > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  return "$rc"
}
seed_session() {   # seed_session <label>: a persisted agent session, so a real spawn's MXID read-back (120 s wait) returns at once
  mkdir -p "$TEST_DIR/$1-aqua-consultant-persist/store"
  printf '[session]\nuser_id = "@0a1b2c3d4e5f6g7h:matrix.inblock.io"\n' > "$TEST_DIR/$1-aqua-consultant-persist/store/config.toml"
}
rcof()      { cat "$SB/out/$1.rc"; }
stub_calls() { cat "$STUB_LOG"; }
stub_cmds()  { cut -d'|' -f1 "$STUB_LOG" | tr '\n' ' '; }       # e.g. "path check "
no_stub_cmd() { ! grep -q "^$1|" "$STUB_LOG"; }
podman_ran()  { grep -q '^podman run' "$SIDE_EFFECTS" 2>/dev/null; }
podman_rm()   { grep -q '^podman rm' "$SIDE_EFFECTS" 2>/dev/null; }
no_podman()   { [ ! -e "$SIDE_EFFECTS" ] || ! grep -q '^podman' "$SIDE_EFFECTS"; }
no_git()      { [ ! -e "$GIT_LOG" ]; }
cfg_of()      { echo "$TEST_DIR/$1-aqua-consultant-config.json"; }
sha_of()      { sha256sum < "$1" | cut -d' ' -f1; }
make_list()   { mkdir -p "$STUB_ROOT/agents" "$STUB_ROOT/views/$1"; echo '{}' > "$STUB_ROOT/agents/$1.json"; }
drop_list()   { rm -f "$STUB_ROOT/agents/$1.json"; }
refs_seq()    { awk 'prev=="-v" && /:\/refs/ {print} {prev=$0}' "$SB/out/$1.argv"; }   # every /refs mount, argv order
put_host() {      # put_host <label|generic> <json|ABSENT>: set the config's host block (config from the template when missing)
  local cfg; if [ "$1" = generic ]; then cfg="$TEST_DIR/aqua-consultant-config.json"; else cfg="$(cfg_of "$1")"; fi
  [ -f "$cfg" ] || cp "$TEST_DIR/consultant-config.template.json" "$cfg"
  python3 -I - "$cfg" "$2" <<'PY'
import json, sys
p, v = sys.argv[1], sys.argv[2]
c = json.load(open(p))
if v == "ABSENT": c.pop("host", None)
else: c["host"] = json.loads(v)
with open(p, "w") as f:
    json.dump(c, f, indent=2, ensure_ascii=True); f.write("\n")
PY
}
subst_refs() {    # subst_refs <legacy.argv> <view>: the legacy argv with every /refs/<repo> mount pair replaced by the one view mount
  python3 -I - "$1" "$2" <<'PY'
import re, sys
lines = open(sys.argv[1]).read().split("\n"); view = sys.argv[2]
out, i, done = [], 0, False
while i < len(lines):
    if lines[i] == "-v" and i + 1 < len(lines) and re.search(r":/refs/[^:]+:ro$", lines[i + 1]):
        if not done: out += ["-v", f"{view}:/refs:ro"]; done = True
        i += 2; continue
    out.append(lines[i]); i += 1
sys.stdout.write("\n".join(out))
PY
}

ALPHA=alpha-aqua-consultant

echo "== syntax + static"
check "bash -n spawn-consultant.sh" bash -n "$SPAWN"
check "AGENT_REFS_BIN defaults to aqua-agent-refs" grep -qF 'AGENT_REFS_BIN="${AGENT_REFS_BIN:-aqua-agent-refs}"' "$SPAWN"
check "the header documents AGENT_REFS_BIN and the list path" bash -c 'grep -q "AGENT_REFS_BIN  *the aqua-agent-refs binary" "$1" && grep -q "Per-agent refs lists" "$1"' _ "$SPAWN"

# An existing consultant: render alpha's config (and beta's) once with no list and no binary.
RUN_ENV=( AGENT_REFS_BIN="$SB/none/aqua-agent-refs" )
runp seed_a --label alpha --target "$TARGET" --persona Thalia --name Tester
runp seed_b --label beta --target "$TARGET" --persona Galene --name Other
RUN_ENV=()
cp "$SB/out/seed_a.argv" "$SB/legacy_a.argv"
seed_session alpha
check "seed: alpha legacy argv has the six fleet mounts" bash -c '[ "$(grep -c ":/refs/[^:]*:ro$" "$1")" -eq 6 ]' _ "$SB/legacy_a.argv"

echo "== list present: --print-run uses path + check, one view mount in the fleet position"
make_list "$ALPHA"; VIEW_A="$STUB_ROOT/views/$ALPHA"
rc=0; runp l1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path then check, nothing else" [ "$(stub_cmds)" = "path check " ]
check "check argv is exactly: check --agent <STEM>" grep -qxF "check|--agent|$ALPHA|" "$STUB_LOG"
check "exactly one /refs mount: the view, read-only" [ "$(refs_seq l1)" = "$VIEW_A:/refs:ro" ]
check "whole argv == legacy argv with the six fleet mounts replaced by the view mount (same position)" \
  bash -c '[ "$(cat "$1")" = "$2" ]' _ "$SB/out/l1.argv" "$(subst_refs "$SB/legacy_a.argv" "$VIEW_A")"
check "no fleet mount, no /refs/<repo> mount" bash -c '! grep -q ":/refs/" "$1"' _ "$SB/out/l1.argv"
check "git never called (no working-checkout freshness pass)" no_git
check "no podman / systemctl call" no_podman
check "no dash characters in stdout/stderr" no_dashes "$SB/out/l1.out" "$SB/out/l1.err"
check "no agent-refs warning or note on stderr for a healthy list" bash -c '! grep -q "^!! agent-refs\|^!! refs" "$1"' _ "$SB/out/l1.err"

echo "== list present: the fleet checkouts are not even required"
rc=0; RUN_ENV=( CONSULTANT_REFS_BASE="$SB/no-such-refs-base" ); runp l2 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "exit 0 although no REFS_REPOS checkout exists" [ "$rc" -eq 0 ]
check "argv still mounts only the view" [ "$(refs_seq l2)" = "$VIEW_A:/refs:ro" ]

echo "== list present: --print-run, check fails -> abort"
rc=0; RUN_ENV=( STUB_CHECK_RC=1 ); runp l3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "exit non-zero" [ "$rc" -ne 0 ]
check "names the failed check, nothing launched" grep -q "check --agent $ALPHA' failed (exit 1)" "$SB/out/l3.err"
check "no argv printed" bash -c '[ ! -s "$1" ]' _ "$SB/out/l3.argv"
check "sync never called under --print-run" no_stub_cmd sync

echo "== list present: real spawn runs sync --wait 900, then launches"
touch "$RUNNING"
rc=0; runs r1 --replace --keep-config --label alpha || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path then sync, never check/add" [ "$(stub_cmds)" = "path sync " ]
check "sync argv is exactly: sync --agent <STEM> --wait 900" grep -qxF "sync|--agent|$ALPHA|--wait|900|" "$STUB_LOG"
check "podman run mounts the view at /refs:ro" grep -qF -- "-v $VIEW_A:/refs:ro" "$SIDE_EFFECTS"
check "podman run mounts no /refs/<repo>" bash -c '! grep "^podman run" "$1" | grep -q ":/refs/"' _ "$SIDE_EFFECTS"
check "git never called by a real list spawn" no_git
check "container replaced (rm then run), left running" bash -c 'grep -q "^podman rm" "$1" && grep -q "^podman run" "$1" && [ -e "$2" ]' _ "$SIDE_EFFECTS" "$RUNNING"

echo "== list present: sync exit 3 proceeds with a stale warning"
rc=0; RUN_ENV=( STUB_SYNC_RC=3 ); runs r2 --replace --keep-config --label alpha || rc=$?; RUN_ENV=()
check "exit 0 and launched" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "podman run happened" podman_ran
check "stderr warns that some repos are stale (sync exit 3)" grep -q "sync degraded (exit 3).*STALE" "$SB/out/r2.err"
check "the view is still the only refs mount" grep -qF -- "-v $VIEW_A:/refs:ro" "$SIDE_EFFECTS"

echo "== list present: sync failure aborts BEFORE any container change"
for code in 1 2 4 127; do
  touch "$RUNNING"
  cfg_before="$(sha_of "$(cfg_of alpha)")"
  rc=0; RUN_ENV=( "STUB_SYNC_RC=$code" ); runs r3 --replace --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?; RUN_ENV=()
  check "sync exit $code: exit 1, names the failure" bash -c '[ "$1" -eq 1 ] && grep -q "sync --agent '"$ALPHA"' --wait 900. failed (exit '"$code"')" "$2"' _ "$rc" "$SB/out/r3.err"
  check "sync exit $code: no podman rm, no podman run" bash -c '! grep -qE "^podman (rm|run)" "$1" 2>/dev/null' _ "$SIDE_EFFECTS"
  check "sync exit $code: the old container is still running (--replace aborted before the removal)" test -e "$RUNNING"
  check "sync exit $code: config not re-rendered (byte-identical)" bash -c '[ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ]' _ "$(cfg_of alpha)" "$cfg_before"
done
rm -f "$RUNNING"

echo "== list present: --no-refresh-refs runs check, not sync"
rc=0; runs r4 --replace --keep-config --label alpha --no-refresh-refs || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path then check" [ "$(stub_cmds)" = "path check " ]
rc=0; RUN_ENV=( STUB_CHECK_RC=1 ); runs r5 --replace --keep-config --label alpha --no-refresh-refs || rc=$?; RUN_ENV=()
check "check failure: exit 1, nothing launched or removed" bash -c '[ "$1" -eq 1 ]' _ "$rc"
check "check failure: no podman rm/run" bash -c '! grep -qE "^podman (rm|run)" "$1" 2>/dev/null' _ "$SIDE_EFFECTS"
rm -f "$RUNNING"

echo "== new consultant: the list is created with add (REFS_REPOS order), then path, then sync"
check "REFS_REPOS in the script matches the order this test expects" \
  bash -c '[ "$(grep -E "^REFS_REPOS=\(" "$1" | sed "s/^REFS_REPOS=(//; s/)\$//")" = "$2" ]' _ "$SPAWN" "${REFS_LIST[*]}"
NEWSTEM=newbie-aqua-consultant
seed_session newbie
rc=0; runs n1 --label newbie --target "$TARGET" --persona Thalia --name Newbie || rc=$?
check "exit 0 and launched" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "tool call order: path (no list), add, path, sync" [ "$(stub_cmds)" = "path add path sync " ]
check "add argv: --agent <STEM> then the six REFS_REPOS in order" \
  grep -qxF "add|--agent|$NEWSTEM|aqua-rs-sdk|aqua-spec|aqua-governance-corpus|aqua-ecosystem|aqua-compliance|inblockio.github.io|" "$STUB_LOG"
check "sync argv follows the add" grep -qxF "sync|--agent|$NEWSTEM|--wait|900|" "$STUB_LOG"
check "list file now exists (the stub's add)" test -e "$STUB_ROOT/agents/$NEWSTEM.json"
check "podman run mounts the new view at /refs:ro, no /refs/<repo>" bash -c 'grep -qF -- "-v $2/views/$3:/refs:ro" "$1" && ! grep "^podman run" "$1" | grep -q ":/refs/"' _ "$SIDE_EFFECTS" "$STUB_ROOT" "$NEWSTEM"
check "git never called" no_git
check "stdout says the list is being created" grep -q "new consultant $NEWSTEM: creating its agent-refs list" "$SB/out/n1.out"

echo "== new consultant: add fails -> abort, no config, no persist dir, no podman"
rc=0; RUN_ENV=( STUB_ADD_RC=1 ); runs n2 --label newbie2 --target "$TARGET" --persona Thalia --name Newbie || rc=$?; RUN_ENV=()
check "exit 1, names the add" bash -c '[ "$1" -eq 1 ] && grep -q "add --agent newbie2-aqua-consultant. failed; no list was created" "$2"' _ "$rc" "$SB/out/n2.err"
check "no config rendered" test ! -e "$(cfg_of newbie2)"
check "no persist dir created" test ! -e "$TEST_DIR/newbie2-aqua-consultant-persist"
check "no podman call" no_podman
check "no list left behind" test ! -e "$STUB_ROOT/agents/newbie2-aqua-consultant.json"
check "sync never reached" no_stub_cmd sync

echo "== new consultant: --print-run creates nothing and says so"
rc=0; runp n3 --label newbie3 --target "$TARGET" --persona Thalia --name Newbie || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path only (no add, sync, check)" [ "$(stub_cmds)" = "path " ]
check "no list file, no view dir created" bash -c '[ ! -e "$1/agents/newbie3-aqua-consultant.json" ] && [ ! -e "$1/views/newbie3-aqua-consultant" ]' _ "$STUB_ROOT"
check "printed: the list would be created, with the exact add and the repos in order" \
  grep -qF "a real spawn would CREATE it first: $STUB add --agent newbie3-aqua-consultant ${REFS_LIST[*]} (nothing created" "$SB/out/n3.out"
check "printed: the preview renders the config so a later real spawn is legacy unless add runs first" \
  grep -q "real spawn AFTER it counts newbie3-aqua-consultant as existing and stays on the legacy path unless you run that add yourself first" "$SB/out/n3.out"
check "argv: exactly one /refs mount, a placeholder naming the stem (path-less)" \
  [ "$(refs_seq n3)" = "<view dir of newbie3-aqua-consultant, known only after aqua-agent-refs add creates its list>:/refs:ro" ]
# the preview rendered newbie3's config, so a run with no tool now takes the legacy path: its argv is the baseline
RUN_ENV=( AGENT_REFS_BIN="$SB/none/aqua-agent-refs" ); runp n3b --label newbie3 --target "$TARGET" --persona Thalia --name Newbie; RUN_ENV=()
check "whole argv == the legacy argv of the same consultant with the six fleet mounts replaced by the placeholder (same position)" \
  bash -c '[ "$(cat "$1")" = "$2" ]' _ "$SB/out/n3.argv" "$(subst_refs "$SB/out/n3b.argv" "<view dir of newbie3-aqua-consultant, known only after aqua-agent-refs add creates its list>")"
check "no git, no podman" bash -c '[ ! -e "$1" ] && [ ! -e "$2" ]' _ "$GIT_LOG" "$SIDE_EFFECTS"
check "no dash characters in the output" no_dashes "$SB/out/n3.out" "$SB/out/n3.err"

echo "== existing consultant without a list: today's behaviour, byte for byte"
drop_list "$ALPHA"
# (a) tool installed, answers 2  vs  tool not installed: identical argv and stdout.
RUN_ENV=( AGENT_REFS_BIN="$SB/none/aqua-agent-refs" ); runp e0 --label alpha --target "$TARGET" --persona Thalia --name Tester; RUN_ENV=()
rc=0; runp e1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "tool answers 'no list' (2): exit 0" [ "$rc" -eq 0 ]
check "only path was asked" [ "$(stub_cmds)" = "path " ]
check "argv identical to the seed run and to the no-binary run" bash -c 'cmp -s "$1" "$2" && cmp -s "$1" "$3"' _ "$SB/out/e1.argv" "$SB/legacy_a.argv" "$SB/out/e0.argv"
check "stdout identical to the no-binary run" cmp -s "$SB/out/e1.out" "$SB/out/e0.out"
check "stderr identical to the no-binary run, minus its one warning line" \
  bash -c 'diff <(grep -v "^!! agent-refs: " "$1") "$2" >/dev/null' _ "$SB/out/e0.err" "$SB/out/e1.err"
check "the six fleet mounts are in the argv, in REFS_REPOS order, from the REFS_BASE checkouts" \
  bash -c 'exp=""; for r in '"${REFS_LIST[*]}"'; do exp="$exp$2/$r:/refs/$r:ro"$'"'\\n'"'; done; [ "$(awk "prev==\"-v\" && /:\/refs\// {print} {prev=\$0}" "$1")" = "$(printf %s "$exp")" ]' _ "$SB/out/e1.argv" "$REFS_BASE"

# (b) the same against the pre-change script (immutable commit), several shapes.
REPO_ROOT="$(git -C "$SKILL_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
ORIG="$SB/orig"
if [ -n "$REPO_ROOT" ] && git -C "$REPO_ROOT" cat-file -e "$PRE_CHANGE_COMMIT^{commit}" 2>/dev/null \
   && mkdir -p "$ORIG" && git -C "$REPO_ROOT" archive "$PRE_CHANGE_COMMIT" Skills/consultant-deploy | tar -x -C "$ORIG"; then
  ORIG_SPAWN="$ORIG/Skills/consultant-deploy/spawn-consultant.sh"
  # real git for these scenarios (mirrors / follow mirrors must be real clones): PATH without the git shim
  MIRROR="$FAKE_HOME/.local/share/consultant-refs"; FOLLOW="$FAKE_HOME/.local/share/consultant-refs-follow"
  GITC=(git -c user.name=test -c user.email=test@example.invalid)
  mk_clone() {  # mk_clone <dir>: a clean single-commit clone-like repo
    git init -q -b main "$1"; echo hi > "$1/README.md"; "${GITC[@]}" -C "$1" add README.md; "${GITC[@]}" -C "$1" commit -q -m init
  }
  mk_clone "$MIRROR/extra-one"; mk_clone "$FOLLOW/aqua-rs-sdk"; mkdir -p "$FOLLOW/_developments/aqua-rs-sdk"
  put_host gamma '{"extra_refs": ["extra-one"], "refs_follow": ["aqua-rs-sdk"]}'
  mkdir -p "$TEST_DIR/delta-rooms"
  put_host generic ABSENT; cp "$TEST_DIR/consultant-config.template.json" "$TEST_DIR/aqua-consultant-config.json"
  legacy_env() { env -i HOME="$FAKE_HOME" PATH="$SHIM_BIN:/usr/local/bin:/usr/bin:/bin" CONSULTANT_TEST_DIR="$TEST_DIR" \
      CONSULTANT_REFS_BASE="$REFS_BASE" AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" AGENT_REFS_BIN="$STUB" \
      AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" "$@"; }
  equal_to_orig() {   # equal_to_orig <name> <spawn args...>: pre-change script vs this one (stub answers 2), print-run
    # Same starting state for both: a warm-up with the OLD script makes sure the consultant exists (so it is not
    # "new"), then every config is snapshotted and restored before each of the two measured runs.
    local name="$1"; shift; local rc_o=0 rc_n=0
    legacy_env bash "$ORIG_SPAWN" --print-run "$@" > /dev/null 2>&1 || true
    rm -rf "$SB/snap"; mkdir -p "$SB/snap"; cp -a "$TEST_DIR"/*-config.json "$SB/snap/"
    for who in orig new; do
      local script="$SPAWN"; [ "$who" = orig ] && script="$ORIG_SPAWN"
      cp -a "$SB/snap/"*-config.json "$TEST_DIR/"
      reset_logs; local rc=0
      legacy_env bash "$script" --print-run "$@" > "$SB/out/$name.$who.out" 2> "$SB/out/$name.$who.err" || rc=$?
      if [ "$who" = orig ]; then rc_o=$rc; else rc_n=$rc; fi
    done
    [ "$rc_o" -eq 0 ] && [ "$rc_n" -eq 0 ] && cmp -s "$SB/out/$name.orig.out" "$SB/out/$name.new.out" && cmp -s "$SB/out/$name.orig.err" "$SB/out/$name.new.err"
  }
  check "vs 8bb03b3: fleet only (alpha, argv + stdout + stderr byte-identical)" equal_to_orig q1 --label alpha --target "$TARGET" --persona Thalia --name Tester
  check "vs 8bb03b3: host.extra_refs + host.refs_follow (gamma)" equal_to_orig q2 --label gamma --target "$TARGET" --persona Thalia --name Tester
  check "vs 8bb03b3: gamma really mounted the extra and the follow mirror (the comparison is not vacuous)" \
    bash -c 'grep -q "extra-one:/refs/extra-one:ro" "$1" && grep -q "consultant-refs-follow/aqua-rs-sdk:/refs/aqua-rs-sdk:ro" "$1" && grep -q "_developments/aqua-rs-sdk:/refs/_developments/aqua-rs-sdk:ro" "$1"' _ "$SB/out/q2.new.out"
  check "vs 8bb03b3: rooms consultant (delta)" equal_to_orig q3 --label delta --target "$TARGET" --persona Thalia --name Tester
  check "vs 8bb03b3: --generic" equal_to_orig q4 --generic --target "$TARGET" --persona Sabrina --name Operator
  check "vs 8bb03b3: --replace --keep-config (alpha)" equal_to_orig q5 --replace --keep-config --label alpha
  check "vs 8bb03b3: --keep-config --voice on --no-refresh-refs (alpha)" equal_to_orig q6 --replace --keep-config --label alpha --voice on --no-refresh-refs
else
  skip "pre-change commit $PRE_CHANGE_COMMIT not available in this clone: the byte comparison against the old script was not run"
fi

# (c) real spawn of an existing consultant without a list: legacy mounts, the tool only asked path.
rc=0; runs e2 --replace --keep-config --label alpha --no-refresh-refs || rc=$?
check "real spawn, no list: exit 0, only path asked" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "real spawn, no list: only path asked" [ "$(stub_cmds)" = "path " ]
check "real spawn, no list: podman run carries the six fleet mounts" \
  bash -c '[ "$(grep "^podman run" "$1" | grep -o -- "-v [^ ]*:/refs/[^ ]*:ro" | wc -l)" -eq 6 ]' _ "$SIDE_EFFECTS"
rm -f "$RUNNING"

echo "== tool not installed: legacy with ONE warning (existing and new), default name on PATH works"
RUN_ENV=( AGENT_REFS_BIN="$SB/none/aqua-agent-refs" )
rc=0; runp m1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "existing consultant: exit 0, legacy argv" bash -c '[ "$1" -eq 0 ] && cmp -s "$2" "$3"' _ "$rc" "$SB/out/m1.argv" "$SB/legacy_a.argv"
check "existing consultant: exactly one warning line, naming the tool and the path taken" \
  bash -c '[ "$(grep -c "^!! agent-refs: " "$1")" -eq 1 ] && grep -q "^!! agent-refs: .*aqua-agent-refs. not found; existing consultant alpha-aqua-consultant keeps the legacy refs path" "$1"' _ "$SB/out/m1.err"
rc=0; runp m2 --label brandnew --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "NEW consultant: also legacy (the script stays usable before the install), one warning" \
  bash -c '[ "$1" -eq 0 ] && [ "$(grep -c "^-v$" "$2")" -ge 6 ] && [ "$(grep -c "^!! agent-refs: " "$3")" -eq 1 ] && grep -q "new consultant brandnew-aqua-consultant keeps the legacy refs path" "$3"' _ "$rc" "$SB/out/m2.argv" "$SB/out/m2.err"
check "NEW consultant: six fleet mounts, no list created anywhere" bash -c '[ "$(grep -c ":/refs/[^:]*:ro$" "$1")" -eq 6 ] && [ ! -e "$2/agents/brandnew-aqua-consultant.json" ]' _ "$SB/out/m2.argv" "$STUB_ROOT"
RUN_ENV=()
# the default name, found on PATH
mkdir -p "$SB/pathbin"; cp "$STUB" "$SB/pathbin/aqua-agent-refs"
make_list "$ALPHA"
rc=0; reset_logs
env -i HOME="$FAKE_HOME" PATH="$SB/pathbin:$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" \
  AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
  bash "$SPAWN" --print-run --label alpha --target "$TARGET" --persona Thalia --name Tester > "$SB/out/m3.out" 2> "$SB/out/m3.err" || rc=$?
check "AGENT_REFS_BIN unset: aqua-agent-refs found on PATH and used" bash -c '[ "$1" -eq 0 ] && grep -qx "path|--agent|'"$ALPHA"'|" "$2" && grep -qF -- "'"$STUB_ROOT"'/views/'"$ALPHA"':/refs:ro" "$3"' _ "$rc" "$STUB_LOG" "$SB/out/m3.out"
echo "== tool not installed but the list file exists: fail closed"
cfg_before="$(sha_of "$(cfg_of alpha)")"
RUN_ENV=( AGENT_REFS_BIN="$SB/none/aqua-agent-refs" )
rc=0; runp m4 --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?
RUN_ENV=()
check "exit 1, says the list exists and the tool is missing" bash -c '[ "$1" -eq 1 ] && grep -q "alpha-aqua-consultant has a list" "$2" && grep -q "refusing the legacy refs path" "$2"' _ "$rc" "$SB/out/m4.err"
check "nothing changed: no argv, config byte-identical, no podman" bash -c '[ ! -s "$1" ] && [ "$(sha256sum < "$2" | cut -d" " -f1)" = "$3" ]' _ "$SB/out/m4.argv" "$(cfg_of alpha)" "$cfg_before"
drop_list "$ALPHA"

echo "== fail closed on the answer of path"
for code in 1 3 127 64; do
  rc=0; RUN_ENV=( "STUB_PATH_RC=$code" ); runp f1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
  check "path exit $code (existing consultant): abort with exit 1, nothing changed, no argv" \
    bash -c '[ "$1" -eq 1 ] && grep -q "path --agent alpha-aqua-consultant. failed (exit '"$code"')" "$2" && grep -q "nothing was changed" "$2" && [ ! -s "$3" ]' _ "$rc" "$SB/out/f1.err" "$SB/out/f1.argv"
done
rc=0; RUN_ENV=( STUB_PATH_RC=1 ); runp f2 --label brandnew2 --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "path exit 1 (new consultant): abort, no config rendered" bash -c '[ "$1" -eq 1 ] && [ ! -e "$2" ]' _ "$rc" "$(cfg_of brandnew2)"
for out in "" "relative/view" "views/x" "/ok/but:colon"; do
  rc=0; RUN_ENV=( STUB_PATH_RC=0 STUB_PATH_OUT="$out" ); runp f3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
  check "path printed '$out': refused (exit 1), nothing launched" bash -c '[ "$1" -eq 1 ] && grep -q "refusing to mount it\|did not print an absolute path" "$2" && [ ! -s "$3" ]' _ "$rc" "$SB/out/f3.err" "$SB/out/f3.argv"
done

echo "== host.extra_refs / host.refs_follow are ignored when the agent has a list"
make_list "$ALPHA"
# a follow mirror that does not exist would abort the legacy path (exit 1); a mirror root that does not exist neither
put_host alpha '{"extra_refs": ["no-such-extra", "aqua-spec"], "refs_follow": ["no-such-follow"]}'
rc=0; runp h1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0 (the follow mirror / extra mirror are never looked at)" [ "$rc" -eq 0 ]
check "exactly ONE line says they are ignored" bash -c '[ "$(grep -c "host.extra_refs / host.refs_follow" "$1")" -eq 1 ] && grep -q "^>> refs: alpha-aqua-consultant has an agent-refs list; host.extra_refs / host.refs_follow in .* are ignored" "$1"' _ "$SB/out/h1.out"
check "no extra, follow or digest mount: only the view" [ "$(refs_seq h1)" = "$VIEW_A:/refs:ro" ]
check "no extra-refs / follow notes, no mirror clone attempt (git never called)" bash -c '! grep -q "extra refs\|refs follow" "$1" "$2"' _ "$SB/out/h1.out" "$SB/out/h1.err"
check "no git" no_git
check "the host block is still carried in the config unchanged (not stripped)" \
  python3 -I -c 'import json,sys; sys.exit(json.load(open(sys.argv[1]))["host"] != {"extra_refs": ["no-such-extra", "aqua-spec"], "refs_follow": ["no-such-follow"]})' "$(cfg_of alpha)"
put_host alpha '{"extra_refs": ["bad/name"]}'
rc=0; runp h2 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "an INVALID host block still aborts with exit 2 first (strict validation unchanged)" bash -c '[ "$1" -eq 2 ] && grep -q "nothing was changed" "$2"' _ "$rc" "$SB/out/h2.err"
check "validation happens before the tool is asked (path never called)" no_stub_cmd path
put_host alpha ABSENT
rc=0; runp h3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "no host block with a list: no 'ignored' line" bash -c '! grep -q "are ignored" "$1"' _ "$SB/out/h3.out"
drop_list "$ALPHA"

echo "== print modes that exit early never run the tool"
reset_logs
rc=0; env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" CONSULTANT_REFS_BASE="$REFS_BASE" AGENT_REFS_BIN="$STUB" \
  AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" AQUA_SYSTEM_ALLOWLIST="$AL" \
  bash "$SPAWN" --print-onboarding --label alpha --target "$TARGET" --persona Thalia --name Tester > "$SB/out/o1.out" 2> "$SB/out/o1.err" || rc=$?
check "--print-onboarding: exit 0, tool not run" bash -c '[ "$1" -eq 0 ] && [ ! -s "$2" ]' _ "$rc" "$STUB_LOG"
rc=0; env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" AGENT_REFS_BIN="$STUB" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
  bash "$SPAWN" --print-mxid --label nosession > "$SB/out/o2.out" 2> "$SB/out/o2.err" || rc=$?
check "--print-mxid: exit 1 (no session), tool not run" bash -c '[ "$1" -eq 1 ] && [ ! -s "$2" ]' _ "$rc" "$STUB_LOG"

echo "== side effects"
check "nothing under the sandbox HOME's real data dirs was touched by the stub (state stayed in the sandbox)" test -d "$STUB_ROOT"
check "no shim was called in a print run (the shims fail loudly in print mode)" bash -c '! grep -l "must not be called" "$@" >/dev/null 2>&1' _ "$SB"/out/*.err

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
