#!/usr/bin/env bash
#
# spawn-consultant-fresh.sh, tests for the --fresh persist wipe in spawn-consultant.sh.
#
# The persist dirs are bind-mounted with `:U`, so podman chowns them to the container's mapped
# UID (101000 on the fleet host, not the operator). A plain `rm -rf` from the host then fails with
# "Permission denied", and under `set -e` the spawn died AFTER `--replace` had removed the
# container (2026-10-16, molrey re-point). The wipe therefore runs inside the rootless user
# namespace (`podman unshare`), and the namespace is probed before any container is removed.
#
# Default (hermetic, shims only, nothing live touched), on the script's own wipe_persist and
# preflight_fresh (extracted verbatim):
#   - bash -n on spawn-consultant.sh
#   - wipe_persist runs exactly `podman unshare rm -rf -- <store> <memory>`, then recreates both
#   - an unexpected persist path is refused (exit 2) before podman is called
#   - a failing unshare fails closed (exit 1, clear message), the dirs are not recreated
#   - preflight_fresh probes `podman unshare true` and fails closed when it fails
#   - the lifecycle order: preflight before the --replace removal, wipe after it
#
# --live (real `podman unshare`, rootless, no image, no container): reproduces the incident.
#   Files chowned to a namespace UID cannot be removed by a plain host `rm -rf` (proves the
#   fixture has the bug), wipe_persist removes them and leaves empty dirs owned by the operator.
#
# Usage:  bash Skills/consultant-deploy/tests/spawn-consultant-fresh.sh [--live]
# Exit:   0 when every assertion passes, 1 otherwise (each failure is printed).
#
set -euo pipefail

SKILL_DIR="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
SPAWN="$SKILL_DIR/spawn-consultant.sh"
LIVE=0
[ "${1:-}" = "--live" ] && LIVE=1

SB="$(mktemp -d)"
trap 'podman unshare rm -rf -- "$SB" 2>/dev/null || rm -rf -- "$SB"' EXIT
PASS=0; FAIL=0
check() {         # check <description> <command...>
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then PASS=$((PASS+1)); echo "ok   $desc"
  else FAIL=$((FAIL+1)); echo "FAIL $desc"; fi
}

WIPE_FN="$(sed -n '/^wipe_persist() {$/,/^}$/p' "$SPAWN")"
PREFLIGHT_FN="$(sed -n '/^preflight_fresh() {$/,/^}$/p' "$SPAWN")"

echo "== static"
check "bash -n spawn-consultant.sh" bash -n "$SPAWN"
check "wipe_persist extracted from the script" grep -q 'podman unshare rm -rf --' <(printf '%s\n' "$WIPE_FN")
check "preflight_fresh extracted from the script" grep -q 'podman unshare true' <(printf '%s\n' "$PREFLIGHT_FN")
check "no plain host rm -rf of the persist dirs left" bash -c '! grep -nE "^[[:space:]]*rm -rf -- \"\\\$STORE\"" "$1"' _ "$SPAWN"
# Lifecycle order: preflight_fresh call < `podman rm -f "$NAME"` < wipe_persist call.
order_ok() {
  local pre rm wipe
  pre="$(grep -n '^[[:space:]]*\[ "\$FRESH" -eq 1 \] && preflight_fresh' "$SPAWN" | head -1 | cut -d: -f1)"
  rm="$(grep -n 'podman rm -f "\$NAME"' "$SPAWN" | head -1 | cut -d: -f1)"
  wipe="$(grep -n '^[[:space:]]*\[ "\$FRESH" -eq 1 \] && wipe_persist' "$SPAWN" | head -1 | cut -d: -f1)"
  [ -n "$pre" ] && [ -n "$rm" ] && [ -n "$wipe" ] && [ "$pre" -lt "$rm" ] && [ "$rm" -lt "$wipe" ]
}
check "order: preflight before the --replace removal, wipe after it" order_ok

# Shim podman: records argv to $CALLS; `unshare` runs the rest unless SHIM_FAIL=1.
SHIM="$SB/bin"; mkdir -p "$SHIM"; CALLS="$SB/calls"
cat > "$SHIM/podman" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CALLS"
[ "${SHIM_FAIL:-0}" = 1 ] && { echo "shim: unshare failed" >&2; exit 125; }
[ "$1" = unshare ] && { shift; exec "$@"; }
exit 99
EOF
chmod +x "$SHIM/podman"

