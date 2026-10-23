//! [`InMemoryEnvironmentGovernance`]: the org-level Environment governance
//! store (ADR-0071) without Postgres, for hermetic server tests.
//!
//! Its seeding helper [`with_migrated_legacy_environment`] does to one legacy
//! per-Project environment exactly what migration 0057's backfill does to a
//! `legacy_project_environments` row (ADR-0071 §10 steps 1, 2, 5, 6): so a
//! test written against the per-Project `ProtectionRules` keeps its setup in
//! that vocabulary and is ported by changing only the store it hands the
//! server — never its assertions.
//!
//! [`with_migrated_legacy_environment`]: InMemoryEnvironmentGovernance::with_migrated_legacy_environment

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_project::{
    migrate_legacy_environment, DeployGovernance, DeploymentTargetHome, Environment,
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentMigrationAcknowledgement,
    EnvironmentMigrationReport, EnvironmentMigrationReportProject, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant, EnvironmentProjectRecord,
    EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentError, ProtectionRules,
};

type EnvironmentKey = (String, String);
type ProjectKey = (String, String, String);

/// A split report line as migration 0057 stores it: only what the
/// migration knew. Everything else on [`EnvironmentMigrationReport`] is read
/// live, as the Postgres store reads it.
struct ReportLine {
    referenced_only: bool,
    referencing_projects: Vec<String>,
    acknowledged: Option<EnvironmentMigrationAcknowledgement>,
}

#[derive(Default)]
struct State {
    environments: BTreeMap<EnvironmentKey, OrgEnvironment>,
    overlays: BTreeMap<ProjectKey, EnvironmentProjectOverlay>,
    grants: BTreeMap<ProjectKey, EnvironmentProjectPrivilegeGrant>,
    /// The frozen legacy rows, keyed (org, environment, project).
    legacy: BTreeMap<ProjectKey, ProtectionRules>,
    reports: BTreeMap<EnvironmentKey, ReportLine>,
    /// Seeded DeploymentTarget homes, keyed (org, source, target id) — what
    /// Postgres derives from routing and the Deployment record.
    target_homes: BTreeMap<ProjectKey, String>,
}

/// An in-memory [`EnvironmentGovernanceStore`]. Reads are one lock, so
/// [`deploy_governance`](EnvironmentGovernanceStore::deploy_governance) is
/// trivially the one consistent snapshot the trait asks for.
#[derive(Default)]
pub struct InMemoryEnvironmentGovernance {
    state: Mutex<State>,
}

fn not_found(org: &str, environment: &str) -> OrgEnvironmentError {
    OrgEnvironmentError::NotFound {
        org: org.to_string(),
        environment: environment.to_string(),
    }
}

/// A new Environment as the migration (and the c-min API) creates one:
/// permissive baseline, the default deadline, no sources.
fn new_environment(
    org: &str,
    name: &str,
    eligibility: EnvironmentProjectEligibility,
) -> OrgEnvironment {
    OrgEnvironment {
        org: org.to_string(),
        name: name.to_string(),
        description: String::new(),
        deployment_deadline: EnvironmentDeploymentDeadline::new(
            scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS,
        )
        .expect("the default deadline is within bounds"),
        deployment_sources: Vec::new(),
        protection: EnvironmentProtectionRules::default(),
        eligibility,
        created_at: 0,
        updated_at: 0,
        updated_by: "testkit".to_string(),
    }
}

impl InMemoryEnvironmentGovernance {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed where a DeploymentTarget `(source, id)` of `org` is homed now
    /// ([`EnvironmentGovernanceStore::deployment_target_homes`]).
    pub fn with_deployment_target_home(
        self,
        org: &str,
        source: &str,
        id: &str,
        environment: &str,
    ) -> Self {
        self.state.lock().unwrap().target_homes.insert(
            (org.to_string(), source.to_string(), id.to_string()),
            environment.to_string(),
        );
        self
    }

    /// Seed an Environment with the permissive baseline and `eligibility`,
    /// replacing any by that name.
    pub fn with_environment(
        self,
        org: &str,
        name: &str,
        eligibility: EnvironmentProjectEligibility,
    ) -> Self {
        self.state.lock().unwrap().environments.insert(
            (org.to_string(), name.to_string()),
            new_environment(org, name, eligibility),
        );
        self
    }

