#!/usr/bin/env bash
#
# spawn-consultant-agent-refs.sh, no-live-side-effect tests for the refs path of spawn-consultant.sh
# (header, "Per-agent refs lists"): the agent's aqua-agent-refs view is the ONLY /refs source.
#
# `aqua-agent-refs` does not run here: AGENT_REFS_BIN points at a stub that records every argv and
# plays a tiny list store (AGENT_REFS_ROOT in the sandbox) with scripted exit codes. HOME, the
# config/persist dir, the token and the allow-list are in a temp dir; podman, systemctl and
# systemd-run are shims (podman keeps a "container is running" flag file, so a --replace abort is
# visible as "the old container is still running"); git is a shim that logs and fails (the refs
# path must never reach git).
#
# Covers:
#   - list present: print-run uses `path` + `check` (never sync/add), exactly one `-v <view>:/refs:ro`
#     in the position the refs mounts have in the argv, no /refs/<repo> mount, no git; a real spawn runs
#     `sync --agent=<STEM> --wait 900`; sync exit 3 proceeds with a STALE warning; sync exit
#     1 / 2 / 4 aborts before the config render and any podman call, and on --replace the old
#     container is still running; --no-refresh-refs runs check (failure aborts). Every call passes the
#     key as --agent=<STEM> (the stub rejects the spaced form).
#   - existing vs new is decided by the store identity (agent.pem, or a persisted session) or an existing
#     container, never by the config file: a preview then a real spawn of a new consultant takes the new
#     path; a store identity or a container without a list is an existing consultant and aborts; podman
#     answering anything but exists / does not exist aborts
#   - new consultant: `add --agent=<STEM> --wait 900 <REFS_REPOS in order>` ALWAYS runs first (also when a
#     partial list exists: the retry tops it up), exit 0 or 3 proceed, 1 / 2 / 4 / 127 abort with the
#     partial-list hint and nothing launched; then path, then sync, then exactly one view mount;
#     --print-run runs nothing and says so
#   - existing consultant without a list (path exit 5): exit 2 before any change (print-run, real spawn,
#     --replace, --keep-config, --no-refresh-refs; a container alone also counts), the message names
#     `aqua-agent-refs add --agent=<STEM> <REFS_REPOS>`, config / allow-list / running container untouched
#   - binary not installed: exit 2 before any change for an existing and for a new consultant, also when
#     the list file exists (or is a dangling symlink), with the install hint and no legacy fallback;
#     default name found on PATH works, else ~/.local/bin/aqua-agent-refs when executable (PATH wins)
#   - the retired legacy path is gone: its env vars have no effect, its code is not in the script
#   - fail closed on `path`: any exit but 0 and 5 (1/2/3/4/64/127) aborts with nothing changed; exit 2 or 5
#     while the list file (or a symlink) exists aborts with no podman call at all; a real spawn aborted
#     by `path` leaves the Owner allow-list byte-identical
#   - the view path: exactly <AGENT_REFS_ROOT>/views/<STEM> (realpath -m), nothing stripped (a trailing CR,
#     a second line, a colon, a relative or foreign path are refused); the dir must be a plain directory
#     (missing, file, symlink refused), checked after the sync and again right before podman run
#   - host.extra_refs / host.refs_follow: one "ignored" line, no extra mounts, for an existing and a new
#     consultant, no mirror or follow-root access
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

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok   $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL $1" >&2; }
skip() { echo "  skip $1"; }
check() { local desc="$1"; shift; if "$@"; then ok "$desc"; else bad "$desc"; fi; }

SB="$(mktemp -d "${TMPDIR:-/tmp}/spawn-agent-refs-test.XXXXXX")"
if [ -n "${KEEP_SANDBOX:-}" ]; then echo "sandbox kept: $SB"; else trap 'rm -rf "$SB"' EXIT; fi
FAKE_HOME="$SB/home"; TEST_DIR="$SB/aqua-matrix-test"; SHIM_BIN="$SB/bin"
NOGIT_BIN="$SB/nogit-bin"; STUB="$SB/stub/aqua-agent-refs"; STUB_ROOT="$SB/agent-refs-root"
STUB_LOG="$SB/stub.log"; SIDE_EFFECTS="$SB/side-effects.log"; GIT_LOG="$SB/git.log"; RUNNING="$SB/container.running"
TOKEN_FILE="$FAKE_HOME/.aqua-matrix-heartbeat/claude-oauth-token"; AL="$SB/allowlist.toml"
TARGET='@did-key-zfaketestpeer:matrix.inblock.io'
REFS_LIST=(aqua-rs-sdk aqua-governance-corpus aqua-ecosystem aqua-compliance inblockio.github.io)
mkdir -p "$FAKE_HOME/.aqua-matrix-heartbeat" "$TEST_DIR" "$SHIM_BIN" "$NOGIT_BIN" "$SB/stub" "$STUB_ROOT/agents" "$STUB_ROOT/views" "$SB/out"
echo fake-token > "$TOKEN_FILE"
cp "$TEMPLATE" "$TEST_DIR/consultant-config.template.json"
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
  "rm -f")
    rm -f "@RUNNING@"
    # SHIM_SWAP_VIEW=<dir>: replace the view dir by a symlink between the removal and the launch
    if [ -n "${SHIM_SWAP_VIEW:-}" ]; then rm -rf "$SHIM_SWAP_VIEW"; ln -s /etc "$SHIM_SWAP_VIEW"; fi
    exit 0 ;;
  "run -d")
    # the agent's first login: persist the session in the store mounted at /agent/store
    for a in "$@"; do
      case "$a" in
        *:/agent/store:U) st="${a%:/agent/store:U}"; mkdir -p "$st"
          [ -e "$st/config.toml" ] || printf '[session]\nuser_id = "@0a1b2c3d4e5f6g7h:matrix.inblock.io"\n' > "$st/config.toml" ;;
      esac
    done
    touch "@RUNNING@"; echo 0123456789abcdef0123; exit 0 ;;
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
# git shim: logs and fails. The refs path must never reach git (no checkout is touched any more).
printf '#!/bin/sh\necho "git $*" >> "%s"\nexit 1\n' "$GIT_LOG" > "$NOGIT_BIN/git"
chmod +x "$NOGIT_BIN/git"
printf '#!/usr/bin/env python3\nimport json, sys\nopen("%s", "a").write(json.dumps(sys.argv[1:]) + "\\n")\n' "$SB/notify.jsonl" > "$SB/notify"
chmod +x "$SB/notify"

# --- the stub aqua-agent-refs. Records argv as "a|b|c|" lines. State = $AGENT_REFS_ROOT/agents/<key>.json (one
# repo per line, so a retry's add can top it up) + the view dir $AGENT_REFS_ROOT/views/<key>. The key must come
# in the equals form (--agent=<key>); the spaced form is a stub error (exit 64), so the suite proves it.
# path: forced by STUB_PATH_RC (+ STUB_PATH_OUT), else 0 + the view dir when the list file exists, else 5.
# add: STUB_ADD_RC (default 0). Exit 0 or 3 write the whole list; another code writes only the first two repos
#      when STUB_ADD_PARTIAL is set (a partial list, as the real tool may leave), else nothing.
# sync / check: STUB_SYNC_RC / STUB_CHECK_RC (default 0). STUB_SYNC_SWAP=1: sync swaps the view dir for a symlink.
cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
set -u
{ printf '%s|' "$@"; echo; } >> "$STUB_LOG"
cmd="${1:-}"; shift || true
agent=""; repos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --agent=*) agent="${1#--agent=}"; shift ;;
    --agent) echo "stub: --agent needs the equals form (--agent=<key>)" >&2; exit 64 ;;
    --wait) shift 2 ;;
    *) repos+=("$1"); shift ;;
  esac
