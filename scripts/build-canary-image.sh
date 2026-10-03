#!/usr/bin/env bash
# build-canary-image.sh: build the rooms canary image from a worktree group with the
# aqua-agents `scripts/build-image.sh` of that group (no copy of its logic here).
#
# build-image.sh stages the pinned aqua-node source itself (git archive of the commit the
# aqua-agents Cargo.lock pins), so no credentials are needed. This wrapper only sets its
# environment and refuses the fleet tags:
#   SIBLINGS_ROOT  = the worktree group (aqua-agents plus its sibling checkouts)
#   AQUA_NODE_REPO = the aqua-node clone to `git archive` from (read only)
#   IMAGE_TAG      = the canary tag, never :poc
#   TMPDIR         = /var/tmp (disk), so the staging context stays out of a tmpfs /tmp and
#                    buildah's cargo registry cache stays warm
# It always passes --no-refresh: the worktrees are built exactly as they are.
#
# One-liner equivalent:
#   cd ~/wt/marina-rooms/aqua-agents && SIBLINGS_ROOT=~/wt/marina-rooms AQUA_NODE_REPO=~/aqua-node \
#     IMAGE_TAG=aqua-matrix-agent:canary TMPDIR=/var/tmp scripts/build-image.sh --no-refresh
#
# Usage: build-canary-image.sh [--worktrees DIR] [--tag TAG] [--aqua-node DIR]
#   defaults: --worktrees ~/wt/marina-rooms --tag aqua-matrix-agent:canary
#             --aqua-node ~/aqua-node
set -euo pipefail

WT=$HOME/wt/marina-rooms TAG=aqua-matrix-agent:canary NODE_REPO=$HOME/aqua-node
while [ $# -gt 0 ]; do
  case "$1" in
    --worktrees) WT=$2; shift 2 ;;
    --tag) TAG=$2; shift 2 ;;
    --aqua-node) NODE_REPO=$2; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "${TAG##*:}" in poc|poc-*) echo "refusing to build over $TAG (fleet tag)" >&2; exit 2 ;; esac
case "$TAG" in *:*) ;; *) echo "refusing an untagged image name: $TAG (it would default to :latest)" >&2; exit 2 ;; esac
[ -x "$WT/aqua-agents/scripts/build-image.sh" ] || { echo "no $WT/aqua-agents/scripts/build-image.sh" >&2; exit 1; }
grep -q 'AQUA_NODE_REPO' "$WT/aqua-agents/scripts/build-image.sh" \
  || { echo "$WT/aqua-agents/scripts/build-image.sh predates the aqua-node staging fix" >&2; exit 1; }

echo ">> building $TAG from $WT (aqua-node from $NODE_REPO)"
cd "$WT/aqua-agents"
SIBLINGS_ROOT=$WT AQUA_NODE_REPO=$NODE_REPO IMAGE_TAG=$TAG TMPDIR=/var/tmp \
  exec scripts/build-image.sh --no-refresh
