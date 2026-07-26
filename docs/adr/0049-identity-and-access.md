# 0049. Identity & access: forge-agnostic authn, Scarab-native RBAC

- **Status:** Accepted
- **Date:** 2026-07-17
- **Deciders:** thulasi.ram (architect)
- **Refines:** [0032](0032-slices-3-5-implementation-decisions.md) (auth sketch),
  builds on [0046](0046-forge-auth-and-multi-adapter.md) (`Project` scope),
  [0037](0037-environment-governance.md) (deploy gating)

## Context

Authn/authz is off in production (2026-07-16 audit): `with_auth` is never wired,
so `authorize()` grants every caller `Owner` (`lib.rs:1864-1870`); the only impls
are `FakeAuthenticator`/`InMemorySessions`; there is no PG session table, no
`Secure` cookie, no CSRF; and the scoped `Rbac`/`Binding` model in
`scarab-identity` is dead code — `authorize()` checks only a *global* role, never
role-in-`Project`. CONTEXT is explicit: IAM is **forge-agnostic** (OAuth/OIDC +
Scarab-native RBAC), not forge-coupled.

## Decision

### Authentication — forge-agnostic OAuth/OIDC → `Principal`, PG session

- Login via OAuth/OIDC providers (GitHub, Forgejo, generic OIDC) mapped to a
  Scarab `Principal`. Identity is **not** forge-coupled; a Principal may link
  more than one provider identity.
- **Server-side PG session store** (new table); session id is opaque.
- Cookie: **`HttpOnly` + `Secure` + `SameSite=Lax`** (the audit found `Secure`
  missing), plus a **CSRF token** for browser mutations (audit found none). The
  `Authorization: Bearer <session>` path stays for API/CLI clients.

### Authorization — Scarab-native RBAC is the source of truth

- Role bindings `Principal × scope × Role` in Postgres; **scope ∈ { Org,
  Project }** only. An **Org role inherits down** to its Projects. Reuses the
  existing `Role` (Viewer < Member < Admin < Owner) / action (Read/Write/
  Administer) model in `scarab-identity` — the dead `Rbac`/`Binding`/`Scope`
  types get wired.
- **No per-`Environment` roles.** Deploy authorization is the Environment's
  **protection rules / approval-as-gate** (ADR-0037/0024) — orthogonal to RBAC
  and already sufficient. "Can deploy to prod" is an approver on the gate, not a
  role.
- Enforcement is **per request against the path's `Project`/`Org`** (fixes the
  global-only check). This is also what scopes `list_runs`/`get_run` by tenant
  (the audit's cross-tenant leak — see the tenancy note below).

### Forge-permission import — bootstrap only, native is authoritative

When a `Project` is connected (ADR-0046 `ForgeConnection`), Scarab **may import**
the forge's collaborators/teams as initial role bindings (via `get_permissions`)
so it "just works" without hand-assigning everyone. But:

- The import **seeds/refreshes** native bindings; the **native binding is
  authoritative** — a manual grant/revoke is never clobbered by a re-sync.
- Authorization on the hot path reads **only native bindings** — never a live
  forge API call. This keeps authz forge-agnostic and multi-forge-clean (an Org
  spanning GitHub *and* Forgejo has one coherent role model).

### Tenancy scoping (falls out of the above)

Runs belong to a `Project` (ADR-0046); `Project` belongs to an `Org`. Every
run/log/event query is scoped by the caller's authorized `Project`/`Org` — this
closes the audit's unscoped `list_runs` cross-tenant leak
(`db-postgres/src/lib.rs:716-722`). No separate "tenancy" model is needed; it is
the `Project`/`Org` scope enforced.

## Consequences

- New PG tables: sessions, role bindings (+ principals/provider-identity links).
- Real OAuth/OIDC adapter(s) replacing `FakeAuthenticator`; PG `SessionStore`
  replacing `InMemorySessions`; wired via `with_auth` (which ADR-0048 makes
  mandatory-or-refuse-boot).
- `authorize()` becomes scope-aware; the dead `Rbac` model goes live.
- Ties to ADR-0048: no authenticator wired → refuse to boot (unless
  `SCARAB_DEV_INSECURE`).

## Amendment (2026-07-26): login hardening in the C1 adapter

Three refinements from reviewing the shipped adapter. They do not change the
decision above; they close gaps that only show up against a *real* OIDC issuer.

- **`id_token` is verified; userinfo is the fallback, not the source of truth.**
  A new **optional** `SCARAB_OAUTH_ISSUER` selects *OIDC mode*: when the token
  response carries an `id_token` it is verified — RS256 against the issuer's
  JWKS (found via `{issuer}/.well-known/openid-configuration`, cached in the
  adapter, refetched when a `kid` is unknown, i.e. rotation), plus `iss`,
  `aud == client_id`, `exp`, and the login flow's `nonce` — and its claims are
  the Principal. A **present-but-invalid `id_token` fails the login**; falling
  back to userinfo there would make the verification decorative. With no issuer
  configured (GitHub/Forgejo: plain OAuth2) no `id_token` is expected or
  trusted and userinfo remains the identity, unchanged.
- **PKCE (RFC 7636, S256) on the browser flow.** The redirect carries
  `code_challenge` + `code_challenge_method=S256`; the verifier stays server-side
  in the same one-shot `HttpOnly` state cookie (now `state.verifier.nonce`) and
  is replayed at the token endpoint. The adapter keeps the seam: the HTTP layer
  owns the cookie, the adapter owns the secrets and the exchange, so the
  API/CLI `POST /v1/auth/login` still works with no verifier — and may thread
  its own (`code_verifier`) if it ran its own authorize.
- **Owner bootstrap matches a VERIFIED email.** With GitHub/Forgejo the subject
  is a readable login, but a real issuer's `sub` is an opaque per-client id, so
  subject-only `SCARAB_OAUTH_OWNERS` means discovering and pasting UUIDs before
  anyone can administer anything. An owners entry now matches the subject **or**
  an `email` the issuer asserts as `email_verified`. `sub` remains the stored
  Principal identity — email is only ever a matcher — and an unverified or
  absent `email_verified` never grants `Owner`.

## Alternatives considered

- **Forge-synced-live authorization:** best ergonomics, but couples authz to a
  vendor, breaks multi-forge Orgs, and puts a forge API call on the authz hot
  path. Rejected; kept only as a bootstrap import.
- **Per-Environment RBAC roles:** duplicates the Environment protection-rule /
  approval-gate that already governs deploys. Rejected as redundant.
