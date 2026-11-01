# `just stack <recipe>`: the kind dev stack, the public demo on the Oracle box,
# and the Depot. Recipes run from the repository root.
set working-directory := '..'

# `just stack` alone lists this module rather than bringing anything up.
[private]
[default]
help:
    @just --justfile {{quote(justfile())}} --unsorted --list stack

# The kind dev stack (ADR-0077 Amendment, deploy/kind/README.md): one kind
# cluster `scarab-dev`, Postgres + MinIO in it, and the real chart with the
# server and the Depot running a binary built on the host (cargo-zigbuild on
# macOS), mounted into a thin runtime image. The API and UI answer on the host
# at SCARAB_ADDR from deploy/kind/.env, so `just ui` works against it.
# Kube calls use deploy/kind/.kubeconfig only, checked before every call.
#
# `just test e2e` runs on the same release, switched to its e2e mode (own
# database and bucket).
#
# `just stack up dogfood` is the same stack in GitHub App mode with its own
# database and bucket, GitHub login (no dev-insecure), plus a named Cloudflare
# tunnel. It reads ~/.config/scarab/dogfood.env (CLOUDFLARE_TUNNEL_TOKEN,
# DOGFOOD_HOSTNAME, SCARAB_OAUTH_CLIENT_ID and SCARAB_OAUTH_CLIENT_SECRET are
# required; values are never printed). The tunnel's public hostname must route
# ONLY path ^/webhooks/ to http://scarab.scarab.svc.cluster.local:80 (set in
# the Cloudflare dashboard); up.sh checks from outside and takes the tunnel
# down when / or /v1/repos reaches Scarab. People sign in on
# http://localhost:<SCARAB_ADDR port>, not 127.0.0.1: passkeys refuse an IP
# address.
#
# `just stack up demo-oracle [ref]` deploys the PUBLIC demo (deploy/demo-oracle):
# one Oracle Cloud "Always Free" A1 instance (arm64, 2 OCPU / 12 GB) running
# k3s, the control plane, the Depot and every step Pod, reached through a
# Cloudflare Tunnel. It deploys published images only:
#
#   just stack up demo-oracle             # the latest published ghcr `edge`
#   just stack up demo-oracle sha-<sha>   # a specific published SHA
#
# It runs only ON THE BOX, after `sudo bash deploy/demo-oracle/bootstrap.sh`
# and after filling in deploy/demo-oracle/.env. The recipe refuses unless the
# host is Linux on aarch64, k3s's kubeconfig /etc/rancher/k3s/k3s.yaml exists
# (bootstrap.sh installs k3s, which writes it) and deploy/demo-oracle/.env
# exists. A Mac (the kind stack's home) fails the first check, a CI runner
# (x86_64, no k3s) the second and third, and any machine without the box's
# .env the last. deploy.sh then reads k3s's kubeconfig, not the shell's, passes
# it and the context KUBE_CONTEXT (k3s writes `default`) to every kubectl and
# helm call, and refuses unless that context's API server is the k3s API on
# loopback, so it never targets another cluster.
#
# There is no variant that builds on the box. Two Ampere cores will not compile
# this workspace in a time anyone will wait for, and doing it on the demo box
# would starve the running demo of both cores while it happened. image.yml
# builds arm64 on native runners, so every published tag already carries an
# arm64 manifest: pull, never build.
[doc('Bring up the kind stack (dev or dogfood), or the public demo on the Oracle box')]
up mode="dev" ref="":
    #!/usr/bin/env bash
    set -euo pipefail
    usage() { echo "usage: just stack up [dev|dogfood] | just stack up demo-oracle [ref] (e2e and chaos modes are \`just test e2e\` and \`just test chaos\`)" >&2; exit 2; }
    case "{{mode}}" in
      dev|dogfood)
        [ -z "{{ref}}" ] || usage
        bash deploy/kind/up.sh "{{mode}}"
        ;;
      demo-oracle)
        refuse() { echo "REFUSING: $1. \`just stack up demo-oracle\` runs only on the Oracle demo box (deploy/demo-oracle/README.md)." >&2; exit 2; }
        [ "$(uname -s)" = Linux ] || refuse "this host is $(uname -s), not Linux"
        [ "$(uname -m)" = aarch64 ] || refuse "this host is $(uname -m), not aarch64"
        [ -f /etc/rancher/k3s/k3s.yaml ] || refuse "no k3s here (/etc/rancher/k3s/k3s.yaml is missing; bootstrap.sh installs it)"
        [ -f deploy/demo-oracle/.env ] || refuse "deploy/demo-oracle/.env is missing"
        ref="{{ref}}"; ref="${ref:-edge}"
        owner=ghcr.io/thulasi-ram
        echo "==> deploying published images @ $ref (ghcr, pullPolicy Always)"
        IMAGE_REPOSITORY="$owner/scarab-server" \
        IMAGE_PULL_POLICY=Always \
        SCARAB_CLONE_IMAGE="$owner/scarab-clone:$ref" \
        SCARAB_SIDECAR_IMAGE="$owner/scarab-results-sidecar:$ref" \
        SCARAB_WSFETCH_IMAGE="$owner/scarab-wsfetch:$ref" \
          bash deploy/demo-oracle/deploy.sh "$ref"
        ;;
      *) usage ;;
    esac

