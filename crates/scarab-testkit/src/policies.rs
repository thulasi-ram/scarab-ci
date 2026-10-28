//! [`InMemoryPolicies`]: the `PolicyStore` port (ADR-0075) without Postgres.
//! The parity suite in `scarab-db-postgres/tests/policies_contract.rs` holds
//! it to the Postgres adapter's semantics.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::store::{
    decision_page_limit, DecisionCursor, DecisionRecord, NewPolicy, PolicyChange, PolicyScope,
    PolicyStore, PolicyStoreError, PolicyVersion, StoredPolicy,
};

#[derive(Default)]
struct State {
    policies: BTreeMap<String, StoredPolicy>,
    versions: BTreeMap<String, Vec<PolicyVersion>>,
    /// In insertion order.
    decisions: Vec<DecisionRecord>,
}

impl State {
    fn name_taken(&self, scope: &PolicyScope, name: &str, except: Option<&str>) -> bool {
        self.policies.values().any(|p| {
            !p.deleted && &p.scope == scope && p.name == name && Some(p.id.as_str()) != except
        })
    }
}

#[derive(Default)]
pub struct InMemoryPolicies {
    state: Mutex<State>,
}

impl InMemoryPolicies {
    pub fn new() -> Self {
        Self::default()
    }

    fn write_version(
        &self,
        id: &str,
        expected_version: u64,
        body: Option<&Policy>,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        let mut st = self.state.lock().unwrap();
        let current = match st.policies.get(id) {
            Some(p) if !p.deleted => p.clone(),
            _ => return Err(PolicyStoreError::NotFound),
        };
        if current.version != expected_version {
            return Err(PolicyStoreError::Conflict);
        }
        let deleted = body.is_none();
        let body = body.cloned().unwrap_or_else(|| current.body.clone());
        if !deleted && st.name_taken(&current.scope, &body.name, Some(id)) {
            return Err(PolicyStoreError::NameTaken(body.name.clone()));
        }
        let version = current.version + 1;
        let content_hash = body.content_hash();
        let updated = StoredPolicy {
            name: body.name.clone(),
            version,
            body: body.clone(),
            content_hash: content_hash.clone(),
            updated_at: change.at,
            deleted,
            ..current
        };
        st.policies.insert(id.to_string(), updated.clone());
        st.versions
            .entry(id.to_string())
            .or_default()
            .push(PolicyVersion {
                policy_id: id.to_string(),
                version,
                body,
                content_hash,
                author: change.author.clone(),
                reason: change.reason.clone(),
                step_up_ref: change.step_up_ref.clone(),
                deleted,
                at: change.at,
            });
        Ok(updated)
    }
}

#[async_trait]
impl PolicyStore for InMemoryPolicies {
    async fn create(&self, new: &NewPolicy) -> Result<StoredPolicy, PolicyStoreError> {
        let mut st = self.state.lock().unwrap();
        if st.policies.contains_key(&new.id) {
            return Err(PolicyStoreError::Conflict);
        }
        if st.name_taken(&new.scope, &new.body.name, None) {
            return Err(PolicyStoreError::NameTaken(new.body.name.clone()));
        }
        let content_hash = new.body.content_hash();
        let stored = StoredPolicy {
            id: new.id.clone(),
            scope: new.scope.clone(),
            name: new.body.name.clone(),
            version: 1,
            body: new.body.clone(),
            content_hash: content_hash.clone(),
            created_at: new.at,
            updated_at: new.at,
            deleted: false,
        };
        st.policies.insert(new.id.clone(), stored.clone());
        st.versions.insert(
            new.id.clone(),
            vec![PolicyVersion {
                policy_id: new.id.clone(),
                version: 1,
                body: new.body.clone(),
                content_hash,
                author: new.author.clone(),
                reason: new.reason.clone(),
                step_up_ref: None,
                deleted: false,
                at: new.at,
            }],
        );
        Ok(stored)
    }

    async fn update(
        &self,
        id: &str,
        expected_version: u64,
        body: &Policy,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        self.write_version(id, expected_version, Some(body), change)
    }

    async fn delete(
        &self,
        id: &str,
        expected_version: u64,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        self.write_version(id, expected_version, None, change)
    }

    async fn get(&self, id: &str) -> Result<Option<StoredPolicy>, PolicyStoreError> {
        Ok(self.state.lock().unwrap().policies.get(id).cloned())
    }

    async fn list(&self, scope: &PolicyScope) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<StoredPolicy> = st
            .policies
            .values()
            .filter(|p| !p.deleted && &p.scope == scope)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn versions(&self, id: &str) -> Result<Vec<PolicyVersion>, PolicyStoreError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .versions
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    async fn version(
        &self,
        id: &str,
        version: u64,
    ) -> Result<Option<PolicyVersion>, PolicyStoreError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .versions
            .get(id)
            .and_then(|vs| vs.iter().find(|v| v.version == version).cloned()))
    }

    async fn effective(
        &self,
        org: &str,
        project: Option<&str>,
        checkpoint: Checkpoint,
    ) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<StoredPolicy> = st
            .policies
            .values()
            .filter(|p| {
                !p.deleted
                    && p.scope.org() == org
                    && p.body.checkpoint == checkpoint
                    && (p.scope.project().is_none() || p.scope.project() == project)
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            (a.scope.project().is_some(), &a.name).cmp(&(b.scope.project().is_some(), &b.name))
        });
        Ok(out)
    }

    async fn record_decision(&self, record: &DecisionRecord) -> Result<String, PolicyStoreError> {
        let mut st = self.state.lock().unwrap();
        let step_admit = record.decision.checkpoint == Checkpoint::StepAdmit;
        let clash = st.decisions.iter().any(|d| {
            d.id == record.id
                || (step_admit
                    && d.decision.checkpoint == Checkpoint::StepAdmit
                    && d.run_id == record.run_id
                    && d.step_id == record.step_id
                    && d.attempt == record.attempt
                    // Postgres treats NULLs as distinct in a unique index.
                    && record.run_id.is_some()
                    && record.step_id.is_some()
                    && record.attempt.is_some())
        });
        if clash {
            return Err(PolicyStoreError::Conflict);
        }
        st.decisions.push(record.clone());
        Ok(record.id.clone())
    }

    async fn decision(&self, id: &str) -> Result<Option<DecisionRecord>, PolicyStoreError> {
        let st = self.state.lock().unwrap();
        Ok(st.decisions.iter().find(|d| d.id == id).cloned())
    }

    async fn decisions_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<DecisionRecord> = st
            .decisions
            .iter()
            .filter(|d| d.run_id.as_deref() == Some(run_id))
            .cloned()
            .collect();
        out.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
        Ok(out)
    }

    async fn org_decisions(
        &self,
        org: &str,
        limit: usize,
        before: Option<&DecisionCursor>,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<DecisionRecord> = st
            .decisions
            .iter()
            .filter(|d| d.org == org)
            .filter(|d| before.is_none_or(|c| (d.at, d.id.as_str()) < (c.at, c.id.as_str())))
            .cloned()
            .collect();
        out.sort_by(|a, b| (b.at, &b.id).cmp(&(a.at, &a.id)));
        out.truncate(decision_page_limit(limit));
        Ok(out)
    }
}
