# 0073. Step-up confirmation: a human confirms the exact action with a passkey

- **Status:** Accepted; the loosening rules for adding a deployment source (E3) and for an overlay's longer gate deadline (E1) superseded by [0074](0074-deployments-leave-core.md) (2026-10-28). Built 2026-10-24 as one build on `feat/step-up`; where the build
  differs from the text, the text is amended in place or recorded in
  [Implementation notes](#implementation-notes-2026-10-24).
- **Date:** 2026-10-24
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0049](0049-identity-and-access.md) (principals, sessions, RBAC,
  issued API tokens), [0072](0072-plugins-a-governed-extension-point.md) (PluginPrincipal,
  `approve_gates`, `subscribe_events`), [0071](0071-org-level-environments.md) (what
  "loosening an Environment" means)
- **Relates:** [0014](0014-secrets.md) (secret writes), [0070](0070-deployments-as-a-vendor-neutral-record.md)
  (gate approvals and rejects), CONTEXT.md §4.7 (StepUpConfirmation,
  StepUpConfirmationRequest, OrgSecurityEventLog)

## Context

A Scarab session or API token can do anything its role allows. That was fine when
the only callers were humans at a browser. It is not fine once **AI agents, scripts and
plugins** hold credentials. A leaked token, a CSRF, an unattended laptop, or an agent
that has been talked into "just rotate that secret" can widen production governance,
mint new credentials, open new outbound channels, or destroy data, and nothing
distinguishes that from the owner doing it on purpose.

Decision P7 of the deployments/plugins grill (2026-10-21/22) named the fix,
**StepUpConfirmation**, and parked it for the MMP. This ADR settles how it works.
What exists today: no WebAuthn code. GitHub and Forgejo log in through plain OAuth2,
which cannot prove a login is fresh. Generic OIDC has a verified `id_token`, but
nothing reads `auth_time`. Sessions live in Postgres, and the CLI may present a session
id as `Bearer`.

## Decision

### 1. A passkey is the step-up

A StepUpConfirmation is a **WebAuthn passkey assertion** made by a human in a browser.
A forced re-login counts **only** where the identity provider proves the authentication
is fresh (OIDC with `prompt=login` / `max_age=0` and a checked `auth_time`). A GitHub
or Forgejo OAuth round-trip never counts. It completes with no interaction when the
user is already signed in and has authorised Scarab, so a script driving their browser
would pass it. Those users confirm with a passkey. The OIDC fallback is **not in the
first build**: nobody logs in through generic OIDC today. The relying party is Scarab's
own (`crates/scarab-webauthn`); see Implementation notes for why.

### 2. Enrollment is prompted, and its bootstrap is a fresh login

No one is forced to enroll at login. The **first** time someone attempts a crown-jewel
operation without a passkey, the refusal points them to enrollment. The **first**
passkey can be enrolled only right after a **fresh login** in that browser session
(within the step-up window): a valid session alone is not enough, or a stolen session
could enroll the attacker's own passkey and own step-up forever. Every enrollment is
recorded and shown (see §7). Adding or removing a passkey **after** the first is
itself a crown-jewel operation. No second admin approves an enrollment.

**Recovery.** An Org Owner may reset a member's passkeys, as a stepped-up operation.
A sole Owner who loses every passkey recovers through an **operator break-glass on the
server binary**, `scarab-server admin reset-passkeys --principal …`. It runs where the
database credentials already are, is always recorded, and is never reachable through
the API or the `scarab` CLI. An API-reachable break-glass would be reachable with a
stolen token, which is the threat this ADR exists to stop. Who may reset whom, as built,
is in §10.

### 3. What needs a step-up (crown-jewel operations)

- Plugin install and upgrade.
- PluginConnection create and edit, above all its destination.
- The operator ceiling.
- ForgeConnection edits and identity links.
- Secret writes.
- Role changes.
- API token creation.
- Anything that **loosens** an Environment:
  - protection rules, including **removing an approver** (approvers are ALL-of) and
    **widening an allowed ref**;
  - EnvironmentProjectEligibility;
  - EnvironmentDeploymentSources;
  - the privilege whitelist.
- Approving a Gate in an Environment that has approvers.
- Adding or removing a passkey after the first.
- **Irreversible destruction:** deleting an Org, Project, Environment or secret, and
  shortening retention.

