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
