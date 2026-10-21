//! The **minimal org-level Environment** (ADR-0071, MMP step 1): a name, its
//! EnvironmentDeploymentDeadline, and its EnvironmentDeploymentSources —
//! enough to key Deployments and, from slice (d) on, route source reports.
//!
//! This is CONTEXT §4.1's **Environment**. It is named [`OrgEnvironment`]
//! only because the per-Project ADR-0037 struct already owns the name
//! [`Environment`](crate::Environment); at the full migration (ADR-0071 §10)
//! that struct becomes the EnvironmentProjectOverlay and this one becomes
//! `Environment`.
//!
//! Until then the two **coexist by exact, case-sensitive name** and never
//! reference each other: no foreign key either way, and an OrgEnvironment
//! **never feeds admission** — per-Project Environments keep governing Runs
//! (`resolve_governance` in the server is untouched). Nor does a Deployment
//! need one: the Deployment record is keyed by name, and a name with no
//! OrgEnvironment simply gets the default deadline.

use async_trait::async_trait;

/// The shortest EnvironmentDeploymentDeadline an admin may set: one minute.
/// Anything shorter would read most real syncs as `stalled` before a source
/// could report once.
pub const MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS: i64 = 60_000;

/// The longest EnvironmentDeploymentDeadline an admin may set: seven days.
/// Past that, `stalled` stops meaning anything an operator would act on.
pub const MAX_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The longest OrgEnvironment description, in characters.
pub const MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS: usize = 1000;

/// The longest PluginInstallation name: a DNS-1123 label (ADR-0072 names an
/// installation the way Kubernetes names a label).
pub const MAX_INSTALLATION_NAME_CHARS: usize = 63;

/// The most EnvironmentDeploymentSources one Environment may list. Every
/// source is a row the replace rewrites and a key the routing lookup holds;
/// far more than any real CD layout needs, few enough that one PUT stays a
/// bounded write.
pub const MAX_ENVIRONMENT_DEPLOYMENT_SOURCES: usize = 256;

/// The longest vendor grouping, in characters — a Kubernetes object name's
/// limit, which an Argo CD project name (the motivating grouping) obeys.
pub const MAX_VENDOR_GROUPING_CHARS: usize = 253;

/// How long a Deployment in this Environment may stay `requested` or
/// `in_progress` before it reads `stalled` (ADR-0070 §10). Milliseconds,
/// bounded to [`MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS`]..=
/// [`MAX_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS`] so a typo cannot stall every
/// Deployment at once or hide a stuck one for a month.
///
/// There is deliberately no default here: the only default is
/// `scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS`, which the server
/// applies when a request omits a deadline and when a name has no
/// OrgEnvironment — one number, one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvironmentDeploymentDeadline(i64);

impl EnvironmentDeploymentDeadline {
    /// A deadline of `ms`, refused outside the bounds.
    pub fn new(ms: i64) -> Result<Self, OrgEnvironmentError> {
        if (MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS..=MAX_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS)
            .contains(&ms)
        {
            Ok(Self(ms))
        } else {
            Err(OrgEnvironmentError::Invalid(format!(
                "deployment deadline must be between {MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS} ms \
                 (1 minute) and {MAX_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS} ms (7 days), got {ms}"
            )))
        }
    }

    /// The deadline in milliseconds.
    pub fn millis(self) -> i64 {
        self.0
    }
}

/// One source that may report into an Environment (ADR-0071 §5): a
/// PluginInstallation plus the vendor grouping that maps into this
/// Environment (an Argo CD project `prod`, say).
///
/// `(org, installation, vendor_grouping)` routes to **exactly one**
/// Environment — the store refuses a second claim
/// ([`OrgEnvironmentError::DeploymentSourceTaken`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvironmentDeploymentSource {
    /// The PluginInstallation's name, assigned by Scarab (never by the
    /// plugin). A plain, shape-checked string until PluginInstallations exist
    /// (ADR-0072); slice (d) checks it against the real ones.
    pub installation: String,
    /// Opaque, vendor-defined, matched exactly — no wildcards. `vendor_`
    /// because the plugin asserts it, and `grouping` alone is a Postgres
    /// keyword.
    pub vendor_grouping: String,
}

