# Scarab dev harness. One-command local environment.
#
#   just up      # kind + Postgres + MinIO + Forgejo + the chart, server built on the host
#   just demo    # submit a pipeline and watch it complete
#   just down    # tear it all down
#
# Requires: docker, kind, kubectl, helm, cargo (+ cargo-zigbuild and zig on
# macOS), and python3 for the demo.

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
    [ -f deploy/kind/.env ] || { echo "==> creating deploy/kind/.env from .env.example"; cp deploy/kind/.env.example deploy/kind/.env; }
    set -a && source deploy/kind/.env && set +a
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

# Build the docs site once, the way docs.yml does (npm ci + astro build into
# ui/scarab-docs-ui/dist). The prebuild step syncs CONTEXT.md and docs/adr.
docs-build:
    npm --prefix ui/scarab-docs-ui ci
    npm --prefix ui/scarab-docs-ui run build

# Regenerate the halftone mark's SVGs in ui/brand (python3, standard library only).
brand:
    python3 ui/brand/halftone/generate.py

# Fail if the halftone SVGs on disk differ from what the generator produces.
brand-check:
    python3 ui/brand/halftone/generate.py --check

# Opens a browser tab, which draws each SVG at every size; serve_png.py saves the
# 37 PNGs under ui/brand and exits when the page reports it is done.
# Re-render the halftone PNG icons in a browser, then rebuild favicon.ico.
brand-png:
    python3 ui/brand/halftone/serve_png.py
    python3 ui/brand/halftone/generate.py --ico

# The kind dev stack (ADR-0077 Amendment, deploy/kind/README.md): one kind
# cluster `scarab-dev`, Postgres + MinIO in it, and the real chart with the
# server and the Depot running a binary built on the host (cargo-zigbuild on
# macOS), mounted into a thin runtime image. The API and UI answer on the host
# at SCARAB_ADDR from deploy/kind/.env, so `just ui` works against it.
# Kube calls use deploy/kind/.kubeconfig only, checked before every call.
#
# `just e2e` runs on the same release, switched to its e2e mode (own database
# and bucket).
#
# `just up dogfood` is the same stack in GitHub App mode with its own database
# and bucket, plus a named Cloudflare tunnel. It reads
# ~/.config/scarab/dogfood.env (CLOUDFLARE_TUNNEL_TOKEN and DOGFOOD_HOSTNAME are
# required; values are never printed). The tunnel's public hostname must route
# ONLY path ^/webhooks/ to http://scarab.scarab.svc.cluster.local:80 (set in
# the Cloudflare dashboard); the release is dev-insecure, so up.sh checks from
# outside and takes the tunnel down when / or /v1/repos reaches Scarab. The UI
# stays on SCARAB_ADDR.

# Bring up the kind stack: `just up` (dev) or `just up dogfood`.
up mode="dev":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{mode}}" in dev|dogfood) ;; *) echo "usage: just up [dev|dogfood] (e2e mode is \`just e2e\`)" >&2; exit 2 ;; esac
    bash deploy/kind/up.sh "{{mode}}"

# Rebuild the binary and roll the server and Depot Pods onto it, in the mode the
# stack was last brought up in (dev or dogfood). After `just e2e` or `just
# chaos` it goes to dev: those modes are entered only by their own recipes,
# which open and close what they need (e2e's public Forgejo tunnel, chaos's
# toxiproxy). No image is built for the server.
reload:
    #!/usr/bin/env bash
    set -euo pipefail
    mode="$(cat deploy/kind/.state/mode 2>/dev/null || echo dev)"
    case "$mode" in
      dev|dogfood) ;;
      *) echo "==> the stack is in $mode mode; just reload goes to dev (just e2e / just chaos re-enter $mode)"; mode=dev ;;
    esac
    bash deploy/kind/up.sh "$mode"

# Submit a one-step pipeline to the running server and watch it complete.
demo:
    #!/usr/bin/env bash
    set -euo pipefail
    . deploy/kind/lib.sh; scarab_addr   # base: SCARAB_ADDR, normalised
    SCARAB_BASE="${SCARAB_BASE:-$base}"     SCARAB_DEMO_IMAGE="${SCARAB_DEMO_IMAGE:-scarab-clone:dev}"       bash deploy/kind/demo.sh