done
LIST="$AGENT_REFS_ROOT/agents/$agent.json"
merge() { mkdir -p "$AGENT_REFS_ROOT/agents" "$AGENT_REFS_ROOT/views/$agent"; { [ ! -e "$LIST" ] || cat "$LIST"; printf '%s\n' "$@"; } | sort -u > "$LIST.new"; mv "$LIST.new" "$LIST"; }
case "$cmd" in
  path)
    if [ -n "${STUB_PATH_RC:-}" ]; then
      [ -z "${STUB_PATH_OUT+x}" ] || printf '%s\n' "$STUB_PATH_OUT"
      exit "$STUB_PATH_RC"
    fi
    [ -e "$LIST" ] || exit 5
    printf '%s\n' "$AGENT_REFS_ROOT/views/$agent"; exit 0 ;;
  add)
    rc="${STUB_ADD_RC:-0}"
    if [ "$rc" -eq 0 ] || [ "$rc" -eq 3 ]; then merge "${repos[@]}"
    elif [ -n "${STUB_ADD_PARTIAL:-}" ]; then merge "${repos[0]}" "${repos[1]}"; fi
    exit "$rc" ;;
  sync)
    if [ -n "${STUB_SYNC_SWAP:-}" ]; then rm -rf "$AGENT_REFS_ROOT/views/$agent"; ln -s /etc "$AGENT_REFS_ROOT/views/$agent"; fi
    exit "${STUB_SYNC_RC:-0}" ;;
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
  env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" \
    AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" AGENT_REFS_BIN="$STUB" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
    ${RUN_ENV[@]+"${RUN_ENV[@]}"} \
    bash "$SPAWN" --print-run "$@" > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"
  awk '/^podman$/{p=1} p' "$SB/out/$name.out" > "$SB/out/$name.argv"
  return "$rc"
}
runs() {   # runs <name> <spawn args...>: a REAL spawn with podman shimmed (SHIM_MODE=spawn)
  local name="$1"; shift; local rc=0; reset_logs
  env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" SHIM_MODE=spawn CONSULTANT_TEST_DIR="$TEST_DIR" \
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
mount_after_memory() {   # mount_after_memory <argv file> <source:/refs:ro>: the refs mount is the pair right after the memory mount
  awk -v want="$2" 'prev=="-v" && /:\/agent\/memory:/ {m=NR} prev=="-v" && NR==m+2 {got=$0} {prev=$0} END{exit !(m && got==want)}' "$1"
}

ALPHA=alpha-aqua-consultant

echo "== syntax + static"
check "bash -n spawn-consultant.sh" bash -n "$SPAWN"
check "AGENT_REFS_BIN defaults to aqua-agent-refs (PATH, else ~/.local/bin, see the default-resolution section)" grep -qF 'AGENT_REFS_BIN=aqua-agent-refs' "$SPAWN"
check "the header documents AGENT_REFS_BIN and the list path" bash -c 'grep -q "AGENT_REFS_BIN  *the aqua-agent-refs binary" "$1" && grep -q "Per-agent refs lists" "$1"' _ "$SPAWN"
check "the legacy refs path is gone: no REFS_BASE / MIRROR / FOLLOW_ROOT / REMOTE variable or env var" \
  bash -c '! grep -nE "REFS_BASE|REFS_MIRROR|FOLLOW_ROOT|REFS_REMOTE|CONSULTANT_REFS_(BASE|MIRROR|REMOTE|FOLLOW)" "$1"' _ "$SPAWN"
check "the legacy refs path is gone: no REFS_MODE=legacy, no checkout freshness pass, no extra / follow mirror code" \
  bash -c '! grep -nE "REFS_MODE=legacy|refresh_refs|sync_extra_refs|check_follow_refs|^EXTRA_REFS=|^REFS_FOLLOW=|is_fleet_repo|is_followed|keeps the legacy refs path" "$1"' _ "$SPAWN"
check "exactly one refs mount is added to the argv" bash -c '[ "$(grep -c "REF_MOUNT_ARGS+=" "$1")" -eq 1 ]' _ "$SPAWN"

# An existing consultant: render alpha's config once (as a NEW consultant: a preview creates nothing), then give it an identity.
runp seed_a --label alpha --target "$TARGET" --persona Thalia --name Tester
seed_session alpha
check "seed: alpha's config is rendered, no list exists" bash -c '[ -f "$1" ] && [ ! -e "$2/agents/alpha-aqua-consultant.json" ]' _ "$(cfg_of alpha)" "$STUB_ROOT"

echo "== list present: --print-run uses path + check, one view mount in the fleet position"
make_list "$ALPHA"; VIEW_A="$STUB_ROOT/views/$ALPHA"
rc=0; runp l1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path then check, nothing else" [ "$(stub_cmds)" = "path check " ]
check "check argv is exactly: check --agent=<STEM> (equals form)" grep -qxF "check|--agent=$ALPHA|" "$STUB_LOG"
check "exactly one /refs mount: the view, read-only" [ "$(refs_seq l1)" = "$VIEW_A:/refs:ro" ]
check "the view mount is the pair right after the memory mount (where the refs mounts sit)" mount_after_memory "$SB/out/l1.argv" "$VIEW_A:/refs:ro"
check "no /refs/<repo> mount" bash -c '! grep -q ":/refs/" "$1"' _ "$SB/out/l1.argv"
check "git never called (no checkout is touched)" no_git
check "no podman / systemctl call" no_podman
check "no dash characters in stdout/stderr" no_dashes "$SB/out/l1.out" "$SB/out/l1.err"
check "no agent-refs warning or note on stderr for a healthy list" bash -c '! grep -q "^!! agent-refs\|^!! refs" "$1"' _ "$SB/out/l1.err"

echo "== list present: the retired legacy env vars have no effect"
rc=0; RUN_ENV=( CONSULTANT_REFS_BASE="$SB/refs-base" CONSULTANT_REFS_MIRROR="$SB/refs-mirror" CONSULTANT_REFS_FOLLOW_ROOT="$SB/refs-follow" CONSULTANT_REFS_REMOTE="$SB/refs-remote" )
mkdir -p "$SB/refs-base/aqua-rs-sdk" "$SB/refs-mirror/x" "$SB/refs-follow/x"
runp l2 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "exit 0" [ "$rc" -eq 0 ]
check "argv still mounts only the view, nothing from the retired roots" bash -c '[ "$(awk "prev==\"-v\" && /:\/refs/ {print} {prev=\$0}" "$1")" = "$2:/refs:ro" ] && ! grep -q "refs-base\|refs-mirror\|refs-follow" "$1"' _ "$SB/out/l2.argv" "$VIEW_A"
check "git never called" no_git

echo "== list present: --print-run, check fails -> abort"
rc=0; RUN_ENV=( STUB_CHECK_RC=1 ); runp l3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "exit non-zero" [ "$rc" -ne 0 ]
check "names the failed check, nothing launched" grep -q "check --agent=$ALPHA' failed (exit 1)" "$SB/out/l3.err"
check "no argv printed" bash -c '[ ! -s "$1" ]' _ "$SB/out/l3.argv"
check "sync never called under --print-run" no_stub_cmd sync

echo "== list present: real spawn runs sync --wait 900, then launches"
touch "$RUNNING"
rc=0; runs r1 --replace --keep-config --label alpha || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path then sync, never check/add" [ "$(stub_cmds)" = "path sync " ]
check "sync argv is exactly: sync --agent=<STEM> --wait 900" grep -qxF "sync|--agent=$ALPHA|--wait|900|" "$STUB_LOG"
check "podman run mounts the view at /refs:ro" grep -qF -- "-v $VIEW_A:/refs:ro" "$SIDE_EFFECTS"
check "podman run has exactly one /refs mount, from views/<STEM>, and no /refs/<repo>" \
  bash -c '[ "$(grep "^podman run" "$1" | grep -o -- " -v [^ ]*:/refs[:/][^ ]*" | tr -d " " | tr "\n" " ")" = "-v$2:/refs:ro " ]' _ "$SIDE_EFFECTS" "$VIEW_A"
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
  check "sync exit $code: exit 1, names the failure" bash -c '[ "$1" -eq 1 ] && grep -q "sync --agent='"$ALPHA"' --wait 900. failed (exit '"$code"')" "$2"' _ "$rc" "$SB/out/r3.err"
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

echo "== new consultant: add (REFS_REPOS order, --wait 900), then path, then sync"
check "REFS_REPOS in the script matches the order this test expects" \
  bash -c '[ "$(grep -E "^REFS_REPOS=\(" "$1" | sed "s/^REFS_REPOS=(//; s/)\$//")" = "$2" ]' _ "$SPAWN" "${REFS_LIST[*]}"
NEWSTEM=newbie-aqua-consultant
add_line() { echo "add|--agent=$1|--wait|900|aqua-rs-sdk|aqua-governance-corpus|aqua-ecosystem|aqua-compliance|inblockio.github.io|"; }
rm -f "$RUNNING"
rc=0; runs n1 --label newbie --target "$TARGET" --persona Thalia --name Newbie || rc=$?
check "exit 0 and launched" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "tool call order: path (no list), add, path, sync" [ "$(stub_cmds)" = "path add path sync " ]
check "add argv: --agent=<STEM> --wait 900 then the five REFS_REPOS in order" grep -qxF "$(add_line "$NEWSTEM")" "$STUB_LOG"
check "every call used the equals form (the stub rejects the spaced one with exit 64)" bash -c '! grep -q "^[a-z]*|--agent|" "$1"' _ "$STUB_LOG"
check "sync argv follows the add" grep -qxF "sync|--agent=$NEWSTEM|--wait|900|" "$STUB_LOG"
check "list file now holds all five repos (the stub's add)" bash -c '[ "$(sort "$1/agents/$2.json" | tr "\n" " ")" = "$(printf "%s\n" '"${REFS_LIST[*]}"' | sort | tr "\n" " ")" ]' _ "$STUB_ROOT" "$NEWSTEM"
check "podman run has exactly one /refs mount: the new view at /refs:ro, no /refs/<repo>" \
  bash -c '[ "$(grep "^podman run" "$1" | grep -o -- " -v [^ ]*:/refs[:/][^ ]*" | tr -d " " | tr "\n" " ")" = "-v$2/views/$3:/refs:ro " ]' _ "$SIDE_EFFECTS" "$STUB_ROOT" "$NEWSTEM"
check "the view was synced before podman run (add, path, sync, then the launch)" bash -c 'grep -q "^sync|" "$1" && grep -q "^podman run" "$2"' _ "$STUB_LOG" "$SIDE_EFFECTS"
check "podman was only asked 'container exists' before the launch, never rm" bash -c 'grep -q "^podman container exists" "$1" && ! grep -q "^podman rm" "$1"' _ "$SIDE_EFFECTS"
check "git never called" no_git
check "stdout says the list is being created or topped up" grep -q "new consultant $NEWSTEM: creating or topping up its agent-refs list" "$SB/out/n1.out"
rm -f "$RUNNING"

echo "== new consultant: add fails (no list written) -> abort, no config, no persist dir, no podman change"
rc=0; RUN_ENV=( STUB_ADD_RC=1 ); runs n2 --label newbie2 --target "$TARGET" --persona Thalia --name Newbie || rc=$?; RUN_ENV=()
check "exit 1, names the add and the partial-list hint" bash -c '[ "$1" -eq 1 ] && grep -q "add --agent=newbie2-aqua-consultant --wait 900. failed (exit 1)" "$2" && grep -q "a partial list may exist (inspect it with: .* list --agent=newbie2-aqua-consultant; re-running" "$2"' _ "$rc" "$SB/out/n2.err"
check "no config rendered" test ! -e "$(cfg_of newbie2)"
check "no persist dir created" test ! -e "$TEST_DIR/newbie2-aqua-consultant-persist"
check "no podman rm / run / start" bash -c '! grep -qE "^podman (rm|run|start)" "$1" 2>/dev/null' _ "$SIDE_EFFECTS"
check "no list left behind" test ! -e "$STUB_ROOT/agents/newbie2-aqua-consultant.json"
check "sync never reached" no_stub_cmd sync

echo "== new consultant: add exit 3 proceeds (degraded), 2 / 4 / 127 abort"
rm -f "$RUNNING"
rc=0; RUN_ENV=( STUB_ADD_RC=3 ); runs na3 --label addthree --target "$TARGET" --persona Thalia --name Newbie || rc=$?; RUN_ENV=()
check "add exit 3: exit 0, launched" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "add exit 3: warns, then path and sync still run" bash -c 'grep -q "add degraded (exit 3)" "$1" && [ "$(cut -d"|" -f1 "$2" | tr "\n" " ")" = "path add path sync " ]' _ "$SB/out/na3.err" "$STUB_LOG"
check "add exit 3: podman run mounts the view" grep -qF -- "-v $STUB_ROOT/views/addthree-aqua-consultant:/refs:ro" "$SIDE_EFFECTS"
rm -f "$RUNNING"
for code in 2 4 127; do
  rc=0; RUN_ENV=( "STUB_ADD_RC=$code" ); runs na4 --label "addfail$code" --target "$TARGET" --persona Thalia --name Newbie || rc=$?; RUN_ENV=()
  check "add exit $code: aborts (exit 1), names the exit code and the partial-list hint" bash -c '[ "$1" -eq 1 ] && grep -q "failed (exit '"$code"'); a partial list may exist" "$2"' _ "$rc" "$SB/out/na4.err"
  check "add exit $code: no config, no podman rm/run, no sync" bash -c '[ ! -e "$1" ] && ! grep -qE "^podman (rm|run)" "$2" 2>/dev/null && ! grep -q "^sync|" "$3"' _ "$(cfg_of "addfail$code")" "$SIDE_EFFECTS" "$STUB_LOG"
done

echo "== new consultant: PARTIAL add (list written with a subset, exit 1) aborts, the retry tops the list up"
rc=0; RUN_ENV=( STUB_ADD_RC=1 STUB_ADD_PARTIAL=1 ); runs pa1 --label partial --target "$TARGET" --persona Thalia --name Newbie || rc=$?; RUN_ENV=()
PSTEM=partial-aqua-consultant
check "partial add: exit 1, nothing launched, no config" bash -c '[ "$1" -eq 1 ] && [ ! -e "$2" ] && ! grep -qE "^podman (rm|run)" "$3" 2>/dev/null' _ "$rc" "$(cfg_of partial)" "$SIDE_EFFECTS"
check "partial add: the list file exists with only two repos" bash -c '[ -e "$1/agents/$2.json" ] && [ "$(wc -l < "$1/agents/$2.json")" -eq 2 ]' _ "$STUB_ROOT" "$PSTEM"
rm -f "$RUNNING"
rc=0; runs pa2 --label partial --target "$TARGET" --persona Thalia --name Newbie || rc=$?
check "retry: exit 0, launched" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "retry: path now finds a list, but the consultant is still NEW, so add runs again with all five repos" \
  bash -c '[ "$(cut -d"|" -f1 "$2" | tr "\n" " ")" = "path add path sync " ] && grep -qxF "$3" "$2"' _ "$rc" "$STUB_LOG" "$(add_line "$PSTEM")"
check "retry: the list now holds all five repos" bash -c '[ "$(wc -l < "$1/agents/$2.json")" -eq 5 ]' _ "$STUB_ROOT" "$PSTEM"
check "retry: the view is mounted" grep -qF -- "-v $STUB_ROOT/views/$PSTEM:/refs:ro" "$SIDE_EFFECTS"
rm -f "$RUNNING"

echo "== new consultant: --print-run creates nothing and says so"
rc=0; runp n3 --label newbie3 --target "$TARGET" --persona Thalia --name Newbie || rc=$?
check "exit 0" [ "$rc" -eq 0 ]
check "tool calls: path only (no add, sync, check)" [ "$(stub_cmds)" = "path " ]
check "no list file, no view dir created" bash -c '[ ! -e "$1/agents/newbie3-aqua-consultant.json" ] && [ ! -e "$1/views/newbie3-aqua-consultant" ]' _ "$STUB_ROOT"
check "printed: the add a real spawn would run first, with the repos in order" \
  grep -qF "a real spawn would run this first, which CREATES its agent-refs list (or tops up a partial one): $STUB add --agent=newbie3-aqua-consultant --wait 900 ${REFS_LIST[*]} (nothing run" "$SB/out/n3.out"
check "argv: exactly one /refs mount, a placeholder naming the stem (path-less)" \
  [ "$(refs_seq n3)" = "<view dir of newbie3-aqua-consultant, known only after aqua-agent-refs add creates its list>:/refs:ro" ]
check "the placeholder mount is the pair right after the memory mount" \
  mount_after_memory "$SB/out/n3.argv" "<view dir of newbie3-aqua-consultant, known only after aqua-agent-refs add creates its list>:/refs:ro"
check "no git, no podman (a preview never asks podman)" bash -c '[ ! -e "$1" ] && [ ! -e "$2" ]' _ "$GIT_LOG" "$SIDE_EFFECTS"
check "no dash characters in the output" no_dashes "$SB/out/n3.out" "$SB/out/n3.err"

echo "== PREVIEW THEN REAL SPAWN of a new consultant: the config the preview rendered does not make it existing"
check "precondition: the preview left newbie3's config behind, no store identity, no container" bash -c '[ -f "$1" ] && [ ! -s "$2/agent.pem" ] && [ ! -e "$2/config.toml" ] && [ ! -e "$3" ]' _ "$(cfg_of newbie3)" "$TEST_DIR/newbie3-aqua-consultant-persist/store" "$RUNNING"
rm -f "$RUNNING"
rc=0; runs ps1 --label newbie3 --target "$TARGET" --persona Thalia --name Newbie || rc=$?
check "real spawn after the preview: exit 0" bash -c '[ "$1" -eq 0 ]' _ "$rc"
check "real spawn after the preview takes the NEW path: path, add, path, sync" [ "$(stub_cmds)" = "path add path sync " ]
check "the add carries the five fleet repos" grep -qxF "$(add_line newbie3-aqua-consultant)" "$STUB_LOG"
check "podman run mounts the view at /refs:ro and no /refs/<repo> (not the dev working trees)" bash -c 'grep -qF -- "-v $2/views/newbie3-aqua-consultant:/refs:ro" "$1" && ! grep "^podman run" "$1" | grep -q ":/refs/"' _ "$SIDE_EFFECTS" "$STUB_ROOT"
rm -f "$RUNNING"

echo "== existing consultant without a list: exit 2 before any change, the message names the add command"
echo "   (existing = an identity in the store: alpha has a persisted session)"
drop_list "$ALPHA"; rm -rf "$STUB_ROOT/views/$ALPHA"
cfg_before="$(sha_of "$(cfg_of alpha)")"
al_before="$(sha_of "$AL")"
al_files_before="$(find "$SB" -maxdepth 1 -name 'allowlist*' | sort | tr '\n' ' ')"
ADD_FIX="$STUB add --agent=$ALPHA ${REFS_LIST[*]}"
no_change() {   # no_change <name>: config + allow-list byte-identical, no list or view created, no git, no systemctl / systemd-run
  [ "$(sha_of "$(cfg_of alpha)")" = "$cfg_before" ] && [ "$(sha_of "$AL")" = "$al_before" ] \
    && [ "$(find "$SB" -maxdepth 1 -name 'allowlist*' | sort | tr '\n' ' ')" = "$al_files_before" ] \
    && [ ! -e "$STUB_ROOT/agents/$ALPHA.json" ] && [ ! -e "$STUB_ROOT/views/$ALPHA" ] && no_git \
    && { [ ! -e "$SIDE_EFFECTS" ] || ! grep -qE '^(systemctl|systemd-run|podman (rm|run|start))' "$SIDE_EFFECTS"; }
}
rc=0; runp e1 --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?
check "print-run: exit 2, no argv printed" bash -c '[ "$1" -eq 2 ] && [ ! -s "$2" ]' _ "$rc" "$SB/out/e1.argv"
check "message: the exact fix, '<tool> add --agent=<STEM> <REFS_REPOS>' (the fleet default list, in order)" grep -qF -- "$ADD_FIX" "$SB/out/e1.err"
check "message: says nothing was changed and why (no list, no other refs source)" grep -q "has NO agent-refs list, and the list is its only refs source (nothing was changed)" "$SB/out/e1.err"
check "only path was asked (no add, sync or check)" [ "$(stub_cmds)" = "path " ]
check "nothing changed: config byte-identical (not even re-rendered with the new persona), allow-list untouched, no list or view" no_change
check "no dash characters in the output" no_dashes "$SB/out/e1.out" "$SB/out/e1.err"
touch "$RUNNING"
rc=0; runs e2 --replace --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?
check "real --replace spawn: exit 2, names the same fix" bash -c '[ "$1" -eq 2 ] && grep -qF -- "$3" "$2"' _ "$rc" "$SB/out/e2.err" "$ADD_FIX"
check "real --replace spawn: the old container is still running, no podman rm / run, nothing else changed" bash -c '[ -e "$1" ]' _ "$RUNNING"
check "real --replace spawn: nothing changed (config, allow-list, no list)" no_change
check "real --replace spawn: only path asked" [ "$(stub_cmds)" = "path " ]
rc=0; runs e3 --replace --keep-config --label alpha --no-refresh-refs || rc=$?
check "--keep-config --no-refresh-refs: exit 2 too, old container still running, nothing changed" bash -c '[ "$1" -eq 2 ] && [ -e "$2" ]' _ "$rc" "$RUNNING"
check "--keep-config --no-refresh-refs: nothing changed" no_change
check "--keep-config --no-refresh-refs: only path asked" [ "$(stub_cmds)" = "path " ]
rm -f "$RUNNING"
# once the list exists (as the message says), the same spawn goes through
make_list "$ALPHA"
rc=0; runs e4 --replace --keep-config --label alpha || rc=$?
check "after '$(basename "$STUB") add' created the list: the same spawn works (exit 0, view mounted)" bash -c '[ "$1" -eq 0 ] && grep -qF -- "-v $2/views/$3:/refs:ro" "$4"' _ "$rc" "$STUB_ROOT" "$ALPHA" "$SIDE_EFFECTS"
drop_list "$ALPHA"; rm -f "$RUNNING"; rm -rf "$STUB_ROOT/views/$ALPHA"

echo "== tool not installed: exit 2 before any change, install hint, no legacy fallback (existing and new consultant alike)"
NOBIN="$SB/none/aqua-agent-refs"
drop_list "$ALPHA"
cfg_before="$(sha_of "$(cfg_of alpha)")"; al_before="$(sha_of "$AL")"
al_files_before="$(find "$SB" -maxdepth 1 -name 'allowlist*' | sort | tr '\n' ' ')"
INSTALL_HINT="cargo install --locked --path crates/aqua-agent-refs --root ~/.local"
RUN_ENV=( AGENT_REFS_BIN="$NOBIN" )
rc=0; runp m1 --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?
check "existing consultant, print-run: exit 2, no argv" bash -c '[ "$1" -eq 2 ] && [ ! -s "$2" ]' _ "$rc" "$SB/out/m1.argv"
check "message: tool named, no fallback, nothing changed, install hint" \
  bash -c 'grep -qF "!! aborting: '"'"'$2'"'"' not found; the aqua-agent-refs view is the only /refs source and there is no fallback (nothing was changed)" "$1" && grep -qF -- "$3" "$1" && grep -q "or set AGENT_REFS_BIN" "$1"' _ "$SB/out/m1.err" "$NOBIN" "$INSTALL_HINT"
check "no legacy path: no warning line, no fleet mounts, nothing on stdout" bash -c '! grep -q "legacy" "$1" && [ ! -s "$2" ]' _ "$SB/out/m1.err" "$SB/out/m1.out"
check "nothing changed: config byte-identical, allow-list untouched, no podman, no git" bash -c '[ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ] && [ ! -e "$3" ] && [ ! -e "$4" ]' _ "$(cfg_of alpha)" "$cfg_before" "$SIDE_EFFECTS" "$GIT_LOG"
rc=0; runp m2 --label brandnew --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "NEW consultant, print-run: exit 2, install hint" bash -c '[ "$1" -eq 2 ] && grep -qF -- "$3" "$2"' _ "$rc" "$SB/out/m2.err" "$INSTALL_HINT"
check "NEW consultant: no config rendered, no persist dir, no list anywhere" bash -c '[ ! -e "$1" ] && [ ! -e "$2" ] && [ ! -e "$3/agents/brandnew-aqua-consultant.json" ]' _ "$(cfg_of brandnew)" "$TEST_DIR/brandnew-aqua-consultant-persist" "$STUB_ROOT"
touch "$RUNNING"
rc=0; runs m6 --replace --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?
check "existing consultant, real --replace spawn: exit 2" bash -c '[ "$1" -eq 2 ] && grep -qF -- "$3" "$2"' _ "$rc" "$SB/out/m6.err" "$INSTALL_HINT"
check "real spawn: the old container is still running, no podman call at all, no systemctl" bash -c '[ -e "$1" ] && [ ! -e "$2" ]' _ "$RUNNING" "$SIDE_EFFECTS"
check "real spawn: config and Owner allow-list byte-identical, no backup or temp file beside it" \
  bash -c '[ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ] && [ "$(sha256sum < "$3" | cut -d" " -f1)" = "$4" ] && [ "$(find "$5" -maxdepth 1 -name "allowlist*" | sort | tr "\n" " ")" = "$6" ]' _ "$(cfg_of alpha)" "$cfg_before" "$AL" "$al_before" "$SB" "$al_files_before"
rm -f "$RUNNING"
rc=0; runs m7 --label brandnew2 --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "NEW consultant, real spawn: exit 2, no config, no persist dir, no podman call (not even 'container exists')" \
  bash -c '[ "$1" -eq 2 ] && [ ! -e "$2" ] && [ ! -e "$3" ] && [ ! -e "$4" ]' _ "$rc" "$(cfg_of brandnew2)" "$TEST_DIR/brandnew2-aqua-consultant-persist" "$SIDE_EFFECTS"
check "NEW consultant, real spawn: the Owner allow-list is byte-identical" bash -c '[ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ]' _ "$AL" "$al_before"
check "no dash characters in the output" no_dashes "$SB/out/m1.err" "$SB/out/m2.err" "$SB/out/m6.err" "$SB/out/m7.err"
rc=0; env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" AGENT_REFS_BIN="$NOBIN" AQUA_SYSTEM_ALLOWLIST="$AL" \
  bash "$SPAWN" --print-onboarding --label alpha --target "$TARGET" --persona Thalia --name Tester > "$SB/out/m8.out" 2> "$SB/out/m8.err" || rc=$?
check "--print-onboarding does not need the tool: exit 0" [ "$rc" -eq 0 ]
RUN_ENV=()
# the default name, found on PATH
mkdir -p "$SB/pathbin"; cp "$STUB" "$SB/pathbin/aqua-agent-refs"
make_list "$ALPHA"
rc=0; reset_logs
env -i HOME="$FAKE_HOME" PATH="$SB/pathbin:$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" \
  AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
  bash "$SPAWN" --print-run --label alpha --target "$TARGET" --persona Thalia --name Tester > "$SB/out/m3.out" 2> "$SB/out/m3.err" || rc=$?
check "AGENT_REFS_BIN unset: aqua-agent-refs found on PATH and used" bash -c '[ "$1" -eq 0 ] && grep -qx "path|--agent='"$ALPHA"'|" "$2" && grep -qF -- "'"$STUB_ROOT"'/views/'"$ALPHA"':/refs:ro" "$3"' _ "$rc" "$STUB_LOG" "$SB/out/m3.out"
echo "== tool not installed but the list file exists: exit 2 too, the install hint"
cfg_before="$(sha_of "$(cfg_of alpha)")"
RUN_ENV=( AGENT_REFS_BIN="$NOBIN" )
rc=0; runp m4 --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?
RUN_ENV=()
check "exit 2, says the tool is not installed (the list file does not change that)" bash -c '[ "$1" -eq 2 ] && grep -q "not found; the aqua-agent-refs view is the only /refs source" "$2" && grep -qF -- "$3" "$2"' _ "$rc" "$SB/out/m4.err" "$INSTALL_HINT"
check "nothing changed: no argv, config byte-identical, no podman" bash -c '[ ! -s "$1" ] && [ "$(sha256sum < "$2" | cut -d" " -f1)" = "$3" ] && [ ! -e "$4" ]' _ "$SB/out/m4.argv" "$(cfg_of alpha)" "$cfg_before" "$SIDE_EFFECTS"
drop_list "$ALPHA"

echo "== tool not installed, list file is a dangling symlink: exit 2 too"
ln -s "$SB/no-such-target" "$STUB_ROOT/agents/$ALPHA.json"
RUN_ENV=( AGENT_REFS_BIN="$NOBIN" ); rc=0; runp m5 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "dangling symlink as list file + tool missing: exit 2, install hint, no argv" bash -c '[ "$1" -eq 2 ] && grep -q "not found; the aqua-agent-refs view is the only /refs source" "$2" && [ ! -s "$3" ]' _ "$rc" "$SB/out/m5.err" "$SB/out/m5.argv"
drop_list "$ALPHA"

echo "== existing vs new is decided by the store identity or the container, never by the config file"
# (a) a store identity but no list is an existing consultant: abort, with or without a config; print-run and real spawn
seed_session stored
rc=0; runp c1 --label stored --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "persisted session, no list, no config yet (print-run): existing, exit 2, the add command named, only path asked, no config rendered" \
  bash -c '[ "$1" -eq 2 ] && grep -qF -- "add --agent=stored-aqua-consultant aqua-rs-sdk" "$2" && [ "$(cut -d"|" -f1 "$3" | tr "\n" " ")" = "path " ] && [ ! -e "$4" ]' _ "$rc" "$SB/out/c1.err" "$STUB_LOG" "$(cfg_of stored)"
rm -f "$RUNNING"
rc=0; runs c2 --label stored --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "persisted session, no list (real spawn): exit 2, no podman call, no add, no view" bash -c '[ "$1" -eq 2 ] && [ ! -e "$2" ] && [ "$(cut -d"|" -f1 "$3" | tr "\n" " ")" = "path " ]' _ "$rc" "$SIDE_EFFECTS" "$STUB_LOG"
# (b) agent.pem alone is an identity; an EMPTY agent.pem is not
mkdir -p "$TEST_DIR/pemonly-aqua-consultant-persist/store"; printf 'key\n' > "$TEST_DIR/pemonly-aqua-consultant-persist/store/agent.pem"
rc=0; runp c3 --label pemonly --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "agent.pem in the store (no session yet): existing, no list, exit 2" bash -c '[ "$1" -eq 2 ] && [ ! -s "$2" ] && grep -q "existing consultant" "$3"' _ "$rc" "$SB/out/c3.argv" "$SB/out/c3.err"
mkdir -p "$TEST_DIR/emptypem-aqua-consultant-persist/store"; : > "$TEST_DIR/emptypem-aqua-consultant-persist/store/agent.pem"
rc=0; runp c4 --label emptypem --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "an empty agent.pem is no identity: new, placeholder view mount" bash -c '[ "$1" -eq 0 ] && grep -q "^<view dir of emptypem-aqua-consultant, known only after" "$2"' _ "$rc" "$SB/out/c4.argv"
# (c) a container but no store identity and no config: existing (real spawn only asks podman), no list => abort
touch "$RUNNING"
rc=0; runs c5 --replace --label ctronly --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "container exists, no identity yet, no list: existing, exit 2, the add command named" bash -c '[ "$1" -eq 2 ] && grep -qF -- "add --agent=ctronly-aqua-consultant aqua-rs-sdk" "$2"' _ "$rc" "$SB/out/c5.err"
check "container exists: podman was only asked 'container exists', the container is still running, no config rendered" \
  bash -c 'grep -q "^podman container exists aqua-agent-ctronly-aqua-consultant-1" "$1" && ! grep -qE "^podman (rm|run|start)" "$1" && [ -e "$2" ] && [ ! -e "$3" ]' _ "$SIDE_EFFECTS" "$RUNNING" "$(cfg_of ctronly)"
rm -f "$RUNNING"
# (d) neither: a config file alone (left by a preview or a failed first spawn) does NOT make it existing
rc=0; runp c6 --label cfgonly --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "preview of a new consultant renders its config" test -f "$(cfg_of cfgonly)"
rm -f "$RUNNING"
rc=0; runs c7 --label cfgonly --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "config file but no identity and no container: still NEW (path, add, path, sync, view mount)" bash -c '[ "$1" -eq 0 ] && [ "$(cut -d"|" -f1 "$2" | tr "\n" " ")" = "path add path sync " ] && grep -qF -- "-v $3/views/cfgonly-aqua-consultant:/refs:ro" "$4"' _ "$rc" "$STUB_LOG" "$STUB_ROOT" "$SIDE_EFFECTS"
rm -f "$RUNNING"
# (e) podman answering something other than exists / does not exist: cannot classify, abort
printf '#!/bin/sh\necho "podman $*" >> "%s"\n[ "$1 $2" = "container exists" ] && exit 125\nexit 0\n' "$SIDE_EFFECTS" > "$SB/podman-broken"; chmod +x "$SB/podman-broken"
mkdir -p "$SB/brokenbin"; cp "$SB/podman-broken" "$SB/brokenbin/podman"
rc=0; RUN_ENV=( "PATH=$SB/brokenbin:$BASE_PATH" ); runs c8 --label podbroke --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "podman container exists failing (exit 125): abort with exit 1 before any change, no config" bash -c '[ "$1" -eq 1 ] && grep -q "cannot tell a new consultant from an existing one" "$2" && [ ! -e "$3" ]' _ "$rc" "$SB/out/c8.err" "$(cfg_of podbroke)"

echo "== AGENT_REFS_BIN default: aqua-agent-refs on PATH, else ~/.local/bin/aqua-agent-refs when executable"
dflt() {   # dflt <name> <PATH> : print-run of alpha with AGENT_REFS_BIN unset
  local name="$1" pth="$2" rc=0; reset_logs
  env -i HOME="$FAKE_HOME" PATH="$pth" CONSULTANT_TEST_DIR="$TEST_DIR" \
    AQUA_CLAUDE_TOKEN_FILE="$TOKEN_FILE" AGENT_REFS_ROOT="$STUB_ROOT" STUB_LOG="$STUB_LOG" \
    bash "$SPAWN" --print-run --label alpha --target "$TARGET" --persona Thalia --name Tester > "$SB/out/$name.out" 2> "$SB/out/$name.err" || rc=$?
  echo "$rc" > "$SB/out/$name.rc"; return 0
}
mkdir -p "$FAKE_HOME/.local/bin"
cp "$STUB" "$FAKE_HOME/.local/bin/aqua-agent-refs"
make_list "$ALPHA"
dflt d1 "$BASE_PATH"
check "not on PATH, ~/.local/bin/aqua-agent-refs executable: used (path asked, view mounted)" bash -c '[ "$(cat "$1")" -eq 0 ] && grep -qx "path|--agent='"$ALPHA"'|" "$2" && grep -qF -- "'"$STUB_ROOT"'/views/'"$ALPHA"':/refs:ro" "$3"' _ "$SB/out/d1.rc" "$STUB_LOG" "$SB/out/d1.out"
chmod -x "$FAKE_HOME/.local/bin/aqua-agent-refs"
drop_list "$ALPHA"
dflt d2 "$BASE_PATH"
check "the .local/bin/aqua-agent-refs of HOME NOT executable: ignored (tool not installed: exit 2, nothing run)" bash -c '[ "$(cat "$1")" -eq 2 ] && [ ! -s "$2" ] && grep -q "^!! aborting: .aqua-agent-refs. not found" "$3"' _ "$SB/out/d2.rc" "$STUB_LOG" "$SB/out/d2.err"
chmod +x "$FAKE_HOME/.local/bin/aqua-agent-refs"
rm -f "$FAKE_HOME/.local/bin/aqua-agent-refs"; mkdir "$FAKE_HOME/.local/bin/aqua-agent-refs"
dflt d3 "$BASE_PATH"
check "the .local/bin/aqua-agent-refs of HOME is a directory: ignored (exit 2)" bash -c '[ "$(cat "$1")" -eq 2 ] && grep -q "^!! aborting: .aqua-agent-refs. not found" "$2"' _ "$SB/out/d3.rc" "$SB/out/d3.err"
rmdir "$FAKE_HOME/.local/bin/aqua-agent-refs"
dflt d4 "$BASE_PATH"
check "neither on PATH nor in ~/.local/bin: exit 2 with the install hint, no fallback" bash -c '[ "$(cat "$1")" -eq 2 ] && grep -q "install it:" "$2" && grep -q "no fallback" "$2"' _ "$SB/out/d4.rc" "$SB/out/d4.err"
# PATH wins over ~/.local/bin: the ~/.local/bin one would fail every call
printf '#!/bin/sh\nexit 99\n' > "$FAKE_HOME/.local/bin/aqua-agent-refs"; chmod +x "$FAKE_HOME/.local/bin/aqua-agent-refs"
make_list "$ALPHA"
dflt d5 "$SB/pathbin:$BASE_PATH"
check "on PATH and in ~/.local/bin: the PATH one wins" bash -c '[ "$(cat "$1")" -eq 0 ] && grep -qx "path|--agent='"$ALPHA"'|" "$2"' _ "$SB/out/d5.rc" "$STUB_LOG"
rm -rf "$FAKE_HOME/.local"; drop_list "$ALPHA"

echo "== fail closed on the answer of path (no list file on disk)"
for code in 1 2 3 4 64 127; do
  rc=0; RUN_ENV=( "STUB_PATH_RC=$code" ); runp f1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
  check "path exit $code (existing consultant): abort with exit 1, nothing changed, no argv" \
    bash -c '[ "$1" -eq 1 ] && grep -q "path --agent=alpha-aqua-consultant. failed (exit '"$code"'" "$2" && grep -q "nothing was changed" "$2" && [ ! -s "$3" ]' _ "$rc" "$SB/out/f1.err" "$SB/out/f1.argv"
  check "path exit $code: no refs mount of any kind printed, no add/sync/check" bash -c '! grep -q ":/refs" "$1" && [ "$(cut -d"|" -f1 "$2" | tr "\n" " ")" = "path " ]' _ "$SB/out/f1.out" "$STUB_LOG"
done
rc=0; RUN_ENV=( STUB_PATH_RC=2 ); runp f1b --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "path exit 2 names the usage/settings meaning and the old-tool hint" grep -q "exit 2: usage, invalid key or settings error; a tool older than the exit-5" "$SB/out/f1b.err"
rc=0; RUN_ENV=( STUB_PATH_RC=1 ); runp f2 --label brandnew2 --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "path exit 1 (new consultant): abort, no config rendered" bash -c '[ "$1" -eq 1 ] && [ ! -e "$2" ]' _ "$rc" "$(cfg_of brandnew2)"

echo "== a list file on disk beats any 'no list' answer: path exit 2 or 5 with the list present aborts, no podman at all"
for how in file symlink; do
  drop_list "$ALPHA"; mkdir -p "$STUB_ROOT/agents" "$STUB_ROOT/views/$ALPHA"
  if [ "$how" = file ]; then echo '{}' > "$STUB_ROOT/agents/$ALPHA.json"; else ln -s "$SB/no-such-target" "$STUB_ROOT/agents/$ALPHA.json"; fi
  cfg_before="$(sha_of "$(cfg_of alpha)")"
  for code in 2 5; do
    rc=0; RUN_ENV=( "STUB_PATH_RC=$code" ); runp g1 --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?; RUN_ENV=()
    check "list $how present, path exit $code, print-run: exit 1, nothing printed, config byte-identical" \
      bash -c '[ "$1" -eq 1 ] && [ ! -s "$2" ] && [ "$(sha256sum < "$3" | cut -d" " -f1)" = "$4" ]' _ "$rc" "$SB/out/g1.argv" "$(cfg_of alpha)" "$cfg_before"
    check "list $how present, path exit $code: no podman call, no git, only path asked" bash -c '[ ! -e "$1" ] && [ ! -e "$2" ] && [ "$(cut -d"|" -f1 "$3" | tr "\n" " ")" = "path " ]' _ "$SIDE_EFFECTS" "$GIT_LOG" "$STUB_LOG"
    touch "$RUNNING"
    rc=0; RUN_ENV=( "STUB_PATH_RC=$code" ); runs g2 --replace --label alpha --target "$TARGET" --persona Renamed --name Tester || rc=$?; RUN_ENV=()
    check "list $how present, path exit $code, REAL spawn: exit 1, no podman call at all (the old container is untouched)" \
      bash -c '[ "$1" -eq 1 ] && [ ! -e "$2" ] && [ -e "$3" ]' _ "$rc" "$SIDE_EFFECTS" "$RUNNING"
    check "list $how present, path exit $code, REAL spawn: config byte-identical" bash -c '[ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ]' _ "$(cfg_of alpha)" "$cfg_before"
    rm -f "$RUNNING"
  done
  rm -f "$STUB_ROOT/agents/$ALPHA.json"
done
check "message: the tool says no list but the list file exists" grep -q "says there is no list (exit 5), but .* exists" "$SB/out/g2.err"
drop_list "$ALPHA"

echo "== a real spawn aborted by 'path' leaves the Owner allow-list byte-identical"
TARGET2='@did-key-zsecondpeer:matrix.inblock.io'
cp "$AL" "$SB/allowlist.before"; al_before="$(sha_of "$AL")"; al_files_before="$(find "$SB" -maxdepth 1 -name 'allowlist*' | sort | tr '\n' ' ')"
rc=0; RUN_ENV=( STUB_PATH_RC=1 ); runs w1 --label ownerchk --target "$TARGET2" --persona Thalia --name Newbie || rc=$?; RUN_ENV=()
check "path exit 1 on a real spawn: exit 1" bash -c '[ "$1" -eq 1 ]' _ "$rc"
check "the Owner allow-list is byte-identical, and no backup or temp file appeared beside it" \
  bash -c '[ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ] && [ "$(find "$3" -maxdepth 1 -name "allowlist*" | sort | tr "\n" " ")" = "$4" ]' _ "$AL" "$al_before" "$SB" "$al_files_before"
check "no podman call, no config, no persist dir" bash -c '[ ! -e "$1" ] && [ ! -e "$2" ] && [ ! -e "$3" ]' _ "$SIDE_EFFECTS" "$(cfg_of ownerchk)" "$TEST_DIR/ownerchk-aqua-consultant-persist"
# the control: the same spawn with a healthy tool DOES add the Owner, so the byte-identity above is meaningful
rm -f "$RUNNING"
rc=0; runs w2 --label ownerchk --target "$TARGET2" --persona Thalia --name Newbie || rc=$?
check "control: with a working tool the Owner step writes the allow-list" bash -c '[ "$1" -eq 0 ] && grep -q "did-key-zsecondpeer" "$2" && [ "$(sha256sum < "$2" | cut -d" " -f1)" != "$3" ]' _ "$rc" "$AL" "$al_before"
cp "$SB/allowlist.before" "$AL"; rm -f "$SB"/allowlist.toml.* "$RUNNING"

echo "== the view path is verified exactly: a wrong, relative, multi-line or CR-terminated answer is refused"
make_list "$ALPHA"; VIEW_A="$STUB_ROOT/views/$ALPHA"
OTHER_VIEW="$STUB_ROOT/views/other-aqua-consultant"; mkdir -p "$OTHER_VIEW"
for out in "" "relative/view" "views/x" "/ok/but:colon" "/some/other/dir" "$OTHER_VIEW" "$VIEW_A/../other-aqua-consultant" "$VIEW_A:ro" \
           "$VIEW_A"$'\r' "$VIEW_A"$'\n/b' $'/a\n/b' "$VIEW_A"$'\n' "$VIEW_A"$'\n\n' " $VIEW_A"; do
  rc=0; RUN_ENV=( STUB_PATH_RC=0 "STUB_PATH_OUT=$out" ); runp f3 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
  check "path printed $(printf '%q' "$out"): refused (exit 1), nothing launched" bash -c '[ "$1" -eq 1 ] && grep -q "refusing to mount it" "$2" && [ ! -s "$3" ] && [ ! -e "$4" ]' _ "$rc" "$SB/out/f3.err" "$SB/out/f3.argv" "$SIDE_EFFECTS"
done
# accepted spellings of the same dir: the verified path (not the tool's string) is what gets mounted
rc=0; RUN_ENV=( STUB_PATH_RC=0 "STUB_PATH_OUT=$VIEW_A/" ); runp f4 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?; RUN_ENV=()
check "the same dir with a trailing slash is accepted, mounted as the canonical view path" bash -c '[ "$1" -eq 0 ] && [ "$(awk "prev==\"-v\" && /:\/refs/ {print} {prev=\$0}" "$2")" = "$3:/refs:ro" ]' _ "$rc" "$SB/out/f4.argv" "$VIEW_A"
# the view dir itself: missing, a regular file, a symlink
for kind in missing file symlink; do
  rm -rf "$VIEW_A"
  case "$kind" in file) : > "$VIEW_A" ;; symlink) ln -s /etc "$VIEW_A" ;; esac
  rc=0; runp v1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
  check "view dir $kind (print-run): refused after the check, exit 1, no argv" bash -c '[ "$1" -eq 1 ] && grep -q "is not a plain directory" "$2" && [ ! -s "$3" ]' _ "$rc" "$SB/out/v1.err" "$SB/out/v1.argv"
  touch "$RUNNING"
  rc=0; runs v2 --replace --keep-config --label alpha || rc=$?
  check "view dir $kind (real spawn): refused, the old container is still there, no podman rm/run" bash -c '[ "$1" -eq 1 ] && ! grep -qE "^podman (rm|run)" "$2" 2>/dev/null && [ -e "$3" ]' _ "$rc" "$SIDE_EFFECTS" "$RUNNING"