    /// Seed what ADR-0071 §10 makes of `project`'s legacy per-Project
    /// `legacy` environment: the org Environment of that name (created
    /// permissive if missing — step 1), `project` added to its eligibility
    /// by exact name unless it is already all-projects (step 6), and the
    /// Project's migrated overlay and, if it carried privileges, its grant
    /// ([`migrate_legacy_environment`], steps 2, 3, 5).
    pub fn with_migrated_legacy_environment(
        self,
        org: &str,
        project: &str,
        legacy: &Environment,
    ) -> Self {
        {
            let mut state = self.state.lock().unwrap();
            let env = state
                .environments
                .entry((org.to_string(), legacy.name.clone()))
                .or_insert_with(|| {
                    new_environment(
                        org,
                        &legacy.name,
                        EnvironmentProjectEligibility::Projects(Vec::new()),
                    )
                });
            if let EnvironmentProjectEligibility::Projects(projects) = &mut env.eligibility {
                if !projects.iter().any(|p| p == project) {
                    projects.push(project.to_string());
                    projects.sort();
                }
            }
            let (overlay, grant) = migrate_legacy_environment(org, project, legacy);
            let key = (org.to_string(), legacy.name.clone(), project.to_string());
            state.legacy.insert(key.clone(), legacy.protection.clone());
            state.overlays.insert(key.clone(), overlay);
            match grant {
                Some(grant) => {
                    state.grants.insert(key, grant);
                }
                None => {
                    state.grants.remove(&key);
                }
            }
        }
        self
    }

    /// Seed the split report line migration 0057 writes for `name` (ADR-0071
    /// §10 step 8). The merged Projects are the ones seeded through
    /// [`with_migrated_legacy_environment`](Self::with_migrated_legacy_environment)
    /// for `name`, as the migration derives them from the legacy rows.
    pub fn with_migration_report(
        self,
        org: &str,
        name: &str,
        referenced_only: bool,
        referencing_projects: &[&str],
    ) -> Self {
        self.state.lock().unwrap().reports.insert(
            (org.to_string(), name.to_string()),
            ReportLine {
                referenced_only,
                referencing_projects: referencing_projects.iter().map(|p| p.to_string()).collect(),
                acknowledged: None,
            },
        );
        self
    }

    fn report(
        state: &State,
        org: &str,
        name: &str,
        line: &ReportLine,
    ) -> EnvironmentMigrationReport {
        let merged_projects: Vec<String> = state
            .legacy
            .keys()
            .filter(|(o, e, _)| o == org && e == name)
            .map(|(_, _, p)| p.clone())
            .collect();
        let mut names: Vec<String> = merged_projects
            .iter()
            .chain(&line.referencing_projects)
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        let projects = names
            .into_iter()
            .map(|project| {
                let key = (org.to_string(), name.to_string(), project.clone());
                EnvironmentMigrationReportProject {
                    legacy: state.legacy.get(&key).cloned(),
                    overlay: state.overlays.get(&key).cloned(),
                    project,
                }
            })
            .collect();
        EnvironmentMigrationReport {
            org: org.to_string(),
            environment: name.to_string(),
            merged_projects,
            referenced_only: line.referenced_only,
            referencing_projects: line.referencing_projects.clone(),
            created_at: 0,
            acknowledged: line.acknowledged.clone(),
            baseline: state
                .environments
                .get(&(org.to_string(), name.to_string()))
                .map(|e| e.protection.clone()),
            projects,
        }
    }

    /// Remove an Environment outright (overlays and grants included) — the
    /// "admin deleted it since the Run was created" case. A test seam: the
    /// real store refuses while overlays or grants exist.
    pub fn remove_environment(&self, org: &str, name: &str) {
        let mut state = self.state.lock().unwrap();
        state
            .environments
            .remove(&(org.to_string(), name.to_string()));
        state
            .overlays
            .retain(|(o, e, _), _| !(o == org && e == name));
        state.grants.retain(|(o, e, _), _| !(o == org && e == name));
    }

    fn governance(state: &State, env: &OrgEnvironment, project: &str) -> DeployGovernance {
        let key = (env.org.clone(), env.name.clone(), project.to_string());
        DeployGovernance::new(
            env.clone(),
            project,
            state.overlays.get(&key).cloned(),
            state.grants.get(&key).cloned(),
        )
    }
}

