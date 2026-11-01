# `local-forgejo` — the live Forgejo verification tier

Not a way to *run* Scarab (that is `deploy/kind/`): it exists to answer one
question that only a real Forgejo can answer.

## Why it exists (git-bug 3863d5e)

The Forgejo path had only been tested with unit tests against the
request/response shapes we *believed* Forgejo uses. That is weak evidence, and it
had already let through a bug caused by a wrong guess: `register_webhook` sent no
`config.secret`, so every delivery from a Scarab-registered hook would have been
rejected 401 by our own endpoint. Registration reported success while nothing
ever ran.

Three similar guesses were still unverified:

| Guess | Answered by |
|---|---|
| `/user/repos` response shape + pagination (the bind pick-list depends on it) | seeding **more repos than fit on one page** and asserting the whole set comes back |
| `config.secret` is accepted, and Forgejo then signs with it | a real delivery whose signature `verify_signature` accepts — the same check `/webhooks/forgejo` does |
| the push payload spelling (`owner.username` vs `owner.login`) | normalizing a delivery Forgejo sent |

## Running it

```sh
just forgejo-verify          # release (e2e mode) → Forgejo → seed → verify → remove Forgejo
just forgejo-verify keep     # …but leave Forgejo running afterwards
```

Prerequisites:

- **docker** running, and able to pull from `codeberg.org` (the Forgejo image).
- What `just up` needs (deploy/kind/README.md): the recipe brings the kind
  release up in its e2e mode, because the test also checks that a Run appears.
  The release stays in e2e mode afterwards; `just up` switches it back.
- A free port **3300** (Forgejo; change it in `deploy/local-forgejo/.env`).
- `cargo-nextest`.

Nothing else: the admin user, the access token, the repos and their
`.scarab/ci.yaml` are all seeded by `up.sh`, and `down.sh` deletes the data
volume. Never point this at a Forgejo you care about.

### What the recipe wires up

The Forgejo container joins the kind cluster's `kind` docker network as well as
its own, so the two sides reach each other by name: the server Pod calls
`http://scarab-forgejo:3000` (Pods resolve container names through the node's
docker DNS), and Forgejo delivers hooks to the kind node's API node port,
`http://scarab-dev-control-plane:30080`. The tests on the host use the
published port, `http://127.0.0.1:3300`.

`up.sh` writes two gitignored files:

- `.env.generated` — the `SCARAB_TEST_FORGEJO_*` contract the tests read
  (host URL, server URL, token, owner, repo names, seeded repo count, hook
  secret).
- `.values.json` — Helm values the recipe passes to `deploy/kind/up.sh` through
  `SCARAB_KIND_EXTRA_VALUES`: `scarab.publicUrl` (the address bind stamps onto
  the hook it registers, which must be one Forgejo can reach),
  `secrets.forgejoWebhookSecret` (the same secret the hook carries), and in
  tunnel mode `scarab.libsManifest`. `up.sh values` writes only this file; the
  recipe runs it before the release, which must exist before Forgejo can join
  its network.

Two Forgejo settings in `compose.yaml` are required:

- `webhook.ALLOWED_HOST_LIST=*` — Forgejo refuses to deliver to private/loopback
  addresses by default. Without it the tier would pass without ever exercising
  the ingest path.
- `security.INSTALL_LOCK=true` — no install wizard to click through.

## What runs

| Test | What it checks |
|---|---|
| `crates/scarab-forge-forgejo/tests/live.rs::list_accessible_repos_walks_past_the_first_page` | the pick-list is complete across pages, with no duplicates |
| `crates/scarab-forge-forgejo/tests/live.rs::a_registered_hook_delivers_a_signed_push_we_can_normalize` | registration is idempotent against the real hook-list shape; the delivery is signed with the registered secret; the real push payload normalizes |
| `crates/scarab-e2e/tests/forgejo_onboarding.rs::a_real_forgejo_repo_onboards_and_its_push_becomes_a_run` | connection → pick-list → bind → hook → real push → Run, through the real server |

A green run leaves the real payloads in **`target/forgejo-capture/`**
(`push-payload.json`, `push-headers.txt`, `hooks.json`, `user-repos.json`).
To test a shape without a running Forgejo, turn one of these into a committed
fixture. Only ever use a captured file; never write one by hand.

## Gating: CI does not need any of this

Both tiers skip with a visible message unless their env var is set, mirroring the
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
part of `just forgejo-verify`: it checks capabilities beyond the three shapes
this tier covers, so a failure there would not mean one of these three shapes is wrong. With
the stacks up (`just forgejo-verify keep`) it can be run against the same
instance:

```sh
set -a; . deploy/local-forgejo/.env.generated; set +a
cargo nextest run -p scarab-forge-forgejo --test contract_live --run-ignored all
```
