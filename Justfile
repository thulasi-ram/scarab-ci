# Scarab dev harness. One-command local environment for the slice-1 skeleton.
#
#   just up      # Postgres + MinIO + kind + scarab-server (background)
#   just demo    # submit a pipeline and watch it complete
#   just down    # tear it all down
#
# Requires: docker, kind, kubectl, cargo (and python3 for the demo).

# List recipes by default.
default:
    @just --list

# Run the web UI dev server (Vite on http://localhost:5173, proxies /v1 → server).
# Env (incl. SCARAB_API_URL, which vite refuses to guess) comes from the one dev
# env file — not wired inline here. Point elsewhere by editing that file.
ui:
    #!/usr/bin/env bash
    set -euo pipefail
    npm --prefix ui/scarab-web-ui install
    [ -f deploy/local-proc/.env ] || { echo "==> creating deploy/local-proc/.env from .env.example"; cp deploy/local-proc/.env.example deploy/local-proc/.env; }
    set -a && source deploy/local-proc/.env && set +a
    npm --prefix ui/scarab-web-ui run dev

# Run the web UI against a built-in fixture (no server, no DB) — the fastest way
# to eyeball UI changes. Serves a fixed "acme" org (dashboard/run-detail/env/
# secrets) with the dark theme forced. See ui/scarab-web-ui/src/mock.ts.
ui-mock:
    npm --prefix ui/scarab-web-ui install
    VITE_SCARAB_MOCK=1 npm --prefix ui/scarab-web-ui run dev

# Run the docs site dev server (Astro Starlight; ADR-0040).
docs:
    npm --prefix ui/scarab-docs-ui install
    npm --prefix ui/scarab-docs-ui run dev

# Bring up the full dev stack and start scarab-server against it.
up:
    bash deploy/local-proc/up.sh

# Submit a one-step pipeline to the running server and watch it complete.
demo:
    bash deploy/local-proc/demo.sh

# `just demo` is ONE step with no `needs:`, so it never crosses a Step boundary and
# exercises the workspace drain and none of the feed. Since ADR-0061 s3-feed the
# feed is an init container that dials the workspace service, and this is the
# cheapest real proof that it works — `consume` asserts on the CONTENT it
# inherited, so a silently empty /workspace fails it.

# Submit a CHAINED pipeline (produce → consume) — proves the workspace feed leg.
demo-chain:
    bash deploy/local-proc/demo-chain.sh

# Tear down the whole stack.
down:
    bash deploy/local-proc/down.sh

# Tail the background server log.
logs:
    tail -f deploy/local-proc/server.log

# The workspace service (ADR-0061) is a SECOND process of the same binary, run by
# `just up` with --role workspace, so its output never appears in `just logs`. A
# warm-tier warning ("warm tier full — serving from cold") or a 401 from a Step
# lands in its log, not the server's.

# Tail the workspace service's log (ADR-0061).
workspace-logs:
    tail -f deploy/local-proc/workspace.log

# `warm_used_bytes` is the gauge that matters: LRU eviction is NOT implemented
# yet, so that number against the volume size is the entire budget. Readiness
# here means warm writable + cold reachable — deliberately not the control
# plane's database check.

# Readiness + gauges of the local workspace service (ADR-0061).
workspace-status port="8081":
    #!/usr/bin/env bash
    set -euo pipefail
    echo "== /readyz =="; curl -sf "http://127.0.0.1:{{port}}/readyz" || echo "NOT READY"
    echo; echo "== /metrics =="; curl -sf "http://127.0.0.1:{{port}}/metrics"

# Requires deploy/local-helm/.env and kube context `colima`. Usage:
#   just local-helm             # pull + deploy the latest ghcr `edge`
#   just local-helm sha-abc123  # pull + deploy a specific published SHA
#   just local-helm local       # build server+clone+sidecar+wsfetch locally, then deploy
#
# NFS prerequisite (ADR-0062 stage 2b ONLY — probed 2026-08-02):
#   Nothing here needs an NFS client today, and nothing in stage 2a will: the
#   `LocalPath` delivery hands a co-located Step the Depot's overlay through a
#   `local` PV, no network. Only the NFS-delivered Export (2b) needs a client on
#   each Step node, and the colima VM ships without one:
#       colima ssh -- sudo apt-get update && colima ssh -- sudo apt-get install -y nfs-common
#   Installed once, it survives `colima stop`/`start` and is lost on `colima
#   delete`. Without it kubelet fails with "you might need a /sbin/mount.nfs
#   helper program" and the Pod hangs in ContainerCreating FOREVER — no event
#   worth reading, no timeout. Any PV we create must also pin
#   `mountOptions: [nfsvers=4.2]` (see the kind note in deploy/local-proc/up.sh).

