//! [`InMemoryEnvironmentGovernance`]: the org-level Environment governance
//! store (ADR-0071) without Postgres, for hermetic server tests.
//!
//! Its seeding helper [`with_project_overlay`] gives one Project an overlay
//! over an Environment's permissive baseline and makes the Project eligible
//! for it, which is all a test of one Project's deploy rules needs.
//!
//! [`with_project_overlay`]: InMemoryEnvironmentGovernance::with_project_overlay

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_project::{
    DeployGovernance, EnvironmentCascadeDeletion, EnvironmentGovernanceStore, EnvironmentNameClaim,
    EnvironmentProjectEligibility, EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant,
    EnvironmentProjectRecord, EnvironmentProtectionRules, ImageGrant, OrgEnvironment,
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
    /// The names a Duplicate holds a claim on, as the Postgres store's
    /// advisory locks.
    claims: Arc<Mutex<BTreeSet<EnvironmentKey>>>,
}

/// One held name claim: removed from the set when dropped.
struct HeldClaim {
    claims: Arc<Mutex<BTreeSet<EnvironmentKey>>>,
    key: EnvironmentKey,
}

impl Drop for HeldClaim {
    fn drop(&mut self) {
        self.claims.lock().unwrap().remove(&self.key);
    }
}

/// What an overlay write compares: every field but the key, which names the
/// row. The pattern lists every field, so a new one fails to compile here
/// until it is either compared or named as key.
fn overlay_values(o: &EnvironmentProjectOverlay) -> &EnvironmentProtectionRules {
    let EnvironmentProjectOverlay {
        org: _,
        environment: _,
        project: _,
        rules,
    } = o;
    rules
}

/// What a privilege-grant write compares: every field but the key, which
/// names the row. The pattern lists every field, so a new one fails to
/// compile here until it is either compared or named as key.
fn grant_values(g: &EnvironmentProjectPrivilegeGrant) -> (&Vec<ImageGrant>, bool) {
    let EnvironmentProjectPrivilegeGrant {
        org: _,
        environment: _,
        project: _,
        privileged_images,
        permit_k8s_overlay,
    } = g;
    (privileged_images, *permit_k8s_overlay)
}

/// Eligibility as stored: patterns sorted (and, for comparison, deduped).
fn sorted_eligibility(e: &EnvironmentProjectEligibility) -> EnvironmentProjectEligibility {
    match e {
        EnvironmentProjectEligibility::Projects(p) => {
            let mut p = p.clone();
            p.sort();
            p.dedup();
            EnvironmentProjectEligibility::Projects(p)
        }
        all => all.clone(),
    }
}

fn not_found(org: &str, environment: &str) -> OrgEnvironmentError {
    OrgEnvironmentError::NotFound {
        org: org.to_string(),
        environment: environment.to_string(),
    }
}

