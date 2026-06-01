#!/usr/bin/env bash
#
# build-image.sh — assemble the Phase 1 type-image build context and build it
# with rootless podman. See docs/plans/typed-agent-instances.md.
#
# The image's Cargo path deps point at siblings OUTSIDE this repo:
#   aqua-matrix-hello/Cargo.toml   ->  ../siwx-oidc/siwx-oidc-auth
#   siwx-oidc/siwx-oidc-auth       ->  ../../aqua-auth
# so a single COPY of this repo is not enough. This script stages all three
# sibling dirs (plus the host `claude` binary) into one mktemp context that
# preserves the relative layout, then runs `podman build` against it.
#
# Usage:
#   bash scripts/build-image.sh                # stage + build
#   bash scripts/build-image.sh --stage-only   # stage + print layout, NO build
#
# Idempotent. The staging dir is removed on exit (trap).

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────────────

IMAGE_TAG="aqua-matrix-agent:poc"

# Rootless podman socket (Phase 0). Exported so `podman` (and any bollard caller
# inheriting this env) talks to the user socket rather than a root daemon.
export DOCKER_HOST="${DOCKER_HOST:-unix:///run/user/$(id -u)/podman/podman.sock}"

# This script lives in <repo>/scripts/ ; the repo is its parent, and the three
# sibling dirs live one level above the repo (all under /home/<user>/).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"            # aqua-matrix-hello
SIBLINGS_ROOT="$(cd "${REPO_DIR}/.." && pwd)"          # /home/<user>

REPO_NAME="$(basename "${REPO_DIR}")"                  # aqua-matrix-hello
SIWX_DIR="${SIBLINGS_ROOT}/siwx-oidc"
AQUA_AUTH_DIR="${SIBLINGS_ROOT}/aqua-auth"

DOCKERFILE="${REPO_DIR}/Dockerfile"

# Host `claude` binary — resolve the symlink to the real versioned ELF (the
# exact binary validated in the Phase 0.5 auth spike, glibc-matched to ubuntu).
CLAUDE_LINK="${HOME}/.local/bin/claude"

# ── Args ──────────────────────────────────────────────────────────────────────

STAGE_ONLY=0
case "${1:-}" in
    --stage-only) STAGE_ONLY=1 ;;
    "")           ;;
    *) echo "usage: $0 [--stage-only]" >&2; exit 2 ;;
esac

# ── Preflight ───────────────────────────────────────────────────────────────--

for d in "${REPO_DIR}" "${SIWX_DIR}" "${AQUA_AUTH_DIR}"; do
    if [[ ! -d "${d}" ]]; then
        echo "ERROR: required sibling dir missing: ${d}" >&2
        exit 1
    fi
done

if [[ ! -f "${DOCKERFILE}" ]]; then
    echo "ERROR: Dockerfile not found at ${DOCKERFILE}" >&2
    exit 1
fi

if [[ ! -e "${CLAUDE_LINK}" ]]; then
    echo "ERROR: host claude binary not found at ${CLAUDE_LINK}" >&2
    exit 1
fi
CLAUDE_BIN="$(readlink -f "${CLAUDE_LINK}")"
if [[ ! -x "${CLAUDE_BIN}" ]]; then
    echo "ERROR: resolved claude binary is not executable: ${CLAUDE_BIN}" >&2
    exit 1
fi

if ! command -v rsync >/dev/null 2>&1; then
    echo "ERROR: rsync is required to assemble the staging context" >&2
    exit 1
fi

# ── Staging context ─────────────────────────────────────────────────────────--

CONTEXT="$(mktemp -d -t aqua-matrix-agent-ctx.XXXXXX)"
cleanup() { rm -rf "${CONTEXT}"; }
trap cleanup EXIT

echo ">> Staging build context in ${CONTEXT}"

# rsync excludes: keep the context lean (no build artifacts / VCS) and
# secret-free (no keys, env files, or credentials). Trailing slashes copy
# directory *contents* into a same-named target dir.
RSYNC_EXCLUDES=(
    --exclude '.git'
    --exclude 'target'
    --exclude 'node_modules'
    --exclude '*.pem'
    --exclude '.env'
    --exclude '.env.*'
    --exclude '.credentials*'
)

stage_dir() {
    local src="$1" dest_name="$2"
    mkdir -p "${CONTEXT}/${dest_name}"
    rsync -a --delete "${RSYNC_EXCLUDES[@]}" "${src}/" "${CONTEXT}/${dest_name}/"
}

stage_dir "${REPO_DIR}"      "${REPO_NAME}"   # aqua-matrix-hello/
stage_dir "${SIWX_DIR}"      "siwx-oidc"      # siwx-oidc/
stage_dir "${AQUA_AUTH_DIR}" "aqua-auth"      # aqua-auth/

# Stage the host `claude` binary as `claude-bin` (Dockerfile COPYs it to
# /usr/local/bin/claude).
cp "${CLAUDE_BIN}" "${CONTEXT}/claude-bin"
chmod 0755 "${CONTEXT}/claude-bin"

# ── Report staged layout ──────────────────────────────────────────────────────

echo ">> Staged layout (top level):"
ls -la "${CONTEXT}"

echo ">> Path-dep resolution check (relative to ${REPO_NAME}):"
( cd "${CONTEXT}/${REPO_NAME}" && \
  for p in Cargo.toml \
           ../siwx-oidc/siwx-oidc-auth/Cargo.toml \
           ../siwx-oidc/siwx-oidc-auth/../../aqua-auth/Cargo.toml; do
      if [[ -f "${p}" ]]; then echo "   OK  ${p}"; else echo "   MISSING  ${p}"; fi
  done )

echo ">> Secret-leak scan (must list nothing):"
LEAKS="$(find "${CONTEXT}" \( -name '*.pem' -o -name '.env' -o -name '.env.*' -o -name '.credentials*' \) -print)"
if [[ -n "${LEAKS}" ]]; then
    echo "${LEAKS}"
    echo "ERROR: secrets leaked into the build context (see above)" >&2
    exit 1
fi
echo "   clean"

echo ">> claude-bin: $(ls -la "${CONTEXT}/claude-bin")"
echo ">> Context size: $(du -sh "${CONTEXT}" | cut -f1)"

# ── Build (unless --stage-only) ───────────────────────────────────────────────

if [[ "${STAGE_ONLY}" -eq 1 ]]; then
    echo ">> --stage-only: skipping podman build."
    exit 0
fi

echo ">> Building ${IMAGE_TAG} (this compiles the workspace — first build takes minutes)"
podman build -t "${IMAGE_TAG}" -f "${DOCKERFILE}" "${CONTEXT}"

echo ">> Done. Resulting image:"
podman images "${IMAGE_TAG}"