# Deploy the Helm dogfood stack on colima; pulls ghcr by default, `local` builds.
local-helm ref="edge":
    #!/usr/bin/env bash
    set -euo pipefail
    owner=ghcr.io/thulasi-ram
    if [ "{{ref}}" = "local" ]; then
      echo "==> building server + clone + sidecar + wsfetch from the working tree"
      docker build -t scarab-server:dogfood-local -f docker/server/Dockerfile .
      docker build -t scarab-clone:dogfood docker/clone
      docker build -t scarab-results-sidecar:dogfood docker/sidecar
      # The ADR-0061 wsfetch image — TWO jobs since s3-drain: the s3-feed fetcher
      # init container AND the egress helper every workspace Pod carries
      # (`scarab-wsfetch hold` + the in-Pod `drain` the control plane execs).
      # Context is the repo root: it is a bin target of
      # crates/scarab-workspace-client. Under ADR-0062 the Export replaces the
      # in-cluster FEED, but this image does NOT go away — it stays the fetch
      # path for modes without an Export and the drain helper regardless (the
      # old node-driver deletion note, git-bug 0628369, is closed as
      # superseded). Rollout order: this image must land BEFORE a control
      # plane that expects the drain helper — a stale image makes every drain
      # exit 0 with NO record, a prompt Config failure naming the skew (see
      # deploy/helm values comment).
      docker build -t scarab-wsfetch:dogfood -f docker/wsfetch/Dockerfile .
      IMAGE_REPOSITORY=scarab-server \
      SCARAB_CLONE_IMAGE=scarab-clone:dogfood \
      SCARAB_SIDECAR_IMAGE=scarab-results-sidecar:dogfood \
      SCARAB_WSFETCH_IMAGE=scarab-wsfetch:dogfood \
        bash deploy/local-helm/deploy.sh dogfood-local
    else
      echo "==> pulling + deploying published images @ {{ref}} (ghcr, pullPolicy Always)"
      IMAGE_REPOSITORY="$owner/scarab-server" \
      IMAGE_PULL_POLICY=Always \
      SCARAB_CLONE_IMAGE="$owner/scarab-clone:{{ref}}" \
      SCARAB_SIDECAR_IMAGE="$owner/scarab-results-sidecar:{{ref}}" \
      SCARAB_WSFETCH_IMAGE="$owner/scarab-wsfetch:{{ref}}" \
        bash deploy/local-helm/deploy.sh {{ref}}
    fi

# Persistent port-forward to the in-cluster server (UI + API) on colima. A plain
# `kubectl port-forward` binds to ONE Pod and dies when it rolls — and every
# `just local-helm` now rolls the server Pod — so this loops against the Service
# and auto-reconnects. Leave it running; Ctrl-C to stop. Usage: `just local-helm-ui [port]`.
local-helm-ui port="8080":
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$(kubectl config current-context)" = "colima" ] || { echo "refusing: context is not 'colima'." >&2; exit 1; }
    echo "==> http://127.0.0.1:{{port}}  (auto-reconnects across deploys; Ctrl-C to stop)"
    until kubectl port-forward -n scarab svc/scarab {{port}}:80; do
      echo "   port-forward dropped (Pod roll?) — reconnecting…" >&2
      sleep 1
    done

# The PUBLIC demo (deploy/demo-oracle): one Oracle Cloud "Always Free" A1
# instance — arm64, 2 OCPU / 12 GB — running k3s, the control plane, the
# workspace service and every step Pod, reached through a Cloudflare Tunnel.
#
# Run this ON THE BOX, after `sudo bash deploy/demo-oracle/bootstrap.sh` and
# after filling in deploy/demo-oracle/.env. It refuses unless the kube context
# matches KUBE_CONTEXT (k3s writes `default`) — the same "never target the wrong
# cluster" guard `just local-helm` has for colima.
#
#   just demo-oracle             # deploy the latest published ghcr `edge`
#   just demo-oracle sha-<sha>   # deploy a specific published SHA
#
# There is deliberately NO `local` variant, unlike `just local-helm local`. Two
# Ampere cores will not compile this workspace in a time anyone will wait for,
# and doing it on the demo box would starve the running demo of both cores while
# it happened. image.yml builds arm64 on native runners, so every published tag
# already carries an arm64 manifest — pull, never build.

