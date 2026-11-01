# CLAUDE.md — working in this repo

Scarab is a durable-execution, k8s-native CI engine in Rust. The dir is
`scrarab-ci` but the product, crates, and CLI are all **`scarab`**. See
`CONTEXT.md` for the domain language and invariants, and `docs/adr/` for
decisions.

## Running & testing locally — use the `just` recipes

**The `Justfile` recipes are the canonical entrypoints for running and testing
Scarab. Prefer them for any test run / dogfood / local verification — do not
hand-roll `docker` / `kind` / `kubectl` / `helm` invocations.** The recipes
encode the correct env, isolated kubeconfig, image source, pull policy, and
kube safety checks; a bare command skips those and drifts from what CI does.
CI runs the same recipes: `kind.yml` runs `just kube-tests`, `e2e.yml` runs
`just e2e`.

| Task | Recipe | What it is |
|------|--------|-----------|
| Compile / test the workspace | `just check`, `just test` | `cargo check` / nextest against the compose Postgres + MinIO in `deploy/test-services/` (PG and S3 tests run for real) |
| UI / server from source | `just ui`, `just reload` | Vite against the kind stack; `just reload` rebuilds the server and rolls its Pods; host address from `deploy/kind/.env` |
| **Full local stack** (kind) | `just up` → `just demo` → `just down` | one kind cluster `scarab-dev`: Postgres + MinIO + the real chart in it; server and Depot run a host-built binary (cargo-zigbuild on macOS) mounted into a thin runtime image; `just reload` rolls a rebuild, `just logs` follows the server. See `deploy/kind/README.md` |
| **Workspace service** (ADR-0061) | `just workspace-logs`, `just workspace-status` | the Depot, StatefulSet `scarab-workspace` in the release, with its own log and its own `/readyz` |
| **Dogfood** | `just dogfood` | `just up` in GitHub App mode with its own database + bucket, behind a named Cloudflare tunnel; reads `~/.config/scarab/dogfood.env` |
| **Full-stack E2E** | `just e2e` | the release in its e2e mode (own database + bucket) driven by `crates/scarab-e2e`; `crash_resume` SIGKILLs the server from the kind node |
| **Live Forgejo verification** | `just forgejo-verify` | a REAL Forgejo container on the kind network + the release in e2e mode; drives add-connection → bind → hook → push → Run. `FORGEJO_TUNNEL=1` adds the forge_token cases. Env-gated (`SCARAB_TEST_FORGEJO`), never in CI |
| **Live k8s executor tier** | `just kube-tests` | `scarab-executor-k8s` + `scarab-plugin-k8s` cluster suites against the release's Depot (e2e mode), in namespace `scarab-kube-tests` |

`just up` runs the **workspace service** (ADR-0061) as the release's
StatefulSet, and Step Pods reach it and the server through Services. The test
recipes (`just e2e`, `just forgejo-verify`, `just kube-tests`) switch the same
release to its **e2e mode** (database `scarab_e2e`, bucket `scarab-e2e`) and
leave it there; `just up` switches back to dev. The tests run on the host:
they reach the API on `SCARAB_ADDR`, Postgres and the Depot through
`kubectl port-forward`, and when a test is itself a server a Pod must call (a
fake forge, a results listener), the host address a Pod uses is **probed from
an actual Pod** (`deploy/kind/test-env.sh`), because the kind docker network
gateway is wrong on darwin.

The deployment modes live under `deploy/`: `kind/` (the dev, dogfood and e2e
stack) and `demo-oracle/` (the public demo), plus `local-forgejo/`, the
throwaway Forgejo the verification tier drives, and `test-services/`, the
compose Postgres + MinIO `just test` uses.
Image build contexts live under `docker/` (`server/`, `runtime/`, `clone/`,
`sidecar/`, `wsfetch/`). `deploy/helm/` is the distribution chart.

If a recipe is missing for something you need to test, **add/extend a recipe**
rather than running the raw commands ad hoc — that keeps the next run
reproducible.

## Writing docs

Any prose (docs site, READMEs, ADRs, CONTEXT.md, UI copy) follows
`docs/WRITING.md`: plain and concrete, no intensifiers, few adjectives, none of
the banned jargon. What we may *claim* is governed by `docs/positioning.md`.

## Testing philosophy

Classical (not mockist): mock only true externals; keep the suite minimal in v1
and grow tests from real bugs. See `CONTEXT.md` §8 and ADR-0017.

