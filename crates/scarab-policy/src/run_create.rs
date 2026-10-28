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

impl Timestamp {
    /// `ms` unix milliseconds; `None` outside chrono's range.
    pub fn from_unix_millis(ms: i64) -> Option<Timestamp> {
        chrono::DateTime::from_timestamp_millis(ms).map(Timestamp)
    }
}

/// What a `run.create` consultation knows besides the compiled Pipeline.
#[derive(Clone, Debug)]
pub struct RunCreateFacts {
    pub org: String,
    pub project: String,
    pub trigger: Trigger,
    /// The Pipeline file, e.g. `.scarab/deploy.yaml`: the Pipeline's name
    /// when it declares none.
    pub pipeline_path: String,
    /// The Environment the Run is governed by, when its Pipeline names one.
    pub environment: Option<String>,
    /// The human or token creating the Run; `None` for a webhook.
    pub principal: Option<String>,
    pub time: Timestamp,
}

/// A Run whose Steps or gates do not fit a list cap of the input schema. Its
/// input cannot be checked, so the engine refuses the Run when a Policy
/// applies (ADR-0075 §2); the message names the cap.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CapExceeded(pub String);

/// `items` as a capped list, or the refusal naming `owner`, the list's `noun`
/// and the cap.
fn capped<T, const N: usize>(
    items: Vec<T>,
    owner: &dyn Fn() -> String,
    noun: &str,
) -> Result<Capped<T, N>, CapExceeded> {
    let len = items.len();
    Capped::new(items).ok_or_else(|| {
        CapExceeded(format!(
            "{} has {len} {noun}, more than the {N} a run.create Policy can check, so the Run \
             is refused",
            owner()
        ))
    })
}

fn origin_of(
    chain: &[scarab_pipeline::OriginEntry],
    owner: &dyn Fn() -> String,
) -> Result<Capped<Origin, MAX_ORIGIN>, CapExceeded> {
    let links = chain
        .iter()
        .map(|o| Origin {
            kind: match o.kind {
                scarab_pipeline::OriginKind::File => OriginKind::File,
                scarab_pipeline::OriginKind::Library => OriginKind::Library,
                scarab_pipeline::OriginKind::Plugin => OriginKind::Plugin,
            },
            name: o.name.clone(),
            source: o.source.clone(),
            content_hash: o.content_hash.clone(),
        })
        .collect();
    capped(links, owner, "links in its origin chain")
}

fn gate_kind(step: &scarab_pipeline::StepSpec) -> Result<GateKind, CapExceeded> {
    match step.gate.as_deref() {
        Some("manual") => Ok(GateKind::Manual),
        Some("timer") => Ok(GateKind::Timer),
        Some("external") => Ok(GateKind::External),
        Some("plugin") => Ok(GateKind::Plugin),
        other => Err(CapExceeded(format!(
            "gate `{}` has kind {other:?}, which a run.create Policy cannot check, so the Run is refused",
            step.id
        ))),
    }
}

/// Whether `step` uses the Environment's authority (ADR-0075 §5): it
/// declares secrets, requests a governed grant or a raw overlay, or is a
/// deploy Step (itself, or every non-gate Step of a `deploy: true`
/// Pipeline). The Pipeline IR has no `forge_token` yet, so that item of the
/// list cannot apply.
pub fn uses_environment_authority(
    ir: &scarab_pipeline::PipelineIr,
    step: &scarab_pipeline::StepSpec,
) -> bool {
    if step.is_gate() {
        return false;
    }
    let governed_grant = step
        .security
        .as_ref()
        .is_some_and(|s| s.privileged || !s.add_capabilities.is_empty());
    !step.secrets.is_empty()
        || governed_grant
        || step.k8s_overlay.is_some()
        || step.deploy
        || ir.deploy
}

