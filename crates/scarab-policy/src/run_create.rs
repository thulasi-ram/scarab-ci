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
//!
//! **String caps.** Every string is at most
//! [`MAX_STRING_LEN`](crate::schema::MAX_STRING_LEN) bytes, the dispatch
//! reason (a [`FreeText`]) at most
//! [`MAX_FREE_TEXT_LEN`](crate::schema::MAX_FREE_TEXT_LEN); [`input_value`]
//! refuses a Run with a longer one, as it refuses one over a list cap.

use crate::schema::{check_string_lengths, Capped, FreeText, Schema, Timestamp};
use crate::{policy_enum, policy_struct};

pub const CHECKPOINT: &str = "run.create";
pub const SCHEMA_VERSION: u32 = 1;

/// The most Steps a Run may have, after invoke inlining and matrix expansion.
pub const MAX_STEPS: usize = 512;
/// The most gates a Run may have.
pub const MAX_GATES: usize = 32;
/// The longest invoke chain: `scarab-pipeline` rejects nesting deeper than
/// its `MAX_INVOKE_DEPTH`, so a chain has at most that many targets.
pub const MAX_ORIGIN: usize = scarab_pipeline::MAX_INVOKE_DEPTH;
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
        pub reason: Option<FreeText>,
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
    Capped::new(items).ok_or_else(|| cap_message(owner, len, noun, N))
}

fn cap_message(owner: &dyn Fn() -> String, len: usize, noun: &str, cap: usize) -> CapExceeded {
    CapExceeded(format!(
        "{} has {len} {noun}, more than the {cap} a run.create Policy can check, so the Run \
         is refused",
        owner()
    ))
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

/// A gate of a Run whose origin includes a plugin link: what the closed
/// plugin default ([`plugins_closed_default`](crate::evaluate::plugins_closed_default))
/// checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginGate {
    pub gate: String,
    pub plugin: String,
}

/// Each (gate, plugin) pair of `ir`, in Pipeline order, read from the
/// compiled Pipeline itself: the plugin default holds whether or not the
/// Run's input could be built.
pub fn plugin_gates(ir: &scarab_pipeline::PipelineIr) -> Vec<PluginGate> {
    ir.steps
        .iter()
        .filter(|s| s.is_gate())
        .flat_map(|s| {
            s.origin
                .iter()
                .filter(|o| o.kind == scarab_pipeline::OriginKind::Plugin)
                .map(|o| PluginGate {
                    gate: s.id.clone(),
                    plugin: o.name.clone(),
                })
        })
        .collect()
}

/// Whether a gate of `ir` comes from a plugin: such a Run is consulted at
/// `run.create` even when no Policy is stored, for the closed plugin
/// default ([`plugins_closed_default`](crate::evaluate::plugins_closed_default)).
pub fn has_plugin_gate(ir: &scarab_pipeline::PipelineIr) -> bool {
    ir.steps.iter().any(|s| {
        s.is_gate()
            && s.origin
                .iter()
                .any(|o| o.kind == scarab_pipeline::OriginKind::Plugin)
    })
}

