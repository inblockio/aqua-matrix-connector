#!/usr/bin/env bash
#
# restore-agent-fleet.sh — boot-time restore for the aqua-agent-* container fleet.
#
# Podman restart policies don't survive a WSL/VM reboot, and the fleet runs
# --restart on-failure, which never fires on the clean SIGTERM exit a shutdown
# produces — so after a reboot every agent container sits in 'exited' until
# someone starts it (this is exactly what silenced the whole fleet on
# 2026-06-10). This script `podman start`s every exited container matching the
# filter. Identity, config, and memory live in the per-container persist
# volumes, so a plain start brings each agent back with its DID and crypto
# store intact (connected store_wiped=false).
#
# Canonical home: Skills/consultant-deploy/restore-agent-fleet.sh (repo).
# The host runs it via the ~/restore-agent-fleet.sh symlink from
# aqua-agent-fleet-restore.service (systemd --user oneshot, runs at boot).
#
# NOTE: this starts ANY exited aqua-agent-* container, including ones stopped
# on purpose. `podman rm` a container (or rename it away from the prefix) if
# it must stay down across reboots.
#
# Usage:
#   restore-agent-fleet.sh                  # start all exited aqua-agent-* containers
#   restore-agent-fleet.sh --dry-run        # list what would be started
#   restore-agent-fleet.sh --filter PREFIX  # different name prefix (tests)
#
set -euo pipefail

FILTER="${FLEET_FILTER:-aqua-agent-}"
DRY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1 ;;
    --filter) FILTER="$2"; shift ;;
    *) echo "restore-agent-fleet: unknown arg $1" >&2; exit 2 ;;
  esac
  shift
done

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

mapfile -t stopped < <(podman ps -a --filter "name=$FILTER" --filter status=exited --format '{{.Names}}')

if [ "${#stopped[@]}" -eq 0 ]; then
  echo "restore-agent-fleet: nothing to start (filter='$FILTER')"
  exit 0
fi

echo "restore-agent-fleet: ${#stopped[@]} exited container(s): ${stopped[*]}"
[ "$DRY" = 1 ] && exit 0

rc=0
for c in "${stopped[@]}"; do
  if podman start "$c" >/dev/null; then
    echo "  started $c"
  else
    echo "  FAILED to start $c" >&2
    rc=1
  fi
done
exit "$rc"