/// Builds the `run.create` input of a compiled Pipeline (ADR-0075 §2). Every
/// Step is listed, including ones a `when:` guard will skip: they are part
/// of the Run. Gates are listed under `gates`, every other Step under
/// `steps`. Upstream gates and a gate's dependents are transitive over
/// `needs`.
pub fn input_from_pipeline(
    ir: &scarab_pipeline::PipelineIr,
    facts: RunCreateFacts,
) -> Result<RunCreateInput, CapExceeded> {
    use std::collections::{BTreeMap, BTreeSet};

    let by_id: BTreeMap<&str, &scarab_pipeline::StepSpec> =
        ir.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    // Every Step's transitive upstream, memoised. The DAG is validated, so
    // the walk terminates; `visiting` guards it all the same.
    fn upstream<'a>(
        id: &'a str,
        by_id: &BTreeMap<&'a str, &'a scarab_pipeline::StepSpec>,
        memo: &mut BTreeMap<&'a str, BTreeSet<&'a str>>,
        visiting: &mut BTreeSet<&'a str>,
    ) -> BTreeSet<&'a str> {
        if let Some(done) = memo.get(id) {
            return done.clone();
        }
        let mut out = BTreeSet::new();
        if visiting.insert(id) {
            if let Some(step) = by_id.get(id) {
                for need in &step.needs.0 {
                    if let Some((key, _)) = by_id.get_key_value(need.as_str()) {
                        out.insert(*key);
                        out.extend(upstream(key, by_id, memo, visiting));
                    }
                }
            }
            visiting.remove(id);
        }
        memo.insert(id, out.clone());
        out
    }
    let mut memo = BTreeMap::new();
    let mut ups: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for s in &ir.steps {
        let u = upstream(s.id.as_str(), &by_id, &mut memo, &mut BTreeSet::new());
        ups.insert(s.id.as_str(), u);
    }

    let mut steps = Vec::new();
    let mut gates = Vec::new();
    for s in &ir.steps {
        let what = || format!("Step `{}`", s.id);
        let gate_what = || format!("gate `{}`", s.id);
        if s.is_gate() {
            let dependents: Vec<String> = ir
                .steps
                .iter()
                .filter(|d| !d.is_gate() && ups[d.id.as_str()].contains(s.id.as_str()))
                .map(|d| d.id.clone())
                .collect();
            gates.push(Gate {
                id: s.id.clone(),
                kind: gate_kind(s)?,
                origin: origin_of(&s.origin, &gate_what)?,
                dependents: capped(dependents, &gate_what, "dependent Steps")?,
            });
            continue;
        }
        let mut upstream_gates = Vec::new();
        for g in &ir.steps {
            if g.is_gate() && ups[s.id.as_str()].contains(g.id.as_str()) {
                upstream_gates.push(UpstreamGate {
                    id: g.id.clone(),
                    kind: gate_kind(g)?,
                });
            }
        }
        let mut grants = Vec::new();
        if let Some(sec) = &s.security {
            if sec.run_as_root {
                grants.push("run-as-root".to_string());
            }
            if sec.privileged {
                grants.push("privileged".to_string());
            }
            grants.extend(
                sec.add_capabilities
                    .iter()
                    .map(|c| format!("add-capabilities:{c}")),
            );
        }
        steps.push(Step {
            id: s.id.clone(),
            image: s.image.clone(),
            digest: s
                .image
                .split_once('@')
                .map(|(_, d)| d.to_string())
                .filter(|d| !d.is_empty()),
            grants: capped(grants, &what, "grants")?,
            k8s_overlay: s.k8s_overlay.is_some(),
            secrets: capped(s.secrets.clone(), &what, "secrets")?,
            placement_profiles: capped(s.placement_profiles.clone(), &what, "placement profiles")?,
            deploy: s.deploy || ir.deploy,
            forge_token: None,
            uses_environment_authority: uses_environment_authority(ir, s),
            origin: origin_of(&s.origin, &what)?,
            upstream_gates: capped(upstream_gates, &what, "gates upstream of it")?,
        });
    }
    Ok(RunCreateInput {
        org: Org { name: facts.org },
        project: Project {
            name: facts.project,
        },
        trigger: facts.trigger,
        pipeline: Pipeline {
            name: ir.name.clone().unwrap_or(facts.pipeline_path),
            environment: facts.environment,
            deploy: ir.deploy,
        },
        steps: capped(steps, &|| "the Run".to_string(), "Steps")?,
        gates: capped(gates, &|| "the Run".to_string(), "gates")?,
        principal: facts.principal,
        time: facts.time,
    })
}
