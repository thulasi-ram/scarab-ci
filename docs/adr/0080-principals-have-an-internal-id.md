# 0080. Principals have an internal id; the readable id becomes the handle

- **Status:** Accepted (owner decision 2026-11-10)
- **Date:** 2026-11-10
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0069](0069-identity-links.md) §2 (the readable id is no longer the
  key), §7 (sessions do not survive this upgrade) and its rejected alternative
  "Opaque `usr_<uuid>` ids".
- **Builds on:** [0049](0049-identity-and-access.md) (native RBAC),
  [0069](0069-identity-links.md) (Principals, identities, merge),
  [0073](0073-step-up-confirmation.md) (passkeys, OrgSecurityEventLog),
  [0075](0075-policy-engine.md) (policy inputs), [0078](0078-the-environment-adds-its-approval-gate.md)
  (the approver-set re-check)

## Context

ADR-0069 §2 made a Principal's id readable (`github:alice`, `okta:s.patel@acme.example`)
and froze it. It rejected an opaque id because the id is what an admin types into an
approver list or a grant, what the activity log shows for "cancelled by", and what
`when: event.actor == …` compares. An opaque id would need a resolution layer in each
of those places.

The owner's decision of 2026-11-10:

> The readable id is good for humans and should be used, but it is given by the user
> and whatever we sanitize it will cause issues somewhere. The readable id should be
> the public id; there should be an internal identifier. Yes, the principal should
> have an id.

And: URLs carry only ids that cannot be enumerated.

An audit of where the readable id is stored and compared found these gaps:

- **Personal data as a key.** The readable id is built from what a provider asserts: a
  forge login, a verified email (`okta:s.patel@acme.example`). It is copied as a key
  into `rbac_bindings`, `api_tokens`, approver lists, passkeys and step-up rows, and
  as attribution into append-only tables: the event log (CONTEXT: immutable),
  `org_security_events` (a trigger refuses UPDATE and DELETE), `policy_decisions` and
  `policy_versions`. It reaches every backup of them. None of these can be corrected
  when a person asks for their address to be removed.
- **Ids in URLs.** The member page is `/orgs/:org/settings/members/:subject/manage`
  and passkey reset is `/v1/orgs/{org}/members/{subject}/passkeys/reset`. An email
  in a path lands in proxy logs and browser history, and a readable id can be
  guessed.
- **The `unqualified_ids` path is not sanitised.** ADR-0069 §4 keeps an upgraded
  provider's subjects bare (`sub` → `login` → `id`). Only `:` and `~` are refused
  (`base_principal_id`); `is_clean_handle` is skipped. An Auth0 `sub` such as
  `auth0|abc` becomes a key. Tightening the filter would change ids that already
  exist, which is what the owner rules out.
- **Case.** `github:Alice` and `github:alice` are two keys. A forge treats them as one
  login.