done
rm -rf "$VIEW_A"; make_list "$ALPHA"; rm -f "$RUNNING"
# a view swapped for a symlink by the sync itself
touch "$RUNNING"
rc=0; RUN_ENV=( STUB_SYNC_SWAP=1 ); runs v3 --replace --keep-config --label alpha || rc=$?; RUN_ENV=()
check "view swapped by the sync: refused after the sync, before any container change" bash -c '[ "$1" -eq 1 ] && ! grep -qE "^podman (rm|run)" "$2" 2>/dev/null && [ -e "$3" ] && grep -q "after the refs sync / check" "$4"' _ "$rc" "$SIDE_EFFECTS" "$RUNNING" "$SB/out/v3.err"
rm -rf "$VIEW_A"; make_list "$ALPHA"
# a view swapped between the sync and the launch (the podman shim does it on rm -f): the last check refuses podman run
touch "$RUNNING"
rc=0; RUN_ENV=( "SHIM_SWAP_VIEW=$VIEW_A" ); runs v4 --replace --keep-config --label alpha || rc=$?; RUN_ENV=()
check "view swapped between the sync and podman run: the last check refuses, podman run never happens" bash -c '[ "$1" -eq 1 ] && grep -q "^podman rm" "$2" && ! grep -q "^podman run" "$2" && grep -q "right before podman run" "$3"' _ "$rc" "$SIDE_EFFECTS" "$SB/out/v4.err"
rm -rf "$VIEW_A"; drop_list "$ALPHA"; rm -f "$RUNNING"

