# syntax=docker/dockerfile:1
#
# scarab-plugin-mcp — the MCP Plugin (ADR-0086 §10): an MCP server that apps
# such as claude.ai reach through the endpoint core serves for its
# installation. An org admin installs it like any plugin that registers an
# endpoint: its manifest from crates/scarab-plugin-mcp/manifest.json at a
# commit, and this image PINNED BY DIGEST from a repository the operator
# lists in `plugins.trustedPublishers`. The plugin runtime runs it in the
# `scarab-plugins` namespace under the restricted Pod Security profile.
# .github/workflows/image.yml publishes it as
# ghcr.io/thulasi-ram/scarab-plugin-mcp, with the same tags as the other
# images.
#
# Distroless, uid 65532, nothing but the binary, libc and CA roots: it writes
# nothing, so it runs on the read-only root the runtime sets, and it has no
# shell.
#
#   docker build -f docker/plugin-mcp/Dockerfile -t scarab-plugin-mcp .
#
# The build context is the REPO ROOT: the crate is a workspace member (its
# test-only dev-dependency names another workspace crate, so cargo needs every
# manifest to resolve), although the plugin binary links none of them.

# --- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS chef

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Pin the toolchain before the sources so it caches independently.
COPY rust-toolchain.toml ./
RUN rustc --version
RUN cargo install cargo-chef --locked --version 0.1.78

# Dependency caching (cargo-chef), as docker/plugin-argocd does: the cooked
# dependency graph is an ordinary layer, keyed only by manifests + lockfile,
# so a source-only change reuses it.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked -p scarab-plugin-mcp --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked -p scarab-plugin-mcp --bin scarab-plugin-mcp \
    && cp target/release/scarab-plugin-mcp /usr/local/bin/scarab-plugin-mcp

# --- the Plugin image --------------------------------------------------------
# Debian 12 distroless `cc`, matching the bookworm builder's glibc. Everything
# is rustls (no OpenSSL); CA roots are there for an https broker.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS plugin
COPY --from=builder /usr/local/bin/scarab-plugin-mcp /usr/local/bin/scarab-plugin-mcp
# Numeric, so `runAsNonRoot` can verify it without resolving a name (the
# restricted profile; the runtime also sets runAsUser 65532).
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/scarab-plugin-mcp"]