impl EnvironmentDeploymentSource {
    /// Shape-check one source: the installation is a DNS-1123 label, the
    /// vendor grouping a non-empty string with no control characters and no
    /// leading or trailing whitespace. Inner whitespace is the vendor's
    /// business; an edge space is refused rather than kept, because it is
    /// invisible in every form and a form that trimmed it would silently
    /// change the routing key.
    pub fn validate(&self) -> Result<(), OrgEnvironmentError> {
        let invalid = |m: String| Err(OrgEnvironmentError::Invalid(m));
        let installation = &self.installation;
        if !is_dns1123_label(installation) {
            return invalid(format!(
                "deployment source installation `{installation}` must be a DNS-1123 label: \
                 1-{MAX_INSTALLATION_NAME_CHARS} lowercase letters, digits or `-`, starting \
                 and ending with a letter or digit"
            ));
        }
        let grouping = &self.vendor_grouping;
        if grouping.is_empty() {
            return invalid(format!(
                "deployment source `{installation}` needs a vendor grouping"
            ));
        }
        if grouping.chars().count() > MAX_VENDOR_GROUPING_CHARS {
            return invalid(format!(
                "deployment source `{installation}`'s vendor grouping is longer than \
                 {MAX_VENDOR_GROUPING_CHARS} characters"
            ));
        }
        if grouping.chars().any(char::is_control) {
            return invalid(format!(
                "deployment source `{installation}`'s vendor grouping contains a control character"
            ));
        }
        if grouping.trim() != grouping {
            return invalid(format!(
                "deployment source `{installation}`'s vendor grouping `{grouping}` starts or \
                 ends with whitespace"
            ));
        }
        Ok(())
    }
}

/// A DNS-1123 label: 1-63 of `[a-z0-9-]`, alphanumeric at both ends.
fn is_dns1123_label(s: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    !s.is_empty()
        && s.len() <= MAX_INSTALLATION_NAME_CHARS
        && s.chars().all(|c| alnum(c) || c == '-')
        && s.starts_with(alnum)
        && s.ends_with(alnum)
}

/// An org-level Environment (ADR-0071 §1), in its minimal MMP shape: no
/// protection rules, eligibility, overlays or grants yet — those arrive with
/// the full migration. See the module docs for why it is not yet called
/// `Environment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgEnvironment {
    pub org: String,
    /// The Environment's name, the key Deployments and per-Project
    /// Environments share (exact, case-sensitive).
    pub name: String,
    /// Free text for people; never interpreted.
    pub description: String,
    pub deployment_deadline: EnvironmentDeploymentDeadline,
    /// The sources that may report into this Environment. A set: order is not
    /// kept (the store returns them sorted), and a duplicate is refused.
    pub deployment_sources: Vec<EnvironmentDeploymentSource>,
    /// Unix-ms it was first created. Kept across replaces.
    pub created_at: i64,
    /// Unix-ms it was last written.
    pub updated_at: i64,
    /// The subject of the Principal that last wrote it.
    pub updated_by: String,
}

