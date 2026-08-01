# syntax=docker/dockerfile:1
#
# scarab-wsfetch — the ADR-0061 workspace helper image: fetch, hold, drain.
#
# Three containers wear this image now (one binary, three subcommands):
#   - the FETCH init container (`scarab-wsfetch`, bare == `fetch`): materialises
#     the Step's input Workspace Snapshots straight from the workspace service
#     and exits. Replaced the busybox doorstop the control plane tar-streamed
#     the workspace into over the Kubernetes API server;
#   - the EGRESS hold (`scarab-wsfetch hold`): ignores SIGTERM and waits for
#     the CP's egress-done marker — replacing the busybox `sh` loop
#     (WORKSPACE_HELPER_IMAGE), which died on TERM;
#   - the in-Pod DRAIN (`scarab-wsfetch drain`, exec'd by the CP in the egress
#     container): ingests /workspace to the Depot and posts the DrainRecord.
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

# `tar` is named EXPLICITLY even though Debian marks it Priority: required
# (present in slim already): during a control-plane/image skew an OLD control
# plane still drains the egress container with `kubectl exec … tar -cf -`, and
# this image is what that exec lands in. A base-image change that quietly
# dropped tar would break exactly and only the skew window — the worst place
# to discover it. `command -v tar` makes the requirement fail the BUILD.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        tar \
    && rm -rf /var/lib/apt/lists/* \
    && command -v tar \
    && useradd --system --uid 65532 --user-group \
        --home-dir /home/scarab --create-home --shell /usr/sbin/nologin scarab \
    && mkdir -p /workspace /scarab/secrets

COPY --from=builder /usr/local/bin/scarab-wsfetch /usr/local/bin/scarab-wsfetch

# USER 65532 is the FETCH container's posture (ADR-0039 restricted baseline)
# and stays the image default — unchanged. The EGRESS container overrides it:
# the new executor PINS `run_as_user: 0` in that container's securityContext
# (root-read is the privilege the drain has always had — the busybox drain ran
# as root), and a PodSpec `runAsUser` beats an image USER, so nothing here
# fights it. What "root-runnable" required of this image: no
# `run_as_non_root`-style assumption baked in, and a binary + tar readable and
# executable by any uid — both true (0755 under /usr).
USER 65532
ENV HOME=/home/scarab \
    SCARAB_WORKSPACE_TARGET=/workspace
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/scarab-wsfetch"]