/// Refuses a Run over [`MAX_STEPS`] Steps or [`MAX_GATES`] gates, in time
/// linear in its size. [`input_from_pipeline`] checks this first, before any
/// work that grows faster than the Run: a Pipeline that compiles to 100,000
/// Steps is refused here.
pub fn check_run_caps(ir: &scarab_pipeline::PipelineIr) -> Result<(), CapExceeded> {
    let run = || "the Run".to_string();
    let gate_count = ir.steps.iter().filter(|s| s.is_gate()).count();
    let step_count = ir.steps.len() - gate_count;
    if step_count > MAX_STEPS {
        return Err(cap_message(&run, step_count, "Steps", MAX_STEPS));
    }
    if gate_count > MAX_GATES {
        return Err(cap_message(&run, gate_count, "gates", MAX_GATES));
    }
    Ok(())
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
    use std::collections::HashMap;

    check_run_caps(ir)?;
    let run = || "the Run".to_string();

    // Each Step's transitive upstream gates, as a bit set over the gates
    // (at most MAX_GATES, so one u64), in O(Steps × needs). Steps are
    // visited in dependency order (Kahn's algorithm over `needs`); the DAG is
    // validated, and a Step left on a cycle all the same gets no gates.
    let index: HashMap<&str, usize> = ir
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let gate_bit: HashMap<usize, u64> = ir
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_gate())
        .enumerate()
        .map(|(bit, (i, _))| (i, 1u64 << bit))
        .collect();
    let needs: Vec<Vec<usize>> = ir
        .steps
        .iter()
        .map(|s| {
            s.needs
                .0
                .iter()
                .filter_map(|n| index.get(n.as_str()).copied())
                .collect()
        })
        .collect();
    let mut waiting: Vec<usize> = needs.iter().map(Vec::len).collect();
    let mut needed_by: Vec<Vec<usize>> = vec![Vec::new(); ir.steps.len()];
    for (i, ns) in needs.iter().enumerate() {
        for &n in ns {
            needed_by[n].push(i);
        }
    }
    let mut upstream_gates_of = vec![0u64; ir.steps.len()];
    let mut ready: Vec<usize> = (0..ir.steps.len()).filter(|&i| waiting[i] == 0).collect();
    while let Some(i) = ready.pop() {
        upstream_gates_of[i] = needs[i].iter().fold(0, |acc, &n| {
            acc | upstream_gates_of[n] | gate_bit.get(&n).copied().unwrap_or(0)
        });
        for &d in &needed_by[i] {
            waiting[d] -= 1;
            if waiting[d] == 0 {
                ready.push(d);
            }
        }
    }

    // A gate's dependents, in Pipeline order: every non-gate Step with it
    // upstream.
    let mut dependents_of: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, s) in ir.steps.iter().enumerate() {
        if s.is_gate() {
            continue;
        }
        for (&g, &bit) in &gate_bit {
            if upstream_gates_of[i] & bit != 0 {
                dependents_of.entry(g).or_default().push(s.id.clone());
            }
        }
    }
    // The gates in Pipeline order, for each Step's `upstream_gates`.
    let mut gate_order: Vec<(usize, u64)> = gate_bit.iter().map(|(&i, &b)| (i, b)).collect();
    gate_order.sort_unstable();

    let mut steps = Vec::new();
    let mut gates = Vec::new();
    for (i, s) in ir.steps.iter().enumerate() {
        let what = || format!("Step `{}`", s.id);
        let gate_what = || format!("gate `{}`", s.id);
        if s.is_gate() {
            gates.push(Gate {
                id: s.id.clone(),
                kind: gate_kind(s)?,
                origin: origin_of(&s.origin, &gate_what)?,
                dependents: capped(
                    dependents_of.remove(&i).unwrap_or_default(),
                    &gate_what,
                    "dependent Steps",
                )?,
            });
            continue;
        }
        let mut upstream_gates = Vec::new();
        for &(g, bit) in &gate_order {
            if upstream_gates_of[i] & bit != 0 {
                let g = &ir.steps[g];
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
        steps: capped(steps, &run, "Steps")?,
        gates: capped(gates, &run, "gates")?,
        principal: facts.principal,
        time: facts.time,
    })
}

/// The `run.create` input of `ir` as a PolicyDecision records it: built by
/// [`input_from_pipeline`], then checked against the string caps (see the
/// module documentation). An `Err` names the list or the string that does
/// not fit.
pub fn input_value(
    ir: &scarab_pipeline::PipelineIr,
    facts: RunCreateFacts,
) -> Result<serde_json::Value, CapExceeded> {
    let input = input_from_pipeline(ir, facts)?;
    let value = serde_json::to_value(input).map_err(|e| CapExceeded(e.to_string()))?;
    check_string_lengths(&schema(), &value).map_err(|e| {
        CapExceeded(format!(
            "`{}` is {} bytes, more than the {} a run.create Policy can check, so the Run \
             is refused",
            e.path, e.len, e.max
        ))
    })?;
    Ok(value)
}
