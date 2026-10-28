# syntax=docker/dockerfile:1
#
# scarab-plugin-argocd — the first-party Argo CD Plugin (ADR-0072, ADR-0074).
# After ADR-0074 it reports nothing; it keeps the code that reads an Argo CD
# Application, for the plugin-resolved gates of ADR-0076. Installed by an org admin PINNED BY DIGEST from the catalog
# repository (SCARAB_PLUGIN_ARGOCD_REPO); the plugin runtime runs it in the
# `scarab-plugins` namespace under the restricted Pod Security profile.
#
# Targets:
#   (default) `plugin` — the Plugin image. Distroless, uid 65532, nothing
#             but the binary, libc and CA roots: it writes nothing, so it runs
#             on the read-only root the runtime sets (the only writable mount
#             is the runtime's small /tmp), and it has no shell — a Plugin Pod
#             has nothing in it but the plugin binary.
#   `fake`    — the fake Argo CD API server (`fake-argocd`) that
#             `just plugin-verify` and the contract test drive. Test tooling:
#             never installed as a Plugin, never pushed with the plugin image.
#
#   docker build -f docker/plugin-argocd/Dockerfile -t scarab-plugin-argocd .
#   docker build -f docker/plugin-argocd/Dockerfile --target fake -t fake-argocd .
#
# The build context is the REPO ROOT: the crate is a workspace member (its
# test-only dev-dependencies name other workspace crates, so cargo needs every
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

# Dependency caching (cargo-chef), as docker/server and docker/wsfetch do: the
# cooked dependency graph is an ordinary layer, keyed only by manifests +
# lockfile, so a source-only change reuses it.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
# Cooked WITH `fake` so one layer serves both binaries (the feature only adds
# axum + futures; the plugin's own dependency features are the same).
RUN cargo chef cook --release --locked -p scarab-plugin-argocd --features fake \
        --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# The plugin is built WITHOUT `fake`: its binary links no server code.
RUN cargo build --release --locked -p scarab-plugin-argocd --bin scarab-plugin-argocd \
    && cp target/release/scarab-plugin-argocd /usr/local/bin/scarab-plugin-argocd \
    && cargo build --release --locked -p scarab-plugin-argocd --features fake --bin fake-argocd \
    && cp target/release/fake-argocd /usr/local/bin/fake-argocd

# --- fake Argo CD (test tooling) -------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS fake
COPY --from=builder /usr/local/bin/fake-argocd /usr/local/bin/fake-argocd
USER 65532:65532
EXPOSE 18080
ENTRYPOINT ["/usr/local/bin/fake-argocd", "--addr", "0.0.0.0:18080"]

# --- the Plugin image (default target) -------------------------------------
# Debian 12 distroless `cc`, matching the bookworm builder's glibc. Everything
# is rustls (no OpenSSL); CA roots are there for an https broker.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS plugin
COPY --from=builder /usr/local/bin/scarab-plugin-argocd /usr/local/bin/scarab-plugin-argocd
# Numeric, so `runAsNonRoot` can verify it without resolving a name (the
# restricted profile; the runtime also sets runAsUser 65532).
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/scarab-plugin-argocd"]
