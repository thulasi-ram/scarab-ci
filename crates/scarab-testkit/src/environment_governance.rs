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
    migrate_legacy_environment, DeployGovernance, Environment, EnvironmentDeploymentDeadline,
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, OrgEnvironment,
    OrgEnvironmentError,
};

type EnvironmentKey = (String, String);
type ProjectKey = (String, String, String);

#[derive(Default)]
struct State {
    environments: BTreeMap<EnvironmentKey, OrgEnvironment>,
    overlays: BTreeMap<ProjectKey, EnvironmentProjectOverlay>,
    grants: BTreeMap<ProjectKey, EnvironmentProjectPrivilegeGrant>,
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
}