**Not** on the list, because each is low-risk, self-limiting or tightening:
- dispatching a Deploy Run (its Environment and Gates govern it);
- Plugin uninstall and the kill switch;
- rejecting a Gate and cancelling a Run;
- reading audit logs or the Deployments board.

**Who may reject a Gate** (git-bug 5c86952): anyone with write access to the Run, with
no step-up. A reject can only fail a Gate, never release one. Anyone who can reject can
already cancel the whole Run, both are recorded, and a Retry undoes a reject.

The guard covers the **whole** list from day one. Any operation left off it would be
unguarded.

### 4. The window

A confirmation covers the browser session that made it for **5 minutes from the moment
of confirming**. The window is **fixed**: activity never extends it, because a sliding
window lets a hijacked session stay elevated by staying busy. The window is bound to the
**cookie** session that confirmed it. The same session id presented as `Bearer` never
uses it: a `Bearer` caller always takes §6's request path. Whoever steals the whole
cookie (and its CSRF token) inside the window inherits the window. That is inherent to
any session-bound window, and it is why the operations below confirm every time.

Three operations confirm **every time**, even inside a window, because each mints a new
credential or outbound channel, which turns a 5-minute window into permanent access:
- API token creation;
- Plugin install/upgrade and a PluginConnection's destination;
- adding, removing or resetting passkeys.

They are rare, so the friction is small, and a per-action WebAuthn challenge can show
what is being confirmed. As built, **every** PluginConnection write is
always-ask, not only a destination change: the one write route sets the destination
together with the rest.

### 5. The refusal

A crown-jewel operation without a valid step-up answers **403 with code
`step_up_required`** and a message that it needs a step-up confirmation. The UI uses the
code to open the passkey prompt, or enrollment, and then retry. As built, the body also
carries `mode` (`window`, `action` or `enroll`), the operation id and its summary, and a
ready WebAuthn challenge; with no passkey it carries `enroll_url` and, while the
session's login is fresh, `fresh_login_until`. An **API token can
never satisfy a step-up**. A PluginPrincipal's one pre-authorised exception stands: a
plugin that an Environment names as an approver, and whose install was approved with
`approve_gates`, may approve. Naming it is a tightening, and what was stepped up is the
install.

### 6. Agents and the CLI: StepUpConfirmationRequest

