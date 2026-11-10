//! The `step.admit` PolicyCheckpoint's input (ADR-0075 §2, §3): consulted at
//! every launch of a Step, before any credential is used or the Pod is
//! created. Only `deny` is evaluated. In a Run no Environment governs, the
//! Environment's fields are absent and no built-in rule applies.
//!
//! The Environment's launch-time re-check (allowed refs, the approver set
//! recorded at release, the maximum approval age) and the `env=none` OIDC
//! subject before the gate are a built-in rule at this checkpoint
//! ([`launch_recheck`](crate::environment_rules::launch_recheck)).

use crate::policy_struct;
use crate::run_create::{ForgeToken, Org, Origin, Project, MAX_GATES, MAX_ORIGIN, MAX_SECRETS};
use crate::schema::{Capped, Schema, Timestamp};

pub const CHECKPOINT: &str = "step.admit";
pub const SCHEMA_VERSION: u32 = 1;

/// The most approvers one release lists.
pub const MAX_APPROVERS: usize = 64;

policy_struct! {
    pub struct AdmittedStep {
        pub id: String,
        pub image: String,
        pub deploy: bool,
        pub secrets: Capped<String, MAX_SECRETS>,
        /// The names in `secrets` whose entry has `env: true`: each is an
        /// environment variable of the Step as well as a file. Absent from
        /// inputs recorded before it existed, so a rule reads it as
        /// `has(step.env_secrets) && 'NAME' in step.env_secrets`.
        pub env_secrets: Option<Capped<String, MAX_SECRETS>>,
        pub forge_token: Option<ForgeToken>,
        /// The Step uses the Environment's authority (ADR-0075 §5).
        pub uses_environment_authority: bool,
        pub origin: Capped<Origin, MAX_ORIGIN>,
    }
}

policy_struct! {
    pub struct AdmittedRun {
        pub id: String,
        /// The Run's symbolic ref, when it is Environment-governed.
        pub r#ref: Option<String>,
        /// The Run's Environment, when one governs it.
        pub environment: Option<String>,
    }
}

policy_struct! {
    /// The Run's Environment as it is now.
    pub struct EnvironmentNow {
        pub has_approvers: bool,
        /// The Run's ref is still allowed.
        pub ref_allowed: bool,
        pub max_approval_age_s: u64,
    }
}

policy_struct! {
    /// One Environment approval gate upstream of the Step.
    pub struct UpstreamApproval {
        pub gate: String,
        /// Absent while the gate has not released.
        pub released_at: Option<Timestamp>,
        /// The handles of the approver set the gate's approvals met, when
        /// recorded.
        pub approvers: Capped<String, MAX_APPROVERS>,
        /// Their Principal ids, in the same order (ADR-0080). Absent from
        /// inputs recorded before it existed.
        pub approver_ids: Option<Capped<String, MAX_APPROVERS>>,
        pub required: Option<u64>,
    }
}

policy_struct! {
    /// The root: each field is a CEL variable.
    pub struct StepAdmitInput {
        pub org: Org,
        pub project: Project,
        pub step: AdmittedStep,
        /// The Attempt being launched, from 1.
        pub attempt: u64,
        pub run: AdmittedRun,
        /// The Run's Environment as it is now; absent when none governs it.
        pub environment: Option<EnvironmentNow>,
        /// The Environment approval gates upstream of the Step, in Pipeline
        /// order.
        pub approvals: Capped<UpstreamApproval, MAX_GATES>,
        pub time: Timestamp,
    }
}

pub fn schema() -> Schema {
    Schema::for_root::<StepAdmitInput>(CHECKPOINT, SCHEMA_VERSION)
}
