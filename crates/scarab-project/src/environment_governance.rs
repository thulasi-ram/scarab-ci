//! The org-level Environment's **governance model** and its **pure
//! resolution** (ADR-0071 §2, §3, §8, §10): the baseline protection rules an
//! Environment's admin sets, the per-Project tighten-only overlay, which
//! Projects are eligible at all, the per-Project privilege grant, and the one
//! function that folds a baseline and an overlay into the rules admission
//! enforces ([`effective_protection`]).
//!
//! Since sub-slice c2 this is what **admission reads**: the server resolves a
//! Deploy Run's [`DeployGovernance`] through an
//! [`EnvironmentGovernanceStore`] (one consistent read), refuses an unknown
//! Environment or an ineligible Project before the Run exists, and enforces
//! [`EffectiveProtection`] and the Project's
//! [`EnvironmentProjectPrivilegeGrant`]. The per-Project ADR-0037
//! [`Environment`] and its [`ProtectionRules`] survive only as the **legacy**
//! shape the migration reads (and the golden tests pin it against).
//!
//! [`OrgEnvironment`]: crate::OrgEnvironment

use async_trait::async_trait;

use crate::{
    admit_grants_against_whitelist, glob_match, AdmittedGrants, Environment, GrantRequest,
    ImageGrant, OrgEnvironment, OrgEnvironmentError,
};

/// The protection rules an Environment's admin sets as its **baseline**, and
/// the shape a Project's [`EnvironmentProjectOverlay`] tightens it with
/// (ADR-0071 §1, §3). The same four rules either way, so a baseline and an
/// overlay can be compared field by field.
///
/// [`Default`] is the **permissive** rule set — no approvers, any ref, no
/// timer, no reason — which is what the migration gives
/// every Environment it creates (ADR-0071 §10 step 1) and what an overlay that
/// tightens nothing holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentProtectionRules {
    /// Principals who approve a deploy. Every one must approve (AND, like
    /// [`ProtectionRules::approvers`]) unless the baseline's
    /// [`ApprovalGateRules::required_approvals`] says how many.
    pub approvers: Vec<String>,
    /// Ref globs (`*` wildcards) a deploy's symbolic ref must match. Empty
    /// means **any** ref — which is why two layers are never intersected
    /// (see [`AllowedRefConstraint`]).
    pub allowed_refs: Vec<String>,
    /// A minimum wait (seconds) on the Environment approval gate, counted
    /// on the gate's own wait clock (ADR-0078 §4). It applies only with
    /// approvers: an Environment with none ignores it.
    pub wait_timer_s: u32,
    /// A human dispatch (`manual`/`api`) must carry a reason (ADR-0057 §3).
    pub require_reason: bool,
    /// The Environment approval gate's settings (ADR-0078 §3). **Baseline
    /// only**: an overlay holds the default here, stores nothing for it, and
    /// [`effective_protection`] reads the baseline's alone.
    pub approval_gate: ApprovalGateRules,
}

/// The default maximum approval age, in seconds: 24 hours (ADR-0078 §3).
pub const DEFAULT_MAX_APPROVAL_AGE_S: u32 = 24 * 60 * 60;

/// What an Environment with approvers does at Run creation when a Step that
/// uses its authority has no `manual` gate upstream (ADR-0078 §3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MissingGateMode {
    /// Add one Environment approval gate to the Run (the default).
    #[default]
    Add,
    /// Refuse the Run, naming the Steps and the YAML to add.
    Refuse,
}

impl MissingGateMode {
    /// The wire and column name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Refuse => "refuse",
        }
    }

    /// The mode a wire or column name names; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "add" => Some(Self::Add),
            "refuse" => Some(Self::Refuse),
            _ => None,
        }
    }
}

/// An Environment's settings for its **Environment approval gate**
/// (ADR-0078 §3), set on the baseline only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalGateRules {
    /// Add a gate, or refuse the Run, when an authority Step has no
    /// `manual` gate upstream.
    pub missing_gate: MissingGateMode,
    /// The expiry (seconds) of the gate the Environment adds; `None` waits
    /// until someone approves or rejects it. An authored gate keeps its own.
    pub added_gate_expiry_s: Option<u32>,
    /// How many of the baseline's approvers must approve; `None` means every
    /// one of them (the rule before ADR-0078). A Project overlay's approvers
    /// are always each required, so an overlay still only tightens.
    pub required_approvals: Option<u32>,
    /// The longest (seconds) an approval stays good for: a Step launched
    /// later than this after its approval gate released is refused.
    pub max_approval_age_s: u32,
}

impl Default for ApprovalGateRules {
    fn default() -> Self {
        Self {
            missing_gate: MissingGateMode::Add,
            added_gate_expiry_s: None,
            required_approvals: None,
            max_approval_age_s: DEFAULT_MAX_APPROVAL_AGE_S,
        }
    }
}

impl ApprovalGateRules {
    /// Check the settings against the baseline's `approvers`: a count of at
    /// least one and at most the number of approvers, and a non-zero expiry
    /// and maximum age.
    pub fn validate(&self, approvers: &[String]) -> Result<(), OrgEnvironmentError> {
        let invalid = |m: String| Err(OrgEnvironmentError::Invalid(m));
        if let Some(n) = self.required_approvals {
            if n == 0 {
                return invalid("required approvals must be at least 1".into());
            }
            if n as usize > approvers.len() {
                return invalid(format!(
                    "required approvals is {n}, but the baseline lists {} approver(s)",
                    approvers.len()
                ));
            }
        }
        if self.added_gate_expiry_s == Some(0) {
            return invalid("the added gate's expiry must be at least 1 second".into());
        }
        if self.max_approval_age_s == 0 {
            return invalid("the maximum approval age must be at least 1 second".into());
        }
        Ok(())
    }
}

/// What [`EnvironmentGovernanceStore::delete_environment_cascade`] deleted
/// with the Environment: the Projects whose overlay and whose privilege
/// grant went, each sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentCascadeDeletion {
    pub overlays: Vec<String>,
    pub privilege_grants: Vec<String>,
}

/// One Project's **tighten-only** overlay over one Environment's baseline
/// (ADR-0071 §3). Keyed by `(org, environment, project)`; how it combines
/// with the baseline is [`effective_protection`]'s alone, and no merge it
/// performs can loosen the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentProjectOverlay {
    pub org: String,
    /// The org-level Environment's name (exact, case-sensitive).
    pub environment: String,
    /// The Project's name — its repo's forge name (1:1 in v1).
    pub project: String,
    pub rules: EnvironmentProtectionRules,
}

/// Which Projects an Environment admits Deploy Runs from (ADR-0071 §2).
/// Owned by the Environment, never the Project: the YAML's `environment:`
/// is the request, this is the authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentProjectEligibility {
    /// Every Project in the Org — what the migration gives a name that was
    /// only ever referenced, never defined (ADR-0071 §10 step 6).
    AllProjects,
    /// Exactly the Projects matching one of these name globs (`*`
    /// wildcards). An **empty list admits no Project**: a list is never read
    /// as "unrestricted", unlike an empty `allowed_refs`, because eligibility
    /// starts closed (ADR-0071 §2).
    Projects(Vec<String>),
}

impl Default for EnvironmentProjectEligibility {
    /// A new Environment is **fail-closed**: no eligible Project until an
    /// admin names one (ADR-0071 §2).
    fn default() -> Self {
        Self::Projects(Vec::new())
    }
}

