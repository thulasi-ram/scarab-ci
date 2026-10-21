# 0072. Plugins: a second, governed extension point beside Steps

- **Status:** Proposed
- **Date:** 2026-10-22
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0008](0008-step-contract.md) (a Step is any OCI image; the step
  extension point this sits beside), [0038](0038-invoke-and-local-reuse.md) (the
  plugin shapes already rejected), [0070](0070-deployments-as-a-vendor-neutral-record.md)
  (the record plugins report into), [0071](0071-org-level-environments.md) (the
  Environments that admit them)
- **Relates:** [0014](0014-secrets.md) (SecretProvider, redactor),
  [0021](0021-double-effect-fencing.md) (fence as idempotency key),
  [0049](0049-identity-and-access.md) (principals, RBAC),
  [0056](0056-run-takes-and-attempt-grain-evidence.md) (Takes),
  [0060](0060-org-settings-connections-and-secret-management.md) (connections)

## Context

ADR-0070 makes core own a vendor-neutral Deployment record. Something still has to know
Argo CD: how to watch an Application, how to tell "retrying" from "given up", how to
Resync, how to promote or abort a Rollout. Tomorrow it will be Flux, ECS, a change
ticket in ServiceNow, or a Slack message. None of that belongs in core, and none of it
fits a Step:

- A Step lives inside a Run and dies with its Attempt. Watching a CD tool is
  **long-lived** and happens **outside any Run**: most changes Treebo's Argo applies
  (`selfHeal` reverts, hand-applied hotfixes) have no Run.
- A Step holds its credentials for its whole execution. A watcher that holds a prod Argo
  token for months is the credential nobody audits.
- A Step cannot add a button to a Deployment row, approve a gate on behalf of a change
  system, or report a status.

The governing principle, from the owner, verbatim:

> "We should not overfit scarab to argo, we should ensure the plugin system is powerful
> enough to fit argo."

Argo is the proving workload. Every capability below exists because the Argo plugin
needs it *and* a non-Argo plugin could use it the same way.

Two earlier decisions constrain the design and **still stand**:

- **ADR-0008: a Step is any OCI image + command.** The container ecosystem remains the
  step library. Plugins do not replace that or wrap it.
- **ADR-0038 rejected** (a) a macro / AST-rewriting plugin that runs arbitrary code in
  the control plane and mutates the IR, and (b) a typed plugin SDK / marketplace **for
  steps**. Both rejections stand.

What is different here: a Plugin runs **out of process** in its own Pod, never in the
control plane. It **never touches the IR** or any Pipeline. It gets a **closed,
capability-scoped** interface rather than an SDK, reaches the outside world **only
through a broker** core controls, and extends Scarab where Steps cannot (long-lived
observation, actions on things outside a Run). It does not change how Steps are
written.

## Decision

### 1. A Plugin is a digest-pinned image plus a manifest

A **Plugin** is a digest-pinned OCI image plus a **PluginManifest**. It runs as a
long-lived workload Scarab manages, outside any Run.

- It depends **only on core, never on another Plugin.**
- **First-party Plugins use exactly the contract third parties do.** If the first-party
  Argo plugin needs more, core changes; it gets no private API.
- A Plugin never holds a long-lived credential (§6), and never reads secrets, workspaces,
  artifacts or logs unless a future capability says so.
- A Plugin never resolves a gate (it reports facts; core decides) unless an Environment
  admin has named its PluginPrincipal as an approver (§3). It **never** resolves a
  DeploymentGate.

### 2. A closed PluginCapability vocabulary

The PluginManifest declares every **PluginCapability** the Plugin wants, with scopes.
What an org admin approves is exactly *(image digest, manifest)*. The vocabulary is
**closed** and grows only with a Scarab release.

