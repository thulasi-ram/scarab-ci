# 0069. Identity links: one Principal, many provider identities, pinned by immutable id

- **Status:** Accepted. Built on main (PR #10; migration `0052_identity_links.sql`, the
  `/v1/me/identities/*` routes).
- **Date:** 2026-10-16
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0049](0049-identity-and-access.md) (authn → `Principal`, native
  RBAC, forge-permission import), [0046](0046-forge-auth-and-multi-adapter.md)
  (`ForgeConnection`), [0060](0060-org-settings-connections-and-secret-management.md)
  (multi-forge orgs are onboardable)

## Context

ADR-0049 says a Principal "may link more than one provider identity" and lists
"principals/provider-identity links" among its tables. Neither was built. What
exists is a **string coincidence**. `principal_from` (`oauth.rs`) turns the one
configured provider's claims into `Principal.subject`: OIDC `sub`, else the
forge `login`, else `id`. Every consumer keys on that bare string. The
forge-permission import (`POST …/bindings/import`) takes admin-typed strings,
sends each to the forge as a *login*, and stores the result under that same
string. Authorization is right only when the string a login produced happens to
equal the string an admin typed.

That breaks in five ways, all reachable today:

1. **Generic OIDC never meets the forge.** An Okta `sub` looks like `00u8f3c…`,
   while the import seeds `alice`. The two never match, so the import does nothing.
2. **One provider.** `Config.oauth` is an `Option<OAuthConfig>`. An Org whose
   repos span GitHub and Forgejo (ADR-0060 made that onboardable) cannot give a
   person one identity, and cannot sign in Forgejo users at all.
3. **No namespace.** `alice` on Forgejo and `alice` on GitHub are the same
   subject. Import from a Forgejo repo, log in with GitHub, and Forgejo-alice's
   permissions land on whoever is GitHub-alice.
4. **Renameable key.** With GitHub, the subject is `login`. A login is released
   when its owner renames, and anyone can then claim it. The new holder inherits
   every binding, API-token ceiling and approver entry keyed on it.
5. **Manual, grant-only import.** The caller must already know every login, and
   a re-import never removes anyone. Leaving the forge does not mean leaving
   Scarab.

The subject string is persisted in nine places (inventory, 2026-10-16):

- `rbac_bindings.subject`
- `sessions.principal`
- `api_tokens.owner_subject` and `created_by`
- `deployments.approved_by`
- `environments.protection.approvers`
- `runs.snapshots_pinned_by`
- `runs.origin_actor` (manual/api only)
- the `by` field of seven event variants

Grants, approver lists and `SCARAB_OAUTH_OWNERS` are **typed by an admin** and
matched by exact string equality (`ProtectionRules::admits`, `put_binding`
checks only for empty). So the subject is not just a key. It is the handle
humans write down. Any design that changes its format has to keep it writable,
and has to make a typed handle mean exactly one person.

## Decision

### 1. A Principal is a Scarab record; an identity is pinned by its immutable id

Two new tables:

- `principals(id PK, display_name, created_at, claimed_at, merged_into)`
  - `id` **is** the `Principal.subject` every existing consumer already stores.
  - `claimed_at` is `NULL` until someone has signed in as the Principal
    (see §4).
  - `merged_into` is set when the Principal has been folded into another (§5).
  - **Rows are never deleted.** An id, once minted, is never minted again.
- `principal_identities(provider, external_id, principal_id, handle, email,
  email_verified, linked_at, last_login_at)`
  - Primary key `(provider, external_id)`, plus unique `(principal_id, provider)`.
  - An identity belongs to exactly one Principal.
  - A Principal has at most one identity per provider.

`external_id` is the provider's **immutable** user id: OIDC `sub` if present,
else the numeric `id` (GitHub `/user`, Forgejo `/api/v1/user`; Forgejo's OIDC
`sub` is that same id). It is stored as a plain string, never JSON-quoted. This
fixes `principal_from`'s `v.to_string()`, which keeps the quotes of a string id.

**`login` is never an identity key.** It is recorded as `handle`, refreshed on
every login, and used only to mint ids and to display.

### 2. Principal ids are minted once, readable, then frozen

When a Principal is created its id is chosen, and it never changes afterwards:

- It is `<provider>:<handle>`, for example `github:alice` or
  `okta:ada@example.com`.
- For a provider with a `forge_url` (§4), the handle is the forge `login`.
- For any other provider, the handle is the **verified** `email`, falling back
  to `sub`.
- `preferred_username` is never used. It is not unique, and some IdPs let users
  edit it.
- A handle must match `[A-Za-z0-9._@+-]+`. If it does not, the minted id falls
  back to `<provider>:<external_id>`.
- If the candidate id already exists in `principals` at all (claimed, unclaimed
  or merged), a numeric suffix is appended: `github:alice~2`. `~` cannot appear
  in a handle, so a suffixed id cannot be pre-minted.
- Minting relies on the primary key: insert, and on a conflict retry with the
  next suffix. There is no check-then-act.

**A new identity is never attached to an existing Principal except through the
link flow (§5).** Logging in with an unknown `(provider, external_id)` always
mints a new Principal, even when a Principal with that handle already exists.

The id is readable rather than `usr_<uuid>` because it is what an admin types
into an approver list or a grant. It is also what the activity log shows for
"cancelled by", and what `when: event.actor == …` compares against on
manual/api runs (the only triggers that expose `event.actor`). An opaque id
would need a display-resolution layer in every one of those places, and
would make approver lists unwritable. The cost is cosmetic: after a rename,
`github:alice` keeps reading "alice". `display_name` tracks the current name.

The provider prefix closes (3), and the never-re-mint rule closes (4) for every
id Scarab mints.

### 3. Admin-typed handles must name an existing Principal

`put_binding` and environment approver lists **reject** a subject that is not
the id of an existing, unmerged Principal. A merged id is rewritten to its
target. Pre-assigning someone who has not signed in is done one of two ways:

- **Forge users:** run the import (§4). It creates their Principal, pinned by
  `external_id`, before their first login.
- **OIDC-only users:** they sign in first. Invitations by verified email are a
  horizon (below).

This is what makes a typed handle mean exactly one person: the string is bound
to a pinned identity at the moment it is written. It changes behavior: today an
admin can pre-grant a bare login nobody has used. `GET
/v1/orgs/{org}/principals?q=` backs a typeahead so typing stays practical.

### 4. Several providers, and the upgrade path

`SCARAB_OAUTH_PROVIDERS` / `SCARAB_OAUTH_PROVIDERS_FILE` (Helm
`scarab.oauth.providers`) is a JSON list. Each entry has:

- `id` (`[a-z0-9-]+`): the prefix in §2;
- `name`: the display name;
- the existing endpoint, issuer and scope fields;
- an optional `forge_url`.

The existing single-provider `SCARAB_OAUTH_*` variables still work. They become
one provider with:

- `id` = `SCARAB_OAUTH_PROVIDER_ID` (default `default`);
- `forge_url` = `SCARAB_OAUTH_FORGE_URL`;
- the flag **`unqualified_ids`**.

An `unqualified_ids` provider mints ids exactly as `principal_from` does today
(`sub` → `login` → `id`, with no prefix), so existing bindings, tokens, approver
entries and event `by` fields keep resolving. On a user's first login after the
upgrade, Scarab creates their Principal under the string it would always have
produced, and pins their `external_id` from then on. A raw id containing `:`
cannot be unqualified, because it would sit in another provider's namespace. It
is minted qualified instead.

Boot **refuses**:

- two `unqualified_ids` providers;
- two providers with the same `forge_url` (compared after the same trailing-`/`
  trim connections apply);
- a bare email in `SCARAB_OAUTH_OWNERS` when more than one provider is configured.

**Remaining risk:** under an `unqualified_ids` GitHub provider, the first
post-upgrade login with a given handle claims that handle's history. If the
login was recycled before its original holder signs in again, the new holder
gets it. Once pinned, this cannot recur. Operators who want no window move to a
qualified provider.

**Owners bootstrap.** An `SCARAB_OAUTH_OWNERS` entry matches in one of two ways:

- a **Principal id**, including ids merged into the caller;
- a `<provider>:<email>`, matched only when *that* provider asserts
  `email_verified`. A bare email is accepted only with a single provider.

This replaces today's any-provider email match, under which the weakest IdP that
asserts `email_verified` could mint a global Owner.