impl EnvironmentProjectEligibility {
    /// Whether `project` may run Deploy Runs into this Environment.
    ///
    /// A pattern matches a Project name **ignoring ASCII case**. Project
    /// names keep the case the forge reported, and GitHub and Forgejo both
    /// treat a repository name case-insensitively, so `Web*` admits
    /// `web-api`. A pattern is matched as stored: a row written before
    /// patterns were trimmed on write is not trimmed here, so it admits no
    /// Project it did not admit before.
    pub fn admits(&self, project: &str) -> bool {
        match self {
            Self::AllProjects => true,
            Self::Projects(patterns) => {
                let project = project.to_ascii_lowercase();
                patterns
                    .iter()
                    .any(|p| glob_match(&p.to_ascii_lowercase(), &project))
            }
        }
    }

    /// The eligibility as a write stores it: each pattern trimmed of
    /// surrounding whitespace. Case is kept, as the admin typed it;
    /// [`admits`](Self::admits) ignores it.
    pub fn normalized(&self) -> Self {
        match self {
            Self::AllProjects => Self::AllProjects,
            Self::Projects(patterns) => {
                Self::Projects(patterns.iter().map(|p| p.trim().to_string()).collect())
            }
        }
    }

    /// Shape-check the project patterns as a write stores them (trimmed):
    /// not empty, no control characters, not only `*` (that is
    /// [`AllProjects`](Self::AllProjects)), and none listed twice once
    /// trimmed and compared ignoring ASCII case. An empty pattern matches no
    /// Project name, so it is refused as a typo rather than kept as a row
    /// that silently does nothing.
    ///
    /// Applies on write only. A row stored before a rule existed still
    /// reads, and still matches as stored.
    pub fn validate(&self) -> Result<(), OrgEnvironmentError> {
        let Self::Projects(patterns) = self else {
            return Ok(());
        };
        let mut seen = std::collections::BTreeSet::new();
        for pattern in patterns {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return Err(OrgEnvironmentError::Invalid(
                    "an eligible project pattern must not be empty".into(),
                ));
            }
            if pattern.chars().any(char::is_control) {
                return Err(OrgEnvironmentError::Invalid(format!(
                    "eligible project pattern `{}` must not contain control characters",
                    pattern.escape_debug()
                )));
            }
            if pattern.chars().all(|c| c == '*') {
                return Err(OrgEnvironmentError::Invalid(format!(
                    "eligible project pattern `{pattern}` admits every project: set \
                     `all_projects` instead (\"Every project\" on the Environment's page)"
                )));
            }
            if !seen.insert(pattern.to_ascii_lowercase()) {
                return Err(OrgEnvironmentError::Invalid(format!(
                    "eligible project pattern `{pattern}` is listed twice (case is ignored)"
                )));
            }
        }
        Ok(())
    }
}

/// The privileges **one Project** holds **within one Environment** (ADR-0071
/// §8): the ADR-0039 image-digest whitelist and the ADR-0055 raw
/// `k8s_overlay` permission. Granted per Project, never to the Environment as
/// a whole, so merging per-repo Environments into one never widens anyone's
/// privileges — and never merged with the baseline or an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentProjectPrivilegeGrant {
    pub org: String,
    pub environment: String,
    pub project: String,
    pub privileged_images: Vec<ImageGrant>,
    pub permit_k8s_overlay: bool,
}

impl EnvironmentProjectPrivilegeGrant {
    /// The ADR-0039 grant decision for a step of a Deploy Run that admission
    /// has already governed by an Environment, given the Project's grant
    /// there, if any. **Identical** to [`ProtectionRules::admit_grants`] over
    /// the grant's whitelist — the two share one body — with `None` deciding
    /// exactly as an empty whitelist did: self-service `run-as-root` only,
    /// every governed grant refused. (No Environment at all is the caller's
    /// separate "governed grants require a target Environment" refusal.)
    pub fn admit_grants(
        grant: Option<&Self>,
        req: &GrantRequest,
        image: &str,
        locked_out: bool,
    ) -> Result<AdmittedGrants, Vec<String>> {
        let whitelist = grant.map_or(&[][..], |g| &g.privileged_images[..]);
        admit_grants_against_whitelist(whitelist, req, image, locked_out)
    }

    /// Whether a step may set a raw `k8s_overlay` (ADR-0055): only under a
    /// grant that permits it — **fail-closed**, so no grant means no.
    pub fn permits_k8s_overlay(grant: Option<&Self>) -> bool {
        grant.is_some_and(|g| g.permit_k8s_overlay)
    }
}

/// The allowed-refs rule after a merge: one list **per source layer**
/// (baseline, then overlay), and a ref is allowed only if **every** layer
/// allows it — an empty layer allowing any ref (ADR-0071 §3).
///
/// Kept as layers and never collapsed into one list, because the obvious
/// collapse — intersecting the lists — fails open: baseline
/// `[refs/heads/main]` and overlay `[refs/tags/*]` intersect to `[]`, which
/// reads as "any ref" when the truth is "no ref".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedRefConstraint(Vec<Vec<String>>);

impl AllowedRefConstraint {
    /// A constraint over these layers, in source order.
    pub fn from_layers(layers: impl IntoIterator<Item = Vec<String>>) -> Self {
        Self(layers.into_iter().collect())
    }

    /// The layers as given, for showing a user which layer said what.
    pub fn layers(&self) -> &[Vec<String>] {
        &self.0
    }

    /// Whether `git_ref` matches every non-empty layer.
    pub fn allows(&self, git_ref: &str) -> bool {
        self.0
            .iter()
            .all(|layer| layer.is_empty() || layer.iter().any(|p| glob_match(p, git_ref)))
    }

    /// No layer restricts refs at all.
    pub fn is_unrestricted(&self) -> bool {
        self.0.iter().all(Vec::is_empty)
    }
}

/// The rules admission enforces for **one Project's** Deploy Runs into **one
/// Environment**: its baseline merged with that Project's overlay, if any.
/// Only [`effective_protection`] builds one. Privileges are deliberately
/// absent — they are the Project's [`EnvironmentProjectPrivilegeGrant`]
/// alone, never merged (ADR-0071 §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProtection {
    /// Every baseline approver, then every overlay approver not already
    /// listed. Who must approve is [`EffectiveProtection::approval_violations`]'s.
    pub approvers: Vec<String>,
    /// The baseline's approvers: the pool `required_approvals` counts from.
    pub baseline_approvers: Vec<String>,
    /// The overlay's approvers: each one required, whatever the count.
    pub overlay_approvers: Vec<String>,
    pub allowed_refs: AllowedRefConstraint,
    pub wait_timer_s: u32,
    pub require_reason: bool,
    /// The baseline's approval gate settings (an overlay has none).
    pub approval_gate: ApprovalGateRules,
}

/// Fold an Environment's `baseline` and one Project's `overlay` into the rules admission enforces (ADR-0071
/// §3). Each field merges so the overlay can **only tighten**:
///
/// - **approvers**: ordered, de-duplicated union (baseline first) — both
///   layers' approvers are required.
/// - **allowed refs**: a ref must match **both** layers, an empty layer
///   matching any ([`AllowedRefConstraint`]; never a list intersection).
/// - **wait timer**: the longer.
/// - **require_reason**: either layer setting it sets it.
///
/// With no overlay the baseline applies as-is.
pub fn effective_protection(
    baseline: &EnvironmentProtectionRules,
    overlay: Option<&EnvironmentProjectOverlay>,
) -> EffectiveProtection {
    let mut approvers: Vec<String> = Vec::new();
    let overlay_rules = overlay.map(|o| &o.rules);
    let all_approvers = baseline
        .approvers
        .iter()
        .chain(overlay_rules.into_iter().flat_map(|r| &r.approvers));
    for approver in all_approvers {
        if !approvers.contains(approver) {
            approvers.push(approver.clone());
        }
    }

    let allowed_refs = AllowedRefConstraint::from_layers(
        std::iter::once(baseline.allowed_refs.clone())
            .chain(overlay_rules.map(|r| r.allowed_refs.clone())),
    );

    let (wait_timer_s, require_reason) = match overlay_rules {
        None => (baseline.wait_timer_s, baseline.require_reason),
        Some(o) => (
            baseline.wait_timer_s.max(o.wait_timer_s),
            baseline.require_reason || o.require_reason,
        ),
    };

    EffectiveProtection {
        approvers,
        baseline_approvers: baseline.approvers.clone(),
        overlay_approvers: overlay_rules
            .map(|r| r.approvers.clone())
            .unwrap_or_default(),
        allowed_refs,
        wait_timer_s,
        require_reason,
        approval_gate: baseline.approval_gate.clone(),
    }
}

