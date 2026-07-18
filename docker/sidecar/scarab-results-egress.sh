#!/bin/sh
# The trusted results-egress sidecar entrypoint (ADR-0042): runs alongside the
# step as a NATIVE sidecar (initContainer, restartPolicy Always). The kubelet
# SIGTERMs it after the step container exits — that termination window is when
# it drains /scarab/results and POSTs the merged results to the control
# plane's fence-scoped ingest endpoint. The untrusted step never holds the
# token; only this container does (ADR-0042 trust model).
#
# Env contract (set by the executor's Pod builder):
#   SCARAB_RESULTS        results dir the step wrote (default /scarab/results)
#   SCARAB_RESULTS_URL    the ingest endpoint (…/runs/{run}/steps/{step}/results)
#   SCARAB_RESULTS_TOKEN  fence-scoped HMAC token
#   SCARAB_ATTEMPT        the attempt id the token was minted for
set -u

dir="${SCARAB_RESULTS:-/scarab/results}"

drain() {
  # Merge every <name>.json into one {name: value, …} object. A file that is
  # not valid JSON is skipped loudly rather than poisoning the whole POST.
  payload=$(
    cd "$dir" 2>/dev/null || exit 0
    for f in *.json; do
      [ -f "$f" ] || continue
      jq -c --arg k "${f%.json}" '{($k): .}' "$f" 2>/dev/null \
        || echo "scarab-results-egress: skipping invalid JSON in $f" >&2
    done | jq -cs 'add // {}'
  )
  if [ -z "$payload" ] || [ "$payload" = "{}" ] || [ "$payload" = "null" ]; then
    echo "scarab-results-egress: no results to drain" >&2
    return 0
  fi
  # Confirmed POST: bounded retries — the window is the Pod's termination
  # grace period, and the write is idempotent on the fence (ADR-0021).
  if curl -fsS -m 10 --retry 5 --retry-delay 1 --retry-all-errors \
      -X POST \
      -H "content-type: application/json" \
      -H "x-scarab-results-token: ${SCARAB_RESULTS_TOKEN:?}" \
      -H "x-scarab-attempt: ${SCARAB_ATTEMPT:-0}" \
      -d "$payload" \
      "${SCARAB_RESULTS_URL:?}" >/dev/null; then
    echo "scarab-results-egress: results posted" >&2
  else
    echo "scarab-results-egress: POST failed after retries" >&2
    return 1
  fi
}

terminated=0
on_term() { terminated=1; }
trap on_term TERM INT

echo "scarab-results-egress: watching $dir (draining on SIGTERM)" >&2
while [ "$terminated" -eq 0 ]; do
  sleep 1 &
  wait $! 2>/dev/null
done

drain