# `just stack seed dogfood` grants the dogfood stack's Owner (DOGFOOD_OWNER,
# default github:$SCARAB_ORG) the owner role and its member (DOGFOOD_MEMBER)
# the member role in SCARAB_ORG, with `scarab-server admin grant` in the
# server Pod. `just stack up dogfood` runs it too; run it again after someone
# it named has signed in for the first time (a grant needs a Principal, which
# a first sign-in creates). Idempotent. `checklist` also prints the one-time
# steps of a fresh database that need a passkey. See deploy/kind/seed.sh.
[doc('Grant the dogfood Owner and member their roles; `checklist` adds the fresh-database steps')]
seed mode="dogfood" what="":
    #!/usr/bin/env bash
    set -euo pipefail
    usage() { echo "usage: just stack seed dogfood [checklist]" >&2; exit 2; }
    [ "{{mode}}" = dogfood ] || usage
    case "{{what}}" in
      "") bash deploy/kind/seed.sh ;;
      checklist) bash deploy/kind/seed.sh --checklist ;;
      *) usage ;;
    esac

# Tear down the kind dev stack. `just stack down keep` uninstalls the release
# only and keeps the cluster, Postgres, MinIO and their data.
[doc('Delete the kind cluster; `keep` uninstalls the release and keeps the data')]
down what="all":
    bash deploy/kind/down.sh {{what}}

# Rebuild the binary and roll the server and Depot Pods onto it, in the mode the
# stack was last brought up in (dev or dogfood). After `just test e2e` or `just
# test chaos` it goes to dev: those modes are entered only by their own recipes,
# which open and close what they need (e2e's public Forgejo tunnel, chaos's
# toxiproxy). No image is built for the server.
[doc('Rebuild the server binary and roll the server and Depot Pods onto it')]
reload:
    #!/usr/bin/env bash
    set -euo pipefail
    mode="$(cat deploy/kind/.state/mode 2>/dev/null || echo dev)"
    case "$mode" in
      dev|dogfood) ;;
      *) echo "==> the stack is in $mode mode; just stack reload goes to dev (just test e2e / just test chaos re-enter $mode)"; mode=dev ;;
    esac
    bash deploy/kind/up.sh "$mode"

# `just stack logs` follows the server's log (the converged control plane in the
# cluster). The Depot is the StatefulSet `scarab-workspace`, the same binary
# with SCARAB_ROLE=workspace, so its output never appears there: `just stack
# logs depot` follows it. A warm-tier warning or a 401 from a Step lands in the
# Depot's log.
[doc("Follow the server's log; `depot` follows the Depot's")]
logs what="server":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{what}}" in
      server) bash deploy/kind/kubectl.sh logs -n scarab -f deploy/scarab ;;
      depot) bash deploy/kind/kubectl.sh logs -n scarab -f statefulset/scarab-workspace ;;
      *) echo "usage: just stack logs [depot]" >&2; exit 2 ;;
    esac

# Readiness here means warm writable + cold reachable, not the control plane's
# database check. Read through the API server's Service proxy, so no port is
# opened on the host.
[doc('Readiness and gauges of the Depot')]
status:
    #!/usr/bin/env bash
    set -euo pipefail
    svc=/api/v1/namespaces/scarab/services/scarab-workspace:80/proxy
    echo "== /readyz =="; bash deploy/kind/kubectl.sh get --raw "$svc/readyz" || echo "NOT READY"
    echo; echo "== /metrics =="; bash deploy/kind/kubectl.sh get --raw "$svc/metrics"

# `just stack demo` submits a one-step pipeline to the running server and
# watches it complete. That one step has no `needs:`, so it never crosses a
# Step boundary: it exercises the workspace drain and none of the feed.
#
# `just stack demo chain` submits a CHAINED pipeline (produce → consume), the
# proof of the feed leg. Since ADR-0061 s3-feed the feed is an init container
# that dials the Depot, and this is the cheapest real proof that it works:
# `consume` asserts on the CONTENT it inherited, so a silently empty /workspace
# fails it.
[doc('Submit a one-step pipeline and watch it; `chain` runs produce → consume')]
demo what="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{what}}" in
      "") script=deploy/kind/demo.sh ;;
      chain) script=deploy/kind/demo-chain.sh ;;
      *) echo "usage: just stack demo [chain]" >&2; exit 2 ;;
    esac
    . deploy/kind/lib.sh; scarab_addr   # base: SCARAB_ADDR, normalised
    SCARAB_BASE="${SCARAB_BASE:-$base}" SCARAB_DEMO_IMAGE="${SCARAB_DEMO_IMAGE:-scarab-clone:dev}" bash "$script"