# Deploy the public demo stack on the Oracle box (published images only).
demo-oracle ref="edge":
    #!/usr/bin/env bash
    set -euo pipefail
    owner=ghcr.io/thulasi-ram
    echo "==> deploying published images @ {{ref}} (ghcr, pullPolicy Always)"
    IMAGE_REPOSITORY="$owner/scarab-server" \
    IMAGE_PULL_POLICY=Always \
    SCARAB_CLONE_IMAGE="$owner/scarab-clone:{{ref}}" \
    SCARAB_SIDECAR_IMAGE="$owner/scarab-results-sidecar:{{ref}}" \
    SCARAB_WSFETCH_IMAGE="$owner/scarab-wsfetch:{{ref}}" \
      bash deploy/demo-oracle/deploy.sh {{ref}}

# Run scarab-server in the FOREGROUND against the dev stack (Ctrl-C to stop).
# Useful when iterating; `just up` runs it in the background instead. Needs the
# stack up first (`just up`). Env comes from the one dev env file, not inline.
serve:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f deploy/local-proc/.env ] || { echo "==> creating deploy/local-proc/.env from .env.example"; cp deploy/local-proc/.env.example deploy/local-proc/.env; }
    set -a && source deploy/local-proc/.env && set +a
    export KUBECONFIG=deploy/local-proc/.kubeconfig
    cargo run -p scarab-server -- --role converged --serve

# Run the workspace suite against the compose Postgres (brought up on demand),
# so the PG-backed tests actually run — mirroring the CI `test` job. Uses
# nextest when installed, plain `cargo test` otherwise.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    # Reuse whatever already serves 55432 (a `just up` stack, or an ad-hoc dev
    # container); otherwise bring up just the compose Postgres.
    if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
      echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    # The live-S3 test (crates/scarab-storage-s3/tests/minio.rs) wants the
    # compose MinIO too — same reuse-or-start dance as Postgres above.
    if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
      # A `just up` stack already ran createbuckets; a foreign MinIO with
      # different credentials would wedge it, so do not re-run it here. (A stack
      # from before the scarab-test bucket existed needs one manual
      # `docker compose -f deploy/local-proc/compose.yaml run --rm createbuckets`.)
      echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait minio
      docker compose -f deploy/local-proc/compose.yaml run --rm createbuckets
    fi
    export SCARAB_TEST_S3_ENDPOINT=http://127.0.0.1:9000
    # scarab-test, NOT scarab-logs: tests must never share the `just up` dev stack's
    # bucket (A11) — a suite prune or fixture collision would eat dev-stack state.
    export SCARAB_TEST_S3_BUCKET=scarab-test
    export SCARAB_TEST_S3_ACCESS_KEY=scarab
    export SCARAB_TEST_S3_SECRET_KEY=scarabsecret
    if command -v cargo-nextest >/dev/null 2>&1; then
      cargo nextest run --workspace
    else
      echo "warning: cargo-nextest not installed (https://nexte.st) — falling back to cargo test" >&2
      cargo test --workspace
    fi

# One test (or one filter) against the same on-demand compose Postgres as
# `just test` — the tight loop while iterating on a single PG-backed test.
# Filter is nextest's expression/substring syntax: `just test-one cas_gc`.
test-one FILTER:
    #!/usr/bin/env bash
    set -euo pipefail
    if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
      echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    # The live-S3 test (crates/scarab-storage-s3/tests/minio.rs) wants the
    # compose MinIO too — same reuse-or-start dance as Postgres above.
    if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
      # A `just up` stack already ran createbuckets; a foreign MinIO with
      # different credentials would wedge it, so do not re-run it here. (A stack
      # from before the scarab-test bucket existed needs one manual
      # `docker compose -f deploy/local-proc/compose.yaml run --rm createbuckets`.)
      echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait minio
      docker compose -f deploy/local-proc/compose.yaml run --rm createbuckets
    fi
    export SCARAB_TEST_S3_ENDPOINT=http://127.0.0.1:9000
    # scarab-test, NOT scarab-logs: tests must never share the `just up` dev stack's
    # bucket (A11) — a suite prune or fixture collision would eat dev-stack state.
    export SCARAB_TEST_S3_BUCKET=scarab-test
    export SCARAB_TEST_S3_ACCESS_KEY=scarab
    export SCARAB_TEST_S3_SECRET_KEY=scarabsecret
    if command -v cargo-nextest >/dev/null 2>&1; then
      cargo nextest run --workspace -E 'test(~{{FILTER}})'
    else
      echo "warning: cargo-nextest not installed (https://nexte.st) — falling back to cargo test" >&2
      cargo test --workspace -- {{FILTER}}
    fi

