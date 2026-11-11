//! # scarab-project — the Org → Project → Environment governance model
//!
//! Pure domain crate (ADR-0046, CONTEXT §4.5). A [`Project`] is Scarab's
//! **governed unit of CI** — the aggregate root beneath an [`Org`] that binds
//! a source (a `RepoRef` on a forge) to its governance (the org-level
//! [`OrgEnvironment`]s it may deploy into, its overlay and privilege grant in
//! each, the secret scope) and owns the pipelines and runs produced from that
//! source. There is **no separate governed "Repo" entity** — a Project *is*
//! the governed repo (1:1 with a `RepoRef` in v1). Depends only on the pure
//! `scarab-forge` crate — no infra.

use async_trait::async_trait;
use scarab_forge::RepoRef;
use serde::{Deserialize, Serialize};

/// The org-level Environment (ADR-0071), which governs Deploy Runs.
mod org_environment;
pub use org_environment::{
    duplicate_display_name, normalize_display_name, validate_org_environment_display_name,
    validate_org_environment_name, OrgEnvironment, OrgEnvironmentError, OrgEnvironmentStore,
    WritePrecondition, DUPLICATE_DISPLAY_NAME_SUFFIX, MAX_INSTALLATION_NAME_CHARS,
    MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS, MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS,
};

/// The org-level Environment's governance model and its pure resolution
/// (ADR-0071 §2, §3, §8): baseline, tighten-only overlay, eligibility,
/// per-Project privilege grant, and the store admission reads them through.
mod environment_governance;
pub use environment_governance::{
    effective_protection, AllowedRefConstraint, ApprovalGateRules, DeployGovernance,
    EffectiveProtection, EnvironmentCascadeDeletion, EnvironmentGovernanceStore,
    EnvironmentNameClaim, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProjectRecord, EnvironmentProtectionRules,
    MissingGateMode, DEFAULT_MAX_APPROVAL_AGE_S,
};

/// Which Environment governance edits loosen it (ADR-0073 §3): the pure,
/// fail-closed rules that decide whether a conditional write needs a step-up.
mod step_up_loosening;
pub use step_up_loosening::{
    eligibility_loosenings, glob_subsumes, overlay_loosenings, privilege_grant_loosenings,
    protection_loosenings, Loosening,
};

/// A top-level tenant. Owns Projects. The Scarab tenancy boundary — **not**
/// the forge's `owner` namespace (one Org may span a GitHub org *and* a
/// Forgejo instance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Org {
    pub id: String,
    pub slug: String,
}

/// Scarab's **governed unit of CI** and the aggregate root beneath an [`Org`]
/// (ADR-0046, CONTEXT §4.5): binds a **source** (a [`RepoRef`] on a forge,
/// resolved via a `ForgeConnection`) to its **governance** — the
/// [`OrgEnvironment`]s it may deploy into, its overlay and privilege grant in
/// each, and its secret scope — and owns the pipelines and runs produced from
/// that source. RBAC is enforced at Project scope.
///
/// **1 Project : 1 RepoRef** in v1 (monorepo per-subdir governance is
/// deferred to an optional path scope). The Project's natural key is
/// `(org, name)` where `name` is the repo's forge name — the pair every
/// trigger event carries — which is what the deploy-admission paths key on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    /// The owning Org (tenancy boundary).
    pub org: String,
    /// The forge coordinate this Project governs (1:1 in v1).
    pub repo: RepoRef,
}

impl Project {
    /// The Project's name — its repo's forge name (1:1 in v1), the second half
    /// of the `(org, name)` natural key.
    pub fn name(&self) -> &str {
        &self.repo.name
    }
}

/// A per-image entry in a privilege whitelist (ADR-0039): the
/// *governed* grants a specific image **digest** may use. Governed grants are
/// digest-keyed — a mutable tag is a supply-chain hole, so the bytes an admin
/// approved are pinned exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageGrant {
    /// The image digest this entry grants to, e.g. `sha256:abc…`.
    pub image_digest: String,
    /// Whether this digest may run as a **privileged** container (node-escape
    /// power — `privileged` is digest-keyed *forever*, ADR-0039).
    #[serde(default)]
    pub privileged: bool,
    /// The exact Linux capabilities this digest is allowed to add. A step may add
    /// only capabilities that appear here (the admin bounds the blast radius).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// What a step **requests** (the pipeline author's intent; carries no authority
