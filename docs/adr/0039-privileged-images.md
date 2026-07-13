# 0039. Privileged step images: hardened baseline + governed capability grants per Environment

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** thulasi.ram (architect)
- **Builds on:** [0037](0037-environment-governance.md) (Environment = admin-owned governed bundle), [0038](0038-invoke-and-local-reuse.md) (third-party reuse = OCI image), [0008](0008-step-contract.md) (OCI image = the step), [0005](0005-tenancy-and-k8s-only.md) (soft tenancy), [0018](0018-image-building.md) (rootless BuildKit)
- **Depends-later-on:** [0015](0015-supply-chain-oidc.md) (provenance/cosign — deferred; not required to ship this)

## Context

[0038](0038-invoke-and-local-reuse.md) established that reuse across a trust boundary is an
**OCI image Step** — isolated by its container. Some third-party images legitimately need
elevated privilege (root, extra Linux capabilities, a privileged container for
device access). The question is how to grant that **without** handing arbitrary whitelisted
images a node- or cloud-account-escape on a **soft-multitenant** cluster ([0005](0005-tenancy-and-k8s-only.md):
namespace-per-run, gVisor/Kata deferred).

Two facts force the shape of the decision:

- **There is no floor today.** A normal step Pod is built with **no `SecurityContext` at all**
  (`scarab-executor-k8s`: only the rootless BuildKit pod is hardened). You cannot whitelist
  "privileged" as an *exception* until "unprivileged" is a defined, enforced baseline.
- **Privilege on a shared node is a cross-tenant weapon.** `privileged: true` in a per-run
  namespace still shares the node kernel; `hostNetwork` reaches the node metadata endpoint and
  its IAM credentials. The whitelist is the only thing between "reusable deploy image" and "one
  repo owns the cluster/cloud account" — so it must be admin-owned, digest-pinned, fail-closed.

## Decision

1. **Hardened baseline (the floor).** Every step Pod runs a **Kubernetes "restricted"-equivalent
   posture** by default: `runAsNonRoot: true`, drop **ALL** capabilities,
   `seccompProfile: RuntimeDefault`, `allowPrivilegeEscalation: false`, no host namespaces, no
   hostPath. This is what "unprivileged plugin" means; every escalation below is an explicit,
   named exception to it. Fail-closed.

2. **Privilege is a closed, Scarab-defined capability set — not a binary.** A single "privileged"
   flag is a foot-gun (grants everything, blinds the audit). v1 vocabulary:
   - **`run-as-root`** — `runAsNonRoot:false`, uid 0. The most common baseline break; does **not**
     escape the (caps-dropped, priv-esc-off, seccomp-confined) sandbox.
   - **`add-capabilities`** — adds specific Linux caps; the **exact caps are pinned in the
     Environment whitelist**, not in the pipeline (one grant kind, blast-radius pinned by the admin).
   - **`privileged`** — full privileged container. The blunt "node-level power" escape hatch of
     last resort; loud and rare.

   **Explicitly out / deferred, and *never folded into* `privileged`** (they are orthogonal axes,
   not rungs of one ladder — `privileged` does **not** include the host namespaces):
   - `cluster-access` (in-cluster k8s API) — **out**. Deploys reach a target cluster via
     **kubeconfig-as-a-secret** ([0037](0037-environment-governance.md)); Scarab's own cluster API
     is off-limits to step pods.
   - `host-path` — deferred (rootless BuildKit already removed the docker-socket reason;
     `privileged` reaches it if truly needed).
   - `host-network` / `host-pid` / `host-ipc` — **provisionally a hard no**: `hostNetwork` →
     node metadata endpoint → node IAM credential theft → cloud-account compromise. If ever
     added, behind a louder opt-in than the others and/or hard-tenant isolation.

3. **Request / grant split (separation of duties).** The **pipeline author requests** a grant on a
   step (carries no authority). The **Environment admin grants** it via a whitelist entry in the
   Environment's `ProtectionRules` (writable only with the **Administer** capability). Fail-closed:
   requested-but-not-granted ⇒ the step is **rejected at admission** with a diagnostic naming the
   missing grant (never silently downgraded). A grant is a **ceiling, not a default** — granted-but-
   not-requested escalates nothing.