# Coverage run (mirrors the CI `coverage` job): suite under cargo-llvm-cov
# against the compose Postgres AND MinIO, per-crate summary, and REGENERATES
# docs/audits/coverage-baseline.toml — review + commit the baseline deliberately.
# Requires cargo-nextest + cargo-llvm-cov.
#
# The SCARAB_TEST_REQUIRE_* flags are set here and NOT in `just test`, on
# purpose. This recipe writes the floor the CI ratchet then enforces: a run that
# quietly skipped the PG- or S3-backed tier would bake that narrower suite in as
# a permanently lowered baseline, and the loss would look like a real coverage
# level rather than an absent service. A red run is the only safe failure here.
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
      echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    export SCARAB_TEST_REQUIRE_PG=1
    # Same reuse-or-start dance as `just test-one` — see the note there about
    # not re-running createbuckets against a foreign MinIO.
    if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
      echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
    else
      docker compose -f deploy/local-proc/compose.yaml up -d --wait minio
      docker compose -f deploy/local-proc/compose.yaml run --rm createbuckets
    fi
    export SCARAB_TEST_S3_ENDPOINT=http://127.0.0.1:9000
    export SCARAB_TEST_S3_BUCKET=scarab-test
    export SCARAB_TEST_S3_ACCESS_KEY=scarab
    export SCARAB_TEST_S3_SECRET_KEY=scarabsecret
    export SCARAB_TEST_REQUIRE_S3=1
    cargo llvm-cov nextest --workspace \
      --ignore-filename-regex 'crates/scarab-testkit/|/tests/' \
      --lcov --output-path target/lcov.info
    python3 scripts/coverage_ratchet.py target/lcov.info docs/audits/coverage-baseline.toml --write

# Full-stack E2E (test-strategy Phase 2): own the proc-mode stack for the
# scarab-e2e suite — up.sh → nextest → down.sh. The crate is a pure HTTP
# driver gated on SCARAB_E2E=1 (skips loudly in a plain `just test`); the
# crash/resume scenario additionally spawns its OWN server from
# SCARAB_E2E_SERVER_BIN so its SIGKILLs never poison the shared stack.
# Zero auto-retries: a red run is a real bug, not flake to mask.
e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    bash deploy/local-proc/up.sh
    # Tear the stack down even when the suite fails; keep the suite's exit
    # code, and surface the server log first on a red run (down.sh removes it).
    trap 'code=$?; if [ "$code" -ne 0 ]; then echo "==> scarab-server log (suite failed):"; tail -n 200 deploy/local-proc/server.log 2>/dev/null || true; fi; bash deploy/local-proc/down.sh; exit "$code"' EXIT
    cargo build -p scarab-server -p scarab-cli
    # The same env file the stack booted from — SCARAB_ADDR names the server.
    set -a && source deploy/local-proc/.env && set +a
    export SCARAB_E2E=1
    export SCARAB_E2E_URL="http://${SCARAB_ADDR/0.0.0.0/127.0.0.1}"
    export SCARAB_E2E_DATABASE_URL="$SCARAB_DATABASE_URL"
    export SCARAB_E2E_SERVER_BIN="$PWD/target/debug/scarab-server"
    export SCARAB_E2E_CLI_BIN="$PWD/target/debug/scarab"
    export SCARAB_E2E_KUBECONFIG="$PWD/deploy/local-proc/.kubeconfig"
    cargo nextest run -p scarab-e2e --no-fail-fast

