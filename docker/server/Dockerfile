# syntax=docker/dockerfile:1
#
# scarab-server container image (ADR-0016: one binary, selectable roles).
#
# Multi-stage: a full Rust toolchain builds the release binary, then a slim
# Debian runtime carries only the binary + CA certs. Everything is rustls
# (tls-rustls-ring / reqwest rustls), so no OpenSSL runtime dependency — the
# binary needs nothing but libc and CA roots for outbound TLS.

# --- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS builder

# ring (via rustls) compiles C; the bookworm base ships gcc, but install the
# essentials explicitly so the build never silently depends on base contents.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Pin the toolchain before copying sources so it caches independently.
COPY rust-toolchain.toml ./
RUN rustc --version

# Manifests + all workspace crates. `ui/` is npm (not a cargo member) and is
# excluded by .dockerignore, so it never enters the build context.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build only the server binary in release mode. BuildKit cache mounts keep the
# cargo registry and target dir warm across builds; copy the binary out of the
# (ephemeral) target mount so it survives into the next stage.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p scarab-server \
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

# Default object-store fallback dir lives under the writable home when no S3 is
# configured; production sets SCARAB_S3_BUCKET and never touches it.
WORKDIR /home/scarab
USER scarab

EXPOSE 8080
ENV SCARAB_ADDR=0.0.0.0:8080 \
    SCARAB_OBJECT_DIR=/home/scarab/.scarab/objects

ENTRYPOINT ["/usr/local/bin/scarab-server"]
