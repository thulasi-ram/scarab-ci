# `local-forgejo` — the live Forgejo verification tier

A third `deploy/` mode, alongside `local-proc/` and `local-helm/`. Unlike those
two it is not a way to *run* Scarab: it exists to answer one question that only a
real Forgejo can answer.

## Why it exists (git-bug 3863d5e)

Everything in the Forgejo path was asserted against unit tests of the
request/response shapes we *believed* Forgejo uses. That is thin evidence, and it
already produced a bug that was exactly a wrong guess: `register_webhook` sent no
`config.secret`, so every delivery from a Scarab-registered hook would have been
rejected 401 by our own endpoint — registration reporting success while nothing
ever ran.

Three guesses of the same class stayed unverified:

| Guess | Answered by |
|---|---|
| `/user/repos` response shape + pagination (the bind pick-list depends on it) | seeding **more repos than fit on one page** and asserting the whole set comes back |
| `config.secret` is accepted, and Forgejo then signs with it | a real delivery whose signature `verify_signature` accepts — the same check `/webhooks/forgejo` does |
| the push payload spelling (`owner.username` vs `owner.login`) | normalizing a delivery Forgejo actually sent |

## Running it

```sh
just forgejo-verify          # up → seed → verify → tear both stacks down
just forgejo-verify keep     # …but leave everything running afterwards
```

Prerequisites:

- **docker** running, and able to pull from `codeberg.org` (the Forgejo image).
- **kind** + **kubectl** — the recipe brings up the whole proc-mode stack,
  because the second half of the claim is "…and a Run appears".
- Free ports: **3300** (Forgejo) and **8080** (scarab-server). Change them in
  `deploy/local-forgejo/.env` / `deploy/local-proc/.env`.
- `cargo-nextest`.

Nothing else: the admin user, the access token, the repos and their
`.scarab/ci.yaml` are all seeded by `up.sh`, and `down.sh` deletes the data
volume. Never point this at a Forgejo you care about.

### What the recipe wires up

`up.sh` writes two gitignored files:

- `.env.generated` — the `SCARAB_TEST_FORGEJO_*` contract the tests read
  (instance URL, token, owner, repo names, seeded repo count, hook secret).
- `.env.scarab` — an overlay `deploy/local-proc/up.sh` applies **after** its own
  `.env` (via `SCARAB_ENV_EXTRA`). It binds the server on `0.0.0.0` and sets
  `SCARAB_PUBLIC_URL` to `host.docker.internal`, because a hook registered
  against `127.0.0.1` is a hook the Forgejo *container* can never deliver to. It
  also sets `SCARAB_FORGEJO_WEBHOOK_SECRET` to the same secret the hook carries.

Two Forgejo settings are load-bearing and set in `compose.yaml`:

- `webhook.ALLOWED_HOST_LIST=*` — Forgejo refuses to deliver to private/loopback
  addresses by default. Without this the tier would go green by never exercising
  the ingest path at all.
- `security.INSTALL_LOCK=true` — no install wizard to click through.

## What runs

| Test | What it pins |
|---|---|
| `crates/scarab-forge-forgejo/tests/live.rs::list_accessible_repos_walks_past_the_first_page` | the pick-list is complete across pages, with no duplicates |
| `crates/scarab-forge-forgejo/tests/live.rs::a_registered_hook_delivers_a_signed_push_we_can_normalize` | registration is idempotent against the real hook-list shape; the delivery is signed with the registered secret; the real push payload normalizes |
| `crates/scarab-e2e/tests/forgejo_onboarding.rs::a_real_forgejo_repo_onboards_and_its_push_becomes_a_run` | connection → pick-list → bind → hook → real push → Run, through the real server |

A green run leaves the real payloads in **`target/forgejo-capture/`**
(`push-payload.json`, `push-headers.txt`, `hooks.json`, `user-repos.json`).
Promote one into a committed fixture if you want to pin a shape without a
Forgejo — but only ever from a captured file, never hand-written.

## Gating — CI does not need any of this

Both tiers skip loudly unless their env var is set, mirroring the
`SCARAB_TEST_KUBE` cluster tier:

- `SCARAB_TEST_FORGEJO=1` — the adapter tests (`crates/scarab-forge-forgejo/tests/live.rs`).
- `SCARAB_TEST_FORGEJO=1` **and** `SCARAB_E2E=1` — the onboarding scenario
  (`crates/scarab-e2e/tests/forgejo_onboarding.rs`).

A plain `just test` / `cargo nextest run --workspace` runs them, sees no gate,
prints `SKIPPED (live Forgejo): …`, and passes. `just forgejo-verify` is the only
thing that sets the gate.

## Optional extra: the shared port contract

`crates/scarab-forge-forgejo/tests/contract_live.rs` (the shared `ForgePort`
contract, `#[ignore]`d) reads the same `SCARAB_TEST_FORGEJO_*` env. It is **not**
part of `just forgejo-verify` — it asserts capabilities beyond the three shapes
this tier is about, so a failure there would not mean what this tier claims. With
the stacks up (`just forgejo-verify keep`) it can be run against the same
instance:

```sh
set -a; . deploy/local-forgejo/.env.generated; set +a
cargo nextest run -p scarab-forge-forgejo --test contract_live --run-ignored all
```
