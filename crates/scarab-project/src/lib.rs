//! # scarab-project — the Org → Project → Environment governance model
//!
//! Pure domain crate (ADR-0046, CONTEXT §4.5). A [`Project`] is Scarab's
//! **governed unit of CI** — the aggregate root beneath an [`Org`] that binds
//! a source (a `RepoRef` on a forge) to its governance ([`Environment`]s with
//! [`ProtectionRules`], the privilege whitelist, the secret scope) and owns
//! the pipelines and runs produced from that source. There is **no separate
//! governed "Repo" entity** — a Project *is* the governed repo (1:1 with a
//! `RepoRef` in v1). Depends only on the pure `scarab-secrets` and
//! `scarab-forge` crates — no infra.

use async_trait::async_trait;
use scarab_forge::RepoRef;
use scarab_secrets::SecretScope;
use serde::{Deserialize, Serialize};

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
/// [`Environment`]s with their [`ProtectionRules`], privilege whitelist and
/// secret scope — and owns the pipelines and runs produced from that source.
/// RBAC is enforced at Project scope.
///
/// **1 Project : 1 RepoRef** in v1 (monorepo per-subdir governance is
/// deferred to an optional path scope). The Project's natural key is
/// `(org, name)` where `name` is the repo's forge name — the pair every
/// trigger event carries — which is what the [`EnvironmentStore`] and the
/// deploy-admission paths key on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    /// The owning Org (tenancy boundary).
    pub org: String,
    /// The forge coordinate this Project governs (1:1 in v1).
    pub repo: RepoRef,
    /// The Project's deployment environments with their protection rules.
    #[serde(default)]
    pub environments: Vec<Environment>,
}

impl Project {
    /// The Project's name — its repo's forge name (1:1 in v1), the second half
    /// of the `(org, name)` natural key.
    pub fn name(&self) -> &str {
        &self.repo.name
    }
}

/// A deployment environment with its protection rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub name: String,
    pub protection: ProtectionRules,
}

/// Guardrails that gate deployments into an [`Environment`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionRules {
    /// Principals whose approval is required before a run may proceed.
    pub approvers: Vec<String>,
    /// A mandatory wait timer (seconds) before proceeding.
    pub wait_timer: u32,
    /// Git refs allowed to deploy to this environment (glob patterns).
    pub allowed_refs: Vec<String>,
    /// Max concurrent deployments into this environment.
    pub concurrency: u32,
    /// The secret scope exposed to runs targeting this environment. This is
    /// canonical — fully determined by the environment's `(org, repo, name)`
    /// coordinate — so the server stamps it on write; callers need not (and
    /// cannot meaningfully) supply it. Defaulted here so an API body may omit it.
    #[serde(default = "canonical_scope_placeholder")]
    pub secret_scope: SecretScope,
    /// The OIDC subject claim minted for runs into this environment. Likewise
    /// canonical and server-stamped on write; defaulted so a body may omit it.
    #[serde(default)]
    pub oidc_subject: String,
    /// The privilege whitelist (ADR-0039): which image **digests** may use which
    /// *governed* grants (`add-capabilities`, `privileged`) in this environment.
    /// Written only with the Administer capability (separation of duties).
    /// `run-as-root` is self-service and never appears here.
    #[serde(default)]
    pub privileged_images: Vec<ImageGrant>,
    /// Whether steps targeting this environment may set a raw `k8s_overlay`
    /// (ADR-0055): the governed placement escape hatch. **Off by default
    /// (fail-closed)** — a raw overlay carries no authority and is admitted only
    /// where an admin has opted in. Administer-only, like the privilege whitelist.
    #[serde(default)]
    pub permit_k8s_overlay: bool,
}

/// A stand-in [`SecretScope`] used only as the serde default for
/// [`ProtectionRules::secret_scope`] when a request body omits it. The value is
/// always overwritten with the canonical env-scoped scope on write, so it never
/// escapes as-is — it exists purely so `SecretScope` (which has no `Default`)
/// can be defaulted at the field level.
fn canonical_scope_placeholder() -> SecretScope {
    SecretScope::Org { org: String::new() }
}

/// A per-image entry in an [`Environment`]'s privilege whitelist (ADR-0039): the
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

