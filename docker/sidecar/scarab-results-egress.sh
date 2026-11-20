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
  #
  # Only the ingest's own answer, 202, counts as posted. Any other answer is
  # a failure and is retried, a 2xx included: a URL that matches no API
  # route used to reach the server's web UI, which answered 200 with HTML,
  # and the sidecar reported results it had not delivered.
  tries=0
  while :; do
    tries=$((tries + 1))
    # The reply's body, then a last line `<status> <content type>`.
    out=$(curl -sS -m 10 -w '\n%{http_code} %{content_type}' \
      -X POST \
      -H "content-type: application/json" \
      -H "x-scarab-results-token: ${SCARAB_RESULTS_TOKEN:?}" \
      -H "x-scarab-attempt: ${SCARAB_ATTEMPT:-0}" \
      -d "$payload" \
      "${SCARAB_RESULTS_URL:?}") || out="$out (curl exit $?)"
    answer=$(printf '%s\n' "$out" | tail -n 1)
    reply=$(printf '%s\n' "$out" | sed '$d' | head -c 200)
    code=${answer%% *}
    if [ "$code" = "202" ]; then
      echo "scarab-results-egress: results posted" >&2
      return 0
    fi
    echo "scarab-results-egress: POST to ${SCARAB_RESULTS_URL} answered ${answer} (try ${tries} of 6): ${reply}" >&2
    if [ "$tries" -ge 6 ]; then
      echo "scarab-results-egress: POST failed after retries; the results were NOT delivered" >&2
      return 1
    fi
    sleep 1
  done
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