impl EffectiveProtection {
    /// Whether `git_ref` (a **symbolic** ref — `refs/heads/main`,
    /// `refs/tags/v1` — never a commit) may deploy here.
    pub fn ref_allowed(&self, git_ref: &str) -> bool {
        self.allowed_refs.allows(git_ref)
    }

    /// No layer restricts refs, so any ref — and an event with none — may
    /// deploy here.
    pub fn unrestricted_refs(&self) -> bool {
        self.allowed_refs.is_unrestricted()
    }

    /// The allowed-refs check at Run creation (ADR-0037), given the event's
    /// symbolic ref if it has one (`Event::protection_ref`). An event with
    /// **no** symbolic ref (cron, comment, upstream) is admitted only where
    /// refs are unrestricted — **fail-closed**, exactly as the server's
    /// `admit_and_create_run` decides on [`ProtectionRules`] today.
    pub fn admits_protection_ref(&self, protection_ref: Option<&str>) -> bool {
        match protection_ref {
            Some(git_ref) => self.ref_allowed(git_ref),
            None => self.unrestricted_refs(),
        }
    }

    /// Whether a deploy from `git_ref` carrying `approvals` may proceed: the
    /// ref is allowed and every required approver approved. Returns every
    /// violation (empty = admitted), worded as [`ProtectionRules::admits`]
    /// words them, so a blocked gate reads the same after the migration.
    pub fn admits(&self, git_ref: &str, approvals: &[String]) -> Result<(), Vec<String>> {
        let mut violations = Vec::new();
        if !self.ref_allowed(git_ref) {
            violations.push(format!("ref `{git_ref}` is not allowed to deploy here"));
        }
        violations.extend(self.approval_violations(approvals));
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// What `approvals` still lack (empty = enough). With no
    /// `required_approvals`, every approver must approve. With `n`, `n` of
    /// the baseline's approvers must, and every overlay approver as well.
    pub fn approval_violations(&self, approvals: &[String]) -> Vec<String> {
        let approved = |who: &String| approvals.iter().any(|a| a == who);
        let missing = |who: &String| format!("missing required approval from `{who}`");
        let Some(n) = self.approval_gate.required_approvals else {
            return self
                .approvers
                .iter()
                .filter(|a| !approved(a))
                .map(missing)
                .collect();
        };
        let mut violations: Vec<String> = self
            .overlay_approvers
            .iter()
            .filter(|a| !approved(a))
            .map(missing)
            .collect();
        let have = self
            .baseline_approvers
            .iter()
            .filter(|a| approved(a))
            .count();
        let need = (n as usize).min(self.baseline_approvers.len());
        if have < need {
            violations.push(format!(
                "needs {need} approval(s) from {}; has {have}",
                self.baseline_approvers
                    .iter()
                    .map(|a| format!("`{a}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        violations
    }

    /// The wait timer that applies: none without approvers (ADR-0078 §4).
    pub fn effective_wait_timer_s(&self) -> u32 {
        if self.approvers.is_empty() {
            0
        } else {
            self.wait_timer_s
        }
    }

    /// The approver set an Environment approval gate's release is recorded
    /// with, and every launch compares against: the approvers, sorted, and
    /// how many of the baseline's are required.
    pub fn approver_set(&self) -> (Vec<String>, Option<u32>) {
        let mut approvers = self.approvers.clone();
        approvers.sort();
        (approvers, self.approval_gate.required_approvals)
    }

    /// The `require_reason` guardrail (ADR-0057 §3), with
    /// [`ProtectionRules::admits_reason`]'s semantics and wording: a human
    /// dispatch (`manual`/`api`) with no non-blank reason is blocked; every
    /// other trigger is exempt.
    pub fn admits_reason(
        &self,
        is_human_dispatch: bool,
        has_reason: bool,
    ) -> Result<(), Vec<String>> {
        if self.require_reason && is_human_dispatch && !has_reason {
            Err(vec![
                "a reason is required to dispatch a run into this environment".to_string(),
            ])
        } else {
            Ok(())
        }
    }
}

/// Turn one Project's ADR-0037 [`Environment`] into what replaces it
/// (ADR-0071 §10 steps 2, 3 and 5): that Project's overlay over the new org
/// Environment of the same name, and its privilege grant if it had any
/// privilege to carry.
///
/// - **Overlay**: approvers, allowed refs, wait timer and `require_reason`
///   carried verbatim, so over the **permissive** baseline the migration
///   creates, the Project's effective rules decide exactly as its old rules
///   did.
/// - **Concurrency is dropped** (step 3). The UI stored 1 for every
///   Environment and nothing ever read it.
/// - **Grant** only when the old whitelist is non-empty or raw overlays were
///   permitted (step 5); otherwise `None`, which
///   [`EnvironmentProjectPrivilegeGrant::admit_grants`] decides exactly as
///   the empty whitelist did.
///
/// The old secret scope and OIDC subject are not carried: both are canonical
/// coordinates the server derives, never stored decisions (ADR-0071 §6, §11).
pub fn migrate_legacy_environment(
    org: &str,
    project: &str,
    legacy: &Environment,
) -> (
    EnvironmentProjectOverlay,
    Option<EnvironmentProjectPrivilegeGrant>,
) {
    let old = &legacy.protection;
    let overlay = EnvironmentProjectOverlay {
        org: org.to_string(),
        environment: legacy.name.clone(),
        project: project.to_string(),
        rules: EnvironmentProtectionRules {
            approvers: old.approvers.clone(),
            allowed_refs: old.allowed_refs.clone(),
            wait_timer_s: old.wait_timer,
            require_reason: old.require_reason,
            approval_gate: ApprovalGateRules::default(),
        },
    };
    let grant = (!old.privileged_images.is_empty() || old.permit_k8s_overlay).then(|| {
        EnvironmentProjectPrivilegeGrant {
            org: org.to_string(),
            environment: legacy.name.clone(),
            project: project.to_string(),
            privileged_images: old.privileged_images.clone(),
            permit_k8s_overlay: old.permit_k8s_overlay,
        }
    });
    (overlay, grant)
}

/// Everything admission needs to decide one Project's Deploy Run into one
/// Environment (ADR-0071 §2, §3, §8), read **together** by
/// [`EnvironmentGovernanceStore::deploy_governance`] so one decision never
/// pairs, say, an old eligibility list with a new overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployGovernance {
    /// The Environment, with its baseline and eligibility.
    pub environment: OrgEnvironment,
    /// Whether the Environment's [`EnvironmentProjectEligibility`] admits the
    /// Project this was read for. Computed by [`DeployGovernance::new`] from
    /// `environment.eligibility`, so every store decides eligibility with the
    /// one glob matcher rather than its own query.
    pub eligible: bool,
    /// The Project's tighten-only overlay, if it has one.
    pub overlay: Option<EnvironmentProjectOverlay>,
    /// The Project's privilege grant there, if it has one.
    pub privilege_grant: Option<EnvironmentProjectPrivilegeGrant>,
}

impl DeployGovernance {
    /// The governance of `project`'s Deploy Runs into `environment`.
    pub fn new(
        environment: OrgEnvironment,
        project: &str,
        overlay: Option<EnvironmentProjectOverlay>,
        privilege_grant: Option<EnvironmentProjectPrivilegeGrant>,
    ) -> Self {
        let eligible = environment.eligibility.admits(project);
        Self {
            environment,
            eligible,
            overlay,
            privilege_grant,
        }
    }

    /// The rules admission enforces: the baseline merged with the overlay.
    pub fn effective_protection(&self) -> EffectiveProtection {
        effective_protection(&self.environment.protection, self.overlay.as_ref())
    }
}

/// One Project's standing within one Environment, as the Environment's admin
/// reviews it: whether it is eligible, and the overlay and privilege grant
/// it holds there, if any (ADR-0071 §2, §3, §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentProjectRecord {
    pub project: String,
    /// Whether the Environment's eligibility admits the Project now. A
    /// Project made ineligible keeps its overlay and grant, so `false`
    /// appears here.
    pub eligible: bool,
    pub overlay: Option<EnvironmentProjectOverlay>,
    pub privilege_grant: Option<EnvironmentProjectPrivilegeGrant>,
}

impl EnvironmentProjectRecord {
    /// One record per Project holding an overlay or a grant in
    /// `environment`, by Project name, eligibility decided by the
    /// Environment's own matcher — so every store answers the same way.
    pub fn collect(
        environment: &OrgEnvironment,
        overlays: impl IntoIterator<Item = EnvironmentProjectOverlay>,
        grants: impl IntoIterator<Item = EnvironmentProjectPrivilegeGrant>,
    ) -> Vec<Self> {
        let mut records: std::collections::BTreeMap<String, Self> =
            std::collections::BTreeMap::new();
        fn record<'a>(
            records: &'a mut std::collections::BTreeMap<String, EnvironmentProjectRecord>,
            environment: &OrgEnvironment,
            project: &str,
        ) -> &'a mut EnvironmentProjectRecord {
            records
                .entry(project.to_string())
                .or_insert_with(|| EnvironmentProjectRecord {
                    project: project.to_string(),
                    eligible: environment.eligibility.admits(project),
                    overlay: None,
                    privilege_grant: None,
                })
        }
        for overlay in overlays {
            let project = overlay.project.clone();
            record(&mut records, environment, &project).overlay = Some(overlay);
        }
        for grant in grants {
            let project = grant.project.clone();
            record(&mut records, environment, &project).privilege_grant = Some(grant);
        }
        records.into_values().collect()
    }
}

/// An exclusive claim on one Environment name in one org, from
/// [`EnvironmentGovernanceStore::claim_environment_name`]. It holds whatever
/// the store needs to keep the claim (a Postgres transaction holding an
/// advisory lock, a set entry).
///
/// [`release`](Self::release) frees the claim and returns once another
/// claim on the name can succeed. Dropping it also frees the claim, but a
/// store may finish that after the drop returns: Postgres rolls a dropped
/// transaction back only when its connection goes back to the pool. A
/// request that answers while its claim is still held makes the caller's
/// next request on the name a spurious 409, so a handler releases before it
/// answers, and the drop is for a request that is abandoned.
pub struct EnvironmentNameClaim {
    /// Frees the claim. Dropping it unrun drops what it captured, which
    /// frees the claim too.
    release: Option<ReleaseClaim>,
}

type ReleaseClaim =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

impl EnvironmentNameClaim {
    /// A claim kept by `held` and released by dropping it, for a store
    /// whose drop frees the claim before it returns.
    pub fn new(held: impl std::any::Any + Send) -> Self {
        Self::released_by(move || async move { drop(held) })
    }

    /// A claim released by running `release`, which returns once the claim
    /// is free. Dropping the claim unreleased drops `release`, and what it
    /// captured must free the claim on drop.
    pub fn released_by<F, Fut>(release: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        Self {
            release: Some(Box::new(move || Box::pin(release()))),
        }
    }

    /// Free the claim. When this returns, another claim on the name
    /// succeeds.
    pub async fn release(mut self) {
        if let Some(release) = self.release.take() {
            release().await;
        }
    }
}

impl std::fmt::Debug for EnvironmentNameClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvironmentNameClaim")
    }
}

