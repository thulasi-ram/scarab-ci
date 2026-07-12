//! # scarab-pipeline — pipeline authoring & compilation
//!
//! Pure domain crate (serde / serde_json / serde_yaml / cel-interpreter /
//! thiserror only — all pure-computation deps per ADR-0031; no I/O, no clock,
//! no RNG, no infra). Turns authored YAML
//! into a validated, versioned [`PipelineIr`] — the *real* DSL (ADR-0009); YAML
//! is merely one frontend.
//!
//! ## Submit-time, not run-time
//!
//! Compilation performs **static** matrix expansion (ADR-0023): the cartesian
//! product of a step's matrix is materialised into concrete steps at
//! [`compile_yaml`] time, so the run's DAG is fully known before it starts —
//! bounded state for the engine, a fully drawable graph for the UI, and complete
//! validation up front. Anything the author gets wrong (a cycle, a dangling
//! `needs`, an empty matrix dimension) is rejected here with diagnostics, never
//! discovered mid-run.
//!
//! `when:` guards are kept as raw CEL strings; binding/evaluation of CEL lives in
//! the [`cel`] submodule and is wired up by a later slice.

pub mod cel;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Current schema version emitted by the compiler. Runs are self-describing
/// (ADR-0022): the IR carries this so an engine can reason about older Runs.
pub const IR_VERSION: u32 = 1;

fn default_ir_version() -> u32 {
    IR_VERSION
}

/// The compiled, versioned intermediate representation of a pipeline.
///
/// Post-compile invariant: every [`StepSpec::matrix`] is `None` (all matrices
/// have been expanded) and step ids are unique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineIr {
    /// Schema version of the IR, for forward/backward compatibility.
    #[serde(default = "default_ir_version")]
    pub ir_version: u32,
    /// The events that start this pipeline (`on:`), keyed by trigger kind. Empty
    /// means "no automatic triggers" (API/manual only). Matched against a
    /// normalized forge event via [`matches_trigger`] (ADR-0009, 0010).
    #[serde(default, rename = "on", skip_serializing_if = "Triggers::is_empty")]
    pub triggers: Triggers,
    /// Optional concurrency group: serializes this run against others in the same
    /// group under a [`Concurrency::policy`] (ADR-0011, 0032). Absent means the
    /// run is unconstrained. The engine wiring is `Db::set_run_concurrency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<Concurrency>,
    /// The deployment environment this pipeline targets, if any (ADR-0024, 0032).
    /// A pipeline with an `environment:` is a **deploy**: its runs are enforced
    /// against the environment's protection rules at admission, and they opt out
    /// of newest-wins auto-cancel (a superseded deploy must not be silently
    /// cancelled). Absent for ordinary CI pipelines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    pub steps: Vec<StepSpec>,
}

/// A pipeline's `concurrency:` block (ADR-0011, 0032). The `group` is a (possibly
/// `${{ … }}`-interpolated) key; runs sharing a resolved group contend for its
/// single slot, admitted per `policy`. Kept as strings so this pure crate stays
/// independent of the engine's `ConcurrencyPolicy` — the server maps `policy` via
/// `ConcurrencyPolicy::from_wire` at the wiring boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Concurrency {
    pub group: String,
    /// `queue` (wait for the slot, the safe default) or `cancel-in-progress`
    /// (cancel the current holder). Validated at submit time.
    #[serde(default = "default_policy")]
    pub policy: String,
}

fn default_policy() -> String {
    "queue".to_string()
}

/// The concurrency policies the engine understands (mirrors the wire tokens of
/// `scarab_engine::ConcurrencyPolicy`; duplicated here to keep the crate pure).
const CONCURRENCY_POLICIES: [&str; 2] = ["queue", "cancel-in-progress"];

/// The gate kinds a `gate:` step may declare (ADR-0008, 0032): `manual`
/// (approval), `timer` (wait a duration), `external` (release via API/webhook).
const GATE_KINDS: [&str; 3] = ["manual", "timer", "external"];

