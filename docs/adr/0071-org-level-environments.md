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
owns its protection rules (approvers, wait timer, allowed refs, concurrency, OIDC subject),
the DeploymentTargets in it, and the Deployments to them. **Projects ask; the Environment
decides.** A Pipeline's `environment: prod` is the *request*. The Environment's own
records are the *authority*.

### 2. EnvironmentProjectEligibility: which Projects may deploy here

**EnvironmentProjectEligibility** is the set of Projects an Environment admits Deploy Runs
from: *all projects*, or an explicit list in which project-name globs are allowed.

- It is **owned by the Environment, never the Project.**
- A new Environment starts with **no** eligible Projects. It is fail-closed.
- It is checked **at Run creation**, before any Pod is created or any secret resolved.
- Reading and watching an Environment's DeploymentTargets needs only eligibility.

### 3. EnvironmentProjectOverlay: per-project, tighten-only

A Project may carry an **EnvironmentProjectOverlay** over an Environment's protection
rules. It can only tighten: approvers accumulate, allowed refs narrow (the intersection),
and the stricter wait timer, concurrency and EnvironmentDeploymentDeadline win. A Project
can make *its* deploys stricter than the baseline, never looser. With no overlay, the
baseline applies as-is.

### 4. DeploymentTargetProjectOwnership is mirrored from the source

**DeploymentTargetProjectOwnership** is the set of Projects that may *request* Deployments
on, and invoke actions against, a DeploymentTarget. It is **reported by the source**
(derived from the vendor's own metadata, for example an annotation the PluginInstallation
is configured to read) and never assigned in Scarab. Scarab mirrors the source's
authority and does not invent a parallel one. It may name several Projects.

| Act | Needs |
|---|---|
| Read, watch | EnvironmentProjectEligibility |
| Request a Deployment | eligibility + ownership, or an unowned target |
| Invoke a PluginAction | eligibility + ownership (never on an unowned target) |

**Unowned targets.** A request may name a DeploymentTarget **no one owns yet**, for
example a new service whose source object is created by the very change being deployed.
It is recorded *Unconfirmed*, and flagged if the source later reports owners that
exclude the requester. A request is rejected only when **other** Projects own the target.
**No PluginAction is offered on a DeploymentTarget with no owners.** The fix is for the
source to report one, not a side door around Runs.

### 5. EnvironmentDeploymentSources: which sources may report here

An Environment lists its **EnvironmentDeploymentSources**: each is a PluginInstallation
plus the vendor grouping that maps into this Environment (for example Argo project
`prod`). Reports from any other source into the Environment are **dropped, and the drop
is recorded**.

The **source name is the PluginInstallation's name, assigned by Scarab**, never chosen by
the Plugin. A plugin therefore cannot claim to be another source, and two installations
of one plugin are two sources. This is the consent half of reporting: a plugin with
`report_deployments` (ADR-0072) can report only into the Environments that list it.

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

### 7. RBAC gains an Environment scope

`Scope` (`crates/scarab-identity/src/lib.rs:67`) is Org or Project today. It gains
**Environment**: *Administer on Environment X*. This lets prod be delegated to a platform
team without making it org admin. Org admins hold it implicitly. Until this lands, org
admins administer Environments.

### 8. EnvironmentProjectPrivilegeGrants: privileges per Project within an Environment

The privilege whitelist (ADR-0039: which image digests may use which governed Grants) and
the raw `k8s_overlay` permission (ADR-0055, today `permit_k8s_overlay`) become
**EnvironmentProjectPrivilegeGrants**. Each is granted **to one Project within one
Environment** by that Environment's admin, never to the Environment as a whole. One
team's privileged image never becomes another's, and merging per-repo Environments into
one never widens anyone's privileges.

### 9. Concurrency is per DeploymentTarget

An Environment's concurrency limits in-flight Deployments **on one DeploymentTarget**
(at most N). It is never an org-wide serialisation of the Environment: twenty services
deploying to prod do not queue behind each other. This is the first time the rule is
read at all.

### 10. Migration with no behaviour change

Existing installations migrate without changing what any Run does:

1. Create **one org Environment per distinct name** (all Projects' `prod` become one
   `prod`), with an **empty, permissive baseline**: no approvers, all refs, no timer.
2. Each Project's old rules become **that Project's EnvironmentProjectOverlay**. Because
   the baseline is permissive and overlays only tighten, each Project's effective rules
   equal its old rules.
3. Old environment secrets move to **EnvironmentProjectSecretScope**.
4. Old privilege whitelists and `permit_k8s_overlay` become **that Project's
   EnvironmentProjectPrivilegeGrants**.
5. **EnvironmentProjectEligibility = exactly the Projects that had that Environment.**
6. Old `deployments` rows stay readable as **legacy approval history** (ADR-0070 §1).
   They are not converted into Deployments, because they carry no target or version.
7. A **split report** lists every name that merged across Projects, so an admin can
   split places that are genuinely different (for example a `prod` that is really
   `prod-eu` in one repository).

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
  can be requested (Unconfirmed) but never acted on through a PluginAction.
- The API surface moves from `/v1/repos/{org}/{repo}/environments/...` to an org-level
  Environment resource with per-Project overlays, grants and secret scopes beneath it.
- The split report is the one place the migration asks an admin to think. Everything
  else is mechanical.
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

## Implementation horizon

- **MMP:** org-level Environments and the migration (with the split report);
  EnvironmentProjectEligibility; EnvironmentProjectOverlay; EnvironmentDeploymentSources;
  DeploymentTargetProjectOwnership mirrored from the source; EnvironmentProjectSecretScope
  (the renamed scope); EnvironmentProjectPrivilegeGrants; per-DeploymentTarget
  concurrency. Org admins administer Environments.
- **MLP:** the RBAC Environment scope (*Administer on Environment X*).
- **Target state:** EnvironmentSecretScope (org-wide per Environment) and the
  four-step resolution order; StepUpConfirmation for anything that loosens an Environment
  (ADR-0072).
