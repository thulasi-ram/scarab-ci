# 0014. Secrets: envelope-encrypted PG + pluggable providers

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Secrets need storage, scoping, injection, and one thing everyone gets wrong: **fork-PR
exposure** (untrusted PRs from forks must get no secrets unless a maintainer approves — a
repeated real-world breach vector).

## Decision

- **Storage:** envelope-encrypted in Postgres (per-tenant DEK, KEK in KMS/age/sealed), behind
  a pluggable **`SecretProvider`** port so Vault / cloud managers / External-Secrets-Operator
  drop in later. Batteries-included default, no reinventing Vault.
- **Scoping:** `org → repo → environment` with inheritance.
- **Injection:** **tmpfs files** at `/scarab/secrets/` (not env — env leaks via `/proc` and
  child processes).
- **Masking:** best-effort redaction of known secret values from the live + stored log stream.
- **Hard rule:** **fork-PR runs receive no secrets** unless explicitly approved.

## Consequences

- Extensible without lock-in; secure-by-default injection.
- Environment-scoped secrets underpin Environments ([0024](0024-environments.md)).
- Pairs with keyless OIDC federation ([0015](0015-supply-chain-oidc.md)) to minimize stored
  secrets in the first place.

## Alternatives considered

- **Delegate to k8s/external only** — own no crypto, but weaker scoping/UX, k8s-coupled.
- **Minimal (flat repo scope, env injection)** — leaner, but reworks scoping + injection later.
