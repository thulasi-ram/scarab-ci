//! # scarab-projects — org / repo / project / environment model
//!
//! Pure domain crate. Holds the tenancy + configuration model, including
//! environment [`ProtectionRules`]. Depends only on `serde` and the pure
//! `scarab-secrets` crate (for [`SecretScope`]) — no infra.

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