| Capability | Grants |
|---|---|
| `report_deployments` | DeploymentStatusReports and DeploymentTargetProjectOwnership, for the sources an Environment admits (ADR-0071 §5); bound by the DeploymentReporterContract (ADR-0070 §11) |
| `subscribe_events` | Read-only Run and Deployment events |
| `offer_actions` | Declare PluginActions that humans or Runs may invoke (§4) |
| `use_connection` | Reach a named external system through the CredentialBroker without holding its credential (§5) |
| `contribute_ui` | Add a PluginUiContribution at a declared trust tier (§7) |
| `approve_gates` | Approve or reject approval gates, effective only where an Environment admin names the Plugin as an approver (§3) |

### 3. PluginPrincipal, and the two-key approver rule

Each installation acts under its own **PluginPrincipal**, `plugin:<name>`, which is not
tied to any human. It is what reports, calls the broker, and approves. It appears in
audit and the UI as, for example, "approved by plugin:servicenow (CHG0012345)".

A PluginPrincipal may approve a gate only with **two keys held by two roles**:

1. an **org admin** approved the `approve_gates` capability at install, and
2. the **Environment's admin** named that PluginPrincipal among the Environment's
   approvers.

A named PluginPrincipal is **additive** to the human approvers by default. A
DeploymentGate is never resolved by a Plugin (ADR-0070 §8).

### 4. Every PluginAction is a `plugin_action` Step in a Run

A **PluginAction** is a typed operation a Plugin declares (resync, promote, abort, post
to Slack), with typed inputs. **Every invocation is a `plugin_action` Step in a Run**, so
audit, Environment governance, durability and fencing apply unchanged.

- **`plugin_action`** is a built-in Step kind. It launches no Pod. Core dispatches to the
  Plugin and records the response as the Step's results.
- **From a Pipeline:** an author writes a `plugin_action` Step.
- **From the UI:** a button creates a **single-step Run** in the DeploymentTarget's owning
  Project and Environment. If several Projects own the target, the user picks one. A
  UI-started `plugin_action` Run runs no project code, so it is **exempt from
  `allowed_refs` only**. Eligibility, ownership, the approval flag and `require_reason`
  still apply.
- **Approval.** Each PluginAction declares whether it **needs the Environment's
  approval**. The admin sees this at install and may only tighten it (force approval),
  never loosen it.
- **Authority.** The action's one-shot authority (§5's write rules) is bound to that
  Step's Attempt and dies when the Take is superseded (ADR-0056).
- **Dispatch is at-least-once.** Core passes the Attempt's fence as an idempotency key,
  and the Plugin must accept a repeat (ADR-0021).
- **A Plugin never invokes its own PluginActions.**
- No PluginAction is offered on a DeploymentTarget with no owners (ADR-0071 §4).
- What a PluginAction *means* is the Plugin's business. A mislabelled one is caught by
  install review, the broker's rules and the vendor's own RBAC; anything it makes live
  without a request surfaces as an Unexplained Deployment.

### 5. CredentialBroker, PluginConnection and request rules

A **PluginConnection** is an org-admin binding of **a credential to the one destination
it may be used against**: base URL, credential reference and auth scheme. The
destination is part of the authority, so the two are never separated. The secret bytes
stay in the SecretProvider (ADR-0014). A PluginConnection may be **scoped to an
Environment** (for example `argocd-prod`, with a token limited to Argo's `prod` project),
so the vendor's own RBAC tells prod from staging. The broker uses the connection that
matches the invoked DeploymentTarget's Environment.

The **CredentialBroker** is core's authenticating reverse proxy, at a stable in-cluster
address. The Plugin sends ordinary requests to it and keeps its own client, native SDK,
retries and streaming. For each request the broker:

1. checks it against the manifest's **PluginConnectionRequestRules**,
2. adds the credential,
3. forwards it, and
4. journals it.

A **PluginConnectionRequestRule** is an allowed request shape on a PluginConnection:
method, path pattern and, optionally, a body schema.

- **Read rules** apply at any time.
- **Write rules** are declared **per PluginAction** and are open **only while that action
  is being invoked**. A write rule may bind `{target.id}` in the **path**, in a **query
  parameter** and in a **body field**, wherever the vendor's API names the object (Argo
  names the Application in the path, and its namespace in the `appNamespace` query
  parameter). **The broker fills** each binding from the invoked DeploymentTarget and
  rejects a request whose bound value differs. In the path, the id is
  **percent-encoded as one segment**: target ids are plugin-supplied, and an id such as
  `a/../../b` must not let a write rule reach `b`.