- **Orphan bindings.** `rbac_bindings` rows written before migration 0052 may name a
  string that has no `principals` row. Under an `unqualified_ids` provider, the first
  sign-in that produces that string mints a new `~2` id (§2's never-claim rule) and
  does not get the grants.

What already holds and stays: CEL reads the id as a variable, never as source text;
`${{ event.actor }}` is refused in Pipelines; string lengths are capped in policy
inputs.

ADR-0069 §2's reasons for a readable id still hold for what people type and read.
They do not need the readable string to be the stored key.

## Decision

### 1. A Principal has an id and a handle

- **id.** `p_` followed by 32 lowercase hex characters: a random UUID v4 (122 random
  bits) without its dashes. Postgres assigns it at insert (`gen_random_uuid()`); the
  in-memory store uses `uuid::Uuid::new_v4`. It never changes, is never reused, and
  is never shown as the person's name. It is the stored key of every current-state
  reference to a Principal, the value new history records, and the only person
  reference in a URL.
- **handle.** What ADR-0069 §2 called the id: `github:alice`, minted once by the same
  rules (provider prefix, `~N` suffix, the `unqualified_ids` exception), then frozen.
  It is never re-minted and, for now, never renamed. People type it and read it:
  approver lists, grants, CEL literals, `event.actor`, the CLI,
  `SCARAB_OAUTH_OWNERS`.

A handle is unique. A **new** handle is also refused when it equals an existing one
ignoring case: minting treats a case-only match as taken and moves to the next `~N`.
Handles that already differ only in case are kept as two Principals and reported (§6);
they are never merged automatically, because which one is the person is not something
the database can tell.

A legacy `unqualified_ids` subject that has the shape of an id (`p_` and 32 hex
characters) is minted qualified instead, so a handle can never be mistaken for
someone's id.

### 2. Writes resolve a handle to an id

Every place a person names a Principal resolves what they typed before storing it:

1. A string that is a Principal's id names that Principal.
2. Otherwise, a handle that matches exactly.
3. Otherwise, a handle that matches ignoring case, when exactly one does. When two
   case-only duplicates match, the write is refused and the message names both.
4. Otherwise the write is refused, as ADR-0069 §3 requires.

A merged Principal resolves to the one it was merged into. This is
`resolve_principal` in `scarab-identity`, extended; it covers grants, approver lists,
the forge import, the server's `grant` command and passkey reset.

### 3. Reads accept either form

History keeps whatever was current when it was written: a handle before this upgrade,
an id after it. One lookup takes either form and returns the Principal (id, handle,
display name), without following merges. A string that names no Principal (a webhook
Actor, `plugin:<name>`, `external:<step>`) passes through unchanged.

It is used wherever a stored person reference is compared or shown:

- **Approval counting.** The `GateApproved.by` values of a gate's current wait are
  mapped to ids before they are counted against the Environment's approvers, so an
  approval recorded under a legacy handle counts the same as one recorded under an id.
- **The approver-set re-check** (ADR-0078, git-bug c2f97b4). Each launch compares the
  approver set recorded when the gate released (`EnvironmentApprovalMet.approvers`)
  with the current set. Both sides are mapped to ids first, so a gate released before
  the upgrade, whose record holds handles, still launches after it. Without this, every
  such gate would refuse with "approvers changed … rerun the gate".
- **Display.** API responses that name who did something (activity, cancelled by,
  approved by, updated by, created by, security events) show the handle.
- **Filters.** A security-event filter by person matches the rows stored under the
  person's id and the rows stored under their handle.

### 4. What stores which form

| Kind | Columns | After this ADR |
|---|---|---|
| Current state | `rbac_bindings.subject`, `api_tokens.owner_subject`, `org_environments.approvers`, `environment_project_overlays.approvers`, `passkeys.principal_id`, `passkey_users.principal_id`, `principal_identities.principal_id`, `principals.merged_into`, `step_up_requests.for_human` | rewritten to the id by the migration; new rows store the id |
| Short-lived | `sessions`, `step_up_confirmations`, `webauthn_challenges` | deleted by the migration |
| Attribution | `api_tokens.created_by`, `runs.snapshots_pinned_by`, every `updated_by`, plugin `approved_by` / `killed_by` / `retired_by` / `replaced_by`, library `fetched_by` / `installed_by` / `yanked_by` / `trusted_by`, `policy_versions.author`, `step_up_requests.confirmed_by`, `refused_runs.actor` | old rows keep the handle; new rows store the id; shown as the handle (§3) |
| Append-only | `events.payload` (`by` of GateApproved, RunCancelRequested, GateResolved, RunRerunRequested, StepRetryRequested, AttemptSuperseded, RunSnapshotsPinned / Unpinned; `EnvironmentApprovalMet.approvers`), `org_security_events`, `policy_decisions.input` | never rewritten; new rows store the id |
| The Actor | `event.actor`, `runs.origin_actor` | the handle. For a manual or API Run, the Actor is the dispatching Principal's handle, as for a webhook Run it is the forge login. `when: event.actor == 'github:alice'` keeps working |
| Policy inputs | `actor.principal`, `approvals`, `principal`, `trigger.principal` | the handle, as today; each gains an id field beside it (`actor.principal_id`, `approval_ids`, `principal_id`, `trigger.principal_id`), all optional so stored decisions still check against the schema |
| Policy bodies | `require_approval.principals` | not rewritten. The effect is refused when a Policy is saved today; when it is built, its entries are handles resolved like an approver list |
| Typed config | `SCARAB_OAUTH_OWNERS`, `scarab-server grant --subject` | handles (an id is accepted too) |

### 5. API and URLs

- Inputs that name a person take a handle and accept an id.
- Responses that describe a person return `{id, handle, display_name}`: the bindings
  listing (per row), the principal search, `GET /v1/me`, API token owners.
- A person is addressed by id in a URL. The member page is
  `/orgs/:org/settings/members/:id/manage`, backed by the new
  `GET /v1/orgs/{org}/members/{id}` (org Administer). Passkey reset is
  `POST /v1/orgs/{org}/members/{id}/passkeys/reset`.
- Approver lists in Environment responses are returned as handles, so the edit form
  round-trips what people typed.

### 6. The upgrade (migration 0093)

One migration, each table rewritten in the same transaction:

1. `principals.id` is renamed to `handle`; a new `id` column gets a random value for
   every row and becomes the primary key. A partial unique index on `lower(handle)`
   enforces §1 for new rows; existing case-only duplicates are marked
   (`handle_case_duplicate`) and left out of that index.
2. Every string stored as a current-state Principal reference (§4) that names no
   Principal gets an **unclaimed placeholder** Principal (`legacy_placeholder`) whose
   handle is that string. An `unqualified_ids` sign-in that mints exactly that handle
   claims the placeholder instead of minting `~2`, so pre-0052 grants reach their
   owner, as they did before ADR-0069 (its §4 "remaining risk" applies unchanged).
   No other sign-in can claim one.
3. The current-state columns are rewritten from handle to id by a join on
   `principals`. The `principals` table is the mapping back: each row keeps the
   handle beside the id, so every rewritten value can be returned with one join.
   No separate backup column is kept.
4. Sessions, step-up confirmations and WebAuthn challenges are deleted. Everyone signs
   in again.

At boot the server logs the case-only duplicate handles, so an operator can see
them. Merging two claimed Principals needs the admin merge ADR-0069 lists as a
horizon; until then both keep working under their own handle.

This is a contract step, and ADR-0003 asks for expand-only migrations. It is
acceptable here because a server from before 0093 and one after it cannot share a
database in any form: the old server would compare session subjects, bindings and
token owners as handles against columns that now hold ids, and a dual-write period
would keep personal data in the keys for one more release, which is the gap this ADR
closes. An operator upgrades with the `Recreate` strategy (the chart's default since
0.3.0, as the 0.3.0 notes say), takes a database backup first, and tells users to
sign in again. Rolling back means restoring that backup.

### 7. Merge keeps working

`merge_principal` rewrites bindings, approver entries, tokens and passkeys from one
id to another, as before. Sessions are matched by the id in their Principal
snapshot. History is not rewritten.

## Consequences

- A person's email or login no longer appears as a key, in a URL or in history
  written after the upgrade. History written before it still holds handles.
- Every display of a stored person reference goes through one lookup; a reference
  that names no Principal shows as stored.
- The members page and passkey reset take the id; old member links stop working.
- `GET /v1/orgs/{org}/bindings` rows and the principal search change shape: each
  person is `{id, handle, display_name}`.
- Every user signs in again after the upgrade.
- CEL policies keep their handle literals. A policy that wants to pin a person
  whatever their handle compares `actor.principal_id`.

## Alternatives considered

- **Renamable handles.** A person could change their handle and keep their history.
  It needs a handle history (an old handle in history must still resolve), a rename
  rule for typed lists and CEL literals, and a policy for who may rename. Rejected for
  now as too large; kept as a horizon.
- **Ids in CEL and YAML.** Authors would compare `actor.principal == 'p_3f…'`. Nobody
  can read or review that. Rejected; the id is offered beside the handle instead.
- **Rewriting history to ids.** The event log and the OrgSecurityEventLog are
  append-only by design and by trigger. Rewriting them breaks the guarantee they
  exist for. Rejected; reads accept both forms.
- **Sanitising the readable id further.** Lowercasing or restricting characters
  changes ids that already exist and still leaves personal data in keys. The owner
  ruled it out.
- **An id for URLs only.** A lookup table from an opaque URL id to the readable key
  closes the URL gap and none of the others. Rejected.

## Horizons

- Renamable handles (above).
- An admin merge of two claimed Principals, including the case-only duplicates this
  migration reports.
- Removing a person's handle from history written before this ADR, by rewriting it
  to the id in the tables that allow updates.
