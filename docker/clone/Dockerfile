# The scarab-clone image (ADR-0045, ADR-0076 §9): git + git-lfs + the tmpfs
# askpass helper + the pre-snapshot config-scrub guard. The first-party clone
# library (libs/clone.yaml) pins it by digest; its Step is the one an org
# trusts with a long-lived forge token, because this image keeps .git/config
# credential-free and reads the token only from the mounted file.
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