4. **Self-service vs governed, split by whether the grant escapes the container.**
   - **`run-as-root` is self-service** — author requests it, no admin, no whitelist. Root inside a
     caps-dropped, unprivileged, seccomp-confined container does not escape the sandbox; it's the
     weakest (defense-in-depth) leg of "restricted". Governing it buys friction, not safety — and a
     restricted baseline that made *every* root-needing image an admin ticket would be unusable.
   - **`add-capabilities` and `privileged` are governed** — Environment whitelist, digest-pinned,
     Administer-only. These cross the isolation boundary.

   The floor keeps every **hard** boundary (no privileged, caps dropped, no priv-esc, seccomp, no
   host namespaces); authors may self-opt-out only of the **soft** non-root leg. Net posture is
   stricter than GHA/GitLab (which run as root freely) yet usable without tickets.

5. **Keying tiered to blast radius.**
   - **`privileged` → digest (`@sha256:…`) only, forever.** No signature shortcut for the
     node-escape hammer; the friction (an admin re-approves exact bytes on every image bump) *is*
     the separation-of-duties control.
   - **`run-as-root` / `add-capabilities` → digest now; a signature/provenance rule allowed *later*,
     once [0015](0015-supply-chain-oidc.md) ships.**
   - **Until provenance exists → digest-only for all.** So this ADR ships with **zero dependency**
     on unbuilt provenance machinery. Recommended operational pattern for third-party privileged
     images: **mirror into your own registry and self-sign on ingest.**

6. **Enforcement.** **Admission** (in the engine — [0037](0037-environment-governance.md) moved it
   there) computes the **admitted grant-set** as a pure function of `{the run's Environment whitelist,
   the step's request, the fork-lockout flag}`. The admitted set flows to the **executor**, which
   applies **exactly** it to the Pod `SecurityContext` and **fail-closes** — it never applies an
   escalation admission did not bless.

## Consequences

- **Privileged requires an Environment.** Grants come only from an Environment whitelist, so a run
  with no environment target (ordinary PR CI) cannot obtain `add-capabilities`/`privileged` — they
  fail closed. Acceptable: the classic "need privileged in CI" case (DinD image build) is already
  served **unprivileged** by rootless BuildKit ([0018](0018-image-building.md)).
- **Fork-PR lockout extends to grants.** As fork PRs get no secrets
  ([0037](0037-environment-governance.md)), a fork-PR run gets **no governed grants** — reusing the
  existing `locked_out` flag. An external PR author can never request privileged.
- **A restricted baseline will break root-assuming images** until they add `run-as-root` — a
  deliberate, self-service, one-line opt-out. Documented, not silent.
- **Work to build:** baseline `SecurityContext` on the normal step Pod; the grant vocabulary +
  whitelist on `ProtectionRules` (pure domain); the `security:` request field on `StepSpec` +
  validation; the pure admitted-grant-set function; executor application + fail-close; a Postgres
  migration for the whitelist; fork-lockout suppression.

## Alternatives considered

- **A binary `privileged` flag** — one switch that grants everything. Rejected: foot-gun,
  audit-blinding, no least-privilege.
- **Folding `host-path`/`host-network` under `privileged`** — technically wrong (`privileged` does
  not include host namespaces) and would blind the audit. Rejected; they are distinct orthogonal
  grants, deferred.
- **`cluster-access` in v1** — a step talking to Scarab's own cluster API is a privilege-escalation
  path to the control plane on a soft-multitenant system. Rejected; use kubeconfig-as-secret.
- **Signature keying for `privileged`** — more ergonomic but trusts the *signer forever* rather than
  the *exact bytes* an admin approved, adds I/O to admission, and depends on deferred provenance.
  Rejected for the node-escape grant; allowed later for the low-risk grants only.
- **Governing `run-as-root`** — maximally strict but makes the restricted baseline an admin-ticket
  generator for benign images. Rejected in favour of self-service, since root cannot escape the
  hardened sandbox.
