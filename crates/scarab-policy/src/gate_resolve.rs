//! The `gate.resolve` PolicyCheckpoint's input (ADR-0075 §2): consulted when
//! someone approves a `manual` gate, before the approval is recorded.
//!
//! Only `deny` is evaluated here for now; `require_approval` and
//! `require_step_up` are refused when a Policy is saved. The Environment's
//! release rules (who may approve an Environment approval gate, how many
//! approvals, the wait timer) are a built-in rule at this checkpoint
//! ([`gate_release`](crate::environment_rules::gate_release)).

use crate::run_create::{GateKind, Org, Origin, Project, MAX_ORIGIN, MAX_STEPS};
use crate::schema::{Capped, Schema, Timestamp};
use crate::{policy_enum, policy_struct};

pub const CHECKPOINT: &str = "gate.resolve";
pub const SCHEMA_VERSION: u32 = 1;

/// The most approvals one wait of a gate lists.
pub const MAX_APPROVALS: usize = 64;

policy_enum! {
    /// What the actor asks of the gate. Only `release` is consulted today: a
    /// reject is never held (ADR-0075 §5).
    pub enum Verdict {
        Release => "release",
    }
}

policy_struct! {
    pub struct GateUnderResolve {
        pub id: String,
        pub kind: GateKind,
        pub origin: Capped<Origin, MAX_ORIGIN>,
        /// The gate is an Environment approval gate (ADR-0078 §1).
        pub environment_approval_gate: bool,
        /// The ids of the Steps that need it, directly.
        pub dependents: Capped<String, MAX_STEPS>,
    }
}

policy_struct! {
    pub struct Actor {
        /// The acting Principal.
        pub principal: String,
    }
}

policy_struct! {
    pub struct GateRun {
        pub id: String,
        /// The Run's symbolic ref, when it is Environment-governed.
        pub r#ref: Option<String>,
        /// The Run's Environment, when its Pipeline names one.
        pub environment: Option<String>,
    }
}

policy_struct! {
    /// The root: each field is a CEL variable.
    pub struct GateResolveInput {
        pub org: Org,
        pub project: Project,
        pub gate: GateUnderResolve,
        pub verdict: Verdict,
        pub actor: Actor,
        /// The Principals who have approved in the gate's current wait, this
        /// approval included.
        pub approvals: Capped<String, MAX_APPROVALS>,
        pub run: GateRun,
        pub time: Timestamp,
    }
}

pub fn schema() -> Schema {
    Schema::for_root::<GateResolveInput>(CHECKPOINT, SCHEMA_VERSION)
}