impl ProtectionRules {
    /// Decide whether a deployment from `git_ref`, carrying `approvals` (the set
    /// of principals who have approved), may proceed — the guardrails enforced at
    /// admission (ADR-0024, 0011). Returns every violation (empty = admitted):
    ///
    /// - **allowed refs**: if `allowed_refs` is non-empty, `git_ref` must match
    ///   one of its globs (`*` wildcards).
    /// - **approvers**: every required approver must appear in `approvals`.
    ///
    /// The `wait_timer` and `concurrency` rules are enforced elsewhere (a timer
    /// gate and a concurrency group, respectively) and are not re-checked here.
    pub fn admits(&self, git_ref: &str, approvals: &[String]) -> Result<(), Vec<String>> {
        let mut violations = Vec::new();

        if !self.ref_allowed(git_ref) {
            violations.push(format!("ref `{git_ref}` is not allowed to deploy here"));
        }
        for approver in &self.approvers {
            if !approvals.iter().any(|a| a == approver) {
                violations.push(format!("missing required approval from `{approver}`"));
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Whether `git_ref` is permitted to deploy here — the allowed-refs half of
    /// [`admits`], checked on its own at run creation (ADR-0037) so a disallowed
    /// ref is rejected even when the environment has no approver gate. An empty
    /// `allowed_refs` permits any ref.
    pub fn ref_allowed(&self, git_ref: &str) -> bool {
        self.allowed_refs.is_empty() || self.allowed_refs.iter().any(|p| glob_match(p, git_ref))
    }

    /// Compute the privilege grant-set admitted for a step running `image` in this
    /// environment (ADR-0039), **fail-closed**. Returns the blessed set, or every
    /// violation (a non-empty error must reject the step, never downgrade it):
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
    pub fn admit_grants(
        &self,
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
            let entry =
                digest.and_then(|d| self.privileged_images.iter().find(|g| g.image_digest == d));
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

/// A recorded deployment into an [`Environment`] — the deployment history
/// (ADR-0024, 0037). Scoped to the owning [`Project`] (`org`/`project`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub org: String,
    /// The owning Project's name (its repo's forge name, 1:1 in v1).
    pub project: String,
    pub environment: String,
    pub git_ref: String,
    pub run: String,
    pub approved_by: Vec<String>,
    /// Unix-ms timestamp the deployment was admitted.
    pub at: i64,
}

/// Errors from the projects/environments store.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("store error: {0}")]
    Store(String),
}

/// Durable store for environments (with their protection rules) and the
/// deployment history recorded against them. Keyed by the owning
/// [`Project`]'s natural key `(org, project)` (ADR-0046) — the project name
/// is its repo's forge name (1:1 in v1), the key a run knows from its
/// trigger (ADR-0037).
#[async_trait]
pub trait EnvironmentStore: Send + Sync {
    /// Create or replace an environment's definition within a Project.
    async fn put_environment(
        &self,
        org: &str,
        project: &str,
        env: &Environment,
    ) -> Result<(), ProjectError>;

    /// Fetch an environment by Project + name.
    async fn get_environment(
        &self,
        org: &str,
        project: &str,
        name: &str,
    ) -> Result<Option<Environment>, ProjectError>;

    /// List the environments defined in a Project.
    async fn list_environments(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<Environment>, ProjectError>;

    /// Remove an environment. Idempotent: removing an absent one is Ok.
    async fn delete_environment(
        &self,
        org: &str,
        project: &str,
        name: &str,
    ) -> Result<(), ProjectError>;

    /// Append a deployment to an environment's history.
    async fn record_deployment(&self, deployment: &Deployment) -> Result<(), ProjectError>;

    /// The deployment history for an environment, most recent first.
    async fn deployments(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Vec<Deployment>, ProjectError>;
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

    fn rules(approvers: &[&str], allowed_refs: &[&str]) -> ProtectionRules {
        ProtectionRules {
            approvers: approvers.iter().map(|s| s.to_string()).collect(),
            wait_timer: 0,
            allowed_refs: allowed_refs.iter().map(|s| s.to_string()).collect(),
            concurrency: 1,
            secret_scope: SecretScope::Org { org: "acme".into() },
            oidc_subject: String::new(),
            privileged_images: Vec::new(),
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
        let r = rules(&["alice"], &["refs/heads/main"]);
        assert!(r.admits("refs/heads/main", &["alice".into()]).is_ok());
    }

    #[test]
    fn rejects_disallowed_ref() {
        let r = rules(&[], &["refs/heads/main"]);
        let err = r.admits("refs/heads/dev", &[]).unwrap_err();
        assert!(err.iter().any(|v| v.contains("not allowed")));
    }

    #[test]
    fn rejects_missing_approver() {
        let r = rules(&["alice", "bob"], &["*"]);
        let err = r.admits("refs/heads/main", &["alice".into()]).unwrap_err();
        assert!(err.iter().any(|v| v.contains("bob")));
        // Both present → admitted.
        assert!(r
            .admits("refs/heads/main", &["alice".into(), "bob".into()])
            .is_ok());
    }

    #[test]
    fn empty_allowed_refs_permits_any_ref() {
        let r = rules(&[], &[]);
        assert!(r.admits("refs/heads/whatever", &[]).is_ok());
    }

    // --- ADR-0039 grants ---------------------------------------------------

    #[test]
    fn run_as_root_is_self_service_no_whitelist_needed() {
        let r = rules(&[], &[]); // empty whitelist
        let g = r
            .admit_grants(&req(true, &[], false), "busybox:latest", false)
            .unwrap();
        assert!(g.run_as_root);
        assert!(!g.privileged);
        assert!(g.add_capabilities.is_empty());
    }

    #[test]
    fn run_as_root_survives_fork_lockout() {
        let r = rules(&[], &[]);
        // Even locked out (fork PR), self-service root is fine — it cannot escape.
        let g = r
            .admit_grants(&req(true, &[], false), "busybox:latest", true)
            .unwrap();
        assert!(g.run_as_root);
    }

    #[test]
    fn privileged_rejected_without_whitelist() {
        let r = rules(&[], &[]);
        let err = r
            .admit_grants(&req(false, &[], true), IMG, false)
            .unwrap_err();
        assert!(err.iter().any(|v| v.contains("not whitelisted")));
    }

    #[test]
    fn privileged_rejected_when_image_not_digest_pinned() {
        let mut r = rules(&[], &[]);
        r.privileged_images = vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec![],
        }];
        // Same repo, but pinned by tag not digest → fail closed.
        let err = r
            .admit_grants(&req(false, &[], true), "ghcr.io/acme/deployer:v1", false)
            .unwrap_err();
        assert!(err.iter().any(|v| v.contains("not pinned to a digest")));
    }

    #[test]
    fn privileged_granted_for_whitelisted_digest() {
        let mut r = rules(&[], &[]);
        r.privileged_images = vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec![],
        }];
        let g = r.admit_grants(&req(false, &[], true), IMG, false).unwrap();
        assert!(g.privileged);
    }

    #[test]
    fn governed_grants_forbidden_for_fork_prs() {
        let mut r = rules(&[], &[]);
        r.privileged_images = vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec!["NET_ADMIN".into()],
        }];
        // Whitelisted digest, but locked_out (fork) → still rejected.
        let err = r
            .admit_grants(&req(false, &[], true), IMG, true)
            .unwrap_err();
        assert!(err.iter().any(|v| v.contains("locked out")));
    }

