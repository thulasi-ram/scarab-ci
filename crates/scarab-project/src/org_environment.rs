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

/// The longest OrgEnvironment display name, in characters.
pub const MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS: usize = 200;

/// What a duplicate's display name ends with when the caller gives none.
pub const DUPLICATE_DISPLAY_NAME_SUFFIX: &str = " (copy)";

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
    /// What people see in headings, lists and pickers in place of the name
    /// (git-bug d002a0c). `None` shows the name. Editable, unlike the name,
    /// which pipeline YAML and the OIDC subject (`…/env/<name>/…`) refer to
    /// from outside Scarab.
    pub display_name: Option<String>,
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
    /// the display name rule, the description length and the eligibility
    /// patterns.
    pub fn validate(&self) -> Result<(), OrgEnvironmentError> {
        validate_org_environment_name(&self.name)?;
        if let Some(display_name) = &self.display_name {
            validate_org_environment_display_name(display_name)?;
        }
        if self.description.chars().count() > MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS {
            return Err(OrgEnvironmentError::Invalid(format!(
                "description is longer than {MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS} characters"
            )));
        }
        self.eligibility.validate()
    }

    /// What people see: the display name, or the name when there is none.
    pub fn shown_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The entity tag of this Environment as stored: a digest of every
    /// field, so any change to it (its description, its baseline, its
    /// eligibility) gives a different tag. The whole-Environment PUT checks
    /// `If-Match` against it (git-bug 66cce00). Stable within one build;
    /// after an upgrade a client holding an older tag reloads once.
    pub fn etag(&self) -> String {
        // FNV-1a, 64-bit: a change detector, not a security boundary.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in format!("{self:?}").bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("\"{h:016x}\"")
    }
}

/// A display name as a caller sent it, normalised: trimmed, and an empty one
/// read as none (show the name).
pub fn normalize_display_name(display_name: Option<&str>) -> Option<String> {
    display_name
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

/// Invisible characters that change how text around them reads: bidi
/// embeddings, overrides and isolates, zero-width spaces and joiners, the
/// word joiner and its kin, the byte order mark and the soft hyphen. A
/// display name holding one could show as another Environment's name.
fn is_invisible_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// The display name rule: not empty or blank (none is `None`), no control
/// characters, no invisible bidi or zero-width characters, at most
/// [`MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS`].
pub fn validate_org_environment_display_name(
    display_name: &str,
) -> Result<(), OrgEnvironmentError> {
    let invalid = |m: String| Err(OrgEnvironmentError::Invalid(m));
    if display_name.trim().is_empty() {
        return invalid("display name must not be blank; leave it out to show the name".into());
    }
    if display_name.chars().any(char::is_control) {
        return invalid("display name must not contain control characters".into());
    }
    if display_name.chars().any(is_invisible_format_char) {
        return invalid(
            "display name must not contain invisible bidi or zero-width characters".into(),
        );
    }
    if display_name.chars().count() > MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS {
        return invalid(format!(
            "display name is longer than {MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS} characters"
        ));
    }
    Ok(())
}

/// The display name a duplicate of `source` gets when the caller names
/// none: what `source` shows, plus [`DUPLICATE_DISPLAY_NAME_SUFFIX`], cut so
/// the whole stays within [`MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS`].
pub fn duplicate_display_name(source: &OrgEnvironment) -> String {
    let room =
        MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS - DUPLICATE_DISPLAY_NAME_SUFFIX.chars().count();
    let shown: String = source.shown_name().chars().take(room).collect();
    format!("{shown}{DUPLICATE_DISPLAY_NAME_SUFFIX}")
}

/// What a whole-Environment write requires of the stored Environment
/// (git-bug 66cce00): HTTP's `If-None-Match: *` and `If-Match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePrecondition {
    /// No requirement: create or replace.
    Any,
    /// Create only: no Environment by that name may exist.
    Absent,
    /// Replace only: the stored Environment's [`OrgEnvironment::etag`] is
    /// one of these (HTTP `If-Match`, strong comparison). `*` matches any
    /// stored Environment; a weak tag (`W/"…"`) never matches.
    Matches(Vec<String>),
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
    /// A [`WritePrecondition`] did not hold: the Environment exists (create
    /// only), or is gone or changed since the caller read it. Nothing was
    /// stored.
    #[error("{0}")]
    PreconditionFailed(String),
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

    /// The [`Conflict`](Self::Conflict) a duplicate answers when its new
    /// name is taken.
    pub fn already_exists(org: &str, name: &str) -> Self {
        Self::Conflict(format!("org `{org}` already has an environment `{name}`"))
    }
}

/// Durable store for OrgEnvironments, keyed by `(org, name)`.
#[async_trait]
pub trait OrgEnvironmentStore: Send + Sync {
    /// Create `env`, or replace the one with its `(org, name)` — its display
    /// name and description. On a replace the stored `created_at` is kept and
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
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        self.put_org_environment_if(env, &WritePrecondition::Any)
            .await
    }

    /// [`put_org_environment`](Self::put_org_environment), only while
    /// `precondition` holds, checked and written atomically. A failed check
    /// is [`OrgEnvironmentError::PreconditionFailed`]; nothing changes then.
    async fn put_org_environment_if(
        &self,
        env: &OrgEnvironment,
        precondition: &WritePrecondition,
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
            display_name: None,
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
    fn display_name_rule_and_normalisation() {
        let mut e = env("prod");
        e.display_name = Some("Production (EU)".into());
        assert_eq!(e.validate(), Ok(()));
        assert_eq!(e.shown_name(), "Production (EU)");
        for bad in [
            "",
            "   ",
            "Prod\n",
            "Prod\u{202E}",
            "Pro\u{200B}d",
            "\u{2066}prod\u{2069}",
            &"d".repeat(MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS + 1),
        ] {
            e.display_name = Some(bad.to_string());
            assert!(
                matches!(e.validate(), Err(OrgEnvironmentError::Invalid(_))),
                "{bad:?}"
            );
        }
        e.display_name = None;
        assert_eq!(e.shown_name(), "prod");
        assert_eq!(normalize_display_name(Some("  ")), None);
        assert_eq!(normalize_display_name(Some(" EU ")), Some("EU".into()));
        assert_eq!(normalize_display_name(None), None);
    }

    #[test]
    fn a_duplicate_is_named_after_what_its_source_shows() {
        let mut e = env("prod");
        assert_eq!(duplicate_display_name(&e), "prod (copy)");
        e.display_name = Some("Production".into());
        assert_eq!(duplicate_display_name(&e), "Production (copy)");
        e.display_name = Some("p".repeat(MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS));
        let long = duplicate_display_name(&e);
        assert_eq!(long.chars().count(), MAX_ORG_ENVIRONMENT_DISPLAY_NAME_CHARS);
        assert!(long.ends_with(" (copy)"));
        assert_eq!(validate_org_environment_display_name(&long), Ok(()));
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
