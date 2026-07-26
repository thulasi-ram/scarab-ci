#!/usr/bin/env bash
# Submit a one-step pipeline to a running scarab-server and watch it complete.
# Requires `just up` (or `just serve`) to be running first.
set -euo pipefail

base="${SCARAB_BASE:-http://127.0.0.1:8080}"

# The image MUST default to a non-root USER. ADR-0039's hardened step baseline
# sets `runAsNonRoot: true` and no `runAsUser`, so the kubelet refuses any image
# whose default user is root with `CreateContainerConfigError: container has
# runAsNonRoot and image will run as root` — the Pod never starts and the Attempt
# fails with class `config`. This demo used `busybox:latest` and had therefore
# been failing since ADR-0039 landed, which is a bad state for the one recipe
# whose whole job is to say "the stack works".
#
# `scarab-clone` is used because it is ours, it is `USER 65532`, and it is an
# alpine base so it still has a shell (a distroless :nonroot image would not).
# Opting out via `run_as_root: true` would be the other fix and is worse for a
# smoke test: it needs an admitted governance grant (ADR-0039), so the demo would
# start depending on Environment configuration.
image="${SCARAB_DEMO_IMAGE:-ghcr.io/thulasi-ram/scarab-clone:edge}"
echo "==> POST $base/v1/runs (image: $image)"
id="$(curl -sf -X POST "$base/v1/runs" \
  -H 'content-type: application/json' \
  -d "{
        \"pipeline\": {
          \"ir_version\": 1,
          \"steps\": [
            { \"id\": \"hello\", \"image\": \"$image\",
              \"command\": [\"sh\", \"-c\", \"echo hello from scarab; id\"] }
          ]
        }
      }" \
  | python3 -c 'import sys, json; print(json.load(sys.stdin)["id"])')"
echo "==> created run $id"

echo "==> polling status"
for _ in $(seq 1 60); do
  status="$(curl -sf "$base/v1/runs/$id" \
    | python3 -c 'import sys, json; print(json.load(sys.stdin)["status"])')"
  echo "    status: $status"
  case "$status" in
    succeeded)
      echo "==> ✅ run $id succeeded"
      echo "==> logs:"
      curl -sf "$base/v1/runs/$id/logs" || true
      exit 0
      ;;
    failed | dead_lettered | cancelled)
      echo "==> ❌ run $id ended: $status" >&2
      exit 1
      ;;
  esac
  sleep 2
done

echo "error: run $id did not complete in time" >&2
exit 1