# LIVE Forgejo verification (git-bug 3863d5e). Everything in the Forgejo path was
# asserted against unit tests of the shapes we BELIEVED Forgejo uses, and one of
# those guesses (a hook created with no `config.secret`) shipped. This recipe
# answers the remaining guesses with the software itself: it stands up a real
# Forgejo, seeds an admin + token + more repos than fit on one API page, points
# the proc-mode stack at it, and drives the flow the FakeForge acceptance test
# only simulates — add connection → pick-list → bind → hook registered → real
# push → Run.
#
# Needs: docker (Forgejo is pulled from codeberg.org), kind, and ports 3300 +
# 8080 free. CI does NOT run this — both tiers are env-gated and skip loudly.
# `just forgejo-verify keep` leaves both stacks up for poking at afterwards.
forgejo-verify keep="0":
    #!/usr/bin/env bash
    set -euo pipefail
    # 1. The real Forgejo, seeded. Writes .env.generated (the tests' contract)
    #    and .env.scarab (the server overlay).
    bash deploy/local-forgejo/up.sh
    # 2. The proc-mode stack, with the server bound 0.0.0.0 and publishing a URL
    #    the Forgejo CONTAINER can reach — a hook pointed at 127.0.0.1 would be
    #    registered successfully and then never deliver.
    export SCARAB_ENV_EXTRA="$PWD/deploy/local-forgejo/.env.scarab"
    bash deploy/local-proc/up.sh
    if [ "{{keep}}" = "0" ]; then
      trap 'code=$?; if [ "$code" -ne 0 ]; then echo "==> scarab-server log (verification failed):"; tail -n 200 deploy/local-proc/server.log 2>/dev/null || true; fi; bash deploy/local-proc/down.sh; bash deploy/local-forgejo/down.sh; exit "$code"' EXIT
    else
      echo "==> keep=1: both stacks stay up (tear down with 'bash deploy/local-proc/down.sh; bash deploy/local-forgejo/down.sh')"
    fi
    set -a
    source deploy/local-proc/.env
    source deploy/local-forgejo/.env.scarab
    source deploy/local-forgejo/.env.generated
    set +a
    export SCARAB_E2E=1
    export SCARAB_E2E_URL="http://${SCARAB_ADDR/0.0.0.0/127.0.0.1}"
    export SCARAB_E2E_DATABASE_URL="$SCARAB_DATABASE_URL"
    export SCARAB_E2E_KUBECONFIG="$PWD/deploy/local-proc/.kubeconfig"
    # Adapter-level shapes first (pick-list pagination, hook signing, the push
    # payload spelling), then the whole onboarding flow through the server.
    # Captured real payloads land in target/forgejo-capture/.
    cargo nextest run -p scarab-forge-forgejo --test live --no-fail-fast
    cargo nextest run -p scarab-e2e --test forgejo_onboarding --no-fail-fast

# LIVE k8s executor tier (`crates/scarab-executor-k8s/tests/cluster.rs`) — the
# same tier `.github/workflows/kind.yml` runs, against the proc-mode kind cluster.
#
# It exists because that tier needs far more than a cluster: an ADR-0061 workspace
# service reachable BOTH from the host and from a Pod, the fetcher/clone/sidecar
# images inside the node, an in-cluster registry, and a host address a Pod can
# POST to. Nothing local supplied any of it, so seven cases skipped — five of
# which had been passing — while `kind/cluster-tests` reported PASS. Those cases
# now PANIC on a missing var rather than skip, so this recipe is the local half of
# the repair: it is the only supported way to run the tier by hand.
#
# It does NOT tear the stack down: the suite is slow, you will want to poke at the
# cluster afterwards, and `just down` runs `compose down -v` which would take the
# shared dev Postgres with it. Stop with `just down` when you are actually done.
#
# Usage: `just kube-tests` | `just kube-tests workspace_flows` (nextest filter).
kube-tests filter="":
    #!/usr/bin/env bash
    set -euo pipefail
    # 1. The stack. Idempotent, and it re-probes the Pod-reachable workspace URL,
    #    so a stale .env.generated from a torn-down cluster cannot be used.
    bash deploy/local-proc/up.sh
    set -a
    source deploy/local-proc/.env
    source deploy/local-proc/.env.generated
    set +a
    # The ISOLATED kubeconfig, never the ambient context: production EKS contexts
    # sit next to the local one and this suite creates and deletes Pods.
    export KUBECONFIG="$PWD/deploy/local-proc/.kubeconfig"
    ns="${SCARAB_TEST_KUBE_NS:-scarab}"
    cluster=scarab-dev
    # 2. The step images the tier drives. up.sh needs neither (it only builds the
    #    fetcher), so they are built and kind-loaded here.
    for entry in "scarab-clone:kube-tests docker/clone" "scarab-results-sidecar:kube-tests docker/sidecar"; do
      set -- $entry
      echo "==> building $1"
      docker build -q -t "$1" "$2" >/dev/null
      kind load docker-image "$1" --name "$cluster" >/dev/null
    done
    export SCARAB_TEST_CLONE_IMAGE=scarab-clone:kube-tests
    export SCARAB_TEST_SIDECAR_IMAGE=scarab-results-sidecar:kube-tests
    # 3. The plain-HTTP registry the `kind: build` case pushes to and then reads
    #    the tag list of, from inside the cluster (same shape as kind.yml).
    echo "==> ensuring an in-cluster registry in namespace $ns"
    kubectl apply -n "$ns" -f - <<EOF
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: registry
    spec:
      replicas: 1
      selector:
        matchLabels: { app: registry }
      template:
        metadata:
          labels: { app: registry }
        spec:
          containers:
            - name: registry
              image: registry:2
              ports:
                - containerPort: 5000
    ---
    apiVersion: v1
    kind: Service
    metadata:
      name: registry
    spec:
      selector: { app: registry }
      ports:
        - port: 5000
          targetPort: 5000
    EOF
    kubectl rollout status deployment/registry -n "$ns" --timeout=180s
    export SCARAB_TEST_REGISTRY="registry.$ns.svc.cluster.local:5000"
    # 4. The clone cases fetch THIS repo at a SHA that must exist ON THE FORGE, so
    #    the default is origin/main and not HEAD (which may be unpushed). The repo
    #    is private, hence the token — delivered via tmpfs + GIT_ASKPASS (ADR-0045).
    export SCARAB_TEST_CLONE_SHA="${SCARAB_TEST_CLONE_SHA:-$(git rev-parse origin/main)}"
    export SCARAB_TEST_CLONE_TOKEN="${SCARAB_TEST_CLONE_TOKEN:-$(gh auth token 2>/dev/null || true)}"
    export SCARAB_TEST_KUBE=1
    echo "==> running the live tier (ns=$ns, workspace=$SCARAB_TEST_WORKSPACE_URL,"
    echo "    pod-facing=$SCARAB_TEST_WORKSPACE_POD_URL, fetcher=$SCARAB_TEST_WSFETCH_IMAGE)"
    if [ -n "{{filter}}" ]; then
      cargo nextest run -p scarab-executor-k8s --test cluster \
        --run-ignored all --no-fail-fast -E 'test(~{{filter}})'
    else
      cargo nextest run -p scarab-executor-k8s --test cluster \
        --run-ignored all --no-fail-fast
    fi

