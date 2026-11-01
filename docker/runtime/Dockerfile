# syntax=docker/dockerfile:1
#
# scarab-runtime: the image `just stack up` runs scarab-server in (ADR-0077,
# Amendment 2026-11-01). It holds no Scarab binary. The binary is built on the
# host (cargo-zigbuild on macOS, cargo on Linux) and mounted as a directory:
# kind `extraMounts` puts deploy/kind/.mount on the node, a hostPath volume
# puts it at /opt/scarab in the Pod, and the chart's `command` runs
# /opt/scarab/bin/scarab-server. A new build takes effect on a Pod restart,
# with no image build.
#
# The base, packages and user match docker/server/Dockerfile's runtime stage,
# so the binary sees the same glibc (bookworm, 2.36), CA roots and uid 65532.
# The zigbuild target is pinned to that glibc: aarch64-unknown-linux-gnu.2.36.

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --user-group \
        --home-dir /home/scarab --create-home --shell /usr/sbin/nologin scarab \
    && mkdir -p /opt/scarab

# The web UI's dist is mounted here when it was built (deploy/kind/up.sh); the
# server serves it at / when index.html exists, and is API-only otherwise.
ENV SCARAB_UI_DIR=/opt/scarab/ui
ENV SCARAB_ADDR=0.0.0.0:8080

WORKDIR /home/scarab
USER scarab
EXPOSE 8080