**Login.** Login takes a provider id: `?provider=` on `GET /v1/auth/login`, or
`provider` on `POST /v1/auth/login`. If omitted, the only provider or the
`unqualified_ids` one is used. The id is carried in the existing one-shot state
cookie, so there is still **one** callback URL and registered OAuth apps keep
working. `GET /v1/auth/providers` (unauthenticated) lists ids and names for the
login page.

### 5. Forge import resolves identities and creates unclaimed Principals

A provider's `forge_url` declares that its identities **are** accounts on the
forge instance whose `ForgeConnection.base_url` equals it. For GitHub that is
`https://api.github.com`; for Forgejo it is the web host, which is what Forgejo
connections store.

`POST /v1/repos/{org}/{repo}/bindings/import` works as follows:

1. **Resolve.** Find the repo's connection (as today) and the provider whose
   `forge_url` matches it. **If there is none, return a 409** naming
   `SCARAB_OAUTH_FORGE_URL` / `forge_url`. Without that provider, imported roles
   could never be signed into. Failing loudly here closes (1).
2. **List.** Call the new `ForgePort::list_collaborators(repo)`, which returns
   `{external_id, login, permissions}` and defaults to `Unsupported`.
   - **GitHub:** `affiliation=all`, which includes team and org access, paged
     through `Link`.
   - **Forgejo:** the repo's collaborators **plus the members of its teams**,
     with each user's effective permission from the per-user permission
     endpoint. Without the teams, Forgejo team access would vanish on re-sync.
   - **The listing is all or nothing.** Any page failure fails the import, and
     nothing is written.
3. **Pin.** For each collaborator, look up `(provider, external_id)`. If it is
   absent, **create an unclaimed Principal** (id minted per §2, `claimed_at
   NULL`) holding that identity. The insert is `ON CONFLICT DO NOTHING`, then a
   re-read, so a racing login or import converges on one row.
4. **Grant.** Grant roles to the resolved Principal id. The `import`-origin
   upsert records `imported_via = provider`, including on conflict.
5. **Re-sync.** An **unfiltered** import is a re-sync. Any `import`-origin row at
   that Project scope whose subject is not in the new set, and whose
   `imported_via` is this provider or `NULL` (pre-upgrade rows), is deleted.
   - Native grants and native revoke tombstones are never touched (ADR-0049).
   - A filtered import (`subjects` given, now a login filter) never prunes.
   - An empty listing never prunes.

Because the identity is pinned at import by `external_id`, whoever signs in with
that forge account claims those roles, and nobody else does. Claiming is
atomic: `UPDATE principals SET claimed_at = now() WHERE id = $1 AND claimed_at
IS NULL`. The team does not have to sign in before an admin can import them.

Authorization keeps its shape. `RbacStore::role_of(subject, scope)` still reads
only native storage, keyed on the Principal id, with no live forge call and no
join on the hot path.

### 6. Linking is explicit; merging is self-service only when control is proven

A signed-in user starts a link with `POST /v1/me/identities/link {provider}`.
The request is CSRF-guarded and returns the authorize URL. The state cookie
carries `intent=link` and a hash of the session id. When the provider calls
back, the identity can be in one of three states:

- **Not yet known.** It is attached to the caller.
- **Already the caller's.** Nothing happens.
- **Held by another Principal P.** The OAuth round-trip just proved the caller
  controls this identity.
  - **If it is P's only identity**, the caller controls P outright, so P is
    **merged** into the caller. This covers an unclaimed P created by import. It
    also covers the common trap: an Okta user who once clicked "Sign in with
    GitHub" instead of linking now has two claimed Principals.
  - **If P has other identities**, linking returns 409. Merging two people who
    have both signed in, and whom the caller has not fully proven, needs an
    admin and is out of scope here.

Merging P into the caller happens in one transaction:

1. **Identity.** P's identity moves to the caller.
2. **Bindings.** For each scope, in order:
   - the caller's own native row (grant or tombstone) stays;
   - if the caller has none, P's native row moves, tombstones included;
   - between import rows, the higher role wins.
   Native rows always beat import rows, regardless of rank.
3. **Approver lists.** Every environment's approver entries naming P are
   rewritten, across all orgs.
4. **API tokens.** P's tokens get `owner_subject` = caller, so they stay capped
   by the caller's live roles.
