//! Reading stored person references (ADR-0080 §3).
//!
//! A Principal has an id (`p_…`, the key) and a handle (`github:alice`, what
//! people type and read). History written before ADR-0080 holds handles and
//! history written after it holds ids, so every read of a stored person
//! reference goes through [`People`]: one lookup that takes either form.
//! A reference that names no Principal (a webhook Actor, `plugin:<name>`,
//! `external:<step>`) passes through unchanged, as does every reference when
//! no identity store is wired (dev, tests), where id and handle are one
//! string.

use std::collections::HashMap;

use scarab_identity::{IdentityStore, PrincipalRecord};
use serde::Serialize;
use utoipa::ToSchema;

/// A person as an API response names them (ADR-0080 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PersonDto {
    /// The Principal's id: what a URL carries. For a reference that names
    /// no Principal, the reference itself.
    pub id: String,
    /// What people type and read.
    pub handle: String,
    pub display_name: Option<String>,
}

/// The Principals a batch of stored references name, looked up once.
pub struct People {
    by_ref: HashMap<String, PrincipalRecord>,
}

impl People {
    /// Look up `refs` (ids or handles, mixed). A store error only costs
    /// display: the references then show as stored.
    pub async fn lookup<'a>(
        store: Option<&dyn IdentityStore>,
        refs: impl IntoIterator<Item = &'a str>,
    ) -> People {
        let mut refs: Vec<String> = refs
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .collect();
        refs.sort();
        refs.dedup();
        let mut by_ref = HashMap::new();
        if let (Some(store), false) = (store, refs.is_empty()) {
            match store.lookup_many(&refs).await {
                Ok(found) => {
                    for p in found {
                        by_ref.insert(p.id.clone(), p.clone());
                        by_ref.insert(p.handle.clone(), p);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "could not look up Principals for display"),
            }
        }
        People { by_ref }
    }

    /// The Principal `reference` names, if any.
    pub fn get(&self, reference: &str) -> Option<&PrincipalRecord> {
        self.by_ref.get(reference)
    }

    /// The id `reference` names, or the reference itself: what a comparison
    /// with stored current state (approver lists, bindings) uses.
    pub fn id(&self, reference: &str) -> String {
        self.get(reference)
            .map_or_else(|| reference.to_string(), |p| p.id.clone())
    }

    /// The handle `reference` names, or the reference itself: what people
    /// read.
    pub fn handle(&self, reference: &str) -> String {
        self.get(reference)
            .map_or_else(|| reference.to_string(), |p| p.handle.clone())
    }

    /// [`Self::handle`] over an optional reference.
    pub fn handle_opt(&self, reference: Option<&str>) -> Option<String> {
        reference.map(|r| self.handle(r))
    }

    /// `reference` as `{id, handle, display_name}`.
    pub fn person(&self, reference: &str) -> PersonDto {
        match self.get(reference) {
            Some(p) => PersonDto {
                id: p.id.clone(),
                handle: p.handle.clone(),
                display_name: p.display_name.clone(),
            },
            None => PersonDto {
                id: reference.to_string(),
                handle: reference.to_string(),
                display_name: None,
            },
        }
    }

    /// `refs` mapped to ids, order kept, later duplicates (the same person
    /// under both forms) dropped.
    pub fn ids(&self, refs: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(refs.len());
        for r in refs {
            let id = self.id(r);
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// `refs` mapped to handles, order kept.
    pub fn handles(&self, refs: &[String]) -> Vec<String> {
        refs.iter().map(|r| self.handle(r)).collect()
    }
}

/// `refs` mapped to ids through the store (one lookup), order kept and
/// duplicates dropped: what approval counting compares (ADR-0080 §3).
pub async fn canonical_ids(store: Option<&dyn IdentityStore>, refs: &[String]) -> Vec<String> {
    People::lookup(store, refs.iter().map(String::as_str))
        .await
        .ids(refs)
}

/// `protection` with every approver list mapped to ids, so it compares with
/// approvals and a recorded approver set whatever form each was stored in
/// (ADR-0080 §3).
pub async fn canonical_protection(
    store: Option<&dyn IdentityStore>,
    protection: &scarab_project::EffectiveProtection,
) -> scarab_project::EffectiveProtection {
    let people = People::lookup(
        store,
        protection
            .approvers
            .iter()
            .chain(&protection.baseline_approvers)
            .chain(&protection.overlay_approvers)
            .map(String::as_str),
    )
    .await;
    scarab_project::EffectiveProtection {
        approvers: people.ids(&protection.approvers),
        baseline_approvers: people.ids(&protection.baseline_approvers),
        overlay_approvers: people.ids(&protection.overlay_approvers),
        ..protection.clone()
    }
}

/// A recorded approver set (`EnvironmentApprovalMet.approvers`) mapped to
/// ids and sorted, as [`scarab_project::EffectiveProtection::approver_set`]
/// sorts: a gate released before ADR-0080, whose record holds handles,
/// compares equal to the same set stored as ids.
pub async fn canonical_approver_set(
    store: Option<&dyn IdentityStore>,
    approvers: &[String],
) -> Vec<String> {
    let mut ids = canonical_ids(store, approvers).await;
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_identity::{IdentityStore, Placeholder, ProviderIdentity};

    fn ident(external_id: &str, handle: &str) -> ProviderIdentity {
        ProviderIdentity {
            provider: "github".into(),
            external_id: external_id.into(),
            handle: handle.into(),
            email: None,
            email_verified: false,
            display_name: None,
        }
    }

    #[tokio::test]
    async fn a_reference_in_either_form_names_the_same_person() {
        let store = scarab_testkit::InMemoryIdentities::new();
        let ada = store
            .sign_in(&ident("1", "ada"), "github:ada", Placeholder::Never, 0)
            .await
            .unwrap();
        let refs = vec![
            "github:ada".to_string(),
            ada.id.clone(),
            "plugin:argocd".to_string(),
        ];
        let people = People::lookup(Some(&store), refs.iter().map(String::as_str)).await;
        assert_eq!(
            people.ids(&refs),
            vec![ada.id.clone(), "plugin:argocd".into()]
        );
        assert_eq!(people.handle(&ada.id), "github:ada");
        assert_eq!(people.handle("plugin:argocd"), "plugin:argocd");
        // No store: every reference is its own id and handle.
        let none = People::lookup(None, refs.iter().map(String::as_str)).await;
        assert_eq!(none.ids(&refs), refs);
    }

    /// Two people who must both approve: ada and bob, stored (after
    /// migration 0093) as ids. ada approved before the upgrade, so her
    /// `GateApproved.by` is her handle; bob approved after it, under his id.
    async fn ada_and_bob() -> (
        scarab_testkit::InMemoryIdentities,
        scarab_identity::PrincipalRecord,
        scarab_identity::PrincipalRecord,
        scarab_project::EffectiveProtection,
    ) {
        let store = scarab_testkit::InMemoryIdentities::new();
        let ada = store
            .sign_in(&ident("1", "ada"), "github:ada", Placeholder::Never, 0)
            .await
            .unwrap();
        let bob = store
            .sign_in(&ident("2", "bob"), "github:bob", Placeholder::Never, 0)
            .await
            .unwrap();
        let protection = scarab_project::effective_protection(
            &scarab_project::EnvironmentProtectionRules {
                approvers: vec![ada.id.clone(), bob.id.clone()],
                ..Default::default()
            },
            None,
        );
        (store, ada, bob, protection)
    }

    #[tokio::test]
    async fn approvals_count_whichever_form_they_were_recorded_in() {
        let (store, ada, bob, protection) = ada_and_bob().await;
        let recorded = vec!["github:ada".to_string(), bob.id.clone()];
        // As stored, the handle matches nobody on a list of ids.
        assert_eq!(protection.approval_violations(&recorded).len(), 1);
        // Through the resolver both count.
        let approvals = canonical_ids(Some(&store), &recorded).await;
        assert_eq!(approvals, vec![ada.id.clone(), bob.id.clone()]);
        assert!(protection.approval_violations(&approvals).is_empty());
        // The same person under both forms is one approval, not two.
        let twice = vec!["github:ada".to_string(), ada.id.clone()];
        assert_eq!(canonical_ids(Some(&store), &twice).await, vec![ada.id]);
    }

    #[tokio::test]
    async fn a_gate_released_before_the_upgrade_still_launches_after_it() {
        use scarab_policy::environment_rules::{launch_recheck, LaunchFacts, UpstreamGateFacts};
        let (store, _ada, _bob, protection) = ada_and_bob().await;
        // The release recorded the approver set as it was then: handles,
        // sorted, as `approver_set` sorted them.
        let met_then = vec!["github:ada".to_string(), "github:bob".to_string()];
        let check = |met: Vec<String>, protection: &scarab_project::EffectiveProtection| {
            let gates = [UpstreamGateFacts {
                gate: "approve".into(),
                released_at: Some(1_000),
                met: Some((met, None)),
            }];
            launch_recheck(LaunchFacts {
                environment: "prod",
                git_ref: "refs/heads/main",
                protection,
                step: "deploy",
                uses_authority: Some(true),
                approval_gates: &gates,
                now_ms: 2_000,
            })
            .refusal
        };
        // Compared as stored, the set "changed" and the launch is refused.
        let raw = check(met_then.clone(), &protection).expect("refused as stored");
        assert!(raw.contains("approvers changed"), "{raw}");
        // Both sides through the resolver: the same people, so it launches.
        let met = canonical_approver_set(Some(&store), &met_then).await;
        let now = canonical_protection(Some(&store), &protection).await;
        assert_eq!(check(met, &now), None);
        // A real change still refuses: bob is no longer an approver.
        let fewer = scarab_project::effective_protection(
            &scarab_project::EnvironmentProtectionRules {
                approvers: vec![protection.approvers[0].clone()],
                ..Default::default()
            },
            None,
        );
        let met = canonical_approver_set(Some(&store), &met_then).await;
        let refused = check(met, &canonical_protection(Some(&store), &fewer).await)
            .expect("a changed set refuses");
        assert!(refused.contains("approvers changed"), "{refused}");
    }
}

/// A response that names people by stored reference (ids after ADR-0080,
/// handles before it): [`Self::refs`] lists them, [`Self::show`] swaps each
/// for the handle people read.
pub trait ShowsPeople {
    fn refs(&self) -> Vec<&str>;
    fn show(&mut self, people: &People);
}

impl<T: ShowsPeople> ShowsPeople for Vec<T> {
    fn refs(&self) -> Vec<&str> {
        self.iter().flat_map(ShowsPeople::refs).collect()
    }
    fn show(&mut self, people: &People) {
        for t in self {
            t.show(people);
        }
    }
}

impl<T: ShowsPeople> ShowsPeople for Option<T> {
    fn refs(&self) -> Vec<&str> {
        self.as_ref().map(ShowsPeople::refs).unwrap_or_default()
    }
    fn show(&mut self, people: &People) {
        if let Some(t) = self {
            t.show(people);
        }
    }
}

impl People {
    /// Replace each reference in `list` with its handle.
    pub fn show_all(&self, list: &mut [String]) {
        for r in list {
            *r = self.handle(r);
        }
    }

    /// Replace `reference` with its handle.
    pub fn show_one(&self, reference: &mut String) {
        *reference = self.handle(reference);
    }
}

/// `value` with every person it names shown by handle: one lookup for all
/// of them.
pub async fn shown<T: ShowsPeople>(store: Option<&dyn IdentityStore>, mut value: T) -> T {
    let people = People::lookup(store, value.refs()).await;
    value.show(&people);
    value
}

impl ShowsPeople for scarab_engine::EventKind {
    fn refs(&self) -> Vec<&str> {
        use scarab_engine::EventPayload as P;
        match &self.kind {
            P::GateApproved { by, .. } => vec![by.as_str()],
            P::EnvironmentApprovalMet { approvers, .. } => {
                approvers.iter().map(String::as_str).collect()
            }
            P::RunCancelRequested { by, .. }
            | P::GateResolved { by, .. }
            | P::RunRerunRequested { by, .. }
            | P::StepRetryRequested { by, .. }
            | P::AttemptSuperseded { by, .. }
            | P::RunSnapshotsPinned { by, .. }
            | P::RunSnapshotsUnpinned { by, .. } => by.as_deref().into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The event as the activity log shows it: each person by handle. The
    /// stored event is never changed (ADR-0080 §3).
    fn show(&mut self, people: &People) {
        use scarab_engine::EventPayload as P;
        match &mut self.kind {
            P::GateApproved { by, .. } => people.show_one(by),
            P::EnvironmentApprovalMet { approvers, .. } => people.show_all(approvers),
            P::RunCancelRequested { by, .. }
            | P::GateResolved { by, .. }
            | P::RunRerunRequested { by, .. }
            | P::StepRetryRequested { by, .. }
            | P::AttemptSuperseded { by, .. }
            | P::RunSnapshotsPinned { by, .. }
            | P::RunSnapshotsUnpinned { by, .. } => {
                if let Some(by) = by {
                    people.show_one(by);
                }
            }
            _ => {}
        }
    }
}
