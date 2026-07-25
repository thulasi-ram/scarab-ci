# 0060. Org settings surface: connection lifecycle, org/env secret editing, IaC-or-UI connections

- **Status:** Accepted
- **Date:** 2026-07-24
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0014](0014-secrets.md) (secret scopes + inheritance),
  [0037](0037-environment-governance.md) (scoped-secret resolution, effective-status matrix),
  [0046](0046-forge-auth-and-multi-adapter.md) (`ForgeConnection`, `Project` = governed repo,
  Forgejo admin-registration), [0049](0049-identity-and-access.md) (Org/Project RBAC scope)

## Context

Three governance concerns exist in the model and (for secrets) the API, but have **no
management surface**, so they are unreachable in the product:

1. **Org-scoped secrets** — the top of the `env → repo → org` inheritance chain
   ([0014]/[0037]). `/v1/secrets` already supports org scope, but the web-ui `RepoSecrets`
   tab is hardwired to `{ org, repo }`, so org secrets can only be set by raw HTTP.
2. **Environment-scoped secret *values*** — the Environments tab edits protection *rules*;
   the `SecretMatrix` is read-only. Nothing in the UI sets an env-scoped secret value,
   despite [0037] making env-scoped secrets a live-path feature.
3. **Forge connections** — `ForgeConnection` + `ForgeConnectionStore` + Postgres exist, but
   there is **no HTTP API** and the only production `put_connection`/`bind_repo` caller is the
   GitHub `installation` webhook (`scarab-forge-github/src/lib.rs:301`). A **Project is a
   `forge_repos` binding — there is no `projects` table** ([0046], migration 0022/0023), so
   "create a Project" *is* `bind_repo`. Consequently **Forgejo is unusable**: no connection-
   creation path, no repo binding, no webhook registration — its only route into the DB is a
   hand-written row. [0046] shipped Forgejo "in v1"; in practice it cannot be onboarded.

A prior decision fixes the tenancy framing: **there is exactly one Org** (single-tenant
*experience*), but `Org` stays the model's RBAC/secret inheritance root ([0049]/[0037]) — we
do **not** collapse the concept, we just never build org navigation. "Org settings" is
therefore the settings of the one org, surfaced as a single global **Settings** area.

## Decision

### A. A single global **Settings** area (org-scoped), `Administer`-gated

A new top-level `/settings` route holds the org-scoped surfaces: **Connections** and **Org
Secrets**. It is enforced at `Administer` on the Org ([0049]) and the nav entry is **hidden**
from non-admins (nothing there is actionable or informative for them). There is **no org
switcher** — one implicit Org.

### B. Secret editing is a unified, scope-dimensioned surface; the matrix *is* the editor

- **Org secrets** live in global Settings (org scope, the inheritance root).
- **Repo + Environment secret values** live in the Project's **Secrets tab**, where the
  [0037] effective-status matrix becomes **editable**: rows are keys, columns are
  `Repository default` (the Project/`SecretScope::Repo` scope) + one per Environment. Editing
  a cell writes a value **at that scope**; the cell always distinguishes *set-here* /
  *inherited* / *unset* (+ the [0037] "intentionally unset" marker), so an edit reads as
  "override at this scope" vs "fall through."
- The **Environments tab stays rules-only** (approvers, allowed refs, timers). Secrets are
  one key-addressed namespace resolved by scope ([0037] §C) — they are **not** fragmented
  into per-Environment editors.
- User-facing copy says **"repo"** (the end-user abstraction); the scope resolves against the
  **Project** (governance = repo + its Environment link). `SecretScope::Repo` is **not**
  renamed — internal, user-invisible, and a rename is broad churn for no correctness gain.

### C. Connection lifecycle: full onboarding, asymmetric by forge

A new `/v1/connections` surface (list / create / delete) plus `bind_repo` / `unbind_repo` /
`register_webhook`, because for any forge without installation-style auto-registration the
binding step *is* the missing **repo→Project onboarding** flow.

- **GitHub is observe-only.** Installing the App on GitHub *is* registration; there is no API
  for Scarab to install/uninstall or change repo selection. The UI shows the installation,
  its bound Projects, and credential/delivery health; offers a **"Manage on GitHub →"** deep
  link and a **re-sync** action (re-fetch `installation_repositories` to heal a missed
  webhook). Auto-bind is untouched.