A non-interactive caller (an API token, or the CLI on a human's session) that hits a
crown-jewel operation gets the 403 **plus a one-time confirmation link** for that
**exact** pending operation. The confirm page names the operation, its target and what
changes ("rotate secret `DB_PASSWORD` in `acme/prod`"), never "approve agent request
#4". A human opens it in a browser and confirms it with a passkey. The server then runs
that one operation **once**, recorded as requested-by the caller and confirmed-by the
human. The link expires in 5 minutes and cannot be reused or widened. How "exact" and
"cannot be widened" are enforced is in Implementation notes.

- **Only the human the caller acts for may confirm**: the token's owner, or the CLI
  session's human. Never another admin. Routing a confirmation to someone with more
  power is a confused-deputy channel. If the agent needs an admin's power, the admin
  does it.
- It runs with the **narrower** of the caller's scope and that human's live role at
  confirmation time.
- A PluginPrincipal has no human and cannot raise one.

Agents can still reach these operations; a human confirms each one.

### 7. OrgSecurityEventLog

One append-only, Org-scoped log records:
- every StepUpConfirmation (who, when, which passkey, which session);
- every crown-jewel operation, with the confirmation or StepUpConfirmationRequest that
  authorised it;
- every **refusal**, meaning which caller attempted which operation without step-up. A
  burst from one token is an agent trying operations to see which ones get through;
- every passkey enrollment, removal and reset, break-glass included.

Org admins see the Org's log; each user sees their own events and passkeys. Core sends
no notifications. Forwarding the log to Slack or email is a Plugin's job, through
`subscribe_events`. As built, Postgres enforces the log as append-only and nothing ever
prunes it; who sees which rows is in Implementation notes.

### 8. Rollout: enforced from day one, with tooling

`SCARAB_STEP_UP` is `enforce` in production, with **no `off` or `log` mode**: the
protection cannot be quietly disabled. Unset or blank means `enforce`; any other value
refuses boot. `SCARAB_DEV_INSECURE` (local dev, every caller an Owner) implies off
unless `SCARAB_STEP_UP=enforce` is set (for testing passkeys locally), so the
dev stack, `just plugin-verify` and the test suite keep working. As built, two more boot
checks:
- enforced step-up needs an `https` `SCARAB_PUBLIC_URL` (plain `http` only on
  `localhost`, `127.0.0.1` or `::1`), because that URL is the WebAuthn relying party;
- outside `SCARAB_DEV_INSECURE`, a server with OAuth login wired but step-up not
  enforced refuses to boot. With no login configured there is no human to confirm, so
  nothing is enforced and the boot log warns.

So that enforcing from day one does not lock people out:

- `scarab security setup` checks for a passkey. If there is none, it opens the browser
  at enrollment (through a fresh login), waits until it completes, and is safe to
  re-run.
- `scarab security status` shows your passkeys and their last use. For Owners it also
  shows the admins with no passkey and the recent refusals, the "what broke" view that a
  `log` mode would otherwise give.
- The boot log and a UI banner say "step-up enforced; N admins have no passkey". As
  built, the banner shows only when step-up is enforced and N > 0.

Enrollment is browser-only, with no libfido2 path in the CLI. One enrollment path is
easier to secure, and platform passkeys (Touch ID, a phone) only work through the
browser.

### 9. One build

Everything above ships together:
- passkeys and the StepUpConfirmation window;
- the full crown-jewel guard and the always-ask operations;
- the 403;
- StepUpConfirmationRequest and its confirm page;
- OrgSecurityEventLog writes and the Org and personal Security pages;
- Owner reset and the server break-glass;
- the CLI's `setup` and `status`.

Enforcing from day one with any of these missing would strand agents or lock people
out. Later, and only when needed: the OIDC fresh-login fallback (§1), and log forwarding
through a `subscribe_events` plugin.

### 10. Recovery, the CLI and sign-in, as built

- **Owner reset** (`POST /v1/orgs/{org}/members/{subject}/passkeys/reset`, always-ask).
  A passkey is not Org-scoped, so the caller must be Owner of **every** Org the target
  belongs to, or a bootstrap owner. An Owner of a small Org must not strip an admin of a
  bigger one. An install-wide owner (matched by id, alias, or verified email under the
  provider's `owners` rules) can be reset **only by a bootstrap owner**. A reset removes
  every passkey, revokes every standing window and unconsumed action confirmation, and
  makes the next enrollment a "first passkey" again, which needs a fresh login.
- **Break-glass**: `scarab-server admin reset-passkeys --principal …` runs against the
  database, resolves aliases, and records `passkey.break_glass_reset` **before** it
  removes anything; if the record fails, nothing is removed. No route reaches it, and
  the `scarab` CLI has no such command (both pinned by tests).
- **`scarab security setup|status`** as in §8. `setup` opens the browser at a fresh login
  whose `next=` lands on enrollment. Sign-in carries a validated `next=` (a same-origin
  path, bounded length, no `\` or control characters), so a confirm link survives login.
- **`scarab api <path> [-X method] [-d body]`**: a raw API call, like `gh api`. It is the
  CLI's only path to a crown jewel today, and it prints the 403's confirmation link.

## Considered options

- **Treat a GitHub/Forgejo re-login as a step-up fallback.** Rejected. It cannot prove
  freshness and passes silently for an already-authorised browser, so it would be a
  fallback in name only.
- **Strict mode for agents** (tokens and the CLI only ever get a 403; a human redoes the
  whole action in the UI). Rejected in favour of StepUpConfirmationRequest. It is
  safer only in theory, because in practice the human retypes what the agent wanted
  with less context about the exact change.
- **Any admin may confirm an agent's request.** Rejected as a confused-deputy hole.
- **A sliding window, or one long window with no always-ask operations.** Rejected. A
  busy hijacked session would stay elevated, and minting a credential would turn the
  window into permanent access.
- **`log` mode first, `enforce` later.** Rejected for this deployment (only dogfood
  exists). The `status` command's refusal view replaces what `log` would have shown.

## Consequences

- Every admin enrolls a passkey once, at their first crown-jewel operation. Until they
  do, those operations are refused with a pointer to enrollment.
- Automation that does crown-jewel operations with an API token stops working on day
  one and must go through StepUpConfirmationRequest, with a human confirming each
  action. That is intended.
- Secret rotations and production approvals cost one passkey tap per 5-minute burst.
- The threat model is stolen tokens and sessions, CSRF, unattended sessions, and
  over-eager agents. It does **not** cover code running as Scarab's own UI
  (`trusted_dom`, ADR-0072), which can swap what is being confirmed.

## Implementation notes (2026-10-24)

Recorded when the build landed, so the ADR stays accurate about how it was built and what
is still open. None of these reverses a decision.

- **The WebAuthn relying party is hand-rolled** (`crates/scarab-webauthn`, on `ring` +
  `minicbor`), not `webauthn-rs`. `webauthn-rs` needs OpenSSL, and this tree and its
  images are rustls/ring only. The one alternative without OpenSSL brought a second
  crypto stack and owned challenge generation and ceremony state, which conflicts with
  challenges that live in Postgres and commit to an action. With attestation `none` the
  verifier is small:
  - each check is a named error citing its WebAuthn L2 §7.1/§7.2 step;
  - every signature check runs in `ring`;
  - attestation is requested as `none`, and any statement is decoded strictly and then
    ignored (untrusted by policy);
  - user verification (UV) is **required**;
  - algorithms are ES256, EdDSA and RS256 (2048–8192-bit);
  - the key's curve point is validated **at registration** (P-256 on-curve, Ed25519
    decompression), so a broken first passkey cannot lock its owner out later;
  - the challenge is single-use, lives 2 minutes, and for always-ask is
    `SHA-256(domain ‖ nonce ‖ action digest)`, where the digest is an HMAC of the
    canonical request under a key derived from the KEK;
  - the signature counter is compare-and-swapped, and a counter that does not increase
    flags the passkey as a suspected clone, which makes it unusable.

  Tests use real-browser and authenticator vectors (SimpleWebAuthn fixtures, pinned by
  commit) as well as a software authenticator written independently of the verifier.
- **The guard.** Every route of the main router is registered once, with its
  classification, in `crates/scarab-server/src/routes.rs`. The classifications live in
  `step_up/ops.rs`. One middleware enforces them. Every `GET`/`HEAD`/`OPTIONS` is a read.
  A mutating request the table does not know is refused (500, fail-closed).
  A governed request whose non-empty body is not parseable JSON is refused (400
  `unreadable_body`), so nothing runs on a body the guard could not summarize.
  `tests/step_up_routes.rs` holds the whole classification as a literal table, so a new
  route or a reclassification is a visible diff. It also checks the table against the
  router and the OpenAPI spec.
- **The judgment calls on the crown-jewel list:**
  - **Crown jewels (window):** re-enabling a killed Plugin; unlinking an identity;
    revoking a role binding (a lock-out vector); binding a repository; resyncing a
    ForgeConnection (changed after review: a resync binds every unbound repository, so
    it is a bulk `bind_repo`).
  - **Not governed:** registering a repo webhook (idempotent for a bound repo);
    unpinning a Run's snapshots (it returns to the declared retention, never below);
    releasing an external gate (its HMAC token is the approval, ADR-0034).
  - **`attach` and `debug-pod`** (a shell into a step Pod) are `GET`s, so they are
    reads. Whether a debug shell should be a crown jewel is open (see Follow-ups).
- **Conditional crown jewels** are decided by the handler after it loads the stored
  state, using the loosening rules in `scarab-project` (`step_up_loosening.rs`). The
  rules are fail-closed: when unsure, a change counts as loosening.
  - **E1, protection rules:** an approver removed; allowed refs widened; a shorter
    wait timer; `require_reason` switched off; a concurrency limit removed or raised.
    An overlay change and an overlay's longer gate deadline count too.
  - **E2:** eligibility widened.
  - **E3:** a deployment source added.
  - **E4:** the privilege whitelist widened.

  Deleting an overlay (back to the baseline) is a crown jewel. A governance write is
  conditional on the state it judged: a change committed in between answers **409**, so
  a loosening is never judged against stale state. A Gate approval needs a step-up when
  the Run's Environment has approvers, and also when its governance cannot be resolved
  (fail-closed). The same snapshot then decides the release.
- **Crown jewels with no API yet:** Plugin upgrade, the operator ceiling, shortening
  retention and Org deletion have no route today. The guard covers each the day its
  route appears: an unclassified mutating route cannot exist.
- **PluginPrincipal approvals (§5).** No plugin approve route exists yet. The exception
  holds by construction: the CredentialBroker is a separate listener the guard never
  sees, and a test pins that no broker path is in the main table.
- **StepUpConfirmationRequest (§6) is a sealed, serialized request plus a route-level
  replay:**
  - **Raise.** The method, path, query and body are sealed under the KEK (AAD = the
    request id) and the body is cleared once the request settles. At most **10 pending
    requests per requester**; the eleventh gets 429.
  - **Confirm.** Only the human the caller acts for may confirm: the token's owner or
    the session's principal, or a Principal that one was merged into (the merge
    direction). Anyone else gets 404, so another admin learns nothing. A request already
    executing or executed gets 409; an expired one gets 410.
  - **Replay.** The request replays through the real router, so the real handler
    validates and authorizes it. The caller is rebuilt as it is **now** and narrowed to
    the human's live role. The replay is satisfied only by the confirmed operation id
    and digest, and only if every loosening it would make now is one the human
    confirmed. A request made wider after confirmation fails instead of running.
  - **The confirm page** shows the method, path and query, and every value in full.
    URLs are never truncated and their parsed host is shown, including when the URL
    carries userinfo. Other long values show head and tail. Control and bidi
    characters are escaped. The page cannot be framed.
- **Identity links.** A link flow's state cookie carries `HMAC(K_link, state,
  session_hash, provider, expires_at)`, which only the guarded `POST
  /v1/me/identities/link` can mint. Without it, the unguarded OAuth callback could be
  promoted into a link by anyone holding a stolen cookie (a review HIGH). The flow
  expires one window (5 minutes) after the POST, and cannot be moved to another provider.
- **Who sees which events (§7).**
  - The Org log shows the Org's own events, plus a fixed allow-list of Org-less kinds
    about its members: confirmations and the passkey lifecycle.
  - An Org-less confirmation shown there carries no operation or target. A confirmation
    for an Org-scoped operation is recorded under that Org when the caller holds a role
    there, and Org-less otherwise.
  - The personal log shows everything about you. `GET /v1/security/events` shows the
    whole install, for global admins.
  - `step_up.confirmed`, `passkey.enrolled`, `op.authorised` and the break-glass record
    are **fail-closed**: if the log cannot be written, the confirmation, enrollment,
    operation or reset does not happen. `op.completed` and refusals are best-effort.
  - A Postgres trigger refuses every `UPDATE` and `DELETE` on the log. Retention reaps
    challenges, expired confirmations and settled requests, never events.
- **Defaults outside `main`.** A hand-built `AppState` (tests) has step-up off. Production
  safety comes from the config and boot checks in §8, which have their own tests.

## Follow-ups

Filed as git-bug tickets:

- 9bbaa1d: lengthening the baseline deployment deadline and unpinning snapshots are
  ungoverned.
- 6856659: revoke standing step-ups when a passkey is removed or flagged as a clone.
- 7c6338f: clone-flag false positives, and the first-passkey fallback when every passkey
  is flagged.
- 69e06b5: record ceremony-level refusals.
- e582326: unbounded security-log and request storage growth.
- ff7a014: do not raise requests the requester can never execute.
- 45a6bea: foreign-Org `op.authorised` rows in a victim Org's log.
- 061450a: posture and membership leaks to Org admins.
- fe777ed: API tokens read their owner's personal security endpoints.
- 0314275: Owner reset and passkeys across identity merges (a merge does not move
  passkeys).
- bd445c8: compound domain names for the step-up identity types.
- d782d69: typed request and challenge context, and one event builder.
- 0485f41: `stored_environment` reads through `deploy_governance`.
- d2c1186: test-harness traps and overlay column drift.
- b5bbbed: stale references to the deleted OpenAPI test.
- 08d676d: the admin count overcounts email-shaped owner entries.
- 584bd03: the fresh-login and `security setup` URLs fail with several login providers
  and no default.
- 7a01b1c: step-up dialog retry UX.
- 51d6bcb: session ids and CSRF tokens are stored in plaintext (predates this build;
  the step-up tables raise its stakes).
- 606102b: whether `attach`/`debug-pod` (a shell into a step Pod) should be a crown jewel.
