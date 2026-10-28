//! The **org-level Environment** (ADR-0071): a name, a description and —
//! since the full migration (c-full sub-slice c2) — its baseline protection
//! rules and EnvironmentProjectEligibility.
//!
//! This is CONTEXT §4.1's **Environment**. It is named [`OrgEnvironment`]
//! only because the per-Project ADR-0037 struct still owns the name
//! [`Environment`](crate::Environment) as the legacy shape the migration
//! reads; the mechanical rename to `Environment` is sub-slice c3.
//!
//! The two **no longer coexist** for admission: an OrgEnvironment **is** what
//! governs a Deploy Run (`resolve_governance` in the server, through
//! [`EnvironmentGovernanceStore`](crate::EnvironmentGovernanceStore)), and a
//! name with no OrgEnvironment is refused (ADR-0071 §10).

use async_trait::async_trait;

use crate::{EnvironmentProjectEligibility, EnvironmentProtectionRules};

/// The longest OrgEnvironment description, in characters.
pub const MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS: usize = 1000;

/// The longest PluginInstallation name: a DNS-1123 label (ADR-0072 names an
/// installation the way Kubernetes names a label).
pub const MAX_INSTALLATION_NAME_CHARS: usize = 63;

/// An org-level Environment (ADR-0071 §1). Per-Project overlays and privilege
/// grants are not part of it — they belong to one Project each and are read
/// with it only through
/// [`DeployGovernance`](crate::DeployGovernance). See the module docs for why
/// it is not yet called `Environment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgEnvironment {
    pub org: String,
    /// The Environment's name, the key per-Project Environments share
    /// (exact, case-sensitive).
    pub name: String,
    /// Free text for people; never interpreted.
    pub description: String,
    /// The baseline protection rules every eligible Project's Deploy Runs
    /// obey (ADR-0071 §1), before any Project's overlay. Written only by
    /// `EnvironmentGovernanceStore::put_environment_protection`:
    /// [`OrgEnvironmentStore::put_org_environment`] ignores it, so the c-min
    /// whole-Environment replace can never reset it by omission.
    pub protection: EnvironmentProtectionRules,
    /// Which Projects may deploy here (ADR-0071 §2). Written only by
    /// `EnvironmentGovernanceStore::put_environment_eligibility`, for the
    /// same reason; a new Environment starts with none (fail-closed).
    pub eligibility: EnvironmentProjectEligibility,
    /// Unix-ms it was first created. Kept across replaces.
    pub created_at: i64,
    /// Unix-ms it was last written.
    pub updated_at: i64,
    /// The subject of the Principal that last wrote it.
    pub updated_by: String,
}

impl OrgEnvironment {
    /// Check everything the type does not already guarantee: the name rule,
    /// the description length and the eligibility patterns.
    pub fn validate(&self) -> Result<(), OrgEnvironmentError> {
        validate_org_environment_name(&self.name)?;
        if self.description.chars().count() > MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS {
            return Err(OrgEnvironmentError::Invalid(format!(
                "description is longer than {MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS} characters"
            )));
        }
        self.eligibility.validate()
    }
}

/// The OrgEnvironment name rule: non-empty, no control characters, and not
/// `.` or `..`. Nothing else — no length cap, and `/` and whitespace are
/// allowed — because a pipeline's `environment:` and a per-Project
/// Environment's name only have to be non-empty today, and every such name
/// must be registrable here or the full migration (which merges them by
/// name, ADR-0071 §10) would strand it. The one exception is `.` / `..`:
/// a client normalises those path segments away, so an Environment so named
/// could be created but never addressed again.
pub fn validate_org_environment_name(name: &str) -> Result<(), OrgEnvironmentError> {
    let invalid = |m: String| Err(OrgEnvironmentError::Invalid(m));
    if name.is_empty() {
        return invalid("environment name must not be empty".into());
    }
    if name == "." || name == ".." {
        return invalid(format!("environment name must not be `{name}`"));
    }
    if name.chars().any(char::is_control) {
        return invalid(format!(
            "environment name `{}` must not contain control characters",
            name.escape_debug()
        ));
    }
    Ok(())
}

