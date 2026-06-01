# syntax=docker/dockerfile:1
#
# aqua-matrix typed-agent-instance — Phase 1 type image
# (see docs/plans/typed-agent-instances.md)
#
# One container = one agent = one identity. The entrypoint is the type-driven
# relay daemon `aqua-matrix-claude-p`, which self-mints its own .pem/DID on
# first boot into the writable layer and forwards DMs through `claude -p`.
#
# Build context layout (assembled by scripts/build-image.sh into a mktemp dir):
#   <context>/
#     aqua-matrix-hello/      <- this repo (workspace root)
#     siwx-oidc/              <- sibling; provides siwx-oidc-auth path-dep
#     aqua-auth/              <- sibling; siwx-oidc-auth depends on ../../aqua-auth
#     claude-bin              <- staged host `claude` binary (glibc-matched)
#
# The three sibling dirs MUST keep their relative layout so the Cargo path deps
# resolve from within aqua-matrix-hello:
#   aqua-matrix-hello/Cargo.toml          siwx-oidc-auth = { path = "../siwx-oidc/siwx-oidc-auth" }
#   siwx-oidc/siwx-oidc-auth/Cargo.toml   aqua-auth      = { path = "../../aqua-auth" }
#
# Build (rootless podman):
#   bash scripts/build-image.sh
#
# CRITICAL — no secrets are baked in. CLAUDE_CODE_OAUTH_TOKEN and
# /agent/config.json are injected / RO-mounted by the orchestrator at RUNTIME.

# ── Stage 1: Builder ──────────────────────────────────────────────────────────
#
# rust:1.93-bookworm = Debian bookworm, glibc 2.36 — deliberately OLDER than the
# runtime's glibc 2.43. A glibc-linked binary is forward-compatible (runs against
# a newer glibc) but not backward-compatible, so building on the older glibc keeps
# the binary runnable on the ubuntu:26.04 runtime stage and on the host.

FROM rust:1.93-bookworm AS builder

WORKDIR /build

# Copy the three sibling dirs preserving the layout the path deps expect.
# (The build context root holds aqua-matrix-hello/, siwx-oidc/, aqua-auth/.)
COPY aqua-matrix-hello/ aqua-matrix-hello/
COPY siwx-oidc/         siwx-oidc/
COPY aqua-auth/         aqua-auth/

WORKDIR /build/aqua-matrix-hello

# Build BOTH workspace binaries in one invocation: the relay daemon and the
# ask_human bridge MCP server. ask_bridge.rs::ask_mcp_bin_path() spawns
# `aqua-matrix-ask-mcp` from beside the running exe, so they must ship together.
#
# DEBUG build for POC iteration speed. For production, swap to `--release`
# (and copy from target/release/ below).
#
# Cache-mount only the cargo registry: registry downloads survive across builds,
# but a cache-mounted `target` would NOT be visible to the later COPY --from
# (cache mounts are not part of the image layer), so we leave `target` as a
# normal directory — the binaries land in target/debug/ ready to copy out.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build -p aqua-matrix-claude-p -p aqua-matrix-ask-mcp

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
#
# ubuntu:26.04 = glibc 2.43, matching the host. Required so the glibc-linked
# builder binaries (and the staged `claude` binary) run unmodified.

FROM ubuntu:26.04 AS runtime

# TLS roots for outbound HTTPS to matrix.inblock.io / siwx-oidc.inblock.io /
# api.anthropic.com. procps (pgrep) backs the HEALTHCHECK. libsqlite3-0 is
# REQUIRED at runtime by matrix-sdk's crypto store: the Rust binary links
# libsqlite3.so.0 dynamically, and without it the daemon aborts at startup with
# "error while loading shared libraries: libsqlite3.so.0".
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        procps \
        libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. /agent is the writable layer holding everything mutable:
#   /agent/store   — self-minted agent.pem + Matrix crypto SQLite (per-incarnation)
#   /agent/memory  — CLAUDE_CONFIG_DIR (claude memory/notes)
#   /agent/config.json — RO-mounted by the orchestrator at runtime
RUN useradd --create-home --home-dir /agent --shell /usr/sbin/nologin agent

# Co-locate both binaries in /usr/local/bin so the ask bridge finds its sibling.
COPY --from=builder /build/aqua-matrix-hello/target/debug/aqua-matrix-claude-p /usr/local/bin/aqua-matrix-claude-p
COPY --from=builder /build/aqua-matrix-hello/target/debug/aqua-matrix-ask-mcp  /usr/local/bin/aqua-matrix-ask-mcp

# The host `claude` binary, staged by build-image.sh as `claude-bin`
# (the exact binary validated in the Phase 0.5 auth spike, glibc-matched).
COPY claude-bin /usr/local/bin/claude

# Make the agent home and its subdirs exist + writable by the non-root user.
RUN mkdir -p /agent/store /agent/memory \
    && chown -R agent:agent /agent \
    && chmod 0755 /usr/local/bin/aqua-matrix-claude-p \
                  /usr/local/bin/aqua-matrix-ask-mcp \
                  /usr/local/bin/claude

# Runtime config — paths the daemon reads. The token and the config.json file
# itself are injected/mounted by the orchestrator, NOT baked here.
ENV AGENT_STORE_DIR=/agent/store \
    AGENT_KEY_FILE=/agent/store/agent.pem \
    AGENT_CONFIG_FILE=/agent/config.json \
    PATH=/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

USER agent
WORKDIR /agent

# The daemon is not an HTTP server, so liveness = "the process is running".
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD pgrep -f aqua-matrix-claude-p > /dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/aqua-matrix-claude-p"]
