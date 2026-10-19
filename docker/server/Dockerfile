# syntax=docker/dockerfile:1
#
# scarab-server container image (ADR-0016: one binary, selectable roles).
#
# Multi-stage: a full Rust toolchain builds the release binary, then a slim
# Debian runtime carries only the binary + CA certs. Everything is rustls
# (tls-rustls-ring / reqwest rustls), so no OpenSSL runtime dependency — the
# binary needs nothing but libc and CA roots for outbound TLS.

# --- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS chef

# ring (via rustls) compiles C; the bookworm base ships gcc, but install the
# essentials explicitly so the build never silently depends on base contents.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Pin the toolchain before copying sources so it caches independently.
COPY rust-toolchain.toml ./
RUN rustc --version
RUN cargo install cargo-chef --locked --version 0.1.78

# Dependency caching (cargo-chef). This build used a BuildKit cache mount on
# target/, which in CI started EMPTY on every runner — `cache-to: type=gha`
# exports image layers, never cache mounts — so every build was a cold release
# compile of ~400 crates. `cook` compiles the dependency graph alone into an ordinary LAYER,
# which the GHA layer cache does keep; its input is only the manifests +
# lockfile (`recipe.json`), so a source-only change reuses it. No cache mount on
# the final build: a mount over target/ would hide the cooked layer.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked -p scarab-server \
        --recipe-path recipe.json

# Manifests + all workspace crates. `ui/` is npm (not a cargo member) and is
# excluded by .dockerignore, so it never enters the build context.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build only the server binary in release mode, on top of the cooked
# dependencies.
RUN cargo build --release --locked -p scarab-server \
    && cp target/release/scarab-server /usr/local/bin/scarab-server

# --- runtime ---------------------------------------------------------------
# --- The web UI (ADR-0054): built once, baked into the runtime image. -----
# The build context mirrors the repo's `ui/` layout: scarab-web-ui imports
# shared brand assets from its sibling `ui/brand` via `../../../brand/...`
# (the ASCII motifs, PR #18), so both must sit under a common `ui/` root here
# or those relative imports resolve outside the tree and rollup fails.
FROM node:22-bookworm-slim AS ui
WORKDIR /ui/scarab-web-ui
COPY ui/scarab-web-ui/package.json ui/scarab-web-ui/package-lock.json ./
RUN npm ci
COPY ui/scarab-web-ui .
COPY ui/brand /ui/brand
COPY openapi.json /openapi.json
RUN sed -i 's|../../openapi.json|/openapi.json|' package.json && npm run gen && npm run build

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --user-group \
        --home-dir /home/scarab --create-home --shell /usr/sbin/nologin scarab

COPY --from=builder /usr/local/bin/scarab-server /usr/local/bin/scarab-server
# The same-origin web UI (ADR-0054): the server serves it at / when present.
COPY --from=ui /ui/scarab-web-ui/dist /usr/share/scarab/ui
ENV SCARAB_UI_DIR=/usr/share/scarab/ui

# No baked-in SCARAB_OBJECT_DIR (ADR-0067 part 1): the object store is a hard
# requirement, so an install that configures no SCARAB_S3_BUCKET fails the boot
# loudly instead of silently writing into this container's ephemeral home. A
# dev container that really wants the local-dir store sets
# SCARAB_OBJECT_DIR=/home/scarab/.scarab/objects explicitly.
WORKDIR /home/scarab
USER scarab

EXPOSE 8080
ENV SCARAB_ADDR=0.0.0.0:8080

ENTRYPOINT ["/usr/local/bin/scarab-server"]