# UI no-DOM suite (test-strategy Phase 3, base of the UI pyramid): vitest over
# the run-detail derivations — event folding, Takes, attempts, logs, DAG layout.
# No jsdom, no browser, no server; runs in under a second.
ui-test:
    npm --prefix ui/scarab-web-ui ci
    npm --prefix ui/scarab-web-ui test

# UI browser suite (top of the UI pyramid): the 2 Playwright specs against mock
# mode. Playwright boots its own Vite dev server on :4173 — nothing else needed.
# First run downloads Chromium.
ui-e2e:
    npm --prefix ui/scarab-web-ui ci
    npx --prefix ui/scarab-web-ui playwright install chromium
    npm --prefix ui/scarab-web-ui run test:e2e

# Contract gate (mirrors the CI `openapi-drift` job, ADR-0054): regenerate the
# OpenAPI spec and the generated TS client, then fail if either differs from
# what is committed. Full-route coverage is asserted by the test suite
# (every_registered_route_is_in_the_openapi_spec) — this gates DRIFT only.
openapi-drift:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> regenerating openapi.json"
    cargo run -p scarab-server -- --emit-openapi openapi.json
    git diff --exit-code openapi.json || {
      echo "error: openapi.json is stale — regenerate and commit it:" >&2
      echo "  cargo run -p scarab-server -- --emit-openapi openapi.json" >&2
      exit 1
    }
    echo "==> regenerating the UI client"
    npm --prefix ui/scarab-web-ui ci
    npm --prefix ui/scarab-web-ui run gen
    npm --prefix ui/scarab-web-ui run typecheck
    git diff --exit-code ui/scarab-web-ui/src/api/schema.ts || {
      echo "error: the generated TS client is stale — regenerate and commit it:" >&2
      echo "  npm --prefix ui/scarab-web-ui run gen" >&2
      exit 1
    }
    echo "==> no drift: openapi.json and schema.ts match the code"

# Merge gate: are PR <n>'s REQUIRED checks green? Run this before merging.
# Branch protection and rulesets are both unavailable on this repo (private,
# free plan → 403), so this is the enforcement, not a convenience: the required
# set lives in .github/required-checks.txt. Exits 8 while checks still run.
pr-gate n:
    python3 scripts/pr_gate.py {{n}}

# Compile-check everything.
check:
    cargo check --workspace

# Re-assert the substrate facts ADR-0062 rests on (kube context must be `colima`).
#
# ADR-0062 picks its design because of a handful of kernel and PodSecurity
# behaviours — Bidirectional needs privilege, `baseline` already forbids it,
# `restricted` forbids inline `nfs:` but allows a PVC, overlayfs copy-up leaves
# the lower layer intact, a hardlink shares its inode, metacopy does not help.
# Every one of those is a property of a kernel version or an admission plugin, so
# a colima/k3s bump can invalidate the ADR's reasoning WITHOUT ANY CODE CHANGING.
#
# This asserts them and fails loudly, naming the ADR section that would have to
# be rewritten. It is not a demo: it prints no number it does not check.
adr0062-substrate:
    bash scripts/adr0062_substrate.sh