/// A new Environment as the API creates one: permissive baseline.
fn new_environment(
    org: &str,
    name: &str,
    eligibility: EnvironmentProjectEligibility,
) -> OrgEnvironment {
    OrgEnvironment {
        org: org.to_string(),
        name: name.to_string(),
        display_name: None,
        description: String::new(),
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

    /// Give `project` the overlay `rules` over Environment `environment`,
    /// replacing any it had. The Environment is created with the permissive
    /// baseline if missing, and `project` is added to its eligibility by
    /// exact name unless it already admits every Project.
    pub fn with_project_overlay(
        self,
        org: &str,
        project: &str,
        environment: &str,
        rules: EnvironmentProtectionRules,
    ) -> Self {
        {
            let mut state = self.state.lock().unwrap();
            let env = state
                .environments
                .entry((org.to_string(), environment.to_string()))
                .or_insert_with(|| {
                    new_environment(
                        org,
                        environment,
                        EnvironmentProjectEligibility::Projects(Vec::new()),
                    )
                });
            if let EnvironmentProjectEligibility::Projects(projects) = &mut env.eligibility {
                if !projects.iter().any(|p| p == project) {
                    projects.push(project.to_string());
                    projects.sort();
                }
            }
            let overlay = EnvironmentProjectOverlay {
                org: org.to_string(),
                environment: environment.to_string(),
                project: project.to_string(),
                rules,
            };
            state.overlays.insert(
                (
                    org.to_string(),
                    environment.to_string(),
                    project.to_string(),
                ),
                overlay,
            );
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
    async fn org_environment(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Option<OrgEnvironment>, OrgEnvironmentError> {
        let state = self.state.lock().unwrap();
        Ok(state
            .environments
            .get(&(org.to_string(), environment.to_string()))
            .cloned())
    }

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

    async fn put_environment_protection_expecting(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        expected: Option<&EnvironmentProtectionRules>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        let mut state = self.state.lock().unwrap();
        let env = state
            .environments
            .get_mut(&(org.to_string(), environment.to_string()))
            .ok_or_else(|| not_found(org, environment))?;
        if expected.is_some_and(|e| e != &env.protection) {
            return Err(OrgEnvironmentError::changed_concurrently());
        }
        env.protection = protection.clone();
        env.updated_by = updated_by.to_string();
        env.updated_at = updated_at;
        Ok(env.clone())
    }

    async fn put_environment_eligibility_expecting(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        expected: Option<&EnvironmentProjectEligibility>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        // Stored trimmed, as Postgres stores it.
        let eligibility = &eligibility.normalized();
        eligibility.validate()?;
        let mut state = self.state.lock().unwrap();
        let env = state
            .environments
            .get_mut(&(org.to_string(), environment.to_string()))
            .ok_or_else(|| not_found(org, environment))?;
        if expected.is_some_and(|e| sorted_eligibility(e) != sorted_eligibility(&env.eligibility)) {
            return Err(OrgEnvironmentError::changed_concurrently());
        }
        env.eligibility = sorted_eligibility(eligibility);
        env.updated_by = updated_by.to_string();
        env.updated_at = updated_at;
        Ok(env.clone())
    }

    async fn put_environment_project_overlay_expecting(
        &self,
        overlay: &EnvironmentProjectOverlay,
        expected: Option<Option<&EnvironmentProjectOverlay>>,
        _updated_by: &str,
        _updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        let mut state = self.state.lock().unwrap();
        let Some(environment) = state
            .environments
            .get(&(overlay.org.clone(), overlay.environment.clone()))
        else {
            return Err(not_found(&overlay.org, &overlay.environment));
        };
        // A new overlay is only for a Project the Environment admits, as in
        // Postgres. The unconditional write does not check.
        if matches!(expected, Some(None)) && !environment.eligibility.admits(&overlay.project) {
            return Err(OrgEnvironmentError::Conflict(format!(
                "`{}` is not eligible for environment `{}`, so it has nothing to tighten",
                overlay.project, overlay.environment
            )));
        }
        let key = (
            overlay.org.clone(),
            overlay.environment.clone(),
            overlay.project.clone(),
        );
        if let Some(expected) = expected {
            if state.overlays.get(&key).map(overlay_values) != expected.map(overlay_values) {
                return Err(OrgEnvironmentError::changed_concurrently());
            }
        }
        // An overlay has no approval gate settings (ADR-0078 §3): Postgres
        // stores none for it and reads back the default, so this does too.
        let mut overlay = overlay.clone();
        overlay.rules.approval_gate = Default::default();
        state.overlays.insert(key, overlay);
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

    async fn put_environment_project_privilege_grant_expecting(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        expected: Option<Option<&EnvironmentProjectPrivilegeGrant>>,
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
        let key = (
            grant.org.clone(),
            grant.environment.clone(),
            grant.project.clone(),
        );
        if let Some(expected) = expected {
            if state.grants.get(&key).map(grant_values) != expected.map(grant_values) {
                return Err(OrgEnvironmentError::changed_concurrently());
            }
        }
        state.grants.insert(key, grant.clone());
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

    async fn claim_environment_name(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<EnvironmentNameClaim>, OrgEnvironmentError> {
        let key = (org.to_string(), name.to_string());
        if !self.claims.lock().unwrap().insert(key.clone()) {
            return Ok(None);
        }
        Ok(Some(EnvironmentNameClaim::new(HeldClaim {
            claims: self.claims.clone(),
            key,
        })))
    }

    async fn delete_environment_cascade(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Option<EnvironmentCascadeDeletion>, OrgEnvironmentError> {
        // One lock: all or nothing, as Postgres' transaction.
        let mut state = self.state.lock().unwrap();
        let key = (org.to_string(), environment.to_string());
        if state.environments.remove(&key).is_none() {
            return Ok(None);
        }
        let in_env = |(o, e, _): &ProjectKey| o == org && e == environment;
        let mut deleted = EnvironmentCascadeDeletion::default();
        let overlays: Vec<ProjectKey> = state
            .overlays
            .keys()
            .filter(|k| in_env(k))
            .cloned()
            .collect();
        for k in overlays {
            state.overlays.remove(&k);
            deleted.overlays.push(k.2);
        }
        let grants: Vec<ProjectKey> = state.grants.keys().filter(|k| in_env(k)).cloned().collect();
        for k in grants {
            state.grants.remove(&k);
            deleted.privilege_grants.push(k.2);
        }
        Ok(Some(deleted))
    }

    async fn duplicate_environment(
        &self,
        org: &str,
        from: &str,
        to: &str,
        display_name: Option<&str>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        // One lock: the copy is all or nothing, as Postgres' transaction.
        let mut state = self.state.lock().unwrap();
        let source = state
            .environments
            .get(&(org.to_string(), from.to_string()))
            .ok_or_else(|| not_found(org, from))?
            .clone();
        let copy = OrgEnvironment {
            name: to.to_string(),
            display_name: display_name.map(str::to_string),
            created_at: updated_at,
            updated_at,
            updated_by: updated_by.to_string(),
            ..source
        };
        // The name and display name only: the eligibility is copied as
        // stored, as Postgres copies it, so a row written before a pattern
        // rule existed does not stop a duplicate.
        scarab_project::validate_org_environment_name(&copy.name)?;
        if let Some(d) = &copy.display_name {
            scarab_project::validate_org_environment_display_name(d)?;
        }
        if state
            .environments
            .contains_key(&(org.to_string(), to.to_string()))
        {
            return Err(OrgEnvironmentError::already_exists(org, to));
        }
        let in_source = |(o, e, _): &ProjectKey| o == org && e == from;
        let overlays: Vec<EnvironmentProjectOverlay> = state
            .overlays
            .iter()
            .filter(|(k, _)| in_source(k))
            .map(|(_, o)| EnvironmentProjectOverlay {
                environment: to.to_string(),
                ..o.clone()
            })
            .collect();
        let grants: Vec<EnvironmentProjectPrivilegeGrant> = state
            .grants
            .iter()
            .filter(|(k, _)| in_source(k))
            .map(|(_, g)| EnvironmentProjectPrivilegeGrant {
                environment: to.to_string(),
                ..g.clone()
            })
            .collect();
        for o in overlays {
            state
                .overlays
                .insert((o.org.clone(), o.environment.clone(), o.project.clone()), o);
        }
        for g in grants {
            state
                .grants
                .insert((g.org.clone(), g.environment.clone(), g.project.clone()), g);
        }
        state
            .environments
            .insert((org.to_string(), to.to_string()), copy.clone());
        Ok(copy)
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
}
