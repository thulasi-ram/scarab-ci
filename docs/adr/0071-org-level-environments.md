# 0071. Org-level Environments: a place that decides who may deploy into it

- **Status:** Proposed
- **Date:** 2026-10-22
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0024](0024-environments.md) (Environment is first-class),
  [0037](0037-environment-governance.md) (approval-as-gate, scoped secrets, admission at
  the point of use), [0070](0070-deployments-as-a-vendor-neutral-record.md) (the
  Deployment record an Environment owns)
- **Revises:** [0037](0037-environment-governance.md)'s **per-repo scoping** of
  Environments (§A, §C, §E paths). 0037's other decisions stand: approval is a gate,
  admission runs at the point of use, secrets resolve by key along a scope chain, and
  completeness is usage-driven. 0037 is not edited; this ADR records the change.
- **Relates:** [0039](0039-privileged-images.md) (privilege whitelist),
  [0055](0055-placement-profiles.md) (`k8s_overlay`), [0049](0049-identity-and-access.md)
  (RBAC scopes), [0015](0015-supply-chain-oidc.md) (OIDC subject),
  [0072](0072-plugins-a-governed-extension-point.md) (plugin sources and grants)

## Context

Under ADR-0037, an Environment belongs to a Project. `prod` in `payments` and `prod` in
`checkout` are two unrelated records, each with its own approvers, allowed refs, secrets
and privilege whitelist (`crates/scarab-project/src/lib.rs:60-107`). That was enough while
an Environment governed only a Run. ADR-0070 makes an Environment the owner of
**DeploymentTargets** and **Deployments**, and per-repo scoping now breaks the model:

- **"What is live in prod" has no single answer.** Deployments from twenty Projects into
  one cluster land in twenty unrelated Environments.
- **Nobody owns the place.** A platform team that runs prod cannot set one rule for it.
  Every repository's admin sets their own rules, and the rules diverge silently.
- **A DeploymentTarget belongs to exactly one Environment** (ADR-0070 §3). Under per-repo
  scoping, two Projects that deploy to one Argo Application would each claim it in a
  different Environment.
- **Sources report per place, not per repository.** An Argo project `prod` reports about
  every Application in it, whichever repository changed it.
- **The concurrency rule is dead code.** `ProtectionRules.concurrency`
  (`crates/scarab-project/src/lib.rs:75`) is stored and never read.

## Decision

### 1. An Environment is an org-level place

An **Environment** is an org-level place that deployments land in (staging, prod). It
owns its protection rules (approvers, wait timer, allowed refs, `require_reason`,
concurrency, EnvironmentDeploymentDeadline), the DeploymentTargets in it, and the
Deployments to them. It does not own an OIDC subject: the subject is still minted per
repository from the Run's coordinates (§11). **Projects ask; the Environment
decides.** A Pipeline's `environment: prod` is the *request*. The Environment's own
records are the *authority*.

### 2. EnvironmentProjectEligibility: which Projects may deploy here

**EnvironmentProjectEligibility** is the set of Projects an Environment admits Deploy Runs
from: *all projects*, or an explicit list in which project-name globs are allowed.

- It is **owned by the Environment, never the Project.**
- A new Environment starts with **no** eligible Projects. It is fail-closed.
- It is checked **at Run creation**, before any Pod is created or any secret resolved.
  It is checked **again** wherever the Environment's authority is used later in the Run:
  a manual or external gate does not release (409) for a Project no longer eligible, or
  for an Environment that no longer exists, and **every** step of a Deploy Run is
  re-checked at launch — whether or not it carries secrets, an OIDC token, governed
  grants or a raw overlay — and fails cleanly (a configuration failure with the reason)
  when the Project is no longer eligible. Timer gates are covered by that launch check.
  An admin withdrawing eligibility therefore stops a long Run at its next step, not only
  its next Run.
- Reading and watching an Environment's DeploymentTargets needs only eligibility.

### 3. EnvironmentProjectOverlay: per-project, tighten-only

A Project may carry an **EnvironmentProjectOverlay** over an Environment's protection
rules. It can only tighten:

- **Approvers accumulate.**
- **Allowed refs:** a ref is allowed only if it **matches the baseline and matches the
  overlay**, where an empty list means any ref. This is never computed as an intersection
  of lists: baseline `[refs/heads/main]` and overlay `[refs/tags/*]` intersect to `[]`,
  which would read as "any ref" and fail open.
- **`require_reason`** holds if either the baseline or the overlay sets it.
- **The stricter wait timer and concurrency win.**
- **A tighter EnvironmentDeploymentDeadline** applies only to that Project's
  DeploymentGates. A Deployment's own status uses the baseline deadline, so one
  Deployment has one status whoever requested it (ADR-0070 §10).