5. **Sessions.** P's sessions are deleted.
6. **P itself.** P is marked `merged_into` = caller and kept, so its id can
   never be re-minted.

History (event `by` fields, `origin_actor`, `approved_by`) is not rewritten. It
keeps the id that acted. For manual and API runs, `event.actor` after a merge is
the caller's id.

`DELETE /v1/me/identities/{provider}` unlinks. It refuses to remove the last
identity, and it deletes the caller's `import`-origin bindings seeded through
that provider.

**Accounts are never linked by matching email.** That convenience becomes account
takeover as soon as one IdP asserts an address it does not own. Email stays an
owners matcher, scoped per provider and gated on `email_verified`.

`GET /v1/me` gains `identities: [{provider, handle}]`.

### 7. What stays a string

- **Actor** (CONTEXT): for webhook runs it is the forge `sender.login`. It stays
  a forge fact, is never a Principal, and is never compared to one.
- **History columns and event `by` fields** keep the id that was current when
  they were written. §4 preserves that id across the upgrade, so nothing is
  rewritten.
- **Sessions** carry a `Principal` snapshot. A pre-upgrade snapshot carries the
  same subject string, so no forced logout is needed.

## Consequences

- **Migration** (expand-only): `principals`, `principal_identities`, and
  `rbac_bindings.imported_via TEXT NULL`. No column is dropped and no history is
  rewritten.
- **`IdentityStore` port** in `scarab-identity`: resolve-or-mint on login,
  pre-create on import, claim, link, unlink, merge, aliases (ids merged into a
  Principal), and search. It has a PG implementation and an in-memory one in
  testkit.
- **Per-provider authenticators.** A `ProviderRegistry` holds one
  `OAuthAuthenticator` per provider. Each maps claims to `(provider,
  external_id, handle, email, email_verified)`, and minting moves into the
  identity store. `token_owner_roles` and owners matching read the registry plus
  the Principal's aliases.
- **`ForgePort::list_collaborators`** is implemented by GitHub, Forgejo (with
  teams), `RegistryForge` and the testkit fake.
- **`put_binding` and `put_environment`** validate subjects against
  `principals`. This is a behavior change for anyone who pre-assigned bare logins.
- **Web UI:**
  - provider buttons on the login page;
  - an Account panel showing linked identities, with link and unlink;
  - an Org Members panel showing bindings, marking unclaimed Principals "not
    signed in yet", with an Import from forge button. Today there is no bindings
    UI at all.
  - Approver and grant inputs use the principals typeahead.

## Horizons

The end state this is a step toward. None of it is decided here:

- **Membership webhooks.** GitHub `member` / `membership` events and their
  Forgejo equivalents trigger a re-sync, so leaving the forge leaves Scarab
  without anyone clicking.
- **Invitations by verified email.** An admin could grant a role before an
  OIDC-only user's first login.
- **Actor ↔ Principal display.** The UI shows "alice · signed in as
  okta:ada@example.com" when a webhook Actor's forge account is linked. Display
  only; nothing relies on it.
- **Admin merge** of two claimed Principals that the caller cannot prove alone.
- **Re-checking global roles per request.** Today the global role snapshotted
  into the session survives an unlink for up to 24h.

## Alternatives considered

- **Opaque `usr_<uuid>` ids.** Every field an admin types into and every attribution
  display would need a resolution layer, and `when: event.actor == …` would
  stop being writable. Rejected for §2.
- **Identity-keyed bindings with a union lookup.** Here `rbac_bindings.subject`
  may be `provider#external_id`, and `role_of` unions a Principal's identity
  keys. It avoids pre-created Principals, but puts two kinds of subject in one
  column, a join on the authz hot path, and tombstone precedence spanning
  subjects. An unclaimed Principal gets the same "import before first login"
  property with the existing single-key authz.
- **Claiming a typed string at first mint.** This was the first draft of this
  ADR. Whoever minted the id first claimed every grant, approver entry and owner
  entry typed against it, which re-opens (4) for everything pre-assigned. The
  skeptical audit of this ADR caught it. Rejected for §3.
- **Linking by verified email.** The usual convenience, and the usual takeover
  vector. Rejected (§6).
- **Live forge authz.** Already rejected by ADR-0049.