/// A pipeline's `on:` block: trigger kind (e.g. `push`, `pull_request`) → filter.
/// Kinds are the canonical vocabulary of `scarab_forge::TriggerKind`, kept as
/// strings so this pure crate need not depend on the forge domain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triggers(pub std::collections::BTreeMap<String, TriggerFilter>);

impl Triggers {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A filter narrowing when a trigger kind fires: an optional CEL predicate over
/// the event context (`when:`). Absent `when` means the kind always matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// One step. The step contract (ADR-0008) is an OCI `image` + a `command`; the
/// rest are DAG/placement modifiers. In authored YAML a step may carry a
/// [`matrix`](StepSpec::matrix); after [`compile_yaml`] that field is always
/// `None` and each concrete instance instead carries its
/// [`matrix_values`](StepSpec::matrix_values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    pub id: String,
    /// OCI image the step runs in. Empty (and defaulted) for a [`gate`](StepSpec::gate)
    /// step, which launches nothing; required for every executed step (enforced
    /// by [`validate`]).
    #[serde(default)]
    pub image: String,
    /// If set, this is a **gate** step (ADR-0008's `kind: gate`): an image-less
    /// durable suspend point of this kind (`manual`/`timer`/`external`). It
    /// launches no unit; when its `needs` are satisfied the run suspends until
    /// released. The engine wiring is `Db::set_step_gate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// For a `timer` gate, how long (seconds) to wait before the run
    /// auto-releases and resumes (ADR-0008). Required for `timer`; forbidden on
    /// any other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_after: Option<u32>,
    /// Entrypoint/command (empty = the image default).
    #[serde(default)]
    pub command: Vec<String>,
    /// Environment overrides for the step.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub needs: Needs,
    /// Authoring-only fan-out modifier; `None` on every compiled step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<Matrix>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    #[serde(default)]
    pub runs_on: RunsOn,
    #[serde(default)]
    pub resources: Resources,
    /// The concrete matrix coordinate this instance was expanded from (empty for
    /// steps authored without a matrix). Consumed later by CEL interpolation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub matrix_values: BTreeMap<String, String>,
}

impl StepSpec {
    /// Is this an image-less gate step (a durable suspend point)?
    pub fn is_gate(&self) -> bool {
        self.gate.is_some()
    }
}

/// The upstream steps this step depends on (its DAG edges).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Needs(pub Vec<String>);

/// A build matrix that fans a single spec into many concrete steps. Expanded to
/// the cartesian product of its `dimensions` at submit time (ADR-0023), minus
/// any combination for which an `exclude` CEL predicate holds. Predicates are
/// evaluated against the combination's own coordinate (each dimension bound as a
/// variable), so expansion stays fully static.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matrix {
    pub dimensions: BTreeMap<String, Vec<String>>,
    /// CEL predicates over the dimension variables; a combination is dropped if
    /// any predicate is true (e.g. `os == 'windows' && arch == 'arm64'`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// A conditional guard, expressed as a CEL expression (kept as a raw string
/// here; evaluated by the [`cel`] submodule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct When(pub String);

/// Runner selector (labels / class the step must be scheduled on).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunsOn {
    pub labels: Vec<String>,
}

/// Requested compute resources for a step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub cpu_millis: Option<u32>,
    pub memory_mib: Option<u32>,
}

/// Errors from compilation / validation.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("yaml parse error: {0}")]
    Parse(String),
    #[error("pipeline validation failed:\n  - {}", .0.join("\n  - "))]
    Validation(Vec<String>),
    #[error("cel error: {0}")]
    Cel(String),
}