echo "== host.extra_refs / host.refs_follow are ignored when the agent has a list"
make_list "$ALPHA"
# the retired keys name repos that exist nowhere on the host: they are never looked at
put_host alpha '{"extra_refs": ["no-such-extra", "aqua-spec"], "refs_follow": ["no-such-follow"]}'
rc=0; runp h1 --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "exit 0 (no follow mirror or extra mirror is looked at)" [ "$rc" -eq 0 ]
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
put_host alpha '{"extra_refs": [], "refs_follow": []}'
rc=0; runp h3b --label alpha --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "empty host.extra_refs / host.refs_follow lists: no 'ignored' line" bash -c '[ "$1" -eq 0 ] && ! grep -q "are ignored" "$2"' _ "$rc" "$SB/out/h3b.out"
put_host alpha ABSENT
drop_list "$ALPHA"
# a NEW consultant (no identity): the same note, a placeholder view mount, still no extra mount
put_host hostnew '{"extra_refs": ["x-extra"], "refs_follow": ["x-follow"]}'
rc=0; runp h4 --label hostnew --target "$TARGET" --persona Thalia --name Tester || rc=$?
check "NEW consultant with host.extra_refs / host.refs_follow: exit 0, the ignored note once" bash -c '[ "$1" -eq 0 ] && [ "$(grep -c "host.extra_refs / host.refs_follow in .* are ignored" "$2")" -eq 1 ]' _ "$rc" "$SB/out/h4.out"
check "NEW consultant: one /refs mount only (the placeholder view), no x-extra / x-follow mount" bash -c '[ "$(awk "prev==\"-v\" && /:\/refs/ {n++} {prev=\$0} END{print n+0}" "$1")" -eq 1 ] && ! grep -q "x-extra\|x-follow\|:/refs/" "$1"' _ "$SB/out/h4.argv"

echo "== print modes that exit early never run the tool"
reset_logs
rc=0; env -i HOME="$FAKE_HOME" PATH="$BASE_PATH" CONSULTANT_TEST_DIR="$TEST_DIR" AGENT_REFS_BIN="$STUB" \
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