# `just demo` is ONE step with no `needs:`, so it never crosses a Step boundary and
# exercises the workspace drain and none of the feed. Since ADR-0061 s3-feed the
# feed is an init container that dials the workspace service, and this is the
# cheapest real proof that it works — `consume` asserts on the CONTENT it
# inherited, so a silently empty /workspace fails it.

# Submit a CHAINED pipeline (produce → consume) — proves the workspace feed leg.
demo-chain:
    #!/usr/bin/env bash
    set -euo pipefail
    . deploy/kind/lib.sh; scarab_addr   # base: SCARAB_ADDR, normalised
    SCARAB_BASE="${SCARAB_BASE:-$base}"     SCARAB_DEMO_IMAGE="${SCARAB_DEMO_IMAGE:-scarab-clone:dev}"       bash deploy/kind/demo-chain.sh

# Tear down the kind dev stack. `just down keep` uninstalls the release only and
# keeps the cluster, Postgres, MinIO and their data.
down what="all":
    bash deploy/kind/down.sh {{what}}

# Follow the server's log (the converged control plane in the cluster).
logs:
    bash deploy/kind/kubectl.sh logs -n scarab -f deploy/scarab

# The Depot (ADR-0061 workspace service) is the StatefulSet `scarab-workspace`,
# the same binary with SCARAB_ROLE=workspace, so its output never appears in
# `just logs`. A warm-tier warning or a 401 from a Step lands in its log.

# Follow the Depot's log.
workspace-logs:
    bash deploy/kind/kubectl.sh logs -n scarab -f statefulset/scarab-workspace

# Readiness here means warm writable + cold reachable, not the control plane's
# database check. Read through the API server's Service proxy, so no port is
# opened on the host.

# Readiness and gauges of the Depot.
workspace-status:
    #!/usr/bin/env bash
    set -euo pipefail
    svc=/api/v1/namespaces/scarab/services/scarab-workspace:80/proxy
    echo "== /readyz =="; bash deploy/kind/kubectl.sh get --raw "$svc/readyz" || echo "NOT READY"
    echo; echo "== /metrics =="; bash deploy/kind/kubectl.sh get --raw "$svc/metrics"

# The PUBLIC demo (deploy/demo-oracle): one Oracle Cloud "Always Free" A1
# instance — arm64, 2 OCPU / 12 GB — running k3s, the control plane, the
# workspace service and every step Pod, reached through a Cloudflare Tunnel.
#
# Run this ON THE BOX, after `sudo bash deploy/demo-oracle/bootstrap.sh` and
# after filling in deploy/demo-oracle/.env. It refuses unless the kube context
# matches KUBE_CONTEXT (k3s writes `default`), so it never targets the wrong
# cluster.
#
#   just demo-oracle             # deploy the latest published ghcr `edge`
#   just demo-oracle sha-<sha>   # deploy a specific published SHA
#
# There is no `local` variant that builds on the box. Two
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