/// Compile authored YAML into a validated [`PipelineIr`].
///
/// Parses the YAML frontend, statically expands every matrix into concrete
/// steps (ADR-0023), fans each `needs` edge onto every expanded instance of its
/// target, then runs full [`validate`] over the result. All problems are
/// reported together via [`PipelineError::Validation`].
pub fn compile_yaml(yaml: &str) -> Result<PipelineIr, PipelineError> {
    let authored: PipelineIr =
        serde_yaml::from_str(yaml).map_err(|e| PipelineError::Parse(e.to_string()))?;

    let mut diagnostics = Vec::new();
    let mut expanded: Vec<StepSpec> = Vec::new();
    // authored step id -> the concrete instance ids it expanded into.
    let mut instances: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for step in &authored.steps {
        match expand_step(step) {
            Ok(concrete) => {
                let ids = concrete.iter().map(|s| s.id.clone()).collect();
                // A duplicate authored id shadows an earlier expansion; the
                // resulting duplicate concrete ids are caught by validate().
                instances.insert(step.id.clone(), ids);
                expanded.extend(concrete);
            }
            Err(msg) => diagnostics.push(msg),
        }
    }

    if !diagnostics.is_empty() {
        return Err(PipelineError::Validation(diagnostics));
    }

    // Rewrite needs: a dependency on an authored step becomes a dependency on
    // every concrete instance of it (fan-in over a matrix). Unknown targets are
    // left untouched so validate() flags them as dangling.
    for step in &mut expanded {
        let mut resolved: Vec<String> = Vec::new();
        for need in &step.needs.0 {
            match instances.get(need) {
                Some(ids) => {
                    for id in ids {
                        if !resolved.contains(id) {
                            resolved.push(id.clone());
                        }
                    }
                }
                None => {
                    if !resolved.contains(need) {
                        resolved.push(need.clone());
                    }
                }
            }
        }
        step.needs.0 = resolved;
    }

    let ir = PipelineIr {
        ir_version: authored.ir_version,
        triggers: authored.triggers,
        concurrency: authored.concurrency,
        environment: authored.environment,
        steps: expanded,
    };

    validate(&ir).map_err(PipelineError::Validation)?;
    Ok(ir)
}

/// Expand one authored step into its concrete instances.
///
/// A step without a matrix yields itself (with an explicitly cleared `matrix`).
/// A step with a matrix yields the cartesian product of its dimensions; each
/// instance gets a deterministic id `id[k1=v1,k2=v2]` (keys sorted) and its
/// coordinate recorded in [`StepSpec::matrix_values`].
fn expand_step(step: &StepSpec) -> Result<Vec<StepSpec>, String> {
    let Some(matrix) = &step.matrix else {
        let mut base = step.clone();
        base.matrix = None;
        return Ok(vec![base]);
    };

    if matrix.dimensions.is_empty() {
        return Err(format!("step `{}`: matrix has no dimensions", step.id));
    }
    for (dim, values) in &matrix.dimensions {
        if values.is_empty() {
            return Err(format!(
                "step `{}`: matrix dimension `{dim}` has no values",
                step.id
            ));
        }
    }

    // Cartesian product over the (sorted) dimensions.
    let mut combos: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for (dim, values) in &matrix.dimensions {
        let mut next = Vec::with_capacity(combos.len() * values.len());
        for combo in &combos {
            for value in values {
                let mut c = combo.clone();
                c.insert(dim.clone(), value.clone());
                next.push(c);
            }
        }
        combos = next;
    }

    // Drop combinations excluded by a CEL predicate (evaluated against the
    // combination's own coordinate — each dimension bound as a variable).
    let mut kept = Vec::with_capacity(combos.len());
    for coord in combos {
        let ctx = serde_json::Value::Object(
            coord
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        );
        let mut excluded = false;
        for pred in &matrix.exclude {
            if cel::eval_bool(pred, &ctx).map_err(|e| format!("step `{}`: {e}", step.id))? {
                excluded = true;
                break;
            }
        }
        if !excluded {
            kept.push(coord);
        }
    }

    Ok(kept
        .into_iter()
        .map(|coord| {
            let suffix = coord
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut instance = step.clone();
            instance.id = format!("{}[{suffix}]", step.id);
            instance.matrix = None;
            instance.matrix_values = coord;
            instance
        })
        .collect())
}

