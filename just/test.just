# `just test [<recipe>]`: the test tiers. Recipes run from the repository root.
set working-directory := '..'

# `just test` with no recipe: the workspace suite (`just test rust`), then the
# UI no-DOM suite (vitest). Stops at the first red suite.
[default]
[doc('The Rust suite, then the UI no-DOM vitest suite')]
default: rust _vitest

# The workspace suite against the compose Postgres and MinIO (brought up on
# demand by deploy/test-services/env.sh), so the PG- and S3-backed tests run,
# mirroring the CI `test-and-coverage` job. Uses nextest when installed, plain
# `cargo test` otherwise.
#
# With a filter it is the tight loop for one test or a group:
# `just test rust cas_gc`. The filter is a nextest substring match on the test
# name (`-E 'test(~<filter>)'`), or cargo test's name filter without nextest.
[doc('The Rust workspace suite (compose Postgres + MinIO); optional nextest filter')]
rust filter="":
    #!/usr/bin/env bash
    set -euo pipefail
    . deploy/test-services/env.sh
    filter="{{filter}}"
    if command -v cargo-nextest >/dev/null 2>&1; then
      cargo nextest run --workspace ${filter:+-E "test(~$filter)"}
    else
      echo "warning: cargo-nextest not installed (https://nexte.st) — falling back to cargo test" >&2
      cargo test --workspace ${filter:+-- "$filter"}
    fi

# UI no-DOM suite (test-strategy Phase 3, base of the UI pyramid): vitest over
# the run-detail derivations — event folding, Takes, attempts, logs, DAG layout.
# No jsdom, no browser, no server; runs in under a second.
[private]
_vitest:
    npm --prefix ui/scarab-web-ui ci
    npm --prefix ui/scarab-web-ui test

# Both UI tiers: the no-DOM vitest suite above, then the browser suite (top of
# the UI pyramid), the 2 Playwright specs against mock mode. Playwright boots
# its own Vite dev server on :4173, so nothing else is needed. The first run
# downloads Chromium.
[doc('UI vitest suite, then the Playwright browser specs against mock mode')]
ui: _vitest
    npx --prefix ui/scarab-web-ui playwright install chromium
    npm --prefix ui/scarab-web-ui run test:e2e

# Every live suite (test-strategy Phase 2) against the kind release in its e2e
# mode: own database `scarab_e2e` and bucket `scarab-e2e`, plugins on, and a
# cloudflared quick tunnel in front of the in-cluster Forgejo (network calls;
# Forgejo is public while it runs, behind a random admin password and
# sign-in-to-view).
# Passes: the scarab-e2e crate, the Forgejo adapter against the real Forgejo,
# Forgejo onboarding, the forge_token cases through the tunnel, the executor's
# and the plugin runtime's live cluster suites, the Argo CD plugin against a
# fake Argo CD (deploy/kind/plugin-verify.sh), and crash_resume last (it
# SIGKILLs the server from the kind node). See deploy/kind/e2e.sh.
# CI runs this recipe (.github/workflows/e2e.yml). Zero auto-retries: a red run
# is a real bug, not flake to mask. Every pass runs; the exit code is non-zero
# when any failed. The release stays in e2e mode afterwards; `just stack up`
# switches it back.
# Usage: `just test e2e` | `just test e2e crash_resume` (a nextest filter,
# applied to every nextest pass; the Argo CD pass runs only when the filter is
# part of its name, `plugin-argocd`).
[doc('Every live suite against the kind release in e2e mode (CI runs this too)')]
e2e filter="":
    bash deploy/kind/e2e.sh "{{filter}}"

# ADR-0077 rank 1 on the kind release: switch it to chaos mode (database
# `scarab_chaos` and bucket `scarab-chaos` through toxiproxy, 2 servers, 2
# Depot replicas), run three chained Runs, delete one Depot Pod with grace 0
# mid-drain and then mid-feed, and check the invariants (crates/scarab-chaos).
# Switches back to the mode that was active before (dev or dogfood; else dev)
# and runs `just stack demo` there. Scenario files land in deploy/kind/.chaos
# (or $SCARAB_CHAOS_DIR). Exit 1 means a verdict failed, 3 that a scenario was
# inconclusive (its fault did not land in its window). See deploy/kind/chaos.sh.
# Usage: `just test chaos` | `just test chaos mid-feed 7` |
# `just test chaos replay <scenario.json>`.
[doc('Fault tier: delete a Depot Pod mid-drain / mid-feed, check the invariants')]
chaos what="all" seed="1":
    bash deploy/kind/chaos.sh "{{what}}" "{{seed}}"

# Coverage run (mirrors the CI `test-and-coverage` job): the suite under
# cargo-llvm-cov against the compose Postgres AND MinIO, per-crate summary, and
# REGENERATES docs/audits/coverage-baseline.toml — review + commit the baseline
# deliberately. Requires cargo-nextest + cargo-llvm-cov.
#
# The SCARAB_TEST_REQUIRE_* flags are set here and NOT in `just test`, on
# purpose. This recipe writes the floor the CI ratchet then enforces: a run that
# quietly skipped the PG- or S3-backed tier would bake that narrower suite in as
# a permanently lowered baseline, and the loss would look like a real coverage
# level rather than an absent service. A red run is the only safe failure here.
[doc('The suite under cargo-llvm-cov; regenerates the coverage baseline')]
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    . deploy/test-services/env.sh
    export SCARAB_TEST_REQUIRE_PG=1
    export SCARAB_TEST_REQUIRE_S3=1
    cargo llvm-cov nextest --workspace \
      --ignore-filename-regex 'crates/scarab-testkit/|/tests/' \
      --lcov --output-path target/lcov.info
    python3 scripts/coverage_ratchet.py target/lcov.info docs/audits/coverage-baseline.toml --write