/// Errors from [`OrgEnvironment`] validation and the [`OrgEnvironmentStore`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrgEnvironmentError {
    /// The OrgEnvironment (or one of its parts) breaks a rule; nothing was
    /// stored.
    #[error("{0}")]
    Invalid(String),
    /// No Environment by that name exists in the Org (ADR-0071): refused by
    /// a governance write that names one. Nothing was stored.
    #[error("org `{org}` has no environment `{environment}`")]
    NotFound { org: String, environment: String },
    /// The Environment is still referenced — Projects hold overlays or
    /// privilege grants in it (ADR-0071 §3, §8) — so it cannot be deleted.
    /// Nothing changed.
    #[error("{0}")]
    InUse(String),
    /// An `…_expecting` write found the stored value changed since the
    /// caller read it (ADR-0073 §3 optimistic concurrency). Nothing was
    /// stored; the caller reloads and decides again.
    #[error("{0}")]
    Conflict(String),
    /// The backing store failed, or held a row this binary cannot read.
    #[error("org environment store: {0}")]
    Store(String),
}

impl OrgEnvironmentError {
    /// The [`Conflict`](Self::Conflict) every store answers when an
    /// `…_expecting` write lost to a concurrent change.
    pub fn changed_concurrently() -> Self {
        Self::Conflict("changed concurrently; reload and retry".into())
    }
}

/// Durable store for OrgEnvironments, keyed by `(org, name)`.
#[async_trait]
pub trait OrgEnvironmentStore: Send + Sync {
    /// Create `env`, or replace the one with its `(org, name)` — its
    /// description. On a replace the stored `created_at` is kept and
    /// `env.created_at` ignored. `env.protection` and `env.eligibility` are
    /// **ignored** too: a new Environment starts permissive and with no
    /// eligible Project, a replace keeps what is stored, and the returned
    /// value carries the stored ones (they have their own writes on
    /// [`EnvironmentGovernanceStore`](crate::EnvironmentGovernanceStore)).
    /// Refuses an invalid `env` ([`OrgEnvironmentError::Invalid`]); nothing
    /// changes then. Returns the OrgEnvironment as stored.
    async fn put_org_environment(
        &self,
        env: &OrgEnvironment,
    ) -> Result<OrgEnvironment, OrgEnvironmentError>;

    /// One OrgEnvironment.
    async fn get_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<OrgEnvironment>, OrgEnvironmentError>;

    /// Every OrgEnvironment in `org`, by name.
    async fn list_org_environments(
        &self,
        org: &str,
    ) -> Result<Vec<OrgEnvironment>, OrgEnvironmentError>;

    /// Remove an OrgEnvironment and its eligibility. `false` if there was
    /// none. Refused with [`OrgEnvironmentError::InUse`] while any Project
    /// holds an overlay or privilege grant in it.
    async fn delete_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<bool, OrgEnvironmentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> OrgEnvironment {
        OrgEnvironment {
            org: "acme".into(),
            name: name.into(),
            description: String::new(),
            protection: EnvironmentProtectionRules::default(),
            eligibility: EnvironmentProjectEligibility::default(),
            created_at: 0,
            updated_at: 0,
            updated_by: "amy".into(),
        }
    }

    #[test]
    fn name_rule_admits_every_name_a_pipeline_can_deploy_to() {
        let long = "x".repeat(500);
        for name in [
            "prod",
            "Prod-EU",
            "staging_2",
            "prod.eu-west-1",
            "ünïcode",
            "a",
            "Production EU",
            "prod/eu",
            " prod",
            "...",
            ".prod",
            long.as_str(),
        ] {
            assert_eq!(env(name).validate(), Ok(()), "{name}");
        }
    }

    #[test]
    fn name_rule_refuses_empty_dot_segments_and_control_characters() {
        for name in ["", ".", "..", "prod\t", "prod\n", "prod\u{0}", "prod\u{7f}"] {
            assert!(
                matches!(env(name).validate(), Err(OrgEnvironmentError::Invalid(_))),
                "{name:?}"
            );
        }
    }

    #[test]
    fn description_is_bounded() {
        let mut e = env("prod");
        e.description = "d".repeat(MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS);
        assert_eq!(e.validate(), Ok(()));
        e.description.push('d');
        assert!(matches!(e.validate(), Err(OrgEnvironmentError::Invalid(_))));
    }
}
