# syntax=docker/dockerfile:1
#
# scarab-wsfetch — the ADR-0061 s3-feed workspace fetcher image.
#
# The init container in every Step Pod that has `needs:`. It materialises the
# Step's input Workspace Snapshots straight from the workspace service and exits.
# It replaces a `busybox:1.36` doorstop that did nothing but spin on a marker
# file while the control plane tar-streamed the workspace in over the Kubernetes
# API server.
#
# ⚠ ADR-0061 s3-feed: DELETE ME with the node driver (git-bug 0628369).
# This image is a temporally-ordered STEPPING STONE, not a fallback. It is eager
# — it moves the whole snapshot to every fresh node — and the CSI/FUSE node
# driver replaces it, at which point this directory, the `scarab-wsfetch` bin
# target, and the executor's `WorkspaceFetch` all go away in one commit. The
# binary prints `mode=eager (...)` on every Pod so the stone cannot quietly
# become a floor.
#
# Non-root per the ADR-0039 restricted baseline: uid 65532, all capabilities
# dropped by the PodSpec, nothing writable but the mounted volumes. (Note the
# baseline is why the doorstop had to be `busybox:1.36` and not `busybox:latest`
# — the latter defaults to root and the kubelet refuses it with
# `CreateContainerConfigError`.)
#
# The build context is the REPO ROOT (not this directory): the fetcher is a bin
# target of `crates/scarab-workspace-client`, deliberately, so it reuses the one
# `Cas::materialize` implementation that carries the mode/mtime/symlink fidelity
# rules rather than growing a second copy of them.

# --- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Pin the toolchain before the sources so it caches independently.
COPY rust-toolchain.toml ./
RUN rustc --version

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p scarab-workspace-client --bin scarab-wsfetch \
    && cp target/release/scarab-wsfetch /usr/local/bin/scarab-wsfetch

# --- runtime ---------------------------------------------------------------
# Debian slim, matching docker/server: everything is rustls (no OpenSSL), so the
# binary needs only libc and CA roots. CA roots are not optional even though the
# in-cluster workspace service is plain HTTP today — an operator who fronts it
# with TLS must not have to rebuild this image.
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --user-group \
        --home-dir /home/scarab --create-home --shell /usr/sbin/nologin scarab \
    && mkdir -p /workspace /scarab/secrets

COPY --from=builder /usr/local/bin/scarab-wsfetch /usr/local/bin/scarab-wsfetch

USER 65532
ENV HOME=/home/scarab \
    SCARAB_WORKSPACE_TARGET=/workspace
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/scarab-wsfetch"]
