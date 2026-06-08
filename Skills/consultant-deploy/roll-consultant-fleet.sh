#!/usr/bin/env bash
#
# roll-consultant-fleet.sh — roll every dedicated Aqua consultant onto the current
# localhost/aqua-matrix-agent:poc image in one shot. For each registry entry it does
# `spawn-consultant.sh --replace --keep-config`, i.e. podman rm -f + re-run reusing the
# per-label persist volume, so each DID + memory is PRESERVED and each hand-customized
# config is used VERBATIM (never re-rendered from the template).
#
# This replaces the old "rm -f each container, then run each recreate-<label>.sh by hand"
# procedure (and the easy-to-get-wrong manual recreate of the generic container).
#
# Registry: ~/.aqua-matrix-test/consultants.registry  (override with CONSULTANTS_REGISTRY)
#
# Usage:
#   bash ~/roll-consultant-fleet.sh --dry-run      # show what would roll, do nothing
#   bash ~/roll-consultant-fleet.sh --build        # rebuild image first, then roll all
#   bash ~/roll-consultant-fleet.sh                # roll all on the existing image
#
set -euo pipefail

REG="${CONSULTANTS_REGISTRY:-/home/waldknoten-01/.aqua-matrix-test/consultants.registry}"
SPAWN=/home/waldknoten-01/spawn-consultant.sh
BUILD_SH=/home/waldknoten-01/aqua-agents/scripts/build-image.sh
DRY=0; BUILD=0

for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --build)   BUILD=1 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "!! unknown arg: $a" >&2; exit 2 ;;
  esac
done

[ -f "$REG" ]   || { echo "!! no registry at $REG" >&2; exit 1; }
[ -x "$SPAWN" ] || { echo "!! spawner missing/not executable: $SPAWN" >&2; exit 1; }

if [ "$BUILD" -eq 1 ]; then
  if [ "$DRY" -eq 1 ]; then
    echo ">> (dry-run) would rebuild image: bash $BUILD_SH"
  else
    echo ">> rebuilding image: bash $BUILD_SH"
    bash "$BUILD_SH"
  fi
fi

rc=0
while IFS=$'\t' read -r label target display _; do
  # Trim leading/trailing whitespace from the label cell so an INDENTED comment ("  # …") is
  # still recognised as a comment, and a stray-whitespace label is normalised before validation.
  label="${label#"${label%%[![:space:]]*}"}"
  label="${label%"${label##*[![:space:]]}"}"
  case "$label" in
    ''|\#*) continue ;;                                  # blank or comment row
    *[!a-z0-9-]*)                                        # not a valid slug (e.g. a shifted MXID from a blank label column)
      echo "!! skipping malformed registry label: '$label' (must be [a-z0-9-])" >&2; rc=1; continue ;;
  esac
  echo
  echo "── rolling: ${label}   (${display})"
  if [ "$DRY" -eq 1 ]; then
    echo "   (dry-run) bash $SPAWN --replace --keep-config --label ${label}"
  else
    if ! bash "$SPAWN" --replace --keep-config --label "$label"; then
      echo "!! roll FAILED for ${label} (continuing with the rest)" >&2
      rc=1
    fi
  fi
done < "$REG"

echo
echo ">> fleet roll complete. verify: podman ps   (every consultant Up, restarts 0)"
echo "   per-instance: podman logs <name> | grep -E 'agent DID|connected|display name set'"
exit "$rc"
