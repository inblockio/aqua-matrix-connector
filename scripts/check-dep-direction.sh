#!/usr/bin/env bash
#
# check-dep-direction.sh — connector/agents dependency-direction guard.
#
# Enforces the one invariant the repo split exists to create
# (see docs/plans/repo-split-execution-handover.md §1, §4):
#
#     one-way only:  aqua-agents -> connector,  NEVER the reverse.
#
# No CONNECTOR crate may name an AGENTS-side (backend) crate
# (aqua-matrix-template / aqua-matrix-heartbeat / aqua-matrix-claude-p)
# in its Cargo.toml [dependencies].
#
# We inspect DEPENDENCY LINES ONLY, never package metadata: a
# `description = "…backend…"` field legitimately mentions crate-ish words,
# so an unanchored grep would false-positive. The anchored regex below
# matches only a dependency-table entry at line start (optionally indented):
#
#     ^[[:space:]]*aqua-matrix-(template|heartbeat|claude-p)\b
#
# A dep line like `aqua-matrix-template.workspace = true` or
# `aqua-matrix-template = { path = ... }` starts that way; a
# `description = "…"` line does NOT.
#
# Exit 0 + "OK" when clean; exit 1 + the offending crate/line on any match.

set -euo pipefail

# Resolve the repo root relative to this script so it runs from anywhere.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

# Connector crates: NONE of these may depend on a backend crate.
CONNECTOR_CRATES=(
  aqua-matrix-agent
  aqua-matrix-relay
  aqua-matrix-ask-mcp
  aqua-matrix-md-mcp
  aqua-matrix-orchestrator
  aqua-matrix-gating
  aqua-activity-watch
)

# Anchored dependency-line regex (backend crate names only).
FORBIDDEN_RE='^[[:space:]]*aqua-matrix-(template|heartbeat|claude-p)\b'

violations=0

for crate in "${CONNECTOR_CRATES[@]}"; do
  manifest="${REPO_ROOT}/crates/${crate}/Cargo.toml"

  if [[ ! -f "${manifest}" ]]; then
    echo "MISSING: ${manifest} (connector crate ${crate} has no Cargo.toml)" >&2
    violations=$((violations + 1))
    continue
  fi

  # "No match" is the SUCCESS case, so grep's exit 1 must not kill us under
  # `set -e`: branch on the `if grep` result instead.
  if matches="$(grep -nE "${FORBIDDEN_RE}" "${manifest}")"; then
    echo "FORBIDDEN connector->backend dependency in ${crate}:"
    while IFS= read -r line; do
      echo "    crates/${crate}/Cargo.toml:${line}"
    done <<< "${matches}"
    violations=$((violations + 1))
  else
    echo "  ✓ ${crate}"
  fi
done

echo
if [[ "${violations}" -ne 0 ]]; then
  echo "FAIL: ${violations} connector crate(s) violate the one-way dependency rule."
  echo "      Connector crates must never name aqua-matrix-{template,heartbeat,claude-p}."
  echo "      See docs/plans/repo-split-execution-handover.md §1, §4."
  exit 1
fi

echo "OK"
exit 0
