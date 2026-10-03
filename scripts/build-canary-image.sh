#!/usr/bin/env bash
# build-canary-image.sh: build the rooms canary image from the marina-rooms worktrees
# WITHOUT touching them, working around one build-tooling gap.
#
# Why this exists: aqua-matrix-claude-p depends on aqua-node-client, a git dep on the
# PRIVATE repo inblockio/aqua-node (since aqua-agents 58c3b8f). The aqua-agents
# Dockerfile only redirects the private aqua-rs-sdk source to a staged copy, so the
# podman builder (no credentials, by design) fails with "failed to authenticate when
# downloading repository" for aqua-node, on main as on feat/consultant-rooms.
# `scripts/build-image.sh` cannot be used as is.
#
# What this does, and nothing else:
#   - stages the same five sibling dirs as aqua-agents/scripts/build-image.sh, with the
#     same rsync excludes, plus the host `claude` binary;
#   - stages `git archive <sha>` of ~/aqua-node at EXACTLY the commit the aqua-agents
#     Cargo.lock pins for aqua-node-client (so the client source is the pinned source);
#   - builds with a copy of the branch's Dockerfile that differs ONLY by
#     `COPY aqua-node/ aqua-node/` and one more `--config patch` line pointing the
#     aqua-node git source at that staged copy (the same mechanism the Dockerfile
#     already uses for aqua-rs-sdk). The diff is printed and saved next to the log.
#   - labels the image with the same io.inblock.src.* / io.inblock.gitdep.* labels as
#     build-image.sh, plus io.inblock.build.* labels naming this patch.
# The context lives on disk (~/.cache), never in tmpfs /tmp, and is removed on exit.
#
# Usage: build-canary-image.sh [--worktrees DIR] [--tag TAG] [--aqua-node DIR]
#   defaults: --worktrees ~/wt/marina-rooms --tag aqua-matrix-agent:marina-rooms
#             --aqua-node ~/aqua-node
set -euo pipefail

WT=$HOME/wt/marina-rooms TAG=aqua-matrix-agent:marina-rooms NODE_REPO=$HOME/aqua-node
while [ $# -gt 0 ]; do
  case "$1" in
    --worktrees) WT=$2; shift 2 ;;
    --tag) TAG=$2; shift 2 ;;
    --aqua-node) NODE_REPO=$2; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$TAG" in *:poc|*:poc-*) echo "refusing to build over $TAG (fleet tag)" >&2; exit 2 ;; esac

AGENTS=$WT/aqua-agents
NODE_GIT='https://github.com/inblockio/aqua-node'
export DOCKER_HOST="${DOCKER_HOST:-unix:///run/user/$(id -u)/podman/podman.sock}"
for d in aqua-agents aqua-matrix-connector siwx-oidc aqua-auth aqua-rs-sdk; do
  [ -d "$WT/$d" ] || { echo "missing $WT/$d" >&2; exit 1; }
done

# The aqua-node commit the lock pins, and its presence in the local clone.
NODE_SHA=$(grep -oE "git\+${NODE_GIT}\?tag=[^#\"]+#[0-9a-f]{40}" "$AGENTS/Cargo.lock" | sed 's/.*#//' | sort -u)
[ "$(printf '%s\n' "$NODE_SHA" | grep -c .)" = 1 ] || { echo "expected one aqua-node pin in Cargo.lock, got: $NODE_SHA" >&2; exit 1; }
NODE_TAG=$(grep -oE "git\+${NODE_GIT}\?tag=[^#\"]+#" "$AGENTS/Cargo.lock" | sed 's/.*tag=//; s/#$//' | sort -u)
git -C "$NODE_REPO" cat-file -e "${NODE_SHA}^{commit}" || { echo "$NODE_REPO lacks $NODE_SHA" >&2; exit 1; }

# shellcheck source=/dev/null
source "$AGENTS/scripts/refresh-repos.sh"   # read-only helpers: repo_provenance, gitdep_provenance
PROVENANCE=$(repo_provenance "$WT" aqua-agents aqua-matrix-connector siwx-oidc aqua-auth aqua-rs-sdk)
GITPINS=$(gitdep_provenance "$AGENTS" "$WT/aqua-matrix-connector")
echo ">> provenance:"; sed 's/^/   /' <<<"$PROVENANCE"; sed 's/^/   git-pin /' <<<"$GITPINS"
echo "   aqua-node-client <- git archive $NODE_REPO $NODE_SHA (tag $NODE_TAG)"