# Run the workspace suite against the compose Postgres (brought up on demand),
# so the PG-backed tests actually run — mirroring the CI `test` job. Uses
# nextest when installed, plain `cargo test` otherwise.
test:
    #!/usr/bin/env bash
    set -euo pipefail
    # Reuse whatever already serves 55432 (an earlier run's container, or an
    # ad-hoc dev one); otherwise bring up just the compose Postgres.
    if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
      echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
    else
      docker compose -f deploy/test-services/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    # The live-S3 test (crates/scarab-storage-s3/tests/minio.rs) wants the
    # compose MinIO too — same reuse-or-start dance as Postgres above.
    if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
      # An earlier run already ran createbuckets; a foreign MinIO with
      # different credentials would wedge it, so do not re-run it here. (A
      # MinIO from before the scarab-test bucket existed needs one manual
      # `docker compose -f deploy/test-services/compose.yaml run --rm createbuckets`.)
      echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
    else
      docker compose -f deploy/test-services/compose.yaml up -d --wait minio
      docker compose -f deploy/test-services/compose.yaml run --rm createbuckets
    fi
    export SCARAB_TEST_S3_ENDPOINT=http://127.0.0.1:9000
    # scarab-test: the suite's own bucket (A11), so a suite prune or fixture
    # collision cannot eat another stack's state.
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
      docker compose -f deploy/test-services/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    # The live-S3 test (crates/scarab-storage-s3/tests/minio.rs) wants the
    # compose MinIO too — same reuse-or-start dance as Postgres above.
    if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
      # An earlier run already ran createbuckets; a foreign MinIO with
      # different credentials would wedge it, so do not re-run it here. (A
      # MinIO from before the scarab-test bucket existed needs one manual
      # `docker compose -f deploy/test-services/compose.yaml run --rm createbuckets`.)
      echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
    else
      docker compose -f deploy/test-services/compose.yaml up -d --wait minio
      docker compose -f deploy/test-services/compose.yaml run --rm createbuckets
    fi
    export SCARAB_TEST_S3_ENDPOINT=http://127.0.0.1:9000
    # scarab-test: the suite's own bucket (A11), so a suite prune or fixture
    # collision cannot eat another stack's state.
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
      docker compose -f deploy/test-services/compose.yaml up -d --wait postgres
    fi
    export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
    export SCARAB_TEST_REQUIRE_PG=1
    # Same reuse-or-start dance as `just test-one` — see the note there about
    # not re-running createbuckets against a foreign MinIO.
    if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
      echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
    else
      docker compose -f deploy/test-services/compose.yaml up -d --wait minio
      docker compose -f deploy/test-services/compose.yaml run --rm createbuckets
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

# Every live suite (test-strategy Phase 2) against the kind release in its e2e
# mode: own database `scarab_e2e` and bucket `scarab-e2e`, and a cloudflared
# quick tunnel in front of the in-cluster Forgejo (network calls; Forgejo is
# public while it runs, behind a random admin password and sign-in-to-view).
# Passes: the scarab-e2e crate, the Forgejo adapter against the real Forgejo,
# Forgejo onboarding, the forge_token cases through the tunnel, the executor's
# and the plugin runtime's live cluster suites, and crash_resume last (it
# SIGKILLs the server from the kind node). See deploy/kind/e2e.sh.
# CI runs this recipe (.github/workflows/e2e.yml). Zero auto-retries: a red run
# is a real bug, not flake to mask. The release stays in e2e mode afterwards;
# `just up` switches it back.
# Usage: `just e2e` | `just e2e crash_resume` (a nextest filter, applied to
# every pass).

# Run every live suite against the kind release in e2e mode (CI runs this too).
e2e filter="":
    bash deploy/kind/e2e.sh "{{filter}}"

# ADR-0077 rank 1 on the kind release: switch it to chaos mode (database
# `scarab_chaos` and bucket `scarab-chaos` through toxiproxy, 2 servers, 2
# Depot replicas), run three chained Runs, delete one Depot Pod with grace 0
# mid-drain and then mid-feed, and check the invariants (crates/scarab-chaos).
# Switches back to the mode that was active before (dev or dogfood; else dev)
# and runs `just demo` there. Scenario files land in deploy/kind/.chaos (or
# $SCARAB_CHAOS_DIR). Exit 1 means a verdict failed, 3 that a scenario was
# inconclusive (its fault did not land in its window). See deploy/kind/chaos.sh.
# Usage: `just chaos` | `just chaos mid-feed 7` | `just chaos replay <scenario.json>`.
chaos what="all" seed="1":
    bash deploy/kind/chaos.sh "{{what}}" "{{seed}}"

