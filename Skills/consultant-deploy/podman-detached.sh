#!/usr/bin/env bash
#
# podman-detached.sh, run a podman command so the containers it starts outlive the caller.
#
# Every fleet launch (spawn-consultant.sh, restore-agent-fleet.sh, canary-rooms.sh) goes
# through this wrapper. Use it for any new script that runs `podman run -d`, `podman start`
# or `podman restart` on an agent container.
#
# Why: conmon, the per-container supervisor that also implements --restart, normally gets its
# own libpod-conmon-<id>.scope. Inside a systemd service podman deliberately skips that
# ($INVOCATION_ID is set) and leaves conmon in the service's cgroup. When that service ends (a
# Type=oneshot timer job without RemainAfterExit=yes, or any `systemctl stop`), systemd
# SIGTERMs everything left in its cgroup: the relays exit 0, --restart on-failure never fires,
# and the containers stay down. On 2026-10-05 04:11 the one-off night-roll unit verified 23/23
# consultants, exited, and took all 23 down with it for 8 h.
#
# Fix: run podman inside a transient systemd scope of its own. conmon then lives either in
# that scope or in podman's own conmon scope, never in the caller's unit, so the caller can
# be a timer job, a tmux pane or a Claude session and exit or be stopped freely.
#
# Fails closed: if systemd-run exists but cannot create the scope (user manager unreachable),
# podman does NOT run and the exit status is 1, so no container is ever started attached to a
# caller that may kill it. Only a host without systemd-run at all runs plain podman (there is
# no unit cgroup there that could take the container down).
#
# Usage:  podman-detached.sh <podman args...>      e.g. podman-detached.sh start my-container
#         PODMAN_DETACHED_LABEL=<name> labels the scope's description (default: none).
# stdout, stderr and the exit status are podman's.
#
set -euo pipefail

[ $# -gt 0 ] || { echo "usage: podman-detached.sh <podman args...>" >&2; exit 2; }

if ! command -v systemd-run >/dev/null 2>&1; then
  exec podman "$@"
fi

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
SCOPE_ARGS=(--user --scope --quiet --collect)

# Probe first: once systemd-run execs podman, a failure to create the scope and a podman
# failure share one exit status, and only the probe can say which it was.
if ! probe_err="$(systemd-run "${SCOPE_ARGS[@]}" -- true 2>&1)"; then
  echo "!! podman-detached: cannot create a systemd --user scope: ${probe_err:-no detail}" >&2
  echo "!! refusing to run 'podman $1' attached to the caller's cgroup (it would die with the caller)." >&2
  exit 1
fi

exec systemd-run "${SCOPE_ARGS[@]}" \
  --description="podman $1${PODMAN_DETACHED_LABEL:+ $PODMAN_DETACHED_LABEL} (detached from the caller)" \
  -- podman "$@"