# LIVE proof of ADR-0062 part 3: mount a REAL overlayfs, write/delete/replace/
# rename through it, and assert the change set read out of the upper layer is
# exactly what the Step did (crates/scarab-server/tests/changeset_overlay.rs).
#
# Everything in `changeset.rs`'s unit tests is a claim about what the KERNEL puts
# in an upper directory. This is the only place that claim meets a kernel — so it
# needs Linux, CAP_SYS_ADMIN (to mount, and to read `trusted.overlay.*` at all)
# and an upperdir on a real disk filesystem. It cannot run on darwin and it
# cannot run in CI, and it FAILS LOUDLY rather than skipping when it cannot:
# `SCARAB_TEST_OVERLAY` unset behind `--ignored` is now a panic, because a tier
# that reports PASS having executed nothing is the exact defect 505f313 repaired.
#
# The binary is built as YOU and only executed under sudo — `sudo cargo test`
# would leave root-owned artifacts all over target/.
#
# Usage: `just overlay-tests` | `just overlay-tests /mnt/scratch` (upperdir).
overlay-tests dir="/var/tmp/scarab-overlay":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(uname -s)" != "Linux" ]; then
      echo "REFUSING: this host is $(uname -s). \`overlayfs\` and the \`trusted.overlay.*\` xattr" >&2
      echo "namespace are LINUX KERNEL features — there is nothing here for this tier to prove," >&2
      echo "and a recipe that shrugged and exited 0 would be the failure it exists to prevent." >&2
      echo "Run it on a Linux host: a VM, a kind/colima node, or 'docker run --privileged'." >&2
      exit 2
    fi
    dir="{{dir}}"
    mkdir -p "$dir"
    # An overlay upperdir cannot live on tmpfs, and it cannot be stacked on an
    # overlay rootfs (ADR-0062 measured both). Caught here rather than as an
    # inscrutable `mount: wrong fs type` two minutes into the run.
    fstype=$(stat -f -c %T "$dir")
    case "$fstype" in
      tmpfs|ramfs|overlayfs|overlay)
        echo "REFUSING: $dir is on $fstype, which cannot serve as an overlayfs upperdir." >&2
        echo "Point the recipe at a real ext4/xfs/btrfs directory: just overlay-tests /mnt/scratch" >&2
        exit 2 ;;
    esac
    echo "==> upperdir $dir ($fstype), kernel $(uname -r)"
    bin=$(cargo test -p scarab-server --test changeset_overlay --no-run --message-format=json \
      | python3 -c "import sys, json; a = [m['executable'] for m in map(json.loads, sys.stdin) if m.get('reason') == 'compiler-artifact' and m.get('executable') and m.get('target', {}).get('name') == 'changeset_overlay']; print(a[-1] if a else '')")
    [ -n "$bin" ] || { echo "could not locate the changeset_overlay test binary" >&2; exit 1; }
    run=(env SCARAB_TEST_OVERLAY=1 SCARAB_TEST_OVERLAY_DIR="$dir" "$bin" --ignored --nocapture --test-threads=1)
    if [ "$(id -u)" = 0 ]; then "${run[@]}"; else sudo "${run[@]}"; fi

