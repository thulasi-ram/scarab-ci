//! The `run.create` PolicyCheckpoint's input (ADR-0075 §2): consulted once per
//! Run, after compiling and trigger matching, before any Step row exists.
//!
//! Library and plugin rules are written here (ADR-0076 §1): every Step and
//! gate carries its `origin`, the invoke chain it was inlined from.
//!
//! **List caps.** Each list is a [`Capped`] list, and the checker's static
//! cost bound multiplies by the caps. They are chosen so ADR-0075 §4's
//! example rule, `steps.exists(s, s.origin.exists(o, …))`, fits under
//! [`MAX_COST`](crate::check::MAX_COST): its bound is
//! `1 + MAX_STEPS × (3 + 18 × MAX_ORIGIN)` = 75,265. ADR-0075 §5's rule, a
//! Step using the Environment's authority with no manual gate upstream, costs
//! 85,505 with [`MAX_UPSTREAM_GATES`] at 32; at 64 it would be 167,425. A Run
//! whose input does not fit the caps cannot be checked, so the engine must
//! refuse it.

use crate::schema::{Capped, Schema, Timestamp};
use crate::{policy_enum, policy_struct};

pub const CHECKPOINT: &str = "run.create";
pub const SCHEMA_VERSION: u32 = 1;

/// The most Steps a Run may have, after invoke inlining and matrix expansion.
pub const MAX_STEPS: usize = 512;
/// The most gates a Run may have.
pub const MAX_GATES: usize = 32;
/// The longest invoke chain: `scarab-pipeline` rejects nesting deeper than 8
/// (its `MAX_INVOKE_DEPTH`), so a chain has at most 8 targets.
pub const MAX_ORIGIN: usize = 8;
/// The most gates upstream of one Step, counted transitively.
pub const MAX_UPSTREAM_GATES: usize = MAX_GATES;
/// The most capability grants one Step may request.
pub const MAX_GRANTS: usize = 16;
/// The most secrets one Step may declare.
pub const MAX_SECRETS: usize = 32;
/// The most placement profiles one Step may name.
pub const MAX_PLACEMENT_PROFILES: usize = 8;

policy_enum! {
    /// What started the Run (`scarab_forge::TriggerKind`).
    pub enum TriggerKind {
        Push => "push",
        PullRequest => "pull_request",
        Tag => "tag",
        Release => "release",
        Comment => "comment",
        Cron => "cron",
        Manual => "manual",
        Api => "api",
        Upstream => "upstream",
    }
}

policy_enum! {
    /// The kind of one link in an invoke chain (ADR-0076).
    pub enum OriginKind {
        File => "file",
        Library => "library",
        Plugin => "plugin",
    }
}

policy_enum! {
    pub enum GateKind {
        Manual => "manual",
        Timer => "timer",
        External => "external",
        Plugin => "plugin",
    }
}

policy_enum! {
    /// The forge token a Step asks for (ADR-0076 §9).
    pub enum ForgeToken {
        Read => "read",
        Write => "write",
    }
}

policy_struct! {
    pub struct Org {
        pub name: String,
    }
}

policy_struct! {
    pub struct Project {
        pub name: String,
    }
}

policy_struct! {
    pub struct Trigger {
        pub kind: TriggerKind,
        /// The symbolic ref (`refs/heads/main`); a cron or API trigger may
        /// have none.
        pub r#ref: Option<String>,
        pub sha: Option<String>,
        /// A pull request from a fork.
        pub fork: bool,
        /// The forge account that caused the event.
        pub actor: Option<String>,
        /// The Principal linked to `actor` (ADR-0069), when there is one.
        pub principal: Option<String>,
        /// The reason given for a manual or API dispatch.
        pub reason: Option<String>,
    }
}

policy_struct! {
    pub struct Pipeline {
        pub name: String,
        /// Absent when the Pipeline names no Environment.
        pub environment: Option<String>,
        pub deploy: bool,
    }
}

policy_struct! {
    /// One link of an invoke chain, outermost first.
    pub struct Origin {
        pub kind: OriginKind,
        pub name: String,
        /// Where an installed library comes from; only `library` links have one.
        pub source: Option<String>,
        pub content_hash: String,
    }
}

policy_struct! {
    pub struct UpstreamGate {
        pub id: String,
        pub kind: GateKind,
    }
}

policy_struct! {
    /// A Step's static `step.admit` input, with what `run.create` adds.
    pub struct Step {
        pub id: String,
        pub image: String,
        pub digest: Option<String>,
        pub grants: Capped<String, MAX_GRANTS>,
        pub k8s_overlay: bool,
        pub secrets: Capped<String, MAX_SECRETS>,
        pub placement_profiles: Capped<String, MAX_PLACEMENT_PROFILES>,
        pub deploy: bool,
        pub forge_token: Option<ForgeToken>,
        /// The Step uses the Environment's authority (ADR-0075 §5).
        pub uses_environment_authority: bool,
        /// Empty for a Step written in the Pipeline file itself.
        pub origin: Capped<Origin, MAX_ORIGIN>,
        pub upstream_gates: Capped<UpstreamGate, MAX_UPSTREAM_GATES>,
    }
}

policy_struct! {
    pub struct Gate {
        pub id: String,
        pub kind: GateKind,
        pub origin: Capped<Origin, MAX_ORIGIN>,
        /// The ids of the Steps that depend on this gate.
        pub dependents: Capped<String, MAX_STEPS>,
    }
}

policy_struct! {
    /// The root: each field is a CEL variable.
    pub struct RunCreateInput {
        pub org: Org,
        pub project: Project,
        pub trigger: Trigger,
        pub pipeline: Pipeline,
        pub steps: Capped<Step, MAX_STEPS>,
        pub gates: Capped<Gate, MAX_GATES>,
        /// The human or token creating the Run; absent for a webhook.
        pub principal: Option<String>,
        pub time: Timestamp,
    }
}

pub fn schema() -> Schema {
    Schema::for_root::<RunCreateInput>(CHECKPOINT, SCHEMA_VERSION)
}