- **Each invocation gets its own broker credential**, carrying the Attempt's fence. The
  broker therefore knows which invocation, and so which target, a request belongs to,
  even when two run at once. The Plugin cannot aim a write at another target.

The rules are the second line of defence. Vendor-side least privilege (an Argo role that
can sync but not override) is the first.

### 6. Credential modes: brokered by default, delivered as an escape hatch

**PluginConnectionCredentialMode** is one of:

- **`brokered`** (default). The Plugin **never holds the credential**. Everything in §5
  applies.
- **`delivered`** (escape hatch, for protocols the broker cannot proxy). The credential is
  mounted into the Plugin. Egress is tunnelled through the broker as TCP, pinned to the
  destination's address and TLS name (SNI). It needs **dual org-admin approval**,
  re-approved on every digest. Everything the Plugin emits (labels, links, action
  results) is scrubbed by the secret redactor.

**Honest limit:** behind a **shared ingress**, a delivered credential's tunnel may reach
other sites on that ingress, because address and SNI pinning cannot tell them apart. The
UI says so. `delivered` is meant for destinations with an address of their own
(databases, SSH, dedicated endpoints).

### 7. UI contributions at two trust tiers

A **PluginUiContribution** declares one **PluginUiTrustTier**:

- **`framed`** (default). The Plugin's UI runs in a **sandboxed, opaque-origin iframe**
  mounted at a **UiExtensionPoint**: a Deployment row's expansion, a Run panel, an
  Environment board column, an org tab, or a Step panel. It owns its own DOM inside the
  frame (any UI library) and receives core's design tokens so it looks native. It talks
  to core only through a narrow bridge: context, plus **proposals** of its own declared
  PluginActions. The bridge only proposes. Core renders the confirmation **outside the
  frame**, and the action runs only when the viewer confirms there, so frame code cannot
  fire an action without a real click (§4: a Plugin never invokes its own PluginActions).
  It **cannot act as the viewer and cannot read Scarab.** One org admin approves.
- **`trusted_dom`**. Same-origin code that can change any page and **can do anything any
  viewer of that page can do.** It is equivalent to adding code to Scarab itself. It is
  allowed only for publishers the **operator** names, needs **dual org-admin approval**,
  is re-approved on every digest, and is served pinned with Subresource Integrity.

The `trusted_dom` guardrails ensure *you chose to trust it*. They do **not** contain it.
**No exfiltration or page-boundary guarantee is claimed**, and the UI and docs must not
imply one.

### 8. StepUpConfirmation for crown-jewel operations

A **StepUpConfirmation** is a fresh confirmation that a script cannot forge, given by a
**human in an interactive session**: a Scarab-registered **passkey** (WebAuthn), or a
forced re-login where the identity provider supports one. It is required for:

- Plugin install and upgrade, and the operator ceiling;
- PluginConnection create and edit, above all its destination;
- ForgeConnection edits, identity links, secret writes, role changes, API token creation;
- anything that **loosens** an Environment: protection rules,
  EnvironmentProjectEligibility, EnvironmentDeploymentSources, privilege grants;
- approvals on gates of Environments that have approvers.

**API tokens cannot perform crown-jewel operations at all.** Tightening needs no
step-up. The one pre-authorised machine exception is a PluginPrincipal named as an
approver (§3). Naming it is a **tightening**, because approvers are ALL-of, so it needs
no step-up. What was stepped up is the install that approved the `approve_gates`
capability (§10).

A local deployment run with `SCARAB_DEV_INSECURE` acts as a synthetic Owner and has no
interactive human to step up. It is not a production mode, and step-up does not apply to
it.

