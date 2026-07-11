# 0010. Forge integration: agnostic core, OIDC identity, in-repo config

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

"Forge-centric" can mean deep (forge owns identity + authz) or shallow (forge is a
trigger/notify integration). Deep deletes an identity subsystem but couples authz to a
vendor's rate-limited, laggy permission API. The requirement was clarified: **IAM must stay
forge-agnostic**, and the core self-contained — without rebuilding a heavyweight parallel IAM
(which would itself be baggage).

## Decision

A **forge-agnostic core** with a clean `Forge` **port** (adapter per vendor):

- **Identity via OAuth/OIDC** — never build password auth; not locked to any one forge.
- **Scarab-native RBAC**, defined in our own terms, *seedable* from a forge's repo/team perms
  but never *dependent* on any forge's semantics. Local-only concepts the forge can't express
  (environment approvers, cross-repo access, bots/service accounts) live here.
- **Config is in-repo canonical (GitOps):** `.scarab/` read at the triggering ref → compiled
  to IR → **stored immutably with the Run** (reproducible, PR-reviewable). UI/API pipelines are
  a secondary path for repos that can't add files.
- **Webhooks normalize into a canonical `Event`**; rich status/checks/deployment/PR feedback
  flows back through the port.
- **GitHub is the first adapter** (reach, GHA-refugee resonance); its App-token/Checks-API
  fiddliness hides behind the port.

## Consequences

- Adding GitLab/Forgejo = a new adapter crate, not core change.
- No parallel identity store; RBAC is ours and portable.
- Webhook signature verification + redelivery idempotency live in the adapter.

## Alternatives considered

- **Deep (forge as control plane)** — deletes IAM but brittle real-time perm derivation +
  vendor lock.
- **Fully independent IAM from scratch** — the baggage we're shedding.