A Project can make *its* deploys stricter than the baseline, never looser. With no
overlay, the baseline applies as-is.

### 4. DeploymentTargetProjectOwnership is mirrored from the source

**DeploymentTargetProjectOwnership** is the set of Projects that may *request* Deployments
on, and invoke actions against, a DeploymentTarget. It is **reported by the source**
(derived from the vendor's own metadata, for example an annotation the PluginInstallation
is configured to read) and never assigned in Scarab. Scarab mirrors the source's
authority and does not invent a parallel one. It may name several Projects.

**Where that authority really sits.** Mirroring the source means ownership is, in effect,
**write access to the gitops source** the metadata lives in. In a shared gitops
repository that is whoever can merge there, and a deploy Step that writes to the
repository can write an owner annotation into its own commit. Scarab does not make that
authority stronger than it is; it only refuses to invent a weaker parallel one.

| Act | Needs |
|---|---|
| Read, watch | EnvironmentProjectEligibility |
| Request a Deployment | eligibility + ownership, or an unowned target |
| Invoke a PluginAction | eligibility + ownership (never on an unowned target) |

**Unowned targets.** A request may name a DeploymentTarget **no one owns yet**, for
example a new service whose source object is created by the very change being deployed.
It is recorded *Unconfirmed*, and flagged if the source later reports owners that
exclude the requester. A request is rejected only when **other** Projects own the target.
That rejection and the flag are not built yet (git-bug `9c1173a`). **No PluginAction is offered on a DeploymentTarget with no owners.** The fix is for the
source to report one, not a side door around Runs.

**A request never explains a Deployment a report created.** If a request joins one, it is
recorded as a separate claim beside it, and the Deployment stays Unexplained (ADR-0070
§2). Otherwise any eligible Project could name an unowned target's live version and turn
a hand-applied hotfix into an explained deploy.

### 5. EnvironmentDeploymentSources: which sources may report here

An Environment lists its **EnvironmentDeploymentSources**: each is a PluginInstallation
plus the vendor grouping that maps into this Environment (for example Argo project
`prod`). Reports from any other source into the Environment are **dropped, and the drop
is recorded**.

The **source name is the PluginInstallation's name, assigned by Scarab**, never chosen by
the Plugin. Core sets it on each report from the identity that delivered the report,
never from the report's body (ADR-0072 §11). A plugin therefore cannot claim to be another
source, and two installations of one plugin are two sources. This is the consent half of
reporting: a plugin with `report_deployments` (ADR-0072) can report only into the
Environments that list it.

**The consent unit is the installation.** The vendor grouping is asserted by the plugin,
so listing *(installation, grouping)* trusts the installation to report groupings
honestly; it does not stop that installation from reporting another grouping's target as
this one. One installation per Environment makes the two the same, and is recommended.

**A source moving a target re-homes it.** When a source reports a known DeploymentTarget
under a grouping that maps to another Environment (an Argo Application moved to another
Argo project), it is recorded as a new DeploymentTarget in the new Environment and the
old one ends (ADR-0070 §3). It is not a permanent conflict.

### 6. Secrets: two Environment scopes and a four-step resolution order

- **EnvironmentProjectSecretScope** is one Project within one Environment. It is today's
  per-repo environment scope, renamed.
- **EnvironmentSecretScope** is org-wide for an Environment, shared by every eligible
  Project's Deploy Runs.

Resolution, most specific first:
**EnvironmentProjectSecretScope → EnvironmentSecretScope → Project → Org.** An
Environment-specific value always beats a generic one. This extends the existing chain
(`SecretProvider::resolve`, `crates/scarab-secrets/src/lib.rs:96`) by one link. ADR-0037's
rules are unchanged: reference by key, resolve on the live path, register with the
redactor, and run the usage-driven completeness check. Known gap, not addressed here:
secrets reach the step container as plaintext Pod environment variables (git-bug
`bcf2c11`).

### 7. RBAC gains an Environment scope (MLP)

`Scope` (`crates/scarab-identity/src/lib.rs:67`) is Org or Project today, and no finer
scope exists. In the MLP it gains **Environment**: *Administer on Environment X*. This
lets prod be delegated to a platform team without making it org admin. Org admins hold it
implicitly. Until this lands, **org admins administer Environments**, and "the
Environment's admin" elsewhere in these ADRs means an org admin.

### 8. EnvironmentProjectPrivilegeGrants: privileges per Project within an Environment

The privilege whitelist (ADR-0039: which image digests may use which governed Grants) and
the raw `k8s_overlay` permission (ADR-0055, today `permit_k8s_overlay`) become
**EnvironmentProjectPrivilegeGrants**. Each is granted **to one Project within one
Environment** by that Environment's admin, never to the Environment as a whole. One
team's privileged image never becomes another's, and merging per-repo Environments into
one never widens anyone's privileges.

