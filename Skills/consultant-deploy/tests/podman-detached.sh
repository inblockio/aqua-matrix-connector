#!/usr/bin/env bash
#
# podman-detached.sh tests: the wrapper every fleet launch goes through so a container outlives
# its caller (2026-10-05: the night-roll oneshot unit SIGTERMed all 23 consultants on exit).
#
# Default (hermetic, shims only, nothing live touched):
#   - bash -n on the wrapper and on its three callers
#   - the scope probe, then `systemd-run --user --scope --quiet --collect --description=... --
#     podman <args>`; podman's stdout and exit status pass through unchanged
#   - fails closed: a failing scope probe -> exit 1, a clear refusal, podman never called
#   - no systemd-run on PATH at all -> plain podman; no args -> usage, exit 2
#   - restore-agent-fleet.sh starts every exited container through the wrapper; --dry-run
#     starts nothing
#
# --live (real podman + the user's systemd manager; needs a LOCAL image, never pulls):
#   reproduces the incident. A transient Type=oneshot unit WITHOUT RemainAfterExit launches one
#   container bare and one through the wrapper, then exits. The bare container's conmon must be
#   killed with the unit (proves the test detects the bug), the wrapped one must keep running
#   with its conmon outside the unit. Both containers and units are removed afterwards.
#   Image: PODMAN_DETACHED_TEST_IMAGE (default docker.io/library/ubuntu:rolling).
#
# Usage:  bash Skills/consultant-deploy/tests/podman-detached.sh [--live]
# Exit:   0 when every assertion passes, 1 otherwise (each failure is printed).
#
set -euo pipefail

SKILL_DIR="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
REPO="$(cd "$SKILL_DIR/../.." && pwd)"
WRAP="$SKILL_DIR/podman-detached.sh"
RESTORE="$SKILL_DIR/restore-agent-fleet.sh"
LIVE=0; [ "${1:-}" = --live ] && LIVE=1

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ok   $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL $1" >&2; }
check() { local d="$1"; shift; if "$@"; then ok "$d"; else bad "$d"; fi; }
not() { ! "$@"; }

SB="$(mktemp -d "${TMPDIR:-/tmp}/podman-detached-test.XXXXXX")"
trap 'rm -rf "$SB"' EXIT
LOG="$SB/calls.log"
BIN="$SB/bin"; NOSR="$SB/nosr"
mkdir -p "$BIN" "$NOSR"

# podman shim: logs its argv; `ps` lists $SHIM_PS (one name per line); anything else prints a
# fake id and exits $SHIM_RC.
cat > "$BIN/podman" <<'SHIM'
#!/bin/sh
echo "podman $*" >> "$LOG"
case "$1" in
  ps) [ -z "${SHIM_PS:-}" ] || printf '%s\n' $SHIM_PS; exit 0 ;;
esac
echo "fake-container-id"
exit "${SHIM_RC:-0}"
SHIM
# systemd-run shim: logs its argv; SHIM_SR=fail makes it refuse like an unreachable manager,
# otherwise it drops its own options and runs the command after `--`.
cat > "$BIN/systemd-run" <<'SHIM'
#!/bin/sh
echo "systemd-run $*" >> "$LOG"
if [ "${SHIM_SR:-ok}" = fail ]; then echo "Failed to connect to bus: No medium found" >&2; exit 1; fi
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
shift
exec "$@"
SHIM
cp "$BIN/podman" "$NOSR/podman"
chmod +x "$BIN/podman" "$BIN/systemd-run" "$NOSR/podman"

# w <path> [VAR=value...] -- <wrapper args...>: runs the wrapper with PATH=<path> (+ /usr/bin:/bin
# unless the path is $NOSR), leaving stdout/stderr/rc in $SB/w.{out,err,rc}.
w() {
  local path="$1"; shift; local envs=() rc=0
  while [ "$1" != -- ]; do envs+=("$1"); shift; done; shift
  [ "$path" = "$NOSR" ] || path="$path:/usr/bin:/bin"
  rm -f "$LOG"
  env -i HOME="$SB" LOG="$LOG" PATH="$path" ${envs[@]+"${envs[@]}"} /bin/bash "$WRAP" "$@" \
    > "$SB/w.out" 2> "$SB/w.err" || rc=$?
  echo "$rc" > "$SB/w.rc"
}
rc_is() { [ "$(cat "$SB/w.rc")" = "$1" ]; }
log_is() { printf '%s\n' "$@" | cmp -s - "$LOG"; }

echo "== syntax"
for f in "$WRAP" "$RESTORE" "$SKILL_DIR/spawn-consultant.sh" "$REPO/scripts/canary-rooms.sh"; do
  check "bash -n $(basename "$f")" bash -n "$f"
done

echo "== scope launch (shims)"
w "$BIN" PODMAN_DETACHED_LABEL=my-agent -- run -d --name my-agent img:tag
check "exit 0, podman's stdout passed through" bash -c '[ "$(cat "$1/w.rc")" = 0 ] && [ "$(cat "$1/w.out")" = fake-container-id ]' _ "$SB"
check "probe, then podman inside a --user scope with the label, args verbatim" log_is \
  "systemd-run --user --scope --quiet --collect -- true" \
  "systemd-run --user --scope --quiet --collect --description=podman run my-agent (detached from the caller) -- podman run -d --name my-agent img:tag" \
  "podman run -d --name my-agent img:tag"
