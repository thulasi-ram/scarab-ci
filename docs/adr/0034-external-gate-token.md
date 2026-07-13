# 0034. External-gate release: HMAC token

- **Status:** Accepted
- **Date:** 2026-07-13
- **Deciders:** thulasi.ram (architect)

## Context

Gates ([0008](0008-step-contract.md)) come in three kinds: `manual` (a human
approves — RBAC), `timer` (auto-releases after a wait, [0033] neighbourhood), and
`external` (released by an outside system: a deploy webhook, a change-management
tool, a downstream job). `external` needs an authorization path that is **not** a
logged-in user session — the caller is a machine.

## Decision

**An external gate is released by presenting a per-run-per-gate HMAC token** to a
dedicated endpoint:

```
POST /v1/runs/{id}/gates/{step}/release
X-Scarab-Gate-Token: sha256=<hex>
token = HMAC-SHA256(server_secret, "{run}:{step}")
```

- The server verifies by recomputing the HMAC and comparing in **constant time**
  (reusing the webhook-signature verifier). **No per-gate storage** — the token
  is a pure function of the fence and the server secret.
- The secret is `SCARAB_GATE_TOKEN_SECRET`, held in `AppState.gate_token_secret`
  — mirroring the existing `SCARAB_GITHUB_WEBHOOK_SECRET` pattern. When unset the
  endpoint is **404** (token release is opt-in).
- Only gates of kind `external` are token-releasable; `manual` gates stay
  approval-only (`…/approve`, RBAC). A bad/missing token is **401**.
- Release reuses the same `release_gate` path as approval (marks the gate
  Succeeded, resumes the run) — exactly-once, idempotent.

## Consequences

- Machine callers release gates without a user session, with no new token store,
  rotation, or retrieval endpoint — rotate by rotating the server secret.
- The token is derivable by anyone holding the shared secret; distribution of the
  secret is the trust boundary (as with webhook secrets). Surfacing the token to
  authorized callers via the run API is a small follow-up.

## Alternatives considered

- **Per-gate stored random token** (generate at creation, store, surface via the
  API) — supports per-gate revocation, but adds generation, a column, and a
  retrieval path; deferred until per-gate revocation is actually needed.
- **Reuse `…/approve` under RBAC** — no machine-friendly auth; conflates
  `external` with `manual`.