Step-up defends against stolen tokens, CSRF and unattended sessions. It does **not**
defend against `trusted_dom` code, which can change what is being confirmed.

### 9. Runtime isolation

- **One instance per org installation.**
- **One shared `scarab-plugins` namespace**, separate from Scarab's own, under the
  `restricted` Pod Security Admission level.
- Each Plugin runs under a ServiceAccount with **no RBAC** and **no token automount**.
  It receives only a projected token for the broker (§11).
- **NetworkPolicy is default-deny**, including Plugin to Plugin. Egress is allowed to DNS
  and the CredentialBroker only. Ingress is allowed from the control plane only.
- **A fail-closed isolation probe.** At install and at every start, core runs a probe Pod
  in the namespace and checks that the policy is actually enforced. A cluster whose CNI
  ignores NetworkPolicy fails the probe, and the Plugin does not start.
- **Container isolation only.** No gVisor or other sandboxed runtime is required.

### 10. Installation, upgrade, publisher trust, operator ceiling, kill switch

- A **PluginInstallation** is an org's approval to run **one pinned digest** of a Plugin
  with its PluginManifest. Nothing changes without an admin action. There is **no
  auto-apply, ever**: a new digest is new code.
- **Upgrades.** A same-or-narrower manifest is a one-click upgrade by one org admin. A
  wider manifest gets a diff review of only the new capabilities. Elevated tiers
  (`delivered`, `trusted_dom`) need dual approval on every digest.
- **Publisher trust.** A **PluginPublisher** is a signing identity (a Sigstore identity
  or key) an org trusts. **Unsigned images, or images from untrusted publishers, cannot be
  installed.** Scarab's own release identity is trusted by default (first-party). "Which
  plugins do we trust?" is answered as "which publishers?".
- **Operator ceiling.** Above org admins, the operator decides which publishers may be
  trusted at all and whether elevated tiers exist on this deployment.
- **Kill switch.** An installation can be stopped at once, without an upgrade or an
  uninstall. It revokes at the token-to-installation mapping (§11), so the installation's
  reports and broker requests stop being accepted even before its token expires.

### 11. Reports reach core by push, under an identity core sets

- **Push to the broker.** A reporting Plugin pushes DeploymentStatusReports to a reserved
  path on the broker's address (for example `/_scarab/plugin/v1/reports`). The broker is
  core and already the only egress §9 allows, so ingress to Plugins stays control-plane
  only, and no Plugin has to be a server.
- **Projected ServiceAccount token.** The Plugin authenticates with a projected token:
  audience `scarab-broker`, short TTL. Projection works with automount off and under the
  `restricted` level, and the ServiceAccount still has no RBAC. Core verifies the token
  with TokenReview (the chart grants the control plane `system:auth-delegator`) or
  against the cluster's JWKS.
- **Core sets the source.** Core maps the ServiceAccount to its PluginInstallation and
  sets each report's `source` from that mapping, **never from the report's body**
  (ADR-0071 §5). The same mapping selects which manifest's read rules apply to the
  Plugin's other broker requests.
- **Idempotent and sequenced.** Each report carries an idempotency key and a source
  sequence per *(installation, DeploymentTarget)*, and follows the
  DeploymentReporterContract (ADR-0070 §11): transitions only, in source order, never
  replaying older history.
- **Known follow-up:** every board read and gate tick refolds the Environment's input
  log. "Transitions only" bounds its growth; a snapshot or cached fold is still owed.

## Consequences

- Vendor knowledge leaves core. Argo, Flux and the rest are plugins against one contract,
  and the first-party Argo plugin proves that contract is sufficient.
- No plugin holds a prod credential in the default mode, and every write it makes is
  bound to a Run's Attempt, a declared action and a specific DeploymentTarget.
- Actions inherit everything Runs already have: audit, governance, durability, fencing.
  The cost is that a one-click Resync is a Run, with a Run's latency and a row in the
  Runs list.
