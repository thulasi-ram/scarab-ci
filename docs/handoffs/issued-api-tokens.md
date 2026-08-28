# Handoff — issued API tokens (the missing machine credential)

Scarab has no credential a machine can hold. Every client surface for one
already exists; the credential itself does not. This is the design work and the
decisions worth making deliberately before writing code.

Found while standing up the public demo (`docs/handoffs/public-demo-oracle-k3s.md`),
where it blocks the obvious implementation of two separate things.

## The gap, traced

- **`session_id()`** (`crates/scarab-server/src/lib.rs:5448`) accepts exactly two
  things: `Authorization: Bearer <session-id>` or the `scarab_session` cookie. A
  session id is the only bearer credential the server understands.
- **Sessions are minted in one place** — `POST /v1/auth/login`, which exchanges an
  OAuth **authorization code** at the provider (`oauth.rs`). Single-use, and only
  obtainable by a browser completing a redirect.
- **TTL is 24h** (`SESSION_TTL_MS`, `lib.rs:4954`), non-renewable.
- **`openapi.json` declares no `securitySchemes` at all** — the API does not
  describe how to authenticate to it.
- The `sessions` table is `(id, principal jsonb, csrf, expires_at)`. A session's
  principal is stored **denormalised**, including its roles.

So there is no PAT, no service account, no client-credentials grant, no
token endpoint.

## What already exists (don't rebuild it)

- **The CLI is already a consumer.** `scarab-cli` takes `--token` /
  `SCARAB_TOKEN` and sends it as a Bearer header (`crates/scarab-cli/src/main.rs:91`).
  The flag works today — there is simply nothing valid to put in it except a
  scraped browser session id.
- **Bearer already bypasses CSRF, correctly.** `authenticate()`
  (`lib.rs:5381`) only demands `x-csrf-token` when `via == AuthVia::Cookie`,
  because a Bearer caller presents its credential explicitly. A token slots into
  that path with no CSRF work.
- **The authorization model is already scoped.** `authorize_scoped()`
  (`lib.rs:5405`) checks the principal's global roles first, then
  `rbac.role_of(subject, scope)`; bindings live in `rbac_bindings`
  `(subject, org, project, role, origin)`. A token does not need a parallel
  authz model — it needs to *carry* or *reference* a principal this already
  understands.

## What it unblocks

1. **`demo-keepalive.yml`.** It should call
   `POST /v1/repos/{org}/{repo}/dispatch`. It cannot, so it force-pushes an
   empty commit to a `demo-keepalive` branch and lets the real GitHub App
   webhook drive a run. That works and is a better demo, but it costs a moving
   branch in the repo and it is a workaround for this gap.
2. **Any non-browser client** — the CLI above, CI jobs on other systems, a
   status poller, an agent driving Scarab. Today all of them are locked out.

## Decisions to make (recommendation first)

1. **Opaque random, hashed at rest — not a JWT.**
   32 bytes of CSPRNG, stored as SHA-256 (constant-time compare on lookup). A
   high-entropy bearer secret needs no slow KDF; it is not a password.
   *Concrete reason to hash rather than store plaintext:* this deployment dumps
   Postgres to R2 nightly (`deploy/demo-oracle/postgres.yaml`). Plaintext tokens
   in the table would be replicated into object storage on a schedule, and a
   backup is a much longer-lived artifact than a token.
   A JWT would remove the DB lookup but make revocation a lie.

2. **A recognisable prefix — `scarab_pat_…`.**
   Two payoffs: `session_id()` can route on it instead of guessing, and secret
   scanners (GitHub push protection, gitleaks) can be taught one pattern.

3. **Least privilege at mint time, bounded by the minter.**
   ADR-0049's model is `Principal × scope × Role`. A token should carry an
   explicit scope+role **subset** of what its minter holds — never "inherit
   whatever the minter has, forever". An Owner minting a token that dispatches
   one repo's pipeline should be able to say exactly that.
   *This is the decision most worth not deferring*, because the easy
   implementation (clone the minter's principal into the token row, exactly as
   `sessions` denormalises it) is the one that produces permanent Owner
   credentials with no way to scope them down later.

4. **A mandatory expiry, and a verb.**
   The lesson is already written down in this repo, in the `values.yaml` comment
   on `workspaceTokenSecret`: the results token "carries no verb and never
   expires", and that combination is precisely why it must never be reused for
   anything else. Do not mint a second credential with that shape. Both an
   expiry and a capability set should be required fields, not options.

5. **Revocation and observability.** `revoked_at`, plus `last_used_at` updated
   on use (throttled — do not write on every request). A token nobody can see
   the last use of is a token nobody will ever dare revoke.

6. **Declare it in the OpenAPI document.** `openapi.json` currently describes an
   API with no authentication. Adding `securitySchemes` is part of the feature,
   not a follow-up — the generated UI client and the CLI both read this.

## Implementation surface

- **New table** `api_tokens`: `id`, `token_hash`, `name`, `principal_subject`,
  scope + role (or a small JSON capability set), `expires_at`, `created_by`,
  `created_at`, `last_used_at`, `revoked_at`.
- **`session_id()` / `authenticate()`** — route on the prefix; on a token, load
  the row, check `revoked_at`/`expires_at`, build the `Principal`. Everything
  downstream (`authorize_scoped`, tenancy scoping) is unchanged.
- **Endpoints** — mint / list / revoke, authorized as `Administer` on the
  scope the token targets. The plaintext is returned **once**, on mint.
- **UI** — a section in Settings (which already exists and is admin-gated).
- **`openapi.json`** — `securitySchemes` + per-operation security.

## Tests worth having

Classical, per `CONTEXT.md` §8 — mock only the clock:

- a valid token authenticates; a revoked one does not; an expired one does not;
- a token cannot exceed its minter's authority at mint time;
- a token scoped to one Project cannot read another's runs (the ADR-0049
  cross-tenant leak, re-asserted on this credential);
- the plaintext never appears in any response after mint, and never in the DB;
- CSRF is not required for Bearer (guards the `authenticate()` branch above).

## Open questions

- **Does a token's principal carry roles, or reference `rbac_bindings`?**
  Referencing means a revoked human loses their tokens' power automatically;
  carrying means a token keeps working after its minter is demoted. Referencing
  is safer and is the model ADR-0049 already argues for ("native bindings are
  authoritative"), but it makes a token's effective power non-obvious at mint.
- **Should tokens be principal-owned or org-owned?** A leaving employee's
  tokens should die with their access; a CI robot's should not depend on a
  person. This may want a distinct `Principal` kind rather than a flag.
- **Does this need its own ADR, or an amendment to ADR-0049?** It changes how
  authentication happens, not authorization — an amendment reads right, but the
  scoping decision in (3) is substantive enough to argue for its own record.