CLAUDE_BIN=$(readlink -f "$HOME/.local/bin/claude"); [ -x "$CLAUDE_BIN" ] || { echo "no claude binary" >&2; exit 1; }
mkdir -p "$HOME/.cache/marina-canary/build"
CONTEXT=$(mktemp -d "$HOME/.cache/marina-canary/build/ctx.XXXXXX")
trap 'rm -rf "$CONTEXT"' EXIT
EXCL=(--exclude .git --exclude target --exclude node_modules --exclude '*.pem' --exclude .env --exclude '.env.*' --exclude '.credentials*')
for d in aqua-agents aqua-matrix-connector siwx-oidc aqua-auth aqua-rs-sdk; do
  mkdir -p "$CONTEXT/$d"; rsync -a --delete "${EXCL[@]}" "$WT/$d/" "$CONTEXT/$d/"
done
mkdir -p "$CONTEXT/aqua-node"
git -C "$NODE_REPO" archive "$NODE_SHA" \
  | tar -x -C "$CONTEXT/aqua-node" --exclude='*.pem' --exclude=.env --exclude='.env.*' --exclude='.credentials*'
[ -f "$CONTEXT/aqua-node/crates/aqua-node-client/Cargo.toml" ] || { echo "no aqua-node-client in the archive" >&2; exit 1; }
cp "$CLAUDE_BIN" "$CONTEXT/claude-bin"; chmod 0755 "$CONTEXT/claude-bin"

LEAKS=$(find "$CONTEXT" \( -name '*.pem' -o -name '.env' -o -name '.env.*' -o -name '.credentials*' \) -print)
[ -z "$LEAKS" ] || { echo "$LEAKS"; echo "secrets in the build context" >&2; exit 1; }
echo ">> secret-leak scan: clean; context $(du -sh "$CONTEXT" | cut -f1)"

python3 - "$AGENTS/Dockerfile" "$CONTEXT/Dockerfile.canary" "$NODE_GIT" <<'PY'
import re, sys
src, dst, node_git = sys.argv[1:]
lines = open(src, encoding="utf-8").read().split("\n")
out, copy_n, patch_n = [], 0, 0
for l in lines:
    # The extra patch goes BEFORE the tsa-provider line: that line ends the agents
    # `cargo build` with `&& \`, so anything after it belongs to the next command.
    if "aqua-tsa-provider.path=" in l and l.lstrip().startswith("--config"):
        out.append(f'        --config "patch.\\"{node_git}\\".aqua-node-client.path=\\"/build/aqua-node/crates/aqua-node-client\\"" \\')
        patch_n += 1
    out.append(l)
    if re.match(r"^COPY aqua-rs-sdk/\s+aqua-rs-sdk/\s*$", l):
        out.append("COPY aqua-node/              aqua-node/"); copy_n += 1
if (copy_n, patch_n) != (1, 1):
    sys.exit(f"Dockerfile anchors not found exactly once (copy={copy_n}, patch={patch_n})")
open(dst, "w", encoding="utf-8").write("\n".join(out))
PY
echo ">> Dockerfile.canary vs $AGENTS/Dockerfile:"
diff "$AGENTS/Dockerfile" "$CONTEXT/Dockerfile.canary" | tee "$HOME/.cache/marina-canary/build/Dockerfile.canary.diff" || true

ARGS=(-t "$TAG" -f "$CONTEXT/Dockerfile.canary")
while IFS='=' read -r k v; do ARGS+=(--label "io.inblock.src.$k=$v"); done <<<"$PROVENANCE"
while IFS='=' read -r k v; do [ -n "$k" ] && ARGS+=(--label "io.inblock.gitdep.$k=$v"); done <<<"$GITPINS"
ARGS+=(--label "io.inblock.gitdep.aqua-node-client=${NODE_SHA:0:7}")
ARGS+=(--label "io.inblock.build.patch=aqua-node-client from git archive of inblockio/aqua-node ${NODE_SHA:0:7} ($NODE_TAG) via cargo --config patch; Dockerfile otherwise unchanged")
ARGS+=(--label "io.inblock.build.script=room-probe/scripts/build-canary-image.sh")
echo ">> podman build $TAG"
podman build "${ARGS[@]}" "$CONTEXT"
podman images "$TAG"
