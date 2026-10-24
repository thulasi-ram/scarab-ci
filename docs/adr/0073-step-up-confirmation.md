# 0073. Step-up confirmation: a human, a passkey, the exact action

- **Status:** Proposed
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
first build**: nobody logs in through generic OIDC today.

### 2. Enrollment is prompted, and its bootstrap is a fresh login

No one is forced to enroll at login. The **first** time someone attempts a crown-jewel
operation without a passkey, the refusal points them to enrollment. The **first**
passkey can be enrolled only right after a **fresh login** in that browser session
(within the step-up window): a valid session alone is not enough, or a stolen session
could enroll the attacker's own passkey and own step-up forever. Every enrollment is
recorded and surfaced (see §7). Adding or removing a passkey **after** the first is
itself a crown-jewel operation. No second admin approves an enrollment.

**Recovery.** An Org Owner may reset a member's passkeys, as a stepped-up operation.
A sole Owner who loses every passkey recovers through an **operator break-glass on the
server binary**, `scarab-server admin reset-passkeys --principal …`. It runs where the
database credentials already are, is always recorded, and is never reachable through
the API or the `scarab` CLI. An API-reachable break-glass would be reachable with a
stolen token, which is the threat this ADR exists to stop.

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

The guard covers the **whole** list from day one. A partial list is a list of holes.

### 4. The window

A confirmation covers the browser session that made it for **5 minutes from the moment
of confirming**. The window is **fixed**: activity never extends it, because a sliding
window lets a hijacked session stay elevated by staying busy. A session presented as
`Bearer` from elsewhere, or another tab's stolen cookie, cannot use it.

Three operations confirm **every time**, even inside a window, because each mints a new
credential or outbound channel, and that is how minutes become permanent access:
- API token creation;
- Plugin install/upgrade and a PluginConnection's destination;
- adding, removing or resetting passkeys.

They are rare, so the friction is small, and a per-action WebAuthn challenge can show
exactly what is being confirmed.

### 5. The refusal

A crown-jewel operation without a valid step-up answers **403 with code
`step_up_required`** and a message that it needs a step-up confirmation. The UI uses the
code to open the passkey prompt, or enrollment, and then retry. An **API token can
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
human. The link expires in 5 minutes and cannot be reused or widened.

- **Only the human the caller acts for may confirm**: the token's owner, or the CLI
  session's human. Never another admin. Routing a confirmation to someone with more
  power is a confused-deputy channel. If the agent needs an admin's power, the admin
  does it.
- It runs with the **narrower** of the caller's scope and that human's live role at
  confirmation time.
- A PluginPrincipal has no human and cannot raise one.

This makes Scarab a human-in-the-loop gate for agents rather than a wall they hit.

### 7. OrgSecurityEventLog

One append-only, Org-scoped log records:
- every StepUpConfirmation (who, when, which passkey, which session);
- every crown-jewel operation, with the confirmation or StepUpConfirmationRequest that
  authorised it;
- every **refusal**, meaning which caller attempted which operation without step-up. A
  burst from one token is an agent probing the edges;
- every passkey enrollment, removal and reset, break-glass included.

Org admins see the Org's log; each user sees their own events and passkeys. Core sends
no notifications. Forwarding the log to Slack or email is a Plugin's job, through
`subscribe_events`.

### 8. Rollout: enforced from day one, with tooling

`SCARAB_STEP_UP` is `enforce` in production, with **no `off` or `log` mode**: the
protection cannot be quietly disabled. `SCARAB_DEV_INSECURE` (local dev, every caller
an Owner) implies off, so the dev stack, `just plugin-verify` and the test suite keep
working. To make day one survivable:

- `scarab security setup` checks for a passkey. If there is none, it opens the browser
  at enrollment (through a fresh login), waits until it completes, and is safe to
  re-run.
- `scarab security status` shows your passkeys and their last use. For Owners it also
  shows the admins with no passkey and the recent refusals, the "what broke" view that a
  `log` mode would otherwise give.
- The boot log and a UI banner say "step-up enforced; N admins have no passkey".

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