run_fn() {        # run_fn <fn-src> <fn> <test_dir> <persist> [env...]: sandboxed subshell
  local src="$1" fn="$2" td="$3" p="$4"; shift 4
  ( set -euo pipefail; eval "$src"
    TEST_DIR="$td"; PERSIST="$p"; STORE="$p/store"; MEM="$p/memory"
    export PATH="$SHIM:$PATH" CALLS "$@"; "$fn" )
}

echo "== hermetic (shim podman)"
TD="$SB/td"; P="$TD/alpha-aqua-consultant-persist"
mkdir -p "$P/store" "$P/memory"; echo pem > "$P/store/agent.pem"; echo m > "$P/memory/.whats_new_seen"
: > "$CALLS"; rc=0; run_fn "$WIPE_FN" wipe_persist "$TD" "$P" > "$SB/w1.out" 2>&1 || rc=$?
check "wipe: exit 0" [ "$rc" -eq 0 ]
check "wipe: exactly one podman call, the unshare rm" \
  bash -c '[ "$(cat "$1")" = "unshare rm -rf -- $2/store $2/memory" ]' _ "$CALLS" "$P"
check "wipe: identity gone, both dirs recreated empty" \
  bash -c '[ -d "$1/store" ] && [ -d "$1/memory" ] && [ -z "$(find "$1/store" "$1/memory" -mindepth 1)" ]' _ "$P"

: > "$CALLS"; rc=0; run_fn "$WIPE_FN" wipe_persist "$TD" "$SB/elsewhere" > "$SB/w2.out" 2>&1 || rc=$?
check "wipe: unexpected persist path refused with exit 2" [ "$rc" -eq 2 ]
check "wipe: refusal names the path" grep -q "refusing --fresh: unexpected persist path" "$SB/w2.out"
check "wipe: podman never called for a refused path" [ ! -s "$CALLS" ]

mkdir -p "$P/store"; echo pem > "$P/store/agent.pem"; rm -rf -- "$P/memory"
: > "$CALLS"; rc=0; run_fn "$WIPE_FN" wipe_persist "$TD" "$P" SHIM_FAIL=1 > "$SB/w3.out" 2>&1 || rc=$?
check "wipe: failing unshare fails closed (exit 1)" [ "$rc" -eq 1 ]
check "wipe: failure message says the wipe failed" grep -q "!! --fresh: could not wipe" "$SB/w3.out"
check "wipe: memory dir not recreated after a failure" [ ! -e "$P/memory" ]

: > "$CALLS"; rc=0; run_fn "$PREFLIGHT_FN" preflight_fresh "$TD" "$P" > "$SB/p1.out" 2>&1 || rc=$?
check "preflight: exit 0 when the namespace works" [ "$rc" -eq 0 ]
check "preflight: probes podman unshare true" grep -qx "unshare true" "$CALLS"
rc=0; run_fn "$PREFLIGHT_FN" preflight_fresh "$TD" "$P" SHIM_FAIL=1 > "$SB/p2.out" 2>&1 || rc=$?
check "preflight: fails closed (exit 1) when unshare fails" [ "$rc" -eq 1 ]
check "preflight: says nothing was changed" grep -q "nothing changed" "$SB/p2.out"

if [ "$LIVE" -eq 1 ]; then
  echo "== live (real podman unshare)"
  LTD="$SB/live"; LP="$LTD/beta-aqua-consultant-persist"
  mkdir -p "$LP/store" "$LP/memory"; echo pem > "$LP/store/agent.pem"; echo m > "$LP/memory/x"
  podman unshare chown -R 1000:1000 "$LP/store" "$LP/memory"
  check "live: fixture is owned by a namespace UID, not the operator" \
    bash -c '[ "$(stat -c %u "$1/store/agent.pem")" != "$(id -u)" ]' _ "$LP"
  check "live: negative control, plain rm -rf is denied (the bug)" bash -c '! rm -rf -- "$1/store" "$1/memory"' _ "$LP"
  rc=0; ( set -euo pipefail; eval "$WIPE_FN"; TEST_DIR="$LTD"; PERSIST="$LP"; STORE="$LP/store"; MEM="$LP/memory"
    wipe_persist ) > "$SB/l1.out" 2>&1 || rc=$?
  check "live: wipe_persist exit 0" [ "$rc" -eq 0 ]
  check "live: dirs empty and owned by the operator" \
    bash -c '[ -z "$(find "$1/store" "$1/memory" -mindepth 1)" ] && [ "$(stat -c %u "$1/store")" = "$(id -u)" ]' _ "$LP"
fi

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