/// Durable store for the governance half of org-level Environments (ADR-0071
/// §2, §3, §8): baseline protection, eligibility, per-Project overlays and
/// privilege grants. The Environment itself (name, description) is the [`OrgEnvironmentStore`](crate::OrgEnvironmentStore)'s.
///
/// The baseline and the eligibility each have **their own write**, never a
/// field of the whole-Environment replace: that replace is what the c-min
/// admin form sends, and a form that knows nothing of governance must not
/// reset it to permissive (or to "no eligible Project") by omission.
///
/// Every write naming an Environment that does not exist is refused with
/// [`OrgEnvironmentError::NotFound`].
///
/// **Optimistic concurrency (ADR-0073 §3).** Each loosenable write has an
/// `…_expecting` form taking what the caller read before deciding whether
/// the write loosens (and so needs a step-up). `Some(old)` makes the write
/// conditional, in the store's own transaction: if what is stored now is not
/// `old`, nothing is written and the answer is
/// [`OrgEnvironmentError::Conflict`] — so a concurrent change can never slip
/// a loosening past a write that was judged a tightening. `None` writes
/// unconditionally, exactly as the plain form (which is that, provided).
#[async_trait]
pub trait EnvironmentGovernanceStore: Send + Sync {
    /// The Environment — baseline and eligibility included — read in one
    /// consistent snapshot, with no Project lens: what a conditional
    /// governance write judges its loosenings against. `None` when the Org
    /// has no Environment by that name (exact, case-sensitive).
    async fn org_environment(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Option<OrgEnvironment>, OrgEnvironmentError>;

    /// The governance of `project`'s Deploy Runs into `environment`, read in
    /// **one consistent snapshot**; `None` when the Org has no Environment by
    /// that name (exact, case-sensitive).
    async fn deploy_governance(
        &self,
        org: &str,
        project: &str,
        environment: &str,
    ) -> Result<Option<DeployGovernance>, OrgEnvironmentError>;

    /// The governance of `project` in every Environment of `org` that it is
    /// eligible for **or** holds an overlay or grant in (a Project made
    /// ineligible keeps its overlay, and an admin must still see it), by
    /// Environment name.
    async fn project_environment_governance(
        &self,
        org: &str,
        project: &str,
    ) -> Result<Vec<DeployGovernance>, OrgEnvironmentError>;

    /// Replace an Environment's baseline protection rules. Returns the
    /// Environment as stored.
    async fn put_environment_protection(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        self.put_environment_protection_expecting(
            org,
            environment,
            protection,
            None,
            updated_by,
            updated_at,
        )
        .await
    }

    /// [`put_environment_protection`](Self::put_environment_protection),
    /// refused with [`OrgEnvironmentError::Conflict`] unless the stored
    /// baseline is `expected` (when given).
    async fn put_environment_protection_expecting(
        &self,
        org: &str,
        environment: &str,
        protection: &EnvironmentProtectionRules,
        expected: Option<&EnvironmentProtectionRules>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError>;

    /// Replace an Environment's eligibility. Refuses an invalid list
    /// ([`EnvironmentProjectEligibility::validate`]). Returns the Environment
    /// as stored.
    async fn put_environment_eligibility(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError> {
        self.put_environment_eligibility_expecting(
            org,
            environment,
            eligibility,
            None,
            updated_by,
            updated_at,
        )
        .await
    }

    /// [`put_environment_eligibility`](Self::put_environment_eligibility),
    /// refused with [`OrgEnvironmentError::Conflict`] unless the stored
    /// eligibility is `expected` (when given; pattern order ignored).
    async fn put_environment_eligibility_expecting(
        &self,
        org: &str,
        environment: &str,
        eligibility: &EnvironmentProjectEligibility,
        expected: Option<&EnvironmentProjectEligibility>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError>;

    /// Create or replace one Project's overlay.
    async fn put_environment_project_overlay(
        &self,
        overlay: &EnvironmentProjectOverlay,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        self.put_environment_project_overlay_expecting(overlay, None, updated_by, updated_at)
            .await
    }

    /// [`put_environment_project_overlay`](Self::put_environment_project_overlay),
    /// refused with [`OrgEnvironmentError::Conflict`] unless the stored
    /// overlay is `expected` (when given): `Some(None)` expects the Project
    /// to hold none, `Some(Some(o))` expects `o`'s rules. `Some(None)` is
    /// also refused with `Conflict` when the Environment does not admit the
    /// Project, checked in the same write so a concurrent withdrawal of
    /// eligibility cannot slip between the check and the insert.
    async fn put_environment_project_overlay_expecting(
        &self,
        overlay: &EnvironmentProjectOverlay,
        expected: Option<Option<&EnvironmentProjectOverlay>>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError>;

    /// Remove one Project's overlay. `false` if there was none.
    async fn delete_environment_project_overlay(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError>;

    /// Create or replace one Project's privilege grant.
    async fn put_environment_project_privilege_grant(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError> {
        self.put_environment_project_privilege_grant_expecting(grant, None, updated_by, updated_at)
            .await
    }

    /// [`put_environment_project_privilege_grant`](Self::put_environment_project_privilege_grant),
    /// refused with [`OrgEnvironmentError::Conflict`] unless the stored grant
    /// is `expected` (when given): `Some(None)` expects none, `Some(Some(g))`
    /// expects `g`'s whitelist and overlay permission.
    async fn put_environment_project_privilege_grant_expecting(
        &self,
        grant: &EnvironmentProjectPrivilegeGrant,
        expected: Option<Option<&EnvironmentProjectPrivilegeGrant>>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<(), OrgEnvironmentError>;

    /// Remove one Project's privilege grant. `false` if there was none.
    async fn delete_environment_project_privilege_grant(
        &self,
        org: &str,
        environment: &str,
        project: &str,
    ) -> Result<bool, OrgEnvironmentError>;

    /// Claim Environment name `name` in `org` for one Duplicate, which writes
    /// to the secret store first and creates the Environment after (the two
    /// cannot share a transaction). While the claim is held, a second claim
    /// on the same name answers `None`, so two Duplicates to one name never
    /// both write secrets under it and one's cleanup never deletes what the
    /// other's Environment relies on. Dropping the claim releases it,
    /// whether the Duplicate succeeded, failed or was abandoned.
    ///
    /// The claim excludes other claims only. The server's create of an
    /// Environment takes it too, so a create and a Duplicate to one name
    /// exclude each other; a caller that writes the store without it can
    /// still take the name, which [`duplicate_environment`](Self::duplicate_environment)
    /// answers with [`OrgEnvironmentError::Conflict`].
    async fn claim_environment_name(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<EnvironmentNameClaim>, OrgEnvironmentError>;

    /// Create Environment `to` in `org` as a copy of `from` (git-bug
    /// d002a0c): its description, its baseline protection rules (every
    /// field), its eligibility, and every Project's overlay and privilege
    /// grant — all in **one** transaction, so a failure leaves no partial
    /// copy. The copy shows `display_name` and is stamped created and
    /// updated by `updated_by` at `updated_at`. `from` is not changed.
    ///
    /// [`OrgEnvironmentError::NotFound`] when `from` does not exist;
    /// [`OrgEnvironmentError::Conflict`] when `to` already does (nothing
    /// changed either way); [`OrgEnvironmentError::Invalid`] for a name or
    /// display name the rules refuse. Returns the copy as stored.
    async fn duplicate_environment(
        &self,
        org: &str,
        from: &str,
        to: &str,
        display_name: Option<&str>,
        updated_by: &str,
        updated_at: i64,
    ) -> Result<OrgEnvironment, OrgEnvironmentError>;

    /// Delete Environment `environment` of `org` with its eligibility and
    /// every Project's overlay and privilege grant in it, in **one**
    /// transaction: all of them go, or none does. What an admin's explicit
    /// cascade delete asks for; a plain delete never drops an overlay or a
    /// grant. `None` when there is no such Environment (nothing changed).
    ///
    /// The Environment's secrets live in the secret store, which this store
    /// cannot share a transaction with: they are the caller's to delete.
    async fn delete_environment_cascade(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Option<EnvironmentCascadeDeletion>, OrgEnvironmentError>;

    /// Every Project holding an overlay or a privilege grant in
    /// `environment`, by Project name, with its eligibility now — what the
    /// Environment's admin reviews (and why a delete is refused).
    /// [`OrgEnvironmentError::NotFound`] if there is no such Environment.
    async fn environment_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<EnvironmentProjectRecord>, OrgEnvironmentError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtectionRules;
    use proptest::prelude::*;
    use scarab_secrets::SecretScope;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn rules(approvers: &[&str], allowed_refs: &[&str]) -> EnvironmentProtectionRules {
        EnvironmentProtectionRules {
            approvers: strings(approvers),
            allowed_refs: strings(allowed_refs),
            ..Default::default()
        }
    }

    fn overlay(rules: EnvironmentProtectionRules) -> EnvironmentProjectOverlay {
        EnvironmentProjectOverlay {
            org: "acme".into(),
            environment: "prod".into(),
            project: "payments".into(),
            rules,
        }
    }

    fn legacy(protection: ProtectionRules) -> Environment {
        Environment {
            name: "prod".into(),
            protection,
        }
    }

    fn legacy_rules() -> ProtectionRules {
        ProtectionRules {
            approvers: Vec::new(),
            wait_timer: 0,
            allowed_refs: Vec::new(),
            concurrency: 1,
            secret_scope: SecretScope::Org { org: "acme".into() },
            oidc_subject: String::new(),
            privileged_images: Vec::new(),
            permit_k8s_overlay: false,
            require_reason: false,
        }
    }

    // --- EnvironmentProjectEligibility -----------------------------------

    #[test]
    fn an_empty_eligibility_list_admits_no_project() {
        let none = EnvironmentProjectEligibility::Projects(Vec::new());
        assert!(!none.admits("payments"));
        assert!(!none.admits(""));
        assert_eq!(EnvironmentProjectEligibility::default(), none);
    }

    #[test]
    fn eligibility_admits_listed_and_globbed_projects_only() {
        let e = EnvironmentProjectEligibility::Projects(strings(&["payments", "team-a-*"]));
        assert!(e.admits("payments"));
        assert!(e.admits("team-a-api"));
        assert!(!e.admits("checkout"));
        assert!(!e.admits("payments-v2"));
        assert!(EnvironmentProjectEligibility::AllProjects.admits("anything"));
    }

    #[test]
    fn eligibility_refuses_empty_and_duplicate_patterns() {
        for bad in [
            strings(&[""]),
            strings(&["a", "a"]),
            vec!["pay\nments".to_string()],
        ] {
            assert!(
                matches!(
                    EnvironmentProjectEligibility::Projects(bad.clone()).validate(),
                    Err(OrgEnvironmentError::Invalid(_))
                ),
                "{bad:?}"
            );
        }
        assert!(
            EnvironmentProjectEligibility::Projects(strings(&["a", "b*"]))
                .validate()
                .is_ok()
        );
        assert!(EnvironmentProjectEligibility::AllProjects
            .validate()
            .is_ok());
    }

    /// A blank pattern, a pattern of only `*`, and two patterns that differ
    /// only by edge spaces or ASCII case are refused; the `*` refusal names
    /// the all-projects setting.
    #[test]
    fn eligibility_refuses_blank_star_only_and_near_duplicate_patterns() {
        let invalid =
            |patterns: &[&str]| match EnvironmentProjectEligibility::Projects(strings(patterns))
                .validate()
            {
                Err(OrgEnvironmentError::Invalid(m)) => m,
                other => panic!("{patterns:?}: {other:?}"),
            };
        assert!(invalid(&["   "]).contains("must not be empty"));
        for star in [&["*"][..], &["**"], &["web", " * "]] {
            let m = invalid(star);
            assert!(m.contains("all_projects"), "{m}");
        }
        assert!(invalid(&["web", " web "]).contains("listed twice"));
        assert!(invalid(&["Web*", "web*"]).contains("listed twice"));
        // A `*` inside a pattern is a glob, not a request for every project.
        assert!(
            EnvironmentProjectEligibility::Projects(strings(&["*-api", "web*"]))
                .validate()
                .is_ok()
        );
    }

    /// Project names keep the forge's case, and both forges treat a
    /// repository name case-insensitively: so does eligibility.
    #[test]
    fn eligibility_matches_project_names_ignoring_ascii_case() {
        let e = EnvironmentProjectEligibility::Projects(strings(&["Web*", "api"]));
        assert!(e.admits("web-api"));
        assert!(e.admits("WEB"));
        assert!(e.admits("API"));
        assert!(!e.admits("checkout"));
    }

    #[test]
    fn eligibility_is_stored_trimmed_with_its_case() {
        let e = EnvironmentProjectEligibility::Projects(strings(&["  Web* ", "api\t"]));
        assert_eq!(
            e.normalized(),
            EnvironmentProjectEligibility::Projects(strings(&["Web*", "api"]))
        );
        assert_eq!(
            EnvironmentProjectEligibility::AllProjects.normalized(),
            EnvironmentProjectEligibility::AllProjects
        );
    }

    /// Rows written before the write rules still read and match as stored:
    /// a `*` row admits every project, an untrimmed one matches nothing it
    /// did not match before.
    #[test]
    fn eligibility_rows_written_before_the_rules_still_match() {
        assert!(EnvironmentProjectEligibility::Projects(strings(&["*"])).admits("anything"));
        let untrimmed = EnvironmentProjectEligibility::Projects(strings(&[" web"]));
        assert!(!untrimmed.admits("web"));
        assert!(untrimmed.admits(" web"));
    }

    // --- effective_protection: one example per merge rule -----------------

    #[test]
    fn no_overlay_applies_the_baseline_as_is() {
        let mut b = rules(&["alice"], &["refs/heads/main"]);
        b.wait_timer_s = 30;
        b.require_reason = true;
        let e = effective_protection(&b, None);
        assert_eq!(e.approvers, strings(&["alice"]));
        assert_eq!(e.allowed_refs.layers(), &[strings(&["refs/heads/main"])]);
        assert_eq!(e.wait_timer_s, 30);
        assert!(e.require_reason);
    }

    #[test]
    fn approvers_are_an_ordered_deduplicated_union() {
        let b = rules(&["alice", "bob", "alice"], &[]);
        let o = overlay(rules(&["carol", "bob"], &[]));
        let e = effective_protection(&b, Some(&o));
        assert_eq!(e.approvers, strings(&["alice", "bob", "carol"]));
        let approvals = strings(&["alice", "bob"]);
        let err = e.admits("refs/heads/main", &approvals).unwrap_err();
        assert_eq!(
            err,
            vec!["missing required approval from `carol`".to_string()]
        );
    }

    #[test]
    fn a_ref_must_match_both_layers_never_their_intersection() {
        // ADR-0071 §3's own counter-example: intersecting the lists gives []
        // which reads as "any ref" — the merge must instead admit nothing.
        let b = rules(&[], &["refs/heads/main"]);
        let o = overlay(rules(&[], &["refs/tags/*"]));
        let e = effective_protection(&b, Some(&o));
        for r in ["refs/heads/main", "refs/tags/v1", "refs/heads/dev", ""] {
            assert!(!e.ref_allowed(r), "{r}");
        }
        assert!(!e.unrestricted_refs());
        assert!(!e.admits_protection_ref(None));

        // Overlapping layers admit only what both admit.
        let b = rules(&[], &["refs/heads/*"]);
        let o = overlay(rules(&[], &["refs/heads/main", "refs/tags/*"]));
        let e = effective_protection(&b, Some(&o));
        assert!(e.ref_allowed("refs/heads/main"));
        assert!(!e.ref_allowed("refs/heads/dev"));
        assert!(!e.ref_allowed("refs/tags/v1"));
    }

    #[test]
    fn an_empty_layer_allows_any_ref_and_no_symbolic_ref_needs_every_layer_empty() {
        let e = effective_protection(&rules(&[], &[]), None);
        assert!(e.unrestricted_refs());
        assert!(e.admits_protection_ref(None));
        assert!(e.admits_protection_ref(Some("refs/pull/7/head")));

        let o = overlay(rules(&[], &["refs/heads/main"]));
        let e = effective_protection(&rules(&[], &[]), Some(&o));
        assert!(e.ref_allowed("refs/heads/main"));
        assert!(!e.ref_allowed("refs/heads/dev"));
        // A cron run has no symbolic ref: refused once any layer restricts.
        assert!(!e.admits_protection_ref(None));
    }

    #[test]
    fn wait_and_reason_take_the_stricter_side() {
        let mut b = rules(&[], &[]);
        b.wait_timer_s = 60;
        let o = overlay(EnvironmentProtectionRules {
            wait_timer_s: 10,
            require_reason: true,
            ..Default::default()
        });
        let e = effective_protection(&b, Some(&o));
        assert_eq!(e.wait_timer_s, 60);
        assert!(e.require_reason);
    }

    #[test]
    fn effective_admits_reason_matches_the_adr_0057_rule() {
        let o = overlay(EnvironmentProtectionRules {
            require_reason: true,
            ..Default::default()
        });
        let e = effective_protection(&rules(&[], &[]), Some(&o));
        let err = e.admits_reason(true, false).unwrap_err();
        assert!(err[0].contains("reason is required"));
        assert!(e.admits_reason(true, true).is_ok());
        assert!(e.admits_reason(false, false).is_ok());
    }

    // --- EnvironmentProjectPrivilegeGrant ---------------------------------

    const IMG: &str = "ghcr.io/acme/deployer@sha256:aaaa";

    fn grant(
        images: Vec<ImageGrant>,
        permit_k8s_overlay: bool,
    ) -> EnvironmentProjectPrivilegeGrant {
        EnvironmentProjectPrivilegeGrant {
            org: "acme".into(),
            environment: "prod".into(),
            project: "payments".into(),
            privileged_images: images,
            permit_k8s_overlay,
        }
    }

    /// One record per Project holding an overlay, a grant or both, in name
    /// order, each Project's eligibility decided by the Environment's own
    /// matcher — a Project made ineligible still shows what it holds.
    #[test]
    fn project_records_merge_overlays_and_grants_per_project() {
        let env = OrgEnvironment {
            org: "acme".into(),
            name: "prod".into(),
            display_name: None,
            description: String::new(),
            protection: EnvironmentProtectionRules::default(),
            eligibility: EnvironmentProjectEligibility::Projects(strings(&["pay*"])),
            created_at: 0,
            updated_at: 0,
            updated_by: "amy".into(),
        };
        let mut web = overlay(rules(&["alice"], &[]));
        web.project = "web".into();
        let records = EnvironmentProjectRecord::collect(
            &env,
            [overlay(rules(&["bob"], &[])), web],
            [grant(vec![], true)],
        );
        let summary: Vec<_> = records
            .iter()
            .map(|r| {
                (
                    r.project.as_str(),
                    r.eligible,
                    r.overlay.is_some(),
                    r.privilege_grant.is_some(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![("payments", true, true, true), ("web", false, true, false)]
        );
    }

    #[test]
    fn no_grant_admits_run_as_root_and_refuses_every_governed_grant() {
        let root = GrantRequest {
            run_as_root: true,
            ..Default::default()
        };
        let admitted =
            EnvironmentProjectPrivilegeGrant::admit_grants(None, &root, IMG, false).unwrap();
        assert!(admitted.run_as_root);

        let privileged = GrantRequest {
            privileged: true,
            ..Default::default()
        };
        let err = EnvironmentProjectPrivilegeGrant::admit_grants(None, &privileged, IMG, false)
            .unwrap_err();
        assert!(err[0].contains("not whitelisted"));
        assert!(!EnvironmentProjectPrivilegeGrant::permits_k8s_overlay(None));
    }

    #[test]
    fn a_grant_admits_exactly_its_whitelist() {
        let g = grant(
            vec![ImageGrant {
                image_digest: "sha256:aaaa".into(),
                privileged: true,
                capabilities: strings(&["NET_ADMIN"]),
            }],
            true,
        );
        let req = GrantRequest {
            privileged: true,
            add_capabilities: strings(&["NET_ADMIN"]),
            ..Default::default()
        };
        let admitted =
            EnvironmentProjectPrivilegeGrant::admit_grants(Some(&g), &req, IMG, false).unwrap();
        assert!(admitted.privileged);
        assert_eq!(admitted.add_capabilities, strings(&["NET_ADMIN"]));
        // Fork-PR lockout still wins over a grant.
        assert!(EnvironmentProjectPrivilegeGrant::admit_grants(Some(&g), &req, IMG, true).is_err());
        assert!(EnvironmentProjectPrivilegeGrant::permits_k8s_overlay(Some(
            &g
        )));
    }

    // --- migrate_legacy_environment ---------------------------------------

    #[test]
    fn migration_carries_rules_drops_concurrency_and_keys_the_overlay() {
        let mut p = legacy_rules();
        p.approvers = strings(&["alice"]);
        p.allowed_refs = strings(&["main"]);
        p.wait_timer = 90;
        p.concurrency = 1;
        p.require_reason = true;
        let (o, g) = migrate_legacy_environment("acme", "payments", &legacy(p));
        assert_eq!(
            o,
            EnvironmentProjectOverlay {
                org: "acme".into(),
                environment: "prod".into(),
                project: "payments".into(),
                rules: EnvironmentProtectionRules {
                    approvers: strings(&["alice"]),
                    // A bare `main` is carried verbatim: it never matched a
                    // symbolic ref before, and must not start to now.
                    allowed_refs: strings(&["main"]),
                    wait_timer_s: 90,
                    require_reason: true,
                    approval_gate: ApprovalGateRules::default(),
                },
            }
        );
        assert_eq!(g, None);
    }

    #[test]
    fn migration_makes_a_grant_only_when_there_is_a_privilege_to_carry() {
        let mut p = legacy_rules();
        p.permit_k8s_overlay = true;
        let (_, g) = migrate_legacy_environment("acme", "payments", &legacy(p.clone()));
        assert_eq!(g, Some(grant(Vec::new(), true)));

        p.permit_k8s_overlay = false;
        p.privileged_images = vec![ImageGrant {
            image_digest: "sha256:aaaa".into(),
            privileged: false,
            capabilities: strings(&["NET_ADMIN"]),
        }];
        let (_, g) = migrate_legacy_environment("acme", "payments", &legacy(p.clone()));
        assert_eq!(g, Some(grant(p.privileged_images.clone(), false)));
    }

    // --- properties --------------------------------------------------------

    /// Ref globs and symbolic refs from a small universe that collides often:
    /// exact, prefix-globbed, tag, PR, catch-all and a bare `main` (which
    /// matches no symbolic ref, a shape real legacy rows carry).
    const PATTERNS: &[&str] = &[
        "refs/heads/main",
        "refs/heads/*",
        "refs/heads/release/*",
        "refs/tags/*",
        "refs/tags/v*",
        "refs/pull/*",
        "*",
        "main",
    ];
    const REFS: &[&str] = &[
        "refs/heads/main",
        "refs/heads/dev",
        "refs/heads/release/1.2",
        "refs/tags/v1",
        "refs/tags/nightly",
        "refs/pull/7/head",
        "main",
    ];
    const PEOPLE: &[&str] = &["alice", "bob", "carol"];

    /// Every trigger kind, as the two facts admission reads from its event:
    /// whether it carries a symbolic ref (`Event::protection_ref`) and
    /// whether it is a human dispatch (the server's `manual`/`api` match).
    #[derive(Debug, Clone, Copy)]
    enum Trigger {
        Push,
        PullRequest,
        Tag,
        Release,
        Manual,
        Api,
        Comment,
        Cron,
        Upstream,
    }
    const TRIGGERS: &[Trigger] = &[
        Trigger::Push,
        Trigger::PullRequest,
        Trigger::Tag,
        Trigger::Release,
        Trigger::Manual,
        Trigger::Api,
        Trigger::Comment,
        Trigger::Cron,
        Trigger::Upstream,
    ];

    impl Trigger {
        fn protection_ref(self, git_ref: &str) -> Option<&str> {
            match self {
                Trigger::Comment | Trigger::Cron | Trigger::Upstream => None,
                _ => Some(git_ref),
            }
        }
        fn is_human_dispatch(self) -> bool {
            matches!(self, Trigger::Manual | Trigger::Api)
        }
    }

    /// Today's run-creation ref check, verbatim from the server's
    /// `admit_and_create_run`: the oracle the migration must reproduce.
    fn legacy_admits_protection_ref(p: &ProtectionRules, protection_ref: Option<&str>) -> bool {
        match protection_ref {
            Some(r) => p.ref_allowed(r),
            None => p.allowed_refs.is_empty(),
        }
    }

    /// Every subset of [`PEOPLE`], as an approval set.
    fn all_approval_sets() -> Vec<Vec<String>> {
        (0..1u32 << PEOPLE.len())
            .map(|mask| {
                PEOPLE
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, p)| p.to_string())
                    .collect()
            })
            .collect()
    }

    fn pick(universe: &'static [&'static str], max: usize) -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(prop::sample::select(universe), 0..=max)
            .prop_map(|xs| xs.into_iter().map(str::to_string).collect())
    }

    fn protection_rules() -> impl Strategy<Value = EnvironmentProtectionRules> {
        (pick(PEOPLE, 4), pick(PATTERNS, 3), 0u32..120, any::<bool>()).prop_map(
            |(approvers, allowed_refs, wait_timer_s, require_reason)| EnvironmentProtectionRules {
                approvers,
                allowed_refs,
                wait_timer_s,
                require_reason,
                approval_gate: ApprovalGateRules::default(),
            },
        )
    }

    fn project_overlay() -> impl Strategy<Value = EnvironmentProjectOverlay> {
        protection_rules().prop_map(overlay)
    }

    const DIGESTS: &[&str] = &["sha256:aaaa", "sha256:bbbb"];
    const IMAGES: &[&str] = &[
        "ghcr.io/acme/x@sha256:aaaa",
        "ghcr.io/acme/x@sha256:bbbb",
        "ghcr.io/acme/x:v1",
    ];
    const CAPS: &[&str] = &["NET_ADMIN", "SYS_ADMIN"];

    fn image_grants() -> impl Strategy<Value = Vec<ImageGrant>> {
        prop::collection::vec(
            (prop::sample::select(DIGESTS), any::<bool>(), pick(CAPS, 2)).prop_map(
                |(d, privileged, capabilities)| ImageGrant {
                    image_digest: d.to_string(),
                    privileged,
                    capabilities,
                },
            ),
            0..3,
        )
    }

    /// Legacy rules covering every field a stored row can hold, including
    /// the stored-but-unread concurrency and the old privilege fields.
    fn legacy_protection_rules() -> impl Strategy<Value = ProtectionRules> {
        (
            pick(PEOPLE, 4),
            0u32..120,
            pick(PATTERNS, 3),
            0u32..4,
            image_grants(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                |(approvers, wait_timer, allowed_refs, concurrency, images, k8s, reason)| {
                    ProtectionRules {
                        approvers,
                        wait_timer,
                        allowed_refs,
                        concurrency,
                        privileged_images: images,
                        permit_k8s_overlay: k8s,
                        require_reason: reason,
                        ..legacy_rules()
                    }
                },
            )
    }

    fn grant_request() -> impl Strategy<Value = GrantRequest> {
        (any::<bool>(), pick(CAPS, 2), any::<bool>()).prop_map(
            |(run_as_root, add_capabilities, privileged)| GrantRequest {
                run_as_root,
                add_capabilities,
                privileged,
            },
        )
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v.dedup();
        v
    }

    proptest! {
        /// ADR-0071 §3 **tighten-only**: whatever the baseline merged with an
        /// overlay admits, the baseline alone admits too — on every check
        /// admission makes — and every field is at least as strict.
        #[test]
        fn an_overlay_only_ever_tightens_the_baseline(
            baseline in protection_rules(),
            o in project_overlay(),
            approvals in pick(PEOPLE, 3),
        ) {
            let with = effective_protection(&baseline, Some(&o));
            let without = effective_protection(&baseline, None);

            for &r in REFS {
                prop_assert!(!with.ref_allowed(r) || without.ref_allowed(r), "{r}");
                prop_assert!(
                    !with.admits(r, &approvals).is_ok() || without.admits(r, &approvals).is_ok(),
                    "{r}"
                );
                // Exactly "matches both": the merge is the baseline's
                // decision AND the overlay's own.
                let overlay_alone = AllowedRefConstraint::from_layers([o.rules.allowed_refs.clone()]);
                prop_assert_eq!(
                    with.ref_allowed(r),
                    without.ref_allowed(r) && overlay_alone.allows(r),
                    "{}", r
                );
            }
            prop_assert!(
                !with.admits_protection_ref(None) || without.admits_protection_ref(None)
            );
            for human in [false, true] {
                for reason in [false, true] {
                    prop_assert!(
                        !with.admits_reason(human, reason).is_ok()
                            || without.admits_reason(human, reason).is_ok()
                    );
                }
            }

            for a in &baseline.approvers {
                prop_assert!(with.approvers.contains(a));
            }
            for a in &o.rules.approvers {
                prop_assert!(with.approvers.contains(a));
            }
            let mut seen = std::collections::BTreeSet::new();
            prop_assert!(with.approvers.iter().all(|a| seen.insert(a)), "duplicate approver");
            prop_assert!(with.wait_timer_s >= without.wait_timer_s);
            prop_assert!(!without.require_reason || with.require_reason);
        }

        /// ADR-0071 §10 **migration equivalence**: over the permissive
        /// baseline the migration creates, a Project's migrated overlay and
        /// grant decide every admission question exactly as its old
        /// `ProtectionRules` did — for every ref × approval set × trigger
        /// kind × reason, and every grant request.
        #[test]
        fn a_migrated_environment_decides_exactly_as_it_did_before(
            old in legacy_protection_rules(),
            grant_requests in prop::collection::vec(
                (grant_request(), prop::sample::select(IMAGES), any::<bool>()),
                1..6,
            ),
        ) {
            let (o, g) = migrate_legacy_environment("acme", "payments", &legacy(old.clone()));
            let e = effective_protection(&EnvironmentProtectionRules::default(), Some(&o));

            for &r in REFS {
                prop_assert_eq!(e.ref_allowed(r), old.ref_allowed(r), "{}", r);
                for approvals in all_approval_sets() {
                    let (now, before) = (e.admits(r, &approvals), old.admits(r, &approvals));
                    prop_assert_eq!(now.is_ok(), before.is_ok(), "{} {:?}", r, approvals);
                    // Same violations, modulo a legacy row that listed an
                    // approver twice (it reported the miss twice).
                    prop_assert_eq!(
                        sorted(now.err().unwrap_or_default()),
                        sorted(before.err().unwrap_or_default())
                    );
                }
                for &trigger in TRIGGERS {
                    let protection_ref = trigger.protection_ref(r);
                    prop_assert_eq!(
                        e.admits_protection_ref(protection_ref),
                        legacy_admits_protection_ref(&old, protection_ref),
                        "{:?} {}", trigger, r
                    );
                    for has_reason in [false, true] {
                        prop_assert_eq!(
                            e.admits_reason(trigger.is_human_dispatch(), has_reason),
                            old.admits_reason(trigger.is_human_dispatch(), has_reason),
                            "{:?} reason={}", trigger, has_reason
                        );
                    }
                }
            }

            for (req, image, locked_out) in &grant_requests {
                prop_assert_eq!(
                    EnvironmentProjectPrivilegeGrant::admit_grants(g.as_ref(), req, image, *locked_out),
                    old.admit_grants(req, image, *locked_out)
                );
            }
            prop_assert_eq!(
                EnvironmentProjectPrivilegeGrant::permits_k8s_overlay(g.as_ref()),
                old.permit_k8s_overlay
            );
            prop_assert_eq!(e.wait_timer_s, old.wait_timer);
        }

        /// The fail-open regression ADR-0071 §3 names: baseline
        /// `[refs/heads/main]` with overlay `[refs/tags/*]` admits **no**
        /// ref — not the "any ref" a list intersection (`[]`) would read as —
        /// and no event without a symbolic ref either, whatever else the two
        /// layers say.
        #[test]
        fn main_baseline_with_tags_overlay_admits_nothing(
            r in prop_oneof![prop::sample::select(REFS).prop_map(str::to_string), "\\PC*"],
            mut baseline in protection_rules(),
            mut o in project_overlay(),
            approvals in pick(PEOPLE, 3),
        ) {
            baseline.allowed_refs = vec!["refs/heads/main".into()];
            o.rules.allowed_refs = vec!["refs/tags/*".into()];
            let e = effective_protection(&baseline, Some(&o));
            prop_assert!(!e.ref_allowed(&r));
            prop_assert!(!e.unrestricted_refs());
            prop_assert!(!e.admits_protection_ref(None));
            prop_assert!(!e.admits_protection_ref(Some(&r)));
            prop_assert!(e.admits(&r, &approvals).is_err());
        }
    }

    /// cd43bf3: with `required_approvals = Some(n)`, n of the baseline's
    /// approvers must approve, and every overlay approver as well.
    #[test]
    fn n_of_the_baseline_approvers_and_every_overlay_approver() {
        let mut baseline = rules(&["amy", "bob", "cid"], &[]);
        baseline.approval_gate.required_approvals = Some(2);
        let p = effective_protection(&baseline, Some(&overlay(rules(&["olga"], &[]))));
        let approvals = |who: &[&str]| p.approval_violations(&strings(who));

        assert_eq!(
            approvals(&["amy", "bob"]),
            vec!["missing required approval from `olga`".to_string()],
            "two of the baseline without the overlay approver hold"
        );
        assert!(approvals(&["amy", "bob", "olga"]).is_empty());
        assert!(approvals(&["bob", "cid", "olga"]).is_empty());
        assert_eq!(
            approvals(&["amy", "olga"]),
            vec!["needs 2 approval(s) from `amy`, `bob`, `cid`; has 1".to_string()]
        );
        // The overlay approver does not count toward the baseline's two.
        assert!(!approvals(&["amy", "olga", "zed"]).is_empty());
    }
}
