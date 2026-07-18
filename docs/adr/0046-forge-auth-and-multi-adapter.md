# 0046. Forge auth is adapter-internal; GitHub + Forgejo adapters in v1

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0010](0010-forge-integration.md) (forge integration)

## Context

The forge adapter is unbuilt: 8 outbound `GithubForge` methods are
`unimplemented!()` and production wires `forge: None` (2026-07-16 audit). Before
building it, a framing error surfaced: treating "use a GitHub App" as a
*Scarab-level auth decision* leaks a vendor concept into what CONTEXT.md and
ADR-0010 require to be forge-agnostic ("Forge is the domain concept; vendors are
adapters, never their own domain"). Forgejo/Codeberg have no App/installation
concept — they have OAuth2 apps and repo-scoped access tokens.

## Decision

### Auth is adapter-internal; the `ForgePort` expresses domain capabilities

The port never mentions "App", "installation", or "JWT". It expresses
capabilities — *read a file/dir at a ref*, *resolve a ref to a commit*, *post a
status*, *ingest a normalized event*, *mint a scoped checkout credential*. Each
adapter satisfies them however its vendor allows. Auth *config* is
adapter-specific (GitHub: App ID + PEM private key; Forgejo: OAuth2 client/secret
or a bot access token).

### Two adapters in v1 — GitHub **and** Forgejo

Both ship in v1, so the port is validated by construction: Forgejo cannot be
satisfied by anything GitHub-shaped. This yields a shared **`ForgePort`
contract-test suite both adapters must pass** — the mechanism that keeps future
adapters (GitLab, Bitbucket) honest. It also plants a flag GitHub Actions
structurally can't: forge-native CI that is not GitHub-locked.

Per-capability mapping:

| Capability | GitHub | Forgejo / Codeberg |
|---|---|---|
| API auth | App JWT → installation token (~1h, cached) | OAuth2 token or bot access token |
| Scoped checkout credential (ADR-0045 S4) | installation token scoped `contents:read`, one repo | repo-scoped access token / short OAuth token |
| Event ingest | one App webhook, all installations | per-repo/org webhook — `register_webhook` **is real** |
| Status feedback | commit status (+ optional Checks enrichment) | commit status |
| API base URL | `api.github.com` **or GHES host** | self-hosted base URL |

### Status feedback sits at commit-status level (LCD) + required run deep-link

The port contract is `set_status(repo, sha, state, context, description,
target_url)` — the level *both* forges guarantee. `target_url` (deep-link back
to the Scarab run) is **required**, not the current hardcoded `None`
(`crates/scarab-server/src/lib.rs:1659-1661`).

- The GitHub adapter **may** enrich internally to the Checks API — inline PR
  annotations (fed by ADR-0008 structured-emit diagnostics), rich output,
  richer conclusions — without the port knowing. Forgejo ignores what it can't
  render.
- **Follow-up (GitHub-only, not v1 core):** wire Checks *action buttons* and
  `rerequested`/`requested_action` webhooks to Scarab's restart-a-step and
  gate-approve primitives — surfacing the durable-restart wedge inside the PR.

### `register_webhook` is a real capability, not a GitHub no-op

Because Forgejo needs per-repo/org webhook registration (no single-app webhook),
`register_webhook` stays in the port. The GitHub adapter implements it as a
no-op (the App receives all installation events).

### The registry — `ForgeConnection` + `Project`; per-forge webhook endpoints

There is no repo registry today (repos are faked in the UI; the ingest route is
hard-bound to GitHub, `crates/scarab-server/src/lib.rs:1735,1746`). Two forges
make a real one unavoidable — resolving *which forge, which base URL, which
credentials* for a given repo.

- **`ForgeConnection`** (pure type in `scarab-forge`) links Scarab to a forge
  account: `{forge_kind, base_url, credential_ref}` owning a set of `RepoRef`s.
  A GitHub App installation and a Forgejo connection are both instances. Its
  **persistence is a store port + Postgres adapter** (I/O stays out of the pure
  crate); its **credentials live in `SecretProvider`** (App PEM / Forgejo token),
  referenced by handle; the installation-token cache is adapter-internal state.
- **`RepoRef` vs `Project`** (redefines the tenancy model): `RepoRef` is the
  *forge coordinate* (`{owner, name}` + forge) — external, mutable, carried by
  `Event`/`Status`, the only concept named "Repo". A **`Project`** is the
  *governed CI unit* — the aggregate root under an `Org` that binds a `RepoRef`
  to its governance (Environments/`ProtectionRules`/privilege/secret-scope) and
  owns its pipelines/runs. `ForgeConnection` resolves `RepoRef` → `Project`.
  **1 Project : 1 RepoRef** in v1 (monorepo per-subdir governance deferred to an
  optional path scope). The old governed `Repo` struct is **absorbed into
  `Project`**; `Org → Project → Environment`. The `scarab-projects` crate is
  renamed **`scarab-project`**; the vestigial `Project` struct becomes this
  aggregate.
- **Webhook routing:** separate endpoints per forge (`/webhooks/github`,
  `/webhooks/forgejo`), each bound to its adapter and verification secret — no
  payload-sniffing on a shared endpoint.
- **Registration flows** (a good agnosticism test): GitHub auto-registers via
  `installation`/`installation_repositories` webhooks (installing the App *is*
  registration); Forgejo connections are **admin-registered** (base URL +
  credential), repos added and per-repo webhooks created via `register_webhook`.
- **Delivery-id dedup:** a webhook delivery-id store guards replay (absent
  today; the `x-github-delivery` id is read but never checked,
  `crates/scarab-server/src/lib.rs:1741`).

### Adapter-internal (recorded, not port concerns)

Installation-token caching across replicas, pagination, and secondary-rate-limit
backoff live inside each adapter.

## Consequences

- A new agnostic port method for scoped checkout credentials (ADR-0045 S4 is
  this capability, not "installation token").
- Base URL becomes adapter config even for GitHub (GHES).
- Two adapters + a contract-test suite is more v1 work, bought deliberately to
  guarantee forge-agnosticism.

## Alternatives considered

- **GitHub-only adapter, Forgejo mapping on paper:** cheaper, but an unbuilt
  second adapter hides GitHub assumptions the paper exercise misses. Rejected in
  favor of two real adapters.
- **Elevate Checks into the port:** richer on GitHub, but unfulfillable on
  Forgejo — a leaky contract. Rejected; Checks is adapter-internal enrichment.