/// on its own — ADR-0039). Governed grants become effective only if the
/// Environment whitelist blesses them at admission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRequest {
    /// Run as uid 0 (opt out of the baseline `runAsNonRoot`). Self-service.
    pub run_as_root: bool,
    /// Linux capabilities the step wants added (governed).
    pub add_capabilities: Vec<String>,
    /// Run as a privileged container (governed).
    pub privileged: bool,
}

/// The escalations admission actually **blessed** (ADR-0039). The executor applies
/// exactly this to the Pod `SecurityContext` and nothing more — fail-closed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmittedGrants {
    pub run_as_root: bool,
    pub add_capabilities: Vec<String>,
    pub privileged: bool,
}

/// The ADR-0039 grant decision for a step running `image` against one
/// privilege `whitelist`, **fail-closed**: the body of
/// [`EnvironmentProjectPrivilegeGrant::admit_grants`]. Returns the blessed
/// set, or every violation (a non-empty error must reject the step, never
/// downgrade it):
///
/// - **`run-as-root`** is *self-service*: admitted iff requested. Root inside
///   the hardened (caps-dropped, priv-esc-off, seccomp) sandbox does not escape,
///   so it needs no whitelist. Not suppressed by `locked_out`.
/// - **`add-capabilities` / `privileged`** are *governed*: admitted only if
///   `image` is pinned to a **digest** that has a whitelist entry granting them.
///   A digest-less image, an un-whitelisted digest, or a capability outside the
///   entry's allow-list is a violation.
/// - **`locked_out`** (fork-PR, ADR-0037) forbids *all governed* grants outright;
///   self-service run-as-root is unaffected.
fn admit_grants_against_whitelist(
    whitelist: &[ImageGrant],
    req: &GrantRequest,
    image: &str,
    locked_out: bool,
) -> Result<AdmittedGrants, Vec<String>> {
    let mut admitted = AdmittedGrants::default();
    let mut violations = Vec::new();

    // run-as-root: self-service, sandbox-bound.
    admitted.run_as_root = req.run_as_root;

    let wants_governed = req.privileged || !req.add_capabilities.is_empty();
    if wants_governed {
        if locked_out {
            return Err(vec![
                "fork-PR run is locked out of governed grants (add-capabilities/privileged)"
                    .to_string(),
            ]);
        }
        let digest = image_digest(image);
        let entry = digest.and_then(|d| whitelist.iter().find(|g| g.image_digest == d));
        match (digest, entry) {
            (None, _) => violations.push(format!(
                "image `{image}` requests a governed grant but is not pinned to a digest (repo@sha256:…)"
            )),
            (Some(d), None) => violations.push(format!(
                "image digest `{d}` is not whitelisted for governed grants in this environment"
            )),
            (Some(_), Some(entry)) => {
                if req.privileged {
                    if entry.privileged {
                        admitted.privileged = true;
                    } else {
                        violations.push(
                            "`privileged` is not granted to this image digest".to_string(),
                        );
                    }
                }
                for cap in &req.add_capabilities {
                    if entry.capabilities.iter().any(|c| c == cap) {
                        admitted.add_capabilities.push(cap.clone());
                    } else {
                        violations.push(format!(
                            "capability `{cap}` is not granted to this image digest"
                        ));
                    }
                }
            }
        }
    }

    if violations.is_empty() {
        Ok(admitted)
    } else {
        Err(violations)
    }
}

/// Extract the `sha256:…` digest from a digest-pinned image reference
/// (`repo@sha256:…`). Returns `None` when the image is not pinned by digest —
/// which fail-closes governed grants (ADR-0039).
fn image_digest(image: &str) -> Option<&str> {
    image
        .rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| d.starts_with("sha256:"))
}

/// Errors from the projects/environments store.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("store error: {0}")]
    Store(String),
}

/// Which column of a Project's secret-coverage view a marker sits in: one of its
/// Environments, or the Project's own repo-scope default (`None`).
///
/// Deliberately *not* [`crate`]-external scope machinery — it addresses a cell of
/// an advisory view, not a place a value can be stored. Secret storage scoping
/// lives in `scarab-secrets`.
pub type CoverageColumn<'a> = Option<&'a str>;

