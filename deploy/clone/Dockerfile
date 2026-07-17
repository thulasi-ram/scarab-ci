# The canonical scarab-clone image (ADR-0045): git + git-lfs + the tmpfs
# askpass helper + the pre-snapshot config-scrub guard. The `clone` step kind
# ALWAYS runs this image (pinned by digest, versioned with the server) — never
# the author's — because the kind carries security invariants
# (credential-free .git/config, tmpfs token delivery) an arbitrary image
# cannot guarantee.
#
# Non-root and minimal per the ADR-0039 restricted baseline: uid 65532, no
# shell tricks required at runtime, nothing writable but HOME and /workspace.
FROM alpine:3.21

RUN apk add --no-cache git git-lfs ca-certificates \
    && adduser -D -u 65532 scarab \
    && mkdir -p /workspace /scarab/secrets \
    && chown 65532:65532 /workspace

COPY scarab-askpass scarab-config-scrub scarab-clone.sh /usr/local/bin/
RUN chmod 0755 /usr/local/bin/scarab-askpass \
               /usr/local/bin/scarab-config-scrub \
               /usr/local/bin/scarab-clone.sh

USER 65532
ENV HOME=/home/scarab \
    GIT_ASKPASS=/usr/local/bin/scarab-askpass \
    GIT_TERMINAL_PROMPT=0
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/scarab-clone.sh"]
