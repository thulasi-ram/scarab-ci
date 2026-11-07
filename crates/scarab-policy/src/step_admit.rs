//! The `step.admit` PolicyCheckpoint's input (ADR-0075 §2, §3): consulted at
//! every launch of a Step of an Environment-governed Run, before any
//! credential is used or the Pod is created. Only `deny` is evaluated.
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
        pub forge_token: Option<ForgeToken>,
        /// The Step uses the Environment's authority (ADR-0075 §5).
        pub uses_environment_authority: bool,
        pub origin: Capped<Origin, MAX_ORIGIN>,
    }
}

policy_struct! {
    pub struct AdmittedRun {
        pub id: String,
        /// The Run's symbolic ref.
        pub r#ref: String,
        pub environment: String,
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
        /// The approver set the gate's approvals met, when recorded.
        pub approvers: Capped<String, MAX_APPROVERS>,
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
        pub environment: EnvironmentNow,
        /// The Environment approval gates upstream of the Step, in Pipeline
        /// order.
        pub approvals: Capped<UpstreamApproval, MAX_GATES>,
        pub time: Timestamp,
    }
}

pub fn schema() -> Schema {
    Schema::for_root::<StepAdmitInput>(CHECKPOINT, SCHEMA_VERSION)
}