impl OrgEnvironment {
    /// Check everything the type does not already guarantee: the name rule,
    /// the description length, each source's shape, and no source listed
    /// twice. Whether another Environment already holds a source is the
    /// store's to decide — it alone sees every Environment at once.
    pub fn validate(&self) -> Result<(), OrgEnvironmentError> {
        validate_org_environment_name(&self.name)?;
        if self.description.chars().count() > MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS {
            return Err(OrgEnvironmentError::Invalid(format!(
                "description is longer than {MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS} characters"
            )));
        }
        if self.deployment_sources.len() > MAX_ENVIRONMENT_DEPLOYMENT_SOURCES {
            return Err(OrgEnvironmentError::Invalid(format!(
                "at most {MAX_ENVIRONMENT_DEPLOYMENT_SOURCES} deployment sources per \
                 environment, got {}",
                self.deployment_sources.len()
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for source in &self.deployment_sources {
            source.validate()?;
            if !seen.insert(source) {
                return Err(OrgEnvironmentError::Invalid(format!(
                    "deployment source `{}` / `{}` is listed twice",
                    source.installation, source.vendor_grouping
                )));
            }
        }
        Ok(())
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
    /// Another Environment in the Org already lists this source, and a
    /// source routes to exactly one Environment (ADR-0071 §5). Nothing was
    /// stored.
    #[error(
        "deployment source `{installation}` / `{vendor_grouping}` already reports into \
         environment `{environment}`"
    )]
    DeploymentSourceTaken {
        installation: String,
        vendor_grouping: String,
        /// The Environment that holds it.
        environment: String,
    },
    /// The backing store failed, or held a row this binary cannot read.
    #[error("org environment store: {0}")]
    Store(String),
}

/// Durable store for OrgEnvironments and their EnvironmentDeploymentSources,
/// keyed by `(org, name)`.
#[async_trait]
pub trait OrgEnvironmentStore: Send + Sync {
    /// Create `env`, or replace the one with its `(org, name)` — its
    /// description, deadline and **whole** source list, atomically. On a
    /// replace the stored `created_at` is kept and `env.created_at` ignored.
    /// Refuses an invalid `env` ([`OrgEnvironmentError::Invalid`]) and a
    /// source another Environment holds
    /// ([`OrgEnvironmentError::DeploymentSourceTaken`]); either way nothing
    /// changes. Returns the OrgEnvironment as stored (sources sorted).
    async fn put_org_environment(
        &self,
        env: &OrgEnvironment,
    ) -> Result<OrgEnvironment, OrgEnvironmentError>;

    /// One OrgEnvironment, sources sorted.
    async fn get_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<OrgEnvironment>, OrgEnvironmentError>;

    /// Every OrgEnvironment in `org`, by name, sources sorted.
    async fn list_org_environments(
        &self,
        org: &str,
    ) -> Result<Vec<OrgEnvironment>, OrgEnvironmentError>;

    /// Remove an OrgEnvironment and its sources. `false` if there was none.
    async fn delete_org_environment(
        &self,
        org: &str,
        name: &str,
    ) -> Result<bool, OrgEnvironmentError>;

    /// The Environment a report from `installation` about `vendor_grouping`
    /// lands in, if any lists that source — the routing slice (d)'s report
    /// ingest needs (ADR-0071 §5: anything else is dropped and the drop
    /// recorded).
    async fn environment_for_deployment_source(
        &self,
        org: &str,
        installation: &str,
        vendor_grouping: &str,
    ) -> Result<Option<String>, OrgEnvironmentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(installation: &str, vendor_grouping: &str) -> EnvironmentDeploymentSource {
        EnvironmentDeploymentSource {
            installation: installation.into(),
            vendor_grouping: vendor_grouping.into(),
        }
    }

    fn env(name: &str, sources: Vec<EnvironmentDeploymentSource>) -> OrgEnvironment {
        OrgEnvironment {
            org: "acme".into(),
            name: name.into(),
            description: String::new(),
            deployment_deadline: EnvironmentDeploymentDeadline::new(
                MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS,
            )
            .unwrap(),
            deployment_sources: sources,
            created_at: 0,
            updated_at: 0,
            updated_by: "amy".into(),
        }
    }

    #[test]
    fn deadline_is_bounded_to_one_minute_through_seven_days_inclusive() {
        for ms in [
            MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS,
            MAX_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS,
        ] {
            assert_eq!(EnvironmentDeploymentDeadline::new(ms).unwrap().millis(), ms);
        }
        for ms in [
            i64::MIN,
            -1,
            0,
            MIN_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS - 1,
            MAX_ENVIRONMENT_DEPLOYMENT_DEADLINE_MS + 1,
            i64::MAX,
        ] {
            assert!(
                matches!(
                    EnvironmentDeploymentDeadline::new(ms),
                    Err(OrgEnvironmentError::Invalid(_))
                ),
                "{ms}"
            );
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
            assert_eq!(env(name, vec![]).validate(), Ok(()), "{name}");
        }
    }

    #[test]
    fn name_rule_refuses_empty_dot_segments_and_control_characters() {
        for name in ["", ".", "..", "prod\t", "prod\n", "prod\u{0}", "prod\u{7f}"] {
            assert!(
                matches!(
                    env(name, vec![]).validate(),
                    Err(OrgEnvironmentError::Invalid(_))
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn installation_must_be_a_dns1123_label() {
        let max = "a".repeat(MAX_INSTALLATION_NAME_CHARS);
        for installation in ["argocd-prod", "a", "0argo9", max.as_str()] {
            assert_eq!(
                source(installation, "prod").validate(),
                Ok(()),
                "{installation}"
            );
        }
        let too_long = "a".repeat(MAX_INSTALLATION_NAME_CHARS + 1);
        for installation in [
            "",
            "ArgoCD",
            "-argo",
            "argo-",
            "argo_cd",
            "argo.cd",
            "argo cd",
            too_long.as_str(),
        ] {
            assert!(
                matches!(
                    source(installation, "prod").validate(),
                    Err(OrgEnvironmentError::Invalid(_))
                ),
                "{installation:?}"
            );
        }
    }

    #[test]
    fn vendor_grouping_is_opaque_but_required() {
        // Opaque: case, spaces and punctuation are the vendor's business.
        for grouping in ["prod", "Team A / Prod", "*", "prod-*"] {
            assert_eq!(source("argocd", grouping).validate(), Ok(()), "{grouping}");
        }
        let too_long = "g".repeat(MAX_VENDOR_GROUPING_CHARS + 1);
        // An edge space is refused, not trimmed: it would be an invisible
        // second routing key.
        for grouping in [
            "",
            "prod\n",
            " prod",
            "prod ",
            "\u{a0}prod",
            too_long.as_str(),
        ] {
            assert!(
                matches!(
                    source("argocd", grouping).validate(),
                    Err(OrgEnvironmentError::Invalid(_))
                ),
                "{grouping:?}"
            );
        }
    }

    #[test]
    fn a_source_listed_twice_is_refused_but_two_groupings_of_one_installation_are_not() {
        assert_eq!(
            env(
                "prod",
                vec![source("argocd", "prod"), source("argocd", "prod-eu")]
            )
            .validate(),
            Ok(())
        );
        assert!(matches!(
            env(
                "prod",
                vec![source("argocd", "prod"), source("argocd", "prod")]
            )
            .validate(),
            Err(OrgEnvironmentError::Invalid(_))
        ));
    }

    #[test]
    fn an_invalid_source_fails_the_whole_environment() {
        assert!(matches!(
            env(
                "prod",
                vec![source("argocd", "prod"), source("Argo", "prod")]
            )
            .validate(),
            Err(OrgEnvironmentError::Invalid(_))
        ));
    }

    #[test]
    fn sources_per_environment_are_capped() {
        let sources = |n: usize| {
            (0..n)
                .map(|i| source("argocd", &format!("g{i}")))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            env("prod", sources(MAX_ENVIRONMENT_DEPLOYMENT_SOURCES)).validate(),
            Ok(())
        );
        assert!(matches!(
            env("prod", sources(MAX_ENVIRONMENT_DEPLOYMENT_SOURCES + 1)).validate(),
            Err(OrgEnvironmentError::Invalid(_))
        ));
    }

    #[test]
    fn description_is_bounded() {
        let mut e = env("prod", vec![]);
        e.description = "d".repeat(MAX_ORG_ENVIRONMENT_DESCRIPTION_CHARS);
        assert_eq!(e.validate(), Ok(()));
        e.description.push('d');
        assert!(matches!(e.validate(), Err(OrgEnvironmentError::Invalid(_))));
    }
}