# LIVE plugin-runtime tier alone (`crates/scarab-plugin-k8s/tests/cluster.rs`,
# ADR-0072 §9/§11): the isolation probe passes on kind and FAILS once the egress
# policies are gone; the namespace baseline refuses a non-`restricted` namespace
# and a stray policy; Pod Security admits the plugin workload; TokenReview accepts
# a live Pod's broker token and nothing else. Self-contained — throwaway
# namespaces and an in-cluster fake broker — so it needs only the kind cluster
# (`just up`), through its own kubeconfig (the suite also refuses any context
# that is not `kind-*`). Usage: `just plugin-kube-tests` | `just plugin-kube-tests token_review`.
plugin-kube-tests filter="":
    #!/usr/bin/env bash
    set -euo pipefail
    . deploy/kind/lib.sh
    verify_kubeconfig
    export KUBECONFIG="$kubeconfig"
    export SCARAB_TEST_KUBE=1
    if [ -n "{{filter}}" ]; then
      cargo nextest run -p scarab-plugin-k8s --test cluster \
        --run-ignored all --no-fail-fast -E 'test(~{{filter}})'
    else
      cargo nextest run -p scarab-plugin-k8s --test cluster \
        --run-ignored all --no-fail-fast
    fi

# LIVE first-party Argo CD Plugin (ADR-0072) on the kind release: builds the
# plugin image and kind-loads it, brings the release up in e2e mode with plugins
# on and the dev-only tag knob, runs a fake Argo CD on the host (the broker in
# the server Pod reaches it at the probed host address), installs the plugin
# through the API and binds its connection; then asserts the isolation probe
# passed, the Pod read Argo CD through the broker's read proxy with the bound
# credential, and that the kill switch makes the broker answer 403 and scales
# the workload to 0. Every cluster call goes through deploy/kind/lib.sh's kc.
# Leaves the release in e2e mode (`just up` switches back).
# Usage: `just plugin-verify` | `KEEP=1 just plugin-verify` (keep the fake Argo).
plugin-verify:
    bash deploy/kind/plugin-verify.sh

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
# (step_up_routes.rs: route_table_equals_the_openapi_spec) — this gates DRIFT only.
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
# Fails while the PR has the ci:skip label (README: "CI labels on pull requests").
pr-gate n:
    python3 scripts/pr_gate.py {{n}}

# Chart guard (mirrors the CI `chart` job step): render the chart at two
# versions and fail if any immutable StatefulSet spec field differs — such a
# field makes every chart upgrade a rejected update. Needs helm + yq v4.
chart-immutable:
    scripts/chart_statefulset_immutable.sh deploy/helm/scarab

# The CEL policy checker (ADR-0075) against the cel-spec conformance cases inside
# its subset. cel-rust's conformance/tests/gen embeds cel-spec's cases, so this
# needs a cel-rust checkout and no network:
#   git clone https://github.com/cel-rust/cel-rust && just policy-conformance cel-rust
policy-conformance cel_rust:
    #!/usr/bin/env bash
    set -euo pipefail
    out="${CARGO_TARGET_DIR:-target}/cel-conformance-cases.jsonl"
    # cargo runs the test from the crate directory: pass an absolute path.
    case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
    mkdir -p "$(dirname "$out")"
    python3 -I crates/scarab-policy/tests/conformance/extract.py "{{cel_rust}}/conformance/tests/gen" > "$out"
    SCARAB_CEL_CONFORMANCE_CASES="$out" cargo test -p scarab-policy --test conformance -- --ignored --nocapture

# Compile-check everything.
check:
    cargo check --workspace

# Re-assert the platform facts ADR-0062 rests on, on the kind cluster `just up`
# creates (through deploy/kind/.kubeconfig, like every other kind recipe).
#
# ADR-0062 picks its design because of a handful of kernel and PodSecurity
# behaviours — Bidirectional needs privilege, `baseline` already forbids it,
# `restricted` forbids inline `nfs:` but allows a PVC, overlayfs copy-up leaves
# the lower layer intact, a hardlink shares its inode, metacopy does not help.
# Every one of those is a property of a kernel version or an admission plugin, so
# a kernel or Kubernetes bump can invalidate the ADR's reasoning WITHOUT ANY CODE
# CHANGING. The facts were first measured on colima's k3s (retired); the kernel
# under kind on a Mac is still colima's VM.
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
    # This recipe runs a --privileged container on the machine that also hosts
    # the kind dev stack. Only docker is used, so the docker context is the one
    # guard: it must be the local colima VM.
    dctx=$(docker context show)
    if [ "$dctx" != "colima" ]; then
      echo "REFUSING: docker context is '$dctx', not 'colima'." >&2
      echo "This recipe runs a --privileged container; it will only do that on the local VM." >&2
      exit 2
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
