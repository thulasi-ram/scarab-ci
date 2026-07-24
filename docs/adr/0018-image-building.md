# 0018. Container image building: rootless BuildKit

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Image building is the single most common CI task, and on k8s it must happen **without a
privileged Docker socket**. Options: rootless BuildKit (daemonless, efficient cache), Kaniko
(simpler, slower, weaker cache, fading momentum), or "bring your own builder" (punts the #1
task and its rootless/cache footguns onto every user).

## Decision

**Bless a first-class `build` capability backed by rootless BuildKit** — no `docker.sock`, no
privileged pods (privilege via user namespaces). Layer cache to a registry / object store.
Secure-by-default for the most common job.

## Consequences

- Great, safe out-of-the-box build experience; strong DX.
- BuildKit integration + cache plumbing to build/maintain.
- Composes with keyless cosign signing ([0015](0015-supply-chain-oidc.md)).

## Alternatives considered

- **Bring-your-own builder** — zero builder code, but punts the #1 task + security footguns.
- **Kaniko** — simpler/daemonless, but slower, weaker cache, fading.

## Amendment (2026-07-17) — registry authentication

> **Status note (2026-07-24 sweep):** the gap below is closed — `build_pod`
> is wired and `ensure_registry_secret` implements both the explicit-secret
> and forge-derived auth paths in scarab-executor-k8s. The decision text
> stands as the record of *how* it was designed.

At the time of writing, `build_pod_for_build` existed but was unwired, with
**no registry auth** (no push/pull credentials). Decision:

- **Registry credentials are a generic scoped secret** (ADR-0037) — a
  `dockerconfigjson`-shaped secret in the Project/Environment scope, injected
  into BuildKit's auth path for both push and private-`FROM` pull. No new
  first-class `registry:` concept (that would reinvent secret-scoping); reuses
  scope inheritance, fork-PR lockout, and log redaction as-is.
- **Zero-config convenience:** the forge adapter may *derive* a registry
  credential for pushing to the forge's **own** registry (GHCR via the GitHub
  installation token; the Forgejo package registry via its token) from the
  `ForgeConnection` (ADR-0046) — the common "push to my forge" case with no
  secret to configure.
- The push stays fenced via the existing `push:{image}@{digest}` idempotency key
  (ADR-0021).
