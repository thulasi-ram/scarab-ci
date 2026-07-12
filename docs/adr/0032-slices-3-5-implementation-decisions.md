# 0032. Slice 3–5 implementation decisions (forge, identity, scheduler, gates, secrets, OIDC, BuildKit)

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)
- **Refines:** [0010](0010-forge-integration.md), [0011](0011-durable-scheduler.md),
  [0014](0014-secrets.md), [0015](0015-supply-chain-oidc.md), [0018](0018-image-building.md),
  [0024](0024-environments.md), [0008](0008-step-contract.md)

## Context

Slices 3–5 are being built by an unattended loop. To avoid mid-run guessing, this ADR fixes the
load-bearing *implementation* choices those slices' ADRs left open. These are defaults consistent
with the accepted ADRs; two (GitHub auth, OIDC subject) were confirmed with the maintainer.

## Decision

### Slice 3 — forge integration + identity (refines 0010)

- **GitHub auth: a GitHub App.** `scarab-forge-github` authenticates as an App: an App JWT
  (RS256, ≤10-min exp) signed with `SCARAB_GITHUB_APP_ID` + `SCARAB_GITHUB_APP_PRIVATE_KEY` (PEM),
  exchanged for a cached, auto-refreshed **installation access token** used for Check Runs,
  statuses, and content reads. (Confirmed with maintainer — the multi-tenant path.)
- **Webhook ingest:** verify `X-Hub-Signature-256` HMAC-SHA256 against
  `SCARAB_GITHUB_WEBHOOK_SECRET`; drop unverified payloads; normalize to the canonical `Event`.
- **In-repo config:** read `.scarab/*.yaml` via the GitHub contents / git-tree API **at the event
  SHA** (no clone).
- **Results back:** post **Check Runs** (status → conclusion) via the installation token, driven
  by the **outbox** on run/step transitions (exactly-once dispatch).
- **Identity is forge-agnostic** (per 0010): login via a GitHub **OAuth App**
  (`SCARAB_GITHUB_OAUTH_CLIENT_ID/SECRET`) mints a **Scarab-native, PG-backed session**
  (server-side session row + httpOnly, secure cookie). RBAC is Scarab-native: roles
  `{Owner, Admin, Member, Viewer}` scoped at org/repo; membership seeded from the forge at login
  but authoritative in Scarab.
- **Testing:** unit-test normalization / signature / JWT→token exchange with the HTTP boundary
  mocked; any live-GitHub test is `#[ignore]`d + gated on env creds.

### Slice 4 — scheduler richness + gates + environments (refines 0011, 0024, 0008)

- **Concurrency group:** key = a CEL-interpolated string from the IR (`concurrency.group`); policy
  `concurrency.policy ∈ {queue, cancel-in-progress}`, default `queue`. Durable slot/queue rows.
- **Auto-cancel superseded:** ON by default for non-deploy pipelines, keyed by (repo, ref,
  pipeline); OFF when the pipeline targets an Environment (deploy) or explicitly opts out.
- **Fairness/backpressure:** per-project max concurrent runs (config, default 20) + a global
  max-in-flight (config, default effectively unbounded); excess waits durably in queue. Integer
  `priority` (higher first), default 0.
- **Cancellation:** mark cancelled → outbox cancel intent → executor SIGTERM + grace → kill →
  durable terminal (no half-cancelled limbo).
- **Gate step kind (built-in):** kinds `manual` (approval by an RBAC role / user list), `timer`
  (wait a duration), `external` (release via API/webhook with a token). Suspends the run
  (`RunStatus::Suspended`); resume via `POST /v1/runs/:id/gates/:step/approve` (authz'd). Survives
  control-plane restarts (durable suspend already exists).
- **Environments:** a `scarab-projects` entity with protection rules `{required_approvers,
  wait_timer, allowed_refs (glob), concurrency_group, secret_scope, oidc_subject_claims}` +
  deployment history; enforced at **admission** before a step targeting the env runs.

### Slice 5 — secrets + OIDC issuer + BuildKit (refines 0014, 0015, 0018)

- **Envelope encryption:** per-secret random 256-bit **data key**; value encrypted with
  **AES-256-GCM** (`aes-gcm`); the data key is wrapped (AES-256-GCM) by a **master key** from
  `SCARAB_MASTER_KEY` (base64, 32 bytes) for dev. Store ciphertext + wrapped key + nonces; never
  plaintext. The master-key provider is pluggable (KMS later).
- **Secret scope:** precedence org < repo < environment (more specific overrides); a step resolves
  the scopes it is authorized for. **Fork-PR runs get no scope.**
- **Injection:** resolved secrets are injected as Pod env and **registered with the log redactor**
  so values never reach stored/streamed logs (0013).
- **OIDC issuer:** **RS256**; signing key from `SCARAB_OIDC_PRIVATE_KEY` (PEM) or generated at
  startup (dev). Serve JWKS at `/.well-known/jwks.json` + an OIDC discovery doc. Token: `iss` =
  configured issuer URL, `aud` configurable per cloud, **`sub = scarab:org/<org>/repo/<repo>/env/
  <env>/ref/<ref>`** (forge-agnostic — confirmed with maintainer), plus claims `{run_id, attempt,
  event, ref, sha}`; short TTL, minted per attempt.
- **Fork-PR lockout:** a fork PR is detected when the event's head repo ≠ base repo; such runs get
  no secret scope, a restricted OIDC subject (`env/none`), and require approval for privileged
  steps.
- **BuildKit:** a built-in `kind: build` step running **rootless buildkitd** in/beside the step
  Pod, invoking `buildctl`; registry push auth via an injected registry secret; the resulting
  image **digest is recorded as an Artifact**; idempotent push via digest/tag fencing (0021). The
  live build is `#[ignore]`-gated.

## Consequences

- The loop cites this ADR instead of re-deciding; env var names above are the config contract.
- More untestable-here surface (GitHub App, BuildKit, cloud OIDC trust) — those paths are
  `#[ignore]`/env-gated and need an attended pass on a machine with the dev harness + creds.
- If any choice proves wrong, supersede it with a focused ADR (0033+), don't edit this one.

## Alternatives considered

- **PAT instead of a GitHub App** — simpler, but single-tenant; rejected for the real path.
- **GitHub-Actions-compatible OIDC subject** (`repo:org/repo:environment:env`) — reuses GHA trust
  tooling, but couples the token to GitHub; rejected for forge-agnosticism (0010).
