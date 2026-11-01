# syntax=docker/dockerfile:1
#
# scarab-wsfetch for the kind dev stack: the runtime base plus a host-built
# scarab-wsfetch binary. Step Pods are created by the server, so this helper
# cannot come from a hostPath mount like the server does; it stays an image,
# built in seconds from the binary deploy/kind/up.sh builds next to the server.
#
# Same user and layout as docker/wsfetch/Dockerfile: uid 65532 by default (the egress
# container's securityContext sets root itself), /workspace and
# /scarab/secrets present, tar available (Debian slim ships it).
#
# Build context: the directory holding the binary. Usage:
#   docker build --build-arg BASE=scarab-runtime:dev \
#     -f docker/runtime/wsfetch.Dockerfile <dir with scarab-wsfetch>

ARG BASE=scarab-runtime:dev
FROM ${BASE}

USER root
RUN command -v tar && mkdir -p /workspace /scarab/secrets
COPY --chmod=0755 scarab-wsfetch /usr/local/bin/scarab-wsfetch

USER 65532
ENV HOME=/home/scarab \
    SCARAB_WORKSPACE_TARGET=/workspace
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/scarab-wsfetch"]