- **Forgejo is fully in-product.** Admin creates a connection (`base_url` + credential), binds
  repos (**creating Projects**), and registers per-repo webhooks ([0046] `register_webhook`).

### D. A connection has exactly one owner: config **or** DB — never both

Connections may be provisioned two ways, and each connection is owned by exactly one source:

- **IaC / declarative** — a `connections:` config block (Helm values → server config), each
  `{ kind, base_url, credential: <env-var | secret-ref> }` (+ optional repos to bind).
  Provisioned at boot, **authoritative, and read-only in the UI** (labelled "managed by
  configuration"). This generalizes today's `SCARAB_GITHUB_APP_PEM[_FILE]` override from "just
  the PEM" to "any connection + credential."
- **Manual / UI** — "Add connection" takes `base_url` + a token; the server **writes the token
  through** to `SecretProvider` under a generated `_forge`-org `credential_ref`, stores a DB
  connection row, and never echoes the secret (write-only; shows "•••• set" on edit).

Credential resolution is one path — **env-override → `SecretProvider`** — i.e. today's
`connection_credential()` generalized. The `_forge` pseudo-org stays an internal mechanism,
never surfaced to users.

Three details the single-owner rule forces, settled while building it:

- **Ownership is persisted, not inferred** (`forge_connections.owned_by_config`). Only a
  durable marker lets a *later* boot tell "the row I provisioned from config" (safe to
  overwrite) from "a row a human created" (a collision). Config-declared connections are
  provisioned as real registry rows, so the forge router, the clone-step enricher and
  webhook resolution need no special case.
- **A collision refuses the boot.** An id declared in config that already exists as a
  DB-owned connection stops the process before any write, naming both sources — the
  operator decides which owns it. Un-declaring an entry **releases** ownership back to the
  UI rather than deleting the connection: a Project (and its Environments, secrets, RBAC)
  hangs off its repo bindings, so removal stays an explicit human act.
- **Only *deployment-supplied* credentials fail the boot.** A missing/empty `credential.env`
  or `credential.file` is a broken promise by the same deploy that made it ⇒ refuse
  (ADR-0048). A `credential.secret_ref` that is not registered yet is reported **DEGRADED**
  instead, because the running server is the only thing that can store it — refusing there
  would deadlock a fresh database, the same bootstrap trap `SCARAB_GITHUB_APP_PEM` exists to
  avoid.

## Consequences

- Forgejo becomes **actually onboardable** from the product for the first time — the [0046]
  multi-adapter promise is delivered, not just modelled.
- Org and Environment secrets get a real editing home; the [0037] matrix stops being read-only
  and becomes the single mental model for "what resolves where."
- New surface: `GET/POST/DELETE /v1/connections`, `bind_repo`/`unbind_repo`/`register_webhook`
  endpoints, a `connections:` config schema, an editable-matrix component, and a global
  Settings route. `SecretScope::Repo` is untouched.
- Single-owner precedence avoids a config-vs-DB dual-write drift hazard (the same reasoning
  [0037] used to reject a standalone approvals table).
- **Deferred:** org **RBAC / member management** (`Principal × scope × Role`, [0049]; the
  `/v1/orgs/{org}/bindings` API already exists) is the natural *third* Settings section but is
  **out of scope here** — identity, not forge/secrets. Tracked as a follow-up
  ([`docs/followups.md`](../followups.md)).

## Alternatives considered

- **Collapse the Org concept** (true single-tenant, no Org entity) — reopens [0037]/[0046]/
  [0049] and rips out the RBAC/secret inheritance root for no functional gain; the single-Org
  *experience* is achieved without it.
- **Env secrets in per-Environment editors** — co-locates with "configuring prod," but splits
  one key-addressed namespace across two tabs and hides the [0037] inheritance view. Rejected
  for the matrix-as-editor.
- **Observe-only connections** — small, but re-displays GitHub facts already implicit and
  leaves Forgejo unusable. Rejected; the value is the onboarding flow.
- **Config seeds, UI overrides win** — reads as flexible but is exactly the dual-write drift
  trap: IaC and the running system disagree with no authority. Rejected for single-owner.
- **Rename `SecretScope::Repo → Project` + `project=` API param** — more internally consistent
  but broad mechanical churn across crates for a concept users never see, risking collision
  with the `repo`-keyed paths. Rejected as feature-time scope creep.