    #[test]
    fn capabilities_bounded_by_whitelist() {
        let mut r = rules(&[], &[]);
        r.privileged_images = vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: false,
            capabilities: vec!["NET_ADMIN".into()],
        }];
        // Requested cap is whitelisted → admitted.
        let g = r
            .admit_grants(&req(false, &["NET_ADMIN"], false), IMG, false)
            .unwrap();
        assert_eq!(g.add_capabilities, vec!["NET_ADMIN".to_string()]);
        assert!(!g.privileged);
        // A cap outside the allow-list → rejected (fail closed).
        let err = r
            .admit_grants(&req(false, &["SYS_ADMIN"], false), IMG, false)
            .unwrap_err();
        assert!(err.iter().any(|v| v.contains("SYS_ADMIN")));
    }

    #[test]
    fn privileged_granted_but_not_requested_stays_off() {
        let mut r = rules(&[], &[]);
        r.privileged_images = vec![ImageGrant {
            image_digest: DIGEST.into(),
            privileged: true,
            capabilities: vec![],
        }];
        // Grant is a ceiling, not a default: request nothing → nothing escalates.
        let g = r.admit_grants(&req(false, &[], false), IMG, false).unwrap();
        assert!(!g.privileged);
        assert!(!g.run_as_root);
    }
}
