# The trusted results-egress sidecar image (ADR-0042): drains the step's
# /scarab/results and POSTs them to the control plane's fence-scoped ingest
# endpoint on shutdown. Runs as a NATIVE sidecar next to every step Pod when
# results egress is enabled — pinned by digest, versioned with the server.
# Minimal on purpose: sh + curl + jq; non-root per the ADR-0039 baseline.
FROM alpine:3.21

RUN apk add --no-cache curl jq \
    && adduser -D -u 65532 scarab

COPY scarab-results-egress.sh /usr/local/bin/scarab-results-egress.sh
RUN chmod 0755 /usr/local/bin/scarab-results-egress.sh

USER 65532
ENTRYPOINT ["/usr/local/bin/scarab-results-egress.sh"]