- Isolation depends on the cluster enforcing NetworkPolicy. The probe makes a cluster
  that does not enforce it fail loudly rather than run unprotected.
- Third-party installs stay **off** until StepUpConfirmation and publisher signatures
  exist. Until then only first-party plugins run.
- New surface: a plugin runtime, a broker, a capability check, `plugin_action`, UI
  extension points, step-up. This is the largest new subsystem since the engine.
- Verifying projected tokens needs TokenReview RBAC (`system:auth-delegator`) for the
  control plane in the chart, or JWKS access.

## Alternatives considered

- **Deployment logic as a plugin, core unaware.** Rejected in ADR-0070 §1. Core must
  understand the status it gates on, and plugins would have to depend on plugins.
- **IR-rewriting control-plane plugins.** Still rejected (ADR-0038): RCE on the durable
  core and an end to self-describing Runs. Plugins here never see the IR.
- **A typed SDK / marketplace for steps.** Still rejected (ADR-0008, ADR-0038). Steps stay
  any container. Plugins add no step SDK.
- **CEL or WASM plugins in the control plane.** Rejected. Observing a CD tool needs
  long-lived connections, streaming and the vendor's native client. CEL cannot express
  that, and WASM would mean rewriting every vendor client and running it inside the
  durable core.
- **Per-verb broker adapters** (core implements "argo sync", "argo get"). Rejected. Core
  would accumulate vendor knowledge one verb at a time, which is exactly what plugins
  exist to keep out. Request rules over the vendor's own API keep core vendor-neutral and
  let the Plugin keep its own client.
- **Credential vending** (short-lived tokens handed to the Plugin). Rejected as the
  default. A vended token is still a credential in the Plugin's memory, usable against
  anything it can reach for its lifetime. Brokering keeps it out entirely. Delivery
  survives only as the governed escape hatch.
- **Same-origin-only plugin UI.** Rejected. It makes every UI contribution equivalent to
  core code. `framed` gives most plugins a native-looking UI with no viewer authority;
  `trusted_dom` remains for the few publishers an operator names.
- **A namespace per plugin, or gVisor.** Rejected for now. One namespace with a
  default-deny policy that includes plugin-to-plugin traffic gives the same network
  isolation more simply. Container isolation matches what Steps already get.
- **Plugins invoking their own actions, or acting outside Runs.** Rejected. Every write
  must sit on a Run's Attempt so governance and fencing apply.
- **Auto-applying new digests from a trusted publisher.** Rejected. A new digest is new
  code, and approval is of *(digest, manifest)*.
- **Pulling reports** (the control plane polls each Plugin). Rejected. Every Plugin would
  have to be a server, and ingress to Plugins would widen beyond what §9 needs.
- **A `source` field in the report body.** Rejected. A Plugin could claim to be another
  source; core sets it from the verified identity.
- **A framed bridge that invokes actions directly.** Rejected. Frame code could fire an
  action as the viewer with no click; the bridge only proposes.

## Implementation horizon

- **MMP:** the plugin runtime (PluginInstallation, PluginPrincipal, `report_deployments`,
  the `scarab-plugins` namespace, NetworkPolicy and the isolation probe, kill switch);
  report push with projected-token authentication (§11); the CredentialBroker with
  **read** rules only; the first-party Argo reporter. Third-party installs **off**. This
  lands at step 2 of ADR-0070's order, right after the minimal org-level Environment and
  together with the read views: an observed-only board, before any Run-side change.
- **MLP:** `offer_actions`, `plugin_action` and UI buttons; per-action write rules with
  `{target.id}` bound in path, query and body and a per-invocation fenced broker
  credential; Environment-scoped PluginConnections; Resync and Argo Rollouts
  promote/abort; `framed` UI with a propose-only bridge.
- **Target state:** StepUpConfirmation; PluginPublisher signatures and the operator
  ceiling (which together switch third-party installs on); `approve_gates`;
  `subscribe_events`; `delivered` credentials; `trusted_dom`.