#[async_trait]
impl EnvironmentGovernanceStore for InMemoryEnvironmentGovernance {
    async fn deploy_governance(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Option<DeployGovernance>, OrgEnvironmentError> {
        let state = self.state.lock().unwrap();
        Ok(state
            .environments
            .get(&(org.to_string(), environment.to_string()))
            .map(|env| Self::governance(&state, env, project)))
    }

    async fn project_environment_governance(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<DeployGovernance>, OrgEnvironmentError> {
        let state = self.state.lock().unwrap();
        Ok(state
            .environments
            .values()
            .filter(|env| env.org == org)
            .map(|env| Self::governance(&state, env, project))
            .filter(|g| g.eligible || g.overlay.is_some() || g.privilege_grant.is_some())
            .collect())
    }

    async fn put_environment_protection(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        let mut state = self.state.lock().unwrap();
        let env = state
            .environments
            .get_mut(&(org.to_string(), environment.to_string()))
            .ok_or_else(|| not_found(org, environment))?;
        env.protection = protection.clone();
        env.updated_by = updated_by.to_string();
        env.updated_at = updated_at;
        Ok(env.clone())
    }

    async fn put_environment_eligibility(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        eligibility.validate()?;
        let mut state = self.state.lock().unwrap();
        let env = state
            .environments
            .get_mut(&(org.to_string(), environment.to_string()))
            .ok_or_else(|| not_found(org, environment))?;
        env.eligibility = match eligibility {
            EnvironmentProjectEligibility::Projects(p) => {
                let mut p = p.clone();
                p.sort();
                EnvironmentProjectEligibility::Projects(p)
            }
            all => all.clone(),
        };
        env.updated_by = updated_by.to_string();
        env.updated_at = updated_at;
        Ok(env.clone())
    }

    async fn put_environment_project_overlay(
        &self,
        overlay: &EnvironmentProjectOverlay,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let mut state = self.state.lock().unwrap();
        if !state
            .environments
            .contains_key(&(overlay.org.clone(), overlay.environment.clone()))
        {
            return Err(not_found(&overlay.org, &overlay.environment));
        }
        state.overlays.insert(
            (
                overlay.org.clone(),
                overlay.environment.clone(),
                overlay.project.clone(),
            ),
            overlay.clone(),
        );
        Ok(())
    }

    async fn delete_environment_project_overlay(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .overlays
            .remove(&(
                org.to_string(),
                environment.to_string(),
                project.to_string(),
            ))
            .is_some())
    }

    async fn put_environment_project_privilege_grant(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let mut state = self.state.lock().unwrap();
        if !state
            .environments
            .contains_key(&(grant.org.clone(), grant.environment.clone()))
        {
            return Err(not_found(&grant.org, &grant.environment));
        }
        state.grants.insert(
            (
                grant.org.clone(),
                grant.environment.clone(),
                grant.project.clone(),
            ),
            grant.clone(),
        );
        Ok(())
    }

    async fn delete_environment_project_privilege_grant(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .grants
            .remove(&(
                org.to_string(),
                environment.to_string(),
                project.to_string(),
            ))
            .is_some())
    }

    async fn environment_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<EnvironmentProjectRecord>, OrgEnvironmentError> {
        let state = self.state.lock().unwrap();
        let env = state
            .environments
            .get(&(org.to_string(), environment.to_string()))
            .ok_or_else(|| not_found(org, environment))?;
        let in_env = |(o, e, _): &ProjectKey| o == org && e == environment;
        Ok(EnvironmentProjectRecord::collect(
            env,
            state
                .overlays
                .iter()
                .filter(|(k, _)| in_env(k))
                .map(|(_, o)| o.clone()),
            state
                .grants
                .iter()
                .filter(|(k, _)| in_env(k))
                .map(|(_, g)| g.clone()),
        ))
    }

    async fn environment_migration_report(
        &self,
        org: &str,
    ) -> Result<Vec<EnvironmentMigrationReport>, OrgEnvironmentError> {
        let state = self.state.lock().unwrap();
        Ok(state
            .reports
            .iter()
            .filter(|((o, _), _)| o == org)
            .map(|((o, name), line)| Self::report(&state, o, name, line))
            .collect())
    }

    async fn deployment_target_homes(
        &self,
        org: &str,
        targets: &[(String, String)],
    ) -> Result<Vec<DeploymentTargetHome>, OrgEnvironmentError> {
        let state = self.state.lock().unwrap();
        Ok(targets
            .iter()
            .filter_map(|(source, id)| {
                let env = state
                    .target_homes
                    .get(&(org.to_string(), source.clone(), id.clone()))?;
                Some(DeploymentTargetHome {
                    source: source.clone(),
                    id: id.clone(),
                    environment: env.clone(),
                })
            })
            .collect())
    }

    async fn acknowledge_environment_migration_report(
        &self,
        org: &str,
        environment: &str,
        acknowledged_by: &str,
        acknowledged_at: i64,
    ) -> Result<Option<EnvironmentMigrationReport>, OrgEnvironmentError> {
        let mut state = self.state.lock().unwrap();
        let key = (org.to_string(), environment.to_string());
        let Some(line) = state.reports.get_mut(&key) else {
            return Ok(None);
        };
        // The first acknowledgement stands, as in Postgres.
        line.acknowledged
            .get_or_insert_with(|| EnvironmentMigrationAcknowledgement {
                at: acknowledged_at,
                by: acknowledged_by.to_string(),
            });
        let line = &state.reports[&key];
        Ok(Some(Self::report(&state, org, environment, line)))
    }
}