/// Validate a compiled [`PipelineIr`], returning **all** discovered problems at
/// once. Checks: unique step ids, no unexpanded matrix (a compile-invariant),
/// `needs` resolve to real steps, and the `needs` graph is acyclic.
pub fn validate(ir: &PipelineIr) -> Result<(), Vec<String>> {
    let mut diagnostics = Vec::new();

    // Unique ids.
    let mut seen = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for step in &ir.steps {
        if !seen.insert(step.id.as_str()) {
            diagnostics.push(format!("duplicate step id `{}`", step.id));
        }
        ids.insert(step.id.as_str());
    }

    for step in &ir.steps {
        // Compile invariant: matrices are gone by now.
        if step.matrix.is_some() {
            diagnostics.push(format!("step `{}`: matrix was not expanded", step.id));
        }
        // Step kind: a gate launches nothing (image-less); every other step must
        // name an image. A gate with an image/command is a contradiction.
        match &step.gate {
            Some(kind) => {
                if !GATE_KINDS.contains(&kind.as_str()) {
                    diagnostics.push(format!(
                        "step `{}`: unknown gate kind `{kind}` (expected one of {})",
                        step.id,
                        GATE_KINDS.join(", ")
                    ));
                }
                if !step.image.is_empty() || !step.command.is_empty() {
                    diagnostics.push(format!(
                        "step `{}`: a gate step launches nothing — it must not set an image or command",
                        step.id
                    ));
                }
                // A `timer` gate needs a positive wait; other kinds must not set one.
                match (kind.as_str(), step.gate_after) {
                    ("timer", None) => diagnostics.push(format!(
                        "step `{}`: a timer gate must set `gate_after` (seconds)",
                        step.id
                    )),
                    ("timer", Some(0)) => diagnostics.push(format!(
                        "step `{}`: `gate_after` must be greater than zero",
                        step.id
                    )),
                    (other, Some(_)) if other != "timer" => diagnostics.push(format!(
                        "step `{}`: `gate_after` is only valid on a timer gate",
                        step.id
                    )),
                    _ => {}
                }
            }
            None => {
                if step.image.is_empty() {
                    diagnostics.push(format!("step `{}`: missing image", step.id));
                }
                if step.gate_after.is_some() {
                    diagnostics.push(format!(
                        "step `{}`: `gate_after` is only valid on a timer gate",
                        step.id
                    ));
                }
            }
        }
        // Dangling needs.
        for need in &step.needs.0 {
            if !ids.contains(need.as_str()) {
                diagnostics.push(format!(
                    "step `{}`: needs unknown step `{need}`",
                    step.id
                ));
            }
        }
        // Submit-time CEL: `when:` guards and every `${{ … }}` interpolation
        // must parse now, not fail mid-run (ADR-0009).
        if let Some(When(expr)) = &step.when {
            if let Err(e) = cel::check(expr) {
                diagnostics.push(format!("step `{}`: when {e}", step.id));
            }
        }
        let mut templates = vec![&step.image];
        templates.extend(&step.command);
        templates.extend(step.env.iter().map(|(_, v)| v));
        for t in templates {
            if let Err(e) = cel::check_interpolation(t) {
                diagnostics.push(format!("step `{}`: {e}", step.id));
            }
        }
    }

    // Submit-time CEL: each trigger's `when:` predicate must parse now.
    for (kind, filter) in &ir.triggers.0 {
        if let Some(expr) = &filter.when {
            if let Err(e) = cel::check(expr) {
                diagnostics.push(format!("trigger `{kind}`: when {e}"));
            }
        }
    }

    // Concurrency: a non-empty group, a known policy, and a group whose `${{ … }}`
    // interpolation parses now (it is resolved per-run at admission, ADR-0032).
    if let Some(c) = &ir.concurrency {
        if c.group.is_empty() {
            diagnostics.push("concurrency: group must not be empty".to_string());
        } else if let Err(e) = cel::check_interpolation(&c.group) {
            diagnostics.push(format!("concurrency: group {e}"));
        }
        if !CONCURRENCY_POLICIES.contains(&c.policy.as_str()) {
            diagnostics.push(format!(
                "concurrency: unknown policy `{}` (expected one of {})",
                c.policy,
                CONCURRENCY_POLICIES.join(", ")
            ));
        }
    }

    // A declared environment target must name a non-empty environment.
    if let Some(env) = &ir.environment {
        if env.is_empty() {
            diagnostics.push("environment: target must not be empty".to_string());
        }
    }

    // Cycle detection over needs edges (Kahn's algorithm). Only run when the
    // graph is well-formed enough to be meaningful (no dangling edges).
    if diagnostics.is_empty() {
        if let Some(cycle) = find_cycle(&ir.steps) {
            diagnostics.push(format!("dependency cycle among steps: {}", cycle.join(" -> ")));
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
}

/// Does this pipeline fire for an event of `kind` in the given `ctx`?
///
/// The pipeline must declare the trigger kind in its `on:`; if that trigger has
/// a `when:` predicate, it must evaluate true against `ctx` (the event context,
/// e.g. `{ "event": { "branch": "main", … } }`). `kind` is the canonical trigger
/// token (`scarab_forge::TriggerKind::as_str`), passed as a string so this pure
/// crate stays independent of the forge domain (ADR-0009, 0010). A bad predicate
/// is a submit-time error (already rejected by [`validate`]).
pub fn matches_trigger(
    ir: &PipelineIr,
    kind: &str,
    ctx: &serde_json::Value,
) -> Result<bool, PipelineError> {
    match ir.triggers.0.get(kind) {
        None => Ok(false),
        Some(filter) => match &filter.when {
            None => Ok(true),
            Some(expr) => cel::eval_bool(expr, ctx),
        },
    }
}

/// Return the members of a dependency cycle if one exists (edge: step -> each of
/// its `needs`). Uses Kahn's algorithm; the leftover nodes after repeatedly
/// removing zero-indegree nodes are exactly those on or feeding a cycle.
fn find_cycle(steps: &[StepSpec]) -> Option<Vec<String>> {
    let mut indegree: BTreeMap<&str, usize> = steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    // dependents[x] = steps that need x (edges to relax when x is removed).
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for step in steps {
        for need in &step.needs.0 {
            *indegree.get_mut(step.id.as_str()).unwrap() += 1;
            dependents.entry(need.as_str()).or_default().push(step.id.as_str());
        }
    }

    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut removed = 0usize;
    while let Some(node) = queue.pop() {
        removed += 1;
        for &dep in dependents.get(node).map(|v| v.as_slice()).unwrap_or(&[]) {
            let d = indegree.get_mut(dep).unwrap();
            *d -= 1;
            if *d == 0 {
                queue.push(dep);
            }
        }
    }

    if removed == steps.len() {
        return None;
    }
    // Remaining nodes (indegree > 0) are on or feed the cycle; report them sorted
    // for a deterministic diagnostic.
    let mut in_cycle: Vec<String> = indegree
        .iter()
        .filter(|(_, &d)| d > 0)
        .map(|(&id, _)| id.to_string())
        .collect();
    in_cycle.sort();
    Some(in_cycle)
}

/// Return a copy of `ir` keeping only steps whose `when:` guard is true (or
/// absent) under `ctx` — the include/exclude primitive (ADR-0009). `needs` edges
/// onto pruned steps are dropped so the result stays well-formed.
///
/// Transitive skip propagation (a step whose *only* dependency was pruned) is
/// the engine's concern, not this pure lowering — TODO(slice-4) when gate/skip
/// semantics land.
pub fn select_steps(
    ir: &PipelineIr,
    ctx: &serde_json::Value,
) -> Result<PipelineIr, PipelineError> {
    let mut kept: Vec<StepSpec> = Vec::new();
    for step in &ir.steps {
        let include = match &step.when {
            Some(When(expr)) => cel::eval_bool(expr, ctx)?,
            None => true,
        };
        if include {
            kept.push(step.clone());
        }
    }
    let kept_ids: BTreeSet<String> = kept.iter().map(|s| s.id.clone()).collect();
    for step in &mut kept {
        step.needs.0.retain(|n| kept_ids.contains(n));
    }
    Ok(PipelineIr {
        ir_version: ir.ir_version,
        triggers: ir.triggers.clone(),
        concurrency: ir.concurrency.clone(),
        environment: ir.environment.clone(),
        steps: kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compile(yaml: &str) -> PipelineIr {
        compile_yaml(yaml).expect("expected valid pipeline")
    }

    fn errors(yaml: &str) -> Vec<String> {
        match compile_yaml(yaml) {
            Err(PipelineError::Validation(d)) => d,
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn valid_diamond_dag_compiles_and_round_trips() {
        let ir = compile(
            r#"
            ir_version: 1
            steps:
              - { id: a, image: busybox }
              - { id: b, image: busybox, needs: [a] }
              - { id: c, image: busybox, needs: [a] }
              - { id: d, image: busybox, needs: [b, c] }
            "#,
        );
        assert_eq!(ir.ir_version, 1);
        assert_eq!(ir.steps.len(), 4);

        // IR round-trips through serde_json unchanged.
        let json = serde_json::to_string(&ir).unwrap();
        let back: PipelineIr = serde_json::from_str(&json).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn ir_version_defaults_when_omitted() {
        let ir = compile("steps: [{ id: a, image: busybox }]");
        assert_eq!(ir.ir_version, IR_VERSION);
    }

    #[test]
    fn matrix_expands_to_cartesian_product() {
        let ir = compile(
            r#"
            steps:
              - id: build
                image: rust
                matrix:
                  dimensions:
                    os: [linux, mac]
                    arch: [amd64, arm64]
            "#,
        );
        assert_eq!(ir.steps.len(), 4);
        // No compiled step keeps its matrix; ids are deterministic (keys sorted).
        assert!(ir.steps.iter().all(|s| s.matrix.is_none()));
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"build[arch=amd64,os=linux]"));
        assert!(ids.contains(&"build[arch=arm64,os=mac]"));
        // Coordinate is recorded for later CEL interpolation.
        let linux_amd = ir
            .steps
            .iter()
            .find(|s| s.id == "build[arch=amd64,os=linux]")
            .unwrap();
        assert_eq!(linux_amd.matrix_values.get("os").unwrap(), "linux");
        assert_eq!(linux_amd.matrix_values.get("arch").unwrap(), "amd64");
    }

    #[test]
    fn needs_on_a_matrixed_step_fans_into_all_instances() {
        let ir = compile(
            r#"
            steps:
              - id: build
                image: rust
                matrix:
                  dimensions:
                    os: [linux, mac]
              - id: release
                image: busybox
                needs: [build]
            "#,
        );
        let release = ir.steps.iter().find(|s| s.id == "release").unwrap();
        assert_eq!(
            release.needs.0,
            vec!["build[os=linux]".to_string(), "build[os=mac]".to_string()]
        );
    }

    #[test]
    fn cyclic_dependencies_are_rejected() {
        let diags = errors(
            r#"
            steps:
              - { id: a, image: busybox, needs: [c] }
              - { id: b, image: busybox, needs: [a] }
              - { id: c, image: busybox, needs: [b] }
            "#,
        );
        assert!(
            diags.iter().any(|d| d.contains("cycle")),
            "expected a cycle diagnostic, got {diags:?}"
        );
    }

    #[test]
    fn dangling_needs_is_rejected() {
        let diags = errors("steps: [{ id: a, image: busybox, needs: [ghost] }]");
        assert!(
            diags.iter().any(|d| d.contains("unknown step `ghost`")),
            "got {diags:?}"
        );
    }

    #[test]
    fn empty_matrix_dimension_is_rejected() {
        let diags = errors(
            r#"
            steps:
              - id: build
                image: rust
                matrix:
                  dimensions:
                    os: []
            "#,
        );
        assert!(
            diags.iter().any(|d| d.contains("dimension `os` has no values")),
            "got {diags:?}"
        );
    }

    #[test]
    fn matrix_exclude_predicate_drops_combinations() {
        let ir = compile(
            r#"
            steps:
              - id: build
                image: rust
                matrix:
                  dimensions:
                    os: [linux, windows]
                    arch: [amd64, arm64]
                  exclude:
                    - "os == 'windows' && arch == 'arm64'"
            "#,
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ir.steps.len(), 3, "one combo excluded, got {ids:?}");
        assert!(!ids.contains(&"build[arch=arm64,os=windows]"));
        assert!(ids.contains(&"build[arch=amd64,os=windows]"));
    }

    #[test]
    fn when_selects_steps_and_prunes_edges() {
        let ir = compile(
            r#"
            steps:
              - { id: build, image: busybox }
              - { id: deploy, image: busybox, needs: [build], when: "event.branch == 'main'" }
              - { id: notify, image: busybox, needs: [deploy] }
            "#,
        );

        let on_main = select_steps(&ir, &json!({ "event": { "branch": "main" } })).unwrap();
        assert_eq!(on_main.steps.len(), 3);

        let on_feature = select_steps(&ir, &json!({ "event": { "branch": "feat" } })).unwrap();
        let ids: Vec<&str> = on_feature.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["build", "notify"], "deploy excluded");
        // notify's edge onto the pruned `deploy` is dropped, not left dangling.
        let notify = on_feature.steps.iter().find(|s| s.id == "notify").unwrap();
        assert!(notify.needs.0.is_empty());
    }

    #[test]
    fn on_triggers_compile_and_match_by_kind_and_predicate() {
        let ir = compile(
            r#"
            on:
              push:
                when: "event.branch == 'main'"
              pull_request: {}
            steps:
              - { id: a, image: busybox }
            "#,
        );

        let on_main = json!({ "event": { "branch": "main" } });
        let on_dev = json!({ "event": { "branch": "dev" } });

        // push fires only when the predicate holds.
        assert!(matches_trigger(&ir, "push", &on_main).unwrap());
        assert!(!matches_trigger(&ir, "push", &on_dev).unwrap(), "ref filtered out");
        // pull_request has no predicate → always fires when declared.
        assert!(matches_trigger(&ir, "pull_request", &on_main).unwrap());
        // a kind not in `on:` never fires.
        assert!(!matches_trigger(&ir, "tag", &on_main).unwrap());
    }

    #[test]
    fn bad_trigger_predicate_fails_at_submit() {
        let diags = errors(
            r#"
            on:
              push:
                when: "1 +"
            steps:
              - { id: a, image: busybox }
            "#,
        );
        assert!(diags.iter().any(|d| d.contains("trigger `push`")), "got {diags:?}");
    }

    #[test]
    fn bad_cel_in_when_or_interpolation_fails_at_submit() {
        let when = errors(r#"steps: [{ id: a, image: busybox, when: "1 +" }]"#);
        assert!(when.iter().any(|d| d.contains("when")), "got {when:?}");

        let interp = errors(r#"steps: [{ id: a, image: "img:${{ 1 + }}" }]"#);
        assert!(!interp.is_empty(), "expected a submit-time CEL error");
    }

    #[test]
    fn duplicate_step_ids_are_rejected() {
        let diags = errors(
            r#"
            steps:
              - { id: a, image: busybox }
              - { id: a, image: alpine }
            "#,
        );
        assert!(diags.iter().any(|d| d.contains("duplicate step id `a`")), "got {diags:?}");
    }

    #[test]
    fn concurrency_block_compiles_and_defaults_policy_to_queue() {
        let ir = compile(
            r#"
            concurrency:
              group: deploy-prod
            steps:
              - { id: a, image: busybox }
            "#,
        );
        let c = ir.concurrency.clone().expect("concurrency present");
        assert_eq!(c.group, "deploy-prod");
        assert_eq!(c.policy, "queue", "policy defaults to the safe queue");

        // Round-trips through serde_json unchanged (self-describing IR).
        let back: PipelineIr =
            serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn concurrency_accepts_cancel_in_progress_and_rejects_unknown_policy() {
        let ir = compile(
            r#"
            concurrency: { group: pr-42, policy: cancel-in-progress }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert_eq!(ir.concurrency.unwrap().policy, "cancel-in-progress");

        let diags = errors(
            r#"
            concurrency: { group: g, policy: nonsense }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(
            diags.iter().any(|d| d.contains("unknown policy `nonsense`")),
            "got {diags:?}"
        );
    }

    #[test]
    fn concurrency_group_interpolation_is_checked_at_submit() {
        // A well-formed interpolated group compiles.
        compile(
            r#"
            concurrency: { group: "deploy-${{ event.branch }}" }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        // A broken interpolation is a submit-time error.
        let diags = errors(
            r#"
            concurrency: { group: "deploy-${{ 1 + }}" }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(
            diags.iter().any(|d| d.contains("concurrency: group")),
            "got {diags:?}"
        );
    }

    #[test]
    fn environment_target_compiles_and_round_trips() {
        let ir = compile(
            r#"
            environment: prod
            steps: [{ id: deploy, image: busybox }]
            "#,
        );
        assert_eq!(ir.environment.as_deref(), Some("prod"));
        let back: PipelineIr =
            serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);

        // A pipeline with no environment stays a plain CI pipeline.
        let ci = compile("steps: [{ id: a, image: busybox }]");
        assert!(ci.environment.is_none());
    }

    #[test]
    fn empty_environment_target_is_rejected() {
        let diags = errors(
            r#"
            environment: ""
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(
            diags.iter().any(|d| d.contains("environment: target must not be empty")),
            "got {diags:?}"
        );
    }

    #[test]
    fn gate_step_compiles_without_an_image() {
        let ir = compile(
            r#"
            steps:
              - { id: build, image: busybox }
              - { id: approve, gate: manual, needs: [build] }
              - { id: deploy, image: busybox, needs: [approve] }
            "#,
        );
        let approve = ir.steps.iter().find(|s| s.id == "approve").unwrap();
        assert!(approve.is_gate());
        assert_eq!(approve.gate.as_deref(), Some("manual"));
        assert!(approve.image.is_empty());
        // The gate keeps its DAG position between build and deploy.
        assert_eq!(approve.needs.0, vec!["build".to_string()]);
    }

    #[test]
    fn gate_step_rejects_unknown_kind_and_a_stray_image() {
        let unknown = errors(r#"steps: [{ id: g, gate: whenever }]"#);
        assert!(
            unknown.iter().any(|d| d.contains("unknown gate kind `whenever`")),
            "got {unknown:?}"
        );
        let with_image = errors(r#"steps: [{ id: g, gate: manual, image: busybox }]"#);
        assert!(
            with_image.iter().any(|d| d.contains("launches nothing")),
            "got {with_image:?}"
        );
    }

    #[test]
    fn timer_gate_requires_a_positive_gate_after() {
        // A timer gate compiles with a wait.
        let ir = compile(r#"steps: [{ id: wait, gate: timer, gate_after: 3600 }]"#);
        let g = ir.steps.iter().find(|s| s.id == "wait").unwrap();
        assert_eq!(g.gate.as_deref(), Some("timer"));
        assert_eq!(g.gate_after, Some(3600));

        // Missing / zero wait is rejected.
        assert!(errors(r#"steps: [{ id: w, gate: timer }]"#)
            .iter()
            .any(|d| d.contains("must set `gate_after`")));
        assert!(errors(r#"steps: [{ id: w, gate: timer, gate_after: 0 }]"#)
            .iter()
            .any(|d| d.contains("greater than zero")));
    }

    #[test]
    fn gate_after_only_valid_on_a_timer_gate() {
        for yaml in [
            r#"steps: [{ id: w, gate: manual, gate_after: 60 }]"#,
            r#"steps: [{ id: w, image: busybox, gate_after: 60 }]"#,
        ] {
            assert!(
                errors(yaml).iter().any(|d| d.contains("only valid on a timer gate")),
                "expected rejection for {yaml}"
            );
        }
    }

    #[test]
    fn non_gate_step_without_an_image_is_rejected() {
        let diags = errors(r#"steps: [{ id: a }]"#);
        assert!(
            diags.iter().any(|d| d.contains("step `a`: missing image")),
            "got {diags:?}"
        );
    }

    #[test]
    fn malformed_yaml_is_a_parse_error() {
        match compile_yaml("steps: [ this is : not valid") {
            Err(PipelineError::Parse(_)) => {}
            other => panic!("expected parse error, got {other:?}"),
        }
    }
}
