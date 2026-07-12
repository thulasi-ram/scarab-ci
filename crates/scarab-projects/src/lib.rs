//! # scarab-projects — org / repo / project / environment model
//!
//! Pure domain crate. Holds the tenancy + configuration model, including
//! environment [`ProtectionRules`]. Depends only on `serde` and the pure
//! `scarab-secrets` crate (for [`SecretScope`]) — no infra.

use async_trait::async_trait;
use scarab_secrets::SecretScope;
use serde::{Deserialize, Serialize};

/// A top-level tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Org {
    pub id: String,
    pub slug: String,
}

/// A repository owned by an org.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub org: String,
    pub name: String,
}

/// A project groups pipelines/config within a repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub repo: String,
    pub name: String,
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
    /// The secret scope exposed to runs targeting this environment.
    pub secret_scope: SecretScope,
    /// The OIDC subject claim minted for runs into this environment.
    pub oidc_subject: String,
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

        if !self.allowed_refs.is_empty()
            && !self.allowed_refs.iter().any(|p| glob_match(p, git_ref))
        {
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
}

/// A recorded deployment into an [`Environment`] — the deployment history
/// (ADR-0024).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
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
/// deployment history recorded against them.
#[async_trait]
pub trait EnvironmentStore: Send + Sync {
    /// Create or replace an environment's definition within a project.
    async fn put_environment(&self, project: &str, env: &Environment)
        -> Result<(), ProjectError>;

    /// Fetch an environment by project + name.
    async fn get_environment(
        &self,
        project: &str,
        name: &str,
    ) -> Result<Option<Environment>, ProjectError>;

    /// Append a deployment to an environment's history.
    async fn record_deployment(&self, deployment: &Deployment) -> Result<(), ProjectError>;

    /// The deployment history for an environment, most recent first.
    async fn deployments(
        &self,
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
}