# The same tier, ACTUALLY EXECUTED, from a darwin laptop (ADR-0062 s2a-1).
#
# `just overlay-tests` refuses on darwin — correctly, and that refusal is exactly
# why the overlay tier had never run ANYWHERE: the claims in `changeset.rs` about
# what a kernel puts in an upper directory were only ever checked by unit tests
# that build the upper themselves. The nearest kernel that can settle them is
# already on this laptop: the colima VM (Ubuntu 24.04, 6.8.0-117-generic, ext4)
# that `just adr0062-substrate` pins. This runs the real test binary there.
#
# Docker, not kubernetes. The tier needs three things — Linux, CAP_SYS_ADMIN, and
# an upperdir on a real disk filesystem — and a `--privileged` container on the
# colima node has all three (verified: mount -t overlay, redirect_dir=on, and
# `trusted.overlay.*` reads all work). A Pod would add a namespace, a PSA
# exemption and a cleanup obligation next to the operator's live dogfood stack
# for no additional proof.
#
# Cargo runs INSIDE the VM because this host has no aarch64-linux cross
# toolchain (no zig, no cross, no musl-cross), and installing one to avoid a
# container is a bigger, less reproducible dependency than the container.
# The VM has 2 vCPU, so the FIRST run is a cold ~40min workspace build; the
# registry and target dir live in named volumes, so a rerun is seconds. The
# container is memory-capped so a rustc that overreaches is killed instead of
# the dogfood stack sharing the VM.
#
# Reset the caches with:
#   docker volume rm scarab-overlay-cargo scarab-overlay-target scarab-overlay-scratch
overlay-tests-colima image="rust:1-bookworm":
    #!/usr/bin/env bash
    set -euo pipefail
    # This recipe runs containers on the machine that also hosts the operator's
    # dogfood stack, and production EKS contexts sit next to the local one. Both
    # handles must point at colima or we stop — even though only docker is used,
    # a kube context that is not colima means this shell is aimed somewhere else.
    dctx=$(docker context show)
    if [ "$dctx" != "colima" ]; then
      echo "REFUSING: docker context is '$dctx', not 'colima'." >&2
      echo "This recipe runs a --privileged container; it will only do that on the local VM." >&2
      exit 2
    fi
    if command -v kubectl >/dev/null 2>&1; then
      kctx=$(kubectl config current-context 2>/dev/null || echo none)
      if [ "$kctx" != "colima" ]; then
        echo "REFUSING: kube context is '$kctx', not 'colima'." >&2
        exit 2
      fi
    fi
    # Named volumes: /var/lib/docker is ext4 on the VM, so /scratch is a legal
    # overlayfs upperdir — the container ROOTFS is not (it is overlayfs itself,
    # and overlay-on-overlay is refused), which is the whole reason for this mount.
    for v in scarab-overlay-cargo scarab-overlay-target scarab-overlay-scratch; do
      docker volume create "$v" >/dev/null
    done
    echo "==> node: $(colima ssh -- uname -r 2>/dev/null || echo '?'), image {{image}}, repo mounted read-only"
    docker run --rm --privileged \
      --memory=2g --memory-swap=2g \
      -v "$PWD:/repo:ro" \
      -v scarab-overlay-cargo:/cargo \
      -v scarab-overlay-target:/target \
      -v scarab-overlay-scratch:/scratch \
      -w /repo \
      -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
      -e CARGO_BUILD_JOBS=2 -e CARGO_INCREMENTAL=0 \
      -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
      -e SCARAB_TEST_OVERLAY=1 -e SCARAB_TEST_OVERLAY_DIR=/scratch/upper \
      {{image}} \
      sh -c 'mkdir -p /scratch/upper && cargo test --locked -p scarab-server \
        --test changeset_overlay -- --ignored --nocapture --test-threads=1'

# Reclaim build-cache disk. Cargo NEVER garbage-collects `target/`: every
# fingerprint change (branch switch, dep bump, feature flag) writes a new
# artifact and keeps the old one forever. This checkout reached 100 GB — 610k
# files in target/debug/deps, 88k of them stale `scarab_server-*` variants —
# purely from two weeks of branch-hopping.
#
# `just sweep` prunes artifacts untouched for <days> (default 7) across this
# checkout AND any git worktrees, which is what keeps it low WITHOUT the full
# rebuild that `cargo clean` costs. Run it when disk gets tight, or on a cron.
#
# Needs cargo-sweep: cargo install cargo-sweep
sweep days="7":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-sweep >/dev/null 2>&1; then
      echo "error: cargo-sweep not installed — run: cargo install cargo-sweep" >&2
      exit 1
    fi
    echo "==> before: $(du -sh target 2>/dev/null | cut -f1) in $PWD/target"
    # --recursive so sibling worktrees under .workspaces/ and .claude/worktrees/
    # are swept too; they are the reason the total multiplies.
    cargo sweep --recursive --time {{days}} .
    echo "==> after:  $(du -sh target 2>/dev/null | cut -f1)"

# What is actually eating the disk? Prints the build-cache breakdown that the
# `sweep` recipe exists to fix, newest-worktree-first.
disk:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "== free =="; df -h /System/Volumes/Data 2>/dev/null | tail -1 || df -h .
    echo "== target/ in this checkout =="
    du -sh target 2>/dev/null || echo "  (none)"
    du -sh target/debug/deps target/debug/incremental 2>/dev/null || true
    echo "== target/ in git worktrees =="
    git worktree list --porcelain | awk '/^worktree /{print $2}' | while read -r w; do
      [ -d "$w/target" ] && du -sh "$w/target" 2>/dev/null || true
    done
