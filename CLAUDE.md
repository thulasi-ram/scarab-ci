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
colima safety guards; a bare command skips those and drifts from what CI does.

| Task | Recipe | What it is |
|------|--------|-----------|
| Compile / test the workspace | `just check`, `just test` | `cargo check` / nextest against the compose Postgres (PG tests run for real) |
| UI / server from source | `just ui`, `just serve` | dev loop against the proc stack; env from `deploy/local-proc/.env` |
| **Full local stack** (proc mode) | `just up` → `just demo` → `just down` | Postgres+MinIO (compose) + kind + `scarab-server` as a host process; `just logs` tails it |
| **Helm dogfood** (helm mode) | `just local-helm` | in-cluster on colima via the real chart + published image; `just local-helm local` builds from the tree, `just local-helm sha-<sha>` pins a build |
| **Live Forgejo verification** | `just forgejo-verify` | a REAL Forgejo container + the proc stack; drives add-connection → bind → hook → push → Run. Env-gated (`SCARAB_TEST_FORGEJO`), never in CI |

The deployment modes live under `deploy/`: `local-proc/` (server = host
process, kind for steps) and `local-helm/` (server = Helm-deployed image on
colima), plus `local-forgejo/` — not a way to run Scarab but the throwaway
Forgejo the verification tier drives. Image build contexts live under `docker/`
(`server/`, `clone/`, `sidecar/`). `deploy/helm/` is the distribution chart.

If a recipe is missing for something you need to test, **add/extend a recipe**
rather than running the raw commands ad hoc — that keeps the next run
reproducible. `just local-helm` needs `deploy/local-helm/.env` and kube context
`colima`; see `deploy/local-helm/README.md`.

## Testing philosophy

Classical (not mockist): mock only true externals; keep the suite minimal in v1
and grow tests from real bugs. See `CONTEXT.md` §8 and ADR-0017.