w "$BIN" SHIM_RC=7 -- start x
check "podman's exit status passes through (7)" rc_is 7
check "unlabelled description still names the verb" grep -qxF "systemd-run --user --scope --quiet --collect --description=podman start (detached from the caller) -- podman start x" "$LOG"

echo "== fails closed"
w "$BIN" SHIM_SR=fail -- run -d --name x img
check "unreachable manager: exit 1" rc_is 1
check "unreachable manager: podman never called" bash -c '! grep -q "^podman" "$1"' _ "$LOG"
check "unreachable manager: says why and refuses" bash -c 'grep -qF "cannot create a systemd --user scope: Failed to connect to bus" "$1" && grep -qF "refusing to run '"'"'podman run'"'"' attached to the caller" "$1"' _ "$SB/w.err"

echo "== no systemd-run on the host"
w "$NOSR" -- start x
check "plain podman, exit 0" bash -c '[ "$(cat "$1/w.rc")" = 0 ] && [ "$(cat "$2")" = "podman start x" ]' _ "$SB" "$LOG"
w "$BIN" --
check "no args: usage, exit 2" bash -c '[ "$(cat "$1/w.rc")" = 2 ] && grep -q "^usage:" "$1/w.err"' _ "$SB"

echo "== restore-agent-fleet.sh"
rrun() { rm -f "$LOG"; env -i HOME="$SB" LOG="$LOG" PATH="$BIN:/usr/bin:/bin" SHIM_PS="aqua-agent-a aqua-agent-b" \
  /bin/bash "$RESTORE" "$@" > "$SB/r.out" 2> "$SB/r.err"; }
check "starts every exited container, exit 0" rrun
check "each start ran through the wrapper's scope" bash -c '
  grep -qxF "systemd-run --user --scope --quiet --collect --description=podman start aqua-agent-a (detached from the caller) -- podman start aqua-agent-a" "$1" &&
  grep -qxF "systemd-run --user --scope --quiet --collect --description=podman start aqua-agent-b (detached from the caller) -- podman start aqua-agent-b" "$1" &&
  [ "$(grep -c "^podman start" "$1")" = 2 ]' _ "$LOG"
check "reports both as started" bash -c 'grep -qx "  started aqua-agent-a" "$1" && grep -qx "  started aqua-agent-b" "$1"' _ "$SB/r.out"
check "--dry-run exits 0" rrun --dry-run
check "--dry-run: no systemd-run, no podman start" bash -c '! grep -qE "^(systemd-run|podman start)" "$1"' _ "$LOG"

if [ "$LIVE" -eq 1 ]; then
  echo "== live: a oneshot unit exits after launching (real podman + systemd --user)"
  IMG="${PODMAN_DETACHED_TEST_IMAGE:-docker.io/library/ubuntu:rolling}"
  if ! podman image exists "$IMG"; then
    bad "live: image $IMG not present locally (set PODMAN_DETACHED_TEST_IMAGE; this test never pulls)"
  else
    T="pdt-$$"
    cleanup_live() {
      podman rm -f -t 0 "$T-bare" "$T-wrapped" >/dev/null 2>&1 || true
      systemctl --user reset-failed "$T-bare.service" "$T-wrapped.service" >/dev/null 2>&1 || true
    }
    trap 'cleanup_live; rm -rf "$SB"' EXIT
    # TimeoutStopSec keeps the bare case fast: conmon ignores the SIGTERM, systemd SIGKILLs it.
    systemd-run --user --wait --quiet --unit="$T-bare" -p Type=oneshot -p TimeoutStopSec=5 \
      podman run -d --name "$T-bare" "$IMG" sleep 300 >/dev/null 2>&1 || true
    systemd-run --user --wait --quiet --unit="$T-wrapped" -p Type=oneshot -p TimeoutStopSec=5 \
      "$WRAP" run -d --name "$T-wrapped" "$IMG" sleep 300 >/dev/null 2>&1 || true
    conmon_alive() { local p; p="$(podman inspect "$1" --format '{{.State.ConmonPid}}' 2>/dev/null)"; [ "${p:-0}" -gt 0 ] && kill -0 "$p" 2>/dev/null; }
    conmon_cgroup() { cat "/proc/$(podman inspect "$1" --format '{{.State.ConmonPid}}')/cgroup" 2>/dev/null; }
    check "live control: the bare launch's conmon died with the unit (the 2026-10-05 failure reproduces)" not conmon_alive "$T-bare"
    # podman keeps reporting "running" after conmon is killed, so the conmon checks below decide.
    check "live: the wrapped container is still running" bash -c '[ "$(podman inspect "$1" --format "{{.State.Status}}")" = running ]' _ "$T-wrapped"
    check "live: its conmon is alive" conmon_alive "$T-wrapped"
    outside_unit() { local cg; cg="$(conmon_cgroup "$1")"; [ -n "$cg" ] && ! grep -qF "/$1.service" <<<"$cg"; }
    check "live: its conmon lives outside the launching unit" outside_unit "$T-wrapped"
    cleanup_live
  fi
fi

echo
echo "podman-detached: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