### 9. Concurrency is per DeploymentTarget

An Environment's concurrency limits in-flight deploys **on one DeploymentTarget**
(at most N), counted as defined below. It is never an org-wide serialisation of the Environment: twenty services
deploying to prod do not queue behind each other. This is the first time the rule is
read at all.

- **What is counted** (as built, ADR-0070 MMP slice (b)) is **requesting Runs**, not
  Deployments. The fold allows at most one in-flight Deployment per target, and two Runs
  requesting the same version join one Deployment yet still count two. A Run is in flight
  on a target if it is not terminal and has a Step that requests the target and is in one
  of two states:
  - `Ready` or `Running`, that is, from its promotion until it finishes;
  - `Succeeded` while some DeploymentGate on that request has no verdict yet.

  A Step that requests the target with no DeploymentGate stops counting when it finishes.
  Counting from the promotion, not from the moment the request is recorded, is
  deliberate. Otherwise two Runs could pass the limit together while both deploy Steps
  run. A Run whose DeploymentGate on the request has **not been reached** (one of the
  gate's needs has not succeeded) stops counting once the Environment's baseline deadline
  has elapsed since the request's effective time, the moment its Deployment reads
  `stalled` on the record. A reached gate counts until its verdict, which the deadline
  bounds. Without this bound, two Runs that acquired two targets in opposite orders, each
  with gates needing both deploys, would hold each other forever (R3). With it, they wait
  at most one deadline, then proceed, and their superseded gates fail `superseded`.
- **It holds, never rejects.** The check runs at the point of use, as a deploy Step is
  about to be promoted, never at admission. It is decided for each Step at its `Pending →
  Ready`, from the same snapshot that found its needs succeeded, so a need finishing
  mid-pass cannot slip a Step past the limit. A Step at the limit stays `Pending`, and its
  Run may suspend while it waits, so the wait bills no Run budget. The Run page shows why
  the Step is held. The limit is the asking Run's Project's effective one, the tighter of
  the baseline and the Project's overlay, and the count covers every Project's Runs on the
  target. If the count cannot be decided, the Step is held (fail closed).
- **A cancelled or rerun holder frees its slot at Step terminal**, before its Pod's
  termination grace ends (git-bug D1).
- **Unset means no limit.** Concurrency is an explicit opt-in.

### 10. Migration that keeps what every Run does today

Existing installations migrate so that every Run the migration can see is admitted and
governed exactly as before:

1. Create **one org Environment per distinct name** (all Projects' `prod` become one
   `prod`), with an **empty, permissive baseline**: no approvers, all refs, no timer, no
   `require_reason`, no concurrency limit.
2. Each Project's old rules become **that Project's EnvironmentProjectOverlay**, including
   `require_reason`. Because the baseline is permissive and overlays only tighten, each
   Project's effective rules equal its old rules.
3. **Concurrency migrates as unset.** The UI stored 1 for every Environment and coerced 0
   to 1 (`ui/scarab-web-ui/src/routes/RepoView.tsx`), and nothing ever read the value
   (`ProtectionRules.concurrency`). Carrying it over would start throttling every target
   the day §9 first reads it. An admin opts in.
4. Old environment secrets move to **EnvironmentProjectSecretScope**.
5. Old privilege whitelists and `permit_k8s_overlay` become **that Project's
   EnvironmentProjectPrivilegeGrants**.
6. **EnvironmentProjectEligibility** is every Project that defined the name, plus every
   Project whose Runs named it without a definition. A name that is only ever
   **referenced**, never defined, gets eligibility **all projects**: today admission
   treats it as a deploy with no protection rules (`resolve_governance`,
   `crates/scarab-server/src/lib.rs`, around lines 3879-3908), so "only the Projects that
   had it" would start rejecting those Runs. The migration finds referenced-only names in
   Run history, and nothing else: no Pipeline YAML is stored where it could read it, so
   what a Pipeline that has never run names is invisible to it. That leaves **two
   never-seen cases**, and both are now refused where they were admitted before:
   - a **name** no Project defined and no Run ever used gets no org Environment, so that
     Pipeline's first Run is refused as an unknown Environment until an admin creates it;
   - a **Project** that names an Environment another Project *did* define, but never ran
     a deploy into it, is not in that Environment's eligibility, so its first Run is
     refused as not eligible until an admin adds it.

   This is the behaviour change, and it is fail-closed on purpose: a scan is impossible
   without a pipeline store, and a grace mode would fail open. A dispatch answers 403
   saying exactly what the admin must do; a webhook names each refused pipeline in its
   delivery body. An org Environment that already existed (created through
   the minimal org-level API before the migration) and that no Project defined is treated
   like a referenced-only name — a pipeline naming it deployed with no rules from any
   Project — and also gets eligibility **all projects**.
7. Old `deployments` rows stay readable as **legacy approval history** (ADR-0070 §1).
   They are not converted into Deployments, because they carry no target or version.
8. A **split report** lists every name that merged across Projects, so an admin can
   split places that are genuinely different (for example a `prod` that is really
   `prod-eu` in one repository). It also lists names migrated with eligibility *all
   projects*, and a name one Project defined that another Project only ran into: that
   Project is eligible under the permissive baseline with no overlay, so the definer's
   rules do not govern it.

### 11. OIDC subject unchanged

The workload-identity subject stays
`scarab:org/<org>/repo/<repo>/env/<env>/ref/<ref>`, minted from the Run's own coordinates
(`Claims::run_subject`, `crates/scarab-identity/src/lib.rs:581-582`). The Environment
moving to org level changes nothing in it, so no cloud trust policy needs editing.

## Consequences

- "What is live in prod" is one org-wide answer, and prod has one owner who can set one
  baseline.
- Separation of duties gets stronger. A Project can no longer create or loosen its own
  Environment; it can only ask for one and tighten its own overlay.
- New Environments are fail-closed. An admin must make Projects eligible before any
  Deploy Run is admitted, and that is a new onboarding step.
- Ownership depends on the source reporting it. A target whose source reports no owner
  can be requested (Unconfirmed) but never acted on through a PluginAction. Ownership is
  only as strong as write access to the gitops source (§4).
- The API surface moves from `/v1/repos/{org}/{repo}/environments/...` to an org-level
  Environment resource with per-Project overlays, grants and secret scopes beneath it.
- The split report is the one place the migration asks an admin to think. Everything
  else is mechanical. The one behaviour change is a name referenced only by a Pipeline
  that has never run (§10).
- Known gap that becomes more visible: an Environment's approvers do not force a gate
  into a Pipeline that omits one (git-bug `cd43bf3`).

## Alternatives considered

- **Keep Environments per-repo (ADR-0037).** Rejected. It cannot give one answer to
  "what is live in prod", gives the place no owner, and conflicts with a DeploymentTarget
  belonging to exactly one Environment.
- **Project opts in to an Environment** (eligibility owned by the Project). Rejected. The
  YAML is the request; letting the requester grant itself access to prod inverts the
  authority.
- **Overlays that can loosen.** Rejected. A Project could then escape the baseline the
  Environment's admin set.
- **Ownership assigned in Scarab** (admin-maintained target-to-project map). Rejected. It
  is a second source of truth that drifts from the CD tool's own metadata.
- **First-claim ownership** (the first Project to request a target owns it). Rejected. A
  race decides authority, and an attacker who names a target first takes it.
- **Reject requests on unowned targets.** Rejected. A new service's source object is
  created by the very change that deploys it, so it could never be deployed through
  Scarab the first time.
- **Plugin-chosen source names.** Rejected. A plugin could impersonate another source.
- **Privilege grants on the Environment as a whole.** Rejected. The merge would widen
  every Project's privileges to the union.
- **Environment-wide concurrency** (one deploy into prod at a time). Rejected. It
  serialises unrelated services and is not what any team asked for.
- **Migrate with a strict merged baseline** (union of approvers, intersection of refs).
  Rejected. It changes behaviour for every Project at once. A permissive baseline plus
  per-Project overlays reproduces today exactly.
- **Migrate concurrency as stored.** Rejected. The stored 1 was a UI default nobody chose
  and nothing read; enforcing it would throttle every target at once.
- **A source moving a target is a standing conflict.** Rejected. Moving an Application
  between Argo projects is routine; it is recorded as a re-home (§5).

## Implementation horizon

- **MMP**, in ADR-0070's order:
  1. **Minimal org-level Environment**: the name and its EnvironmentDeploymentSources,
     enough to key Deployments and route reports. Reads use org Read.
  2. (ADR-0070/0072: the first-party reporter and the read views, an observed-only
     board. DeploymentTargetProjectOwnership starts being reported here.)
  3. **The full migration** (with the split report): EnvironmentProjectEligibility;
     EnvironmentProjectOverlay; EnvironmentProjectSecretScope (the renamed scope);
     EnvironmentProjectPrivilegeGrants.
  4. (ADR-0070: the Run side.) Per-DeploymentTarget concurrency is enforced from here,
     because only requests need it. Ownership was planned for this step too, but its
     rejection of a request is **not built** (git-bug `9c1173a`).

  Org admins administer Environments throughout.
- **MLP:** the RBAC Environment scope (*Administer on Environment X*).
- **Target state:** EnvironmentSecretScope (org-wide per Environment) and the
  four-step resolution order; StepUpConfirmation for anything that loosens an Environment
  (ADR-0072).