/// Markers saying a key is **deliberately** absent at a scope (ADR-0037 D).
///
/// Cross-environment secret parity is advisory UI only: it can show a gap that
/// is a real mistake or a considered decision, and nothing in the data
/// distinguishes them — so a reviewer re-litigates the same cell forever. A
/// marker silences one cell, recording "we meant this."
///
/// It has **no runtime effect**: it never blocks a deploy, never changes
/// resolution, and is not consulted on the run path. A deployment that cannot
/// store markers still runs governance — the matrix simply shows no silenced
/// cells.
#[async_trait]
pub trait SecretCoverageStore: Send + Sync {
    /// Mark `key` as intentionally unset in `column`. Idempotent.
    async fn silence(
        &self,
        org: &str,
        project: &str,
        column: CoverageColumn<'_>,
        key: &str,
    ) -> Result<(), ProjectError>;

    /// Drop the marker for `key` in `column`. Idempotent.
    async fn unsilence(
        &self,
        org: &str,
        project: &str,
        column: CoverageColumn<'_>,
        key: &str,
    ) -> Result<(), ProjectError>;

    /// Every marker in a Project, as `(column, key)` — `None` column is the
    /// repo-scope default.
    async fn silenced(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<(Option<String>, String)>, ProjectError>;
}

/// Minimal glob match supporting `*` (matches any run of characters). Used for
/// `allowed_refs` patterns like `refs/heads/*`.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Classic two-pointer wildcard match with backtracking on the last `*`.
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Environment baseline with these approvers and allowed refs.
    fn rules(approvers: &[&str], allowed_refs: &[&str]) -> EnvironmentProtectionRules {
        EnvironmentProtectionRules {
            approvers: approvers.iter().map(|s| s.to_string()).collect(),
            allowed_refs: allowed_refs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A Project's privilege grant with this whitelist.
    fn grant(privileged_images: Vec<ImageGrant>) -> EnvironmentProjectPrivilegeGrant {
        EnvironmentProjectPrivilegeGrant {
            org: "acme".into(),
            environment: "prod".into(),
            project: "web".into(),
            privileged_images,
            permit_k8s_overlay: false,
        }
    }

    const DIGEST: &str = "sha256:aaaa";
    const IMG: &str = "ghcr.io/acme/deployer@sha256:aaaa";

    fn req(run_as_root: bool, caps: &[&str], privileged: bool) -> GrantRequest {
        GrantRequest {
            run_as_root,
            add_capabilities: caps.iter().map(|s| s.to_string()).collect(),
            privileged,
        }
    }

    #[test]
    fn glob_matches_wildcards() {
        assert!(glob_match("refs/heads/main", "refs/heads/main"));
        assert!(glob_match("refs/heads/*", "refs/heads/main"));
        assert!(glob_match("refs/heads/release/*", "refs/heads/release/1.2"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("refs/heads/main", "refs/heads/dev"));
        assert!(!glob_match("refs/heads/*", "refs/tags/v1"));
    }

    #[test]
    fn admits_when_ref_allowed_and_approvals_present() {
        let r = effective_protection(&rules(&["alice"], &["refs/heads/main"]), None);
        assert!(r.admits("refs/heads/main", &["alice".into()]).is_ok());
    }

    #[test]
    fn rejects_disallowed_ref() {
        let r = effective_protection(&rules(&[], &["refs/heads/main"]), None);
        let err = r.admits("refs/heads/dev", &[]).unwrap_err();
        assert!(err.iter().any(|v| v.contains("not allowed")));
    }

    #[test]
    fn rejects_missing_approver() {
        let r = effective_protection(&rules(&["alice", "bob"], &["*"]), None);
        let err = r.admits("refs/heads/main", &["alice".into()]).unwrap_err();
        assert!(err.iter().any(|v| v.contains("bob")));
        // Both present → admitted.
        assert!(r
            .admits("refs/heads/main", &["alice".into(), "bob".into()])
            .is_ok());
    }

    #[test]
    fn empty_allowed_refs_permits_any_ref() {
        let r = effective_protection(&rules(&[], &[]), None);
        assert!(r.admits("refs/heads/whatever", &[]).is_ok());
    }

    // --- ADR-0057 require_reason ------------------------------------------

    #[test]
    fn require_reason_blocks_an_empty_reason_human_dispatch() {
        let mut b = rules(&[], &[]);
        b.require_reason = true;
        let r = effective_protection(&b, None);
        // manual/api with no reason → blocked, violation reads like a gate miss.
        let err = r.admits_reason(true, false).unwrap_err();
        assert!(err.iter().any(|v| v.contains("reason is required")));
    }

    #[test]
    fn require_reason_admits_a_human_dispatch_that_carries_a_reason() {
        let mut b = rules(&[], &[]);
        b.require_reason = true;
        let r = effective_protection(&b, None);
        assert!(r.admits_reason(true, true).is_ok());
    }

    #[test]
    fn require_reason_exempts_non_human_triggers_regardless() {
        let mut b = rules(&[], &[]);
        b.require_reason = true;
        let r = effective_protection(&b, None);
        // A push (is_human_dispatch == false) is never blocked, reason or not.
        assert!(r.admits_reason(false, false).is_ok());
        assert!(r.admits_reason(false, true).is_ok());
    }

    #[test]
    fn require_reason_off_never_blocks() {
        // Fail-open: existing environments (require_reason unset) are unaffected.
        let r = effective_protection(&rules(&[], &[]), None);
        assert!(!r.require_reason);
        assert!(r.admits_reason(true, false).is_ok());
    }

    // --- ADR-0039 grants ---------------------------------------------------

    #[test]
    fn run_as_root_is_self_service_no_whitelist_needed() {
        let r = grant(vec![]); // empty whitelist
        let g = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(true, &[], false),
            "busybox:latest",
            false,
        )
        .unwrap();
        assert!(g.run_as_root);
        assert!(!g.privileged);
        assert!(g.add_capabilities.is_empty());
    }

    #[test]
    fn run_as_root_survives_fork_lockout() {
        let r = grant(vec![]);
        // Even locked out (fork PR), self-service root is fine — it cannot escape.
        let g = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(true, &[], false),
            "busybox:latest",
            true,
        )
        .unwrap();
        assert!(g.run_as_root);
    }

    #[test]
    fn privileged_rejected_without_whitelist() {
        let r = grant(vec![]);
        let err = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &[], true),
            IMG,
            false,
        )
        .unwrap_err();
        assert!(err.iter().any(|v| v.contains("not whitelisted")));
    }

    #[test]
    fn privileged_rejected_when_image_not_digest_pinned() {
        let r = grant(vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec![],
        }]);
        // Same repo, but pinned by tag not digest → fail closed.
        let err = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &[], true),
            "ghcr.io/acme/deployer:v1",
            false,
        )
        .unwrap_err();
        assert!(err.iter().any(|v| v.contains("not pinned to a digest")));
    }

    #[test]
    fn privileged_granted_for_whitelisted_digest() {
        let r = grant(vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec![],
        }]);
        let g = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &[], true),
            IMG,
            false,
        )
        .unwrap();
        assert!(g.privileged);
    }

    #[test]
    fn governed_grants_forbidden_for_fork_prs() {
        let r = grant(vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec!["NET_ADMIN".into()],
        }]);
        // Whitelisted digest, but locked_out (fork) → still rejected.
        let err = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &[], true),
            IMG,
            true,
        )
        .unwrap_err();
        assert!(err.iter().any(|v| v.contains("locked out")));
    }

    #[test]
    fn capabilities_bounded_by_whitelist() {
        let r = grant(vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: false,
            capabilities: vec!["NET_ADMIN".into()],
        }]);
        // Requested cap is whitelisted → admitted.
        let g = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &["NET_ADMIN"], false),
            IMG,
            false,
        )
        .unwrap();
        assert_eq!(g.add_capabilities, vec!["NET_ADMIN".to_string()]);
        assert!(!g.privileged);
        // A cap outside the allow-list → rejected (fail closed).
        let err = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &["SYS_ADMIN"], false),
            IMG,
            false,
        )
        .unwrap_err();
        assert!(err.iter().any(|v| v.contains("SYS_ADMIN")));
    }

    #[test]
    fn privileged_granted_but_not_requested_stays_off() {
        let r = grant(vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec![],
        }]);
        // Grant is a ceiling, not a default: request nothing → nothing escalates.
        let g = EnvironmentProjectPrivilegeGrant::admit_grants(
            Some(&r),
            &req(false, &[], false),
            IMG,
            false,
        )
        .unwrap();
        assert!(!g.privileged);
        assert!(!g.run_as_root);
    }
}
