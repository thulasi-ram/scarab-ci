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
pub mod params;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Current schema version emitted by the compiler. Runs are self-describing
/// (ADR-0022): the IR carries this so an engine can reason about older Runs.
///
/// Bumped to `2` for ADR-0043: `interface.inputs` grew from a list of bare
/// parameter names into a list of typed [`ParamSpec`]s. Old IRs (`ir_version:
/// 1`, bare-string inputs) still load — the custom [`ParamSpec`] deserialize
/// accepts a bare string as `{ name, type: string, required: true }`.
pub const IR_VERSION: u32 = 2;

fn default_ir_version() -> u32 {
    IR_VERSION
}

/// The compiled, versioned intermediate representation of a pipeline.
///
/// Post-compile invariant: every [`StepSpec::matrix`] is `None` (all matrices
/// have been expanded) and step ids are unique.
// Note: no `Eq` — a [`ParamSpec::default`] may hold an arbitrary
// `serde_json::Value` (which is `PartialEq` but not `Eq`), so the whole IR is
// only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// The reuse **interface** of a Library pipeline (ADR-0038): the parameter
    /// names it requires and the output names it exposes to an `invoke:` caller.
    /// Only meaningful when this pipeline is invoked (it is authoring metadata for
    /// a `.scarab/lib/**` module); irrelevant on a top-level triggered pipeline.
    /// Distinct from the per-step workspace `inputs:`/`outputs:` (ADR-0007, 0035).
    #[serde(default, skip_serializing_if = "Interface::is_empty")]
    pub interface: Interface,
    pub steps: Vec<StepSpec>,
}

/// A pipeline's reuse / launch interface (ADR-0038, ADR-0043) — its explicit
/// contract with an `invoke:` caller *and* with a launch (`POST /v1/runs`,
/// manual dispatch). `inputs` are the **typed launch parameters** the caller
/// supplies (via `with:` for an invoke, or a launch `params` map); `outputs` are
/// the names the pipeline exposes, each of which must be one of its step ids.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<ParamSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
}

impl Interface {
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.outputs.is_empty()
    }
}

/// The declared type of a launch parameter (ADR-0043) — a **closed vocabulary**
/// so a supplied value can be coerced to a known shape and validated
/// fail-closed. Serialized lowercase (`string`, `boolean`, `number`, `choice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    /// A free-form string (the default, and what a bare-string input becomes).
    #[default]
    String,
    /// A boolean; `"true"`/`"false"`/`"yes"`/`"no"` string forms coerce.
    Boolean,
    /// A JSON number; numeric string forms coerce.
    Number,
    /// A string constrained to one of a fixed `options` list.
    Choice,
}

/// A single typed launch parameter (ADR-0043). Backward compatible with the
/// bare-name inputs of ADR-0038: a YAML/JSON **string** deserializes to
/// `ParamSpec { name, type: string, required: true, .. }`, while a **map**
/// deserializes to the full form. Serialization always emits the map form.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParamSpec {
    /// The parameter name — must be env-safe (`SCARAB_PARAM_<NAME>`).
    pub name: String,
    /// The declared type. Defaults to [`ParamType::String`].
    #[serde(rename = "type")]
    pub r#type: ParamType,
    /// Whether the caller must supply a value. Defaults to `true`. An optional
    /// param (`required: false`) **must** carry a `default` — this makes the
    /// resolved parameter set *total* (ADR-0043).
    pub required: bool,
    /// A default value, used when an optional param is not supplied. Mandatory
    /// for `required: false`; nonsensical (and rejected) for `required: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// For `type: choice`, the allowed values (non-empty). A supplied value must
    /// be a string in this list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    /// An optional CEL predicate over the resolved value (bound as `value`),
    /// evaluated at resolve time; `false`/non-bool fails the launch fail-closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<String>,
    /// Human-facing description (for a later describe/catalog surface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl<'de> Deserialize<'de> for ParamSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// The full map form; every field defaulted so a partial map fills in.
        #[derive(Deserialize)]
        struct Full {
            name: String,
            #[serde(rename = "type", default)]
            r#type: ParamType,
            #[serde(default = "default_true")]
            required: bool,
            #[serde(default)]
            default: Option<serde_json::Value>,
            #[serde(default)]
            options: Option<Vec<String>>,
            #[serde(default)]
            validate: Option<String>,
            #[serde(default)]
            description: Option<String>,
        }
        /// A bare string (backward-compat) *or* the full map (ADR-0043).
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Full(Full),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Bare(name) => ParamSpec {
                name,
                r#type: ParamType::String,
                required: true,
                default: None,
                options: None,
                validate: None,
                description: None,
            },
            Raw::Full(f) => ParamSpec {
                name: f.name,
                r#type: f.r#type,
                required: f.required,
                default: f.default,
                options: f.options,
                validate: f.validate,
                description: f.description,
            },
        })
    }
}

fn default_true() -> bool {
    true
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
    /// If set, this is an **invoke** step (ADR-0038): a repo-relative path to a
    /// Library pipeline (under `.scarab/lib/` by convention) that is *inlined at
    /// compile time*, not a runtime object. The step launches nothing itself;
    /// [`compile_yaml_with_libs`] flattens the referenced pipeline's steps into
    /// the caller's DAG, id-namespaced by this step's id (`deploy/build`), with
    /// `needs` rewritten across the seam. Local-only forever: repo-relative,
    /// read at the caller's ref — no absolute paths, no `../` escape, no
    /// cross-repo. After compile this field is always `None` (like `matrix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke: Option<String>,
    /// Inputs supplied to an invoked library (ADR-0038), `name → value`. Only
    /// meaningful on an [`invoke`](StepSpec::invoke) step. Validated at compile
    /// against the library's [`Interface::inputs`] (every required input present,
    /// no unknown extras), then injected into every inlined step as a
    /// `SCARAB_PARAM_<NAME>` env var (ADR-0008's param convention). Consumed by
    /// inlining — no compiled step retains it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub with: BTreeMap<String, String>,
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
    /// Secret keys to resolve and inject as env vars at launch (ADR-0014, 0037).
    /// Resolved against the run's scope with `env → repo → org` inheritance; a
    /// fork-PR run is locked out and receives none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Privilege escalation this step *requests* above the hardened baseline
    /// (ADR-0039). Absent = the restricted default. A request carries no
    /// authority: `run-as-root` is self-service, but `add_capabilities` and
    /// `privileged` take effect only if the run's target Environment whitelists
    /// the image digest at admission (else the step is rejected, fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<StepSecurity>,
    #[serde(default)]
    pub needs: Needs,
    /// Explicit input workspaces (ADR-0007): the subset of `needs` whose output
    /// workspace this step consumes. Absent = implicit-by-default (inherit every
    /// need's workspace). Naming a subset restricts what flows in *and* sharpens
    /// restart invalidation — the step's skip-if-unchanged signature is computed
    /// over exactly these inputs, so a change in a need it does not consume does
    /// not force it to re-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    /// Explicit output workspace paths (ADR-0007): the workspace-relative paths
    /// this step publishes downstream. Absent = the whole workspace (the implicit
    /// default). Naming a subset restricts what flows and gives a precise output
    /// hash. Enforced by the post-step CAS snapshot on the live-workspace path
    /// (ADR-0029); authored + validated here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
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

    /// Is this an image-less invoke step (a compile-time library inline point)?
    /// True only in authored YAML — [`compile_yaml_with_libs`] resolves every
    /// invoke step away, so a compiled IR never contains one.
    pub fn is_invoke(&self) -> bool {
        self.invoke.is_some()
    }
}

/// The privilege escalation a step requests above the hardened "restricted"
/// baseline (ADR-0039). This is the author's *request*; the Environment's
/// whitelist decides what is actually admitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSecurity {
    /// Run as uid 0 (opt out of the baseline `runAsNonRoot`). Self-service —
    /// root inside the caps-dropped, unprivileged, seccomp-confined sandbox does
    /// not escape it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub run_as_root: bool,
    /// Linux capabilities to add (e.g. `NET_ADMIN`). Governed: each must be
    /// whitelisted for the image digest by the Environment admin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_capabilities: Vec<String>,
    /// Run as a privileged container. Governed and digest-keyed forever — the
    /// node-escape hammer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub privileged: bool,
}

impl StepSecurity {
    /// Does this request any escalation at all?
    pub fn is_baseline(&self) -> bool {
        !self.run_as_root && !self.privileged && self.add_capabilities.is_empty()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// A launch-parameter coercion / resolution failure (ADR-0043). Carries a
    /// caller-facing message; composed per-param by [`params::resolve_params`].
    #[error("{0}")]
    Param(String),
}

/// Compile authored YAML into a validated [`PipelineIr`], with no libraries
/// available — a convenience over [`compile_yaml_with_libs`] for pipelines that
/// use no `invoke:` steps (an invoke would fail to resolve against the empty
/// library map).
pub fn compile_yaml(yaml: &str) -> Result<PipelineIr, PipelineError> {
    compile_yaml_with_libs(yaml, &BTreeMap::new())
}

/// Compile authored YAML into a validated [`PipelineIr`], resolving `invoke:`
/// steps against a pre-fetched `{repo-relative path → source}` map of the
/// repo's `.scarab/**` library files (ADR-0038).
///
/// Compilation is **pure** (ADR-0031): the caller (a server trigger path) does
/// the I/O — fetching the library sources at the caller's ref via `ForgePort` —
/// and passes them in here as `libs`. This function performs no I/O.
///
/// Pipeline: parse → **inline every `invoke:` step** by flattening its
/// referenced library's steps into the DAG, id-namespaced by the invoke-step id
/// and with `needs` rewritten across the seam → statically expand every matrix
/// into concrete steps (ADR-0023) → fan each `needs` edge onto every expanded
/// instance of its target → run full [`validate`]. All problems are reported
/// together via [`PipelineError::Validation`].
pub fn compile_yaml_with_libs(
    yaml: &str,
    libs: &BTreeMap<String, String>,
) -> Result<PipelineIr, PipelineError> {
    let authored: PipelineIr =
        serde_yaml::from_str(yaml).map_err(|e| PipelineError::Parse(e.to_string()))?;

    // Inline `invoke:` steps first (ADR-0038): the result is a flat step list
    // with no invoke steps, over which matrix expansion and validation run
    // unchanged. A library's own matrices thus expand normally.
    let authored_steps = inline_invokes(&authored.steps, libs).map_err(PipelineError::Validation)?;

    let mut diagnostics = Vec::new();
    let mut expanded: Vec<StepSpec> = Vec::new();
    // authored step id -> the concrete instance ids it expanded into.
    let mut instances: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for step in &authored_steps {
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
    // Fan an authored-id list onto every concrete instance of each id (matrix
    // fan-in). Unknown targets are left untouched so validate() flags them.
    let fan_in = |ids: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for id in ids {
            match instances.get(id) {
                Some(concrete) => {
                    for c in concrete {
                        if !out.contains(c) {
                            out.push(c.clone());
                        }
                    }
                }
                None => {
                    if !out.contains(id) {
                        out.push(id.clone());
                    }
                }
            }
        }
        out
    };
    for step in &mut expanded {
        step.needs.0 = fan_in(&step.needs.0);
        // Explicit `inputs:` fan in the same way, so a subset of a matrixed need
        // resolves to that need's instances.
        if let Some(inputs) = &step.inputs {
            step.inputs = Some(fan_in(inputs));
        }
    }

    let ir = PipelineIr {
        ir_version: authored.ir_version,
        triggers: authored.triggers,
        concurrency: authored.concurrency,
        environment: authored.environment,
        interface: authored.interface,
        steps: expanded,
    };

    validate(&ir).map_err(PipelineError::Validation)?;
    Ok(ir)
}

/// The library paths an authored pipeline's `invoke:` steps reference, so an
/// I/O edge (the server trigger path) can pre-fetch their sources before the
/// pure [`compile_yaml_with_libs`] (ADR-0038). Returns the normalized,
/// safety-checked repo-relative keys (the same keys [`compile_yaml_with_libs`]
/// looks up in its `libs` map), in declaration order and de-duplicated.
///
/// Best-effort: yaml that does not parse, and paths that fail the path-safety
/// check, are silently omitted here — [`compile_yaml_with_libs`] is the sole
/// authority that turns those into diagnostics. This function only answers
/// "what is safe to fetch?".
pub fn invoke_refs(yaml: &str) -> Vec<String> {
    let Ok(authored) = serde_yaml::from_str::<PipelineIr>(yaml) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for step in &authored.steps {
        if let Some(raw) = &step.invoke {
            if let Ok(key) = resolve_lib_path(raw) {
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        }
    }
    out
}

/// A lightweight, `on:`-only read of an authored pipeline (ADR-0043 catalog).
/// Parses **just the trigger block** — no `invoke:` pre-fetch, no matrix
/// expansion, no full compile — so the manual-dispatch catalog can list what a
/// ref exposes cheaply, deferring the (lib-prefetch + compile) cost to the
/// on-selection [`compile_yaml_with_libs`] interface read. The pipeline→trigger
/// mapping is thus knowable from a single file read: a catalog entry reports
/// `manual`/`api` from `triggers.0.contains_key(..)`. A malformed file surfaces
/// as [`PipelineError::Parse`], which the caller marks per-file rather than
/// failing the whole listing.
pub fn triggers_of(yaml: &str) -> Result<Triggers, PipelineError> {
    /// Only the `on:` block; every other field (`steps`, `interface`, …) is
    /// ignored, so this never needs the libraries a full compile would.
    #[derive(Deserialize)]
    struct ShallowOn {
        #[serde(default, rename = "on")]
        on: Triggers,
    }
    serde_yaml::from_str::<ShallowOn>(yaml)
        .map(|s| s.on)
        .map_err(|e| PipelineError::Parse(e.to_string()))
}

/// Normalize and safety-check a repo-relative library path referenced by an
/// `invoke:` step (ADR-0038), returning the canonical key used to look its
/// source up in the pre-fetched `{path → source}` map.
///
/// `invoke` is **local-only, forever**: the path must be repo-relative and
/// resolve inside the repo tree. Absolute paths, `..` traversal, and cross-repo
/// / remote references are rejected — cross-repo *causation* is `on: upstream`,
/// not `invoke`. A single leading `./` is accepted and stripped.
fn resolve_lib_path(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err("`invoke` path must not be empty".to_string());
    }
    // Cross-repo / remote forms (`scheme://…`, `github.com/org/repo//lib@sha`).
    if path.contains("://") || path.contains('@') {
        return Err(format!(
            "invoke path `{raw}` looks like a cross-repo/remote reference — `invoke` is local-only; use `on: upstream` for cross-repo causation"
        ));
    }
    if path.starts_with('/') {
        return Err(format!("invoke path `{raw}` must be repo-relative (no leading `/`)"));
    }
    if path.contains('\\') {
        return Err(format!("invoke path `{raw}` must use `/` path separators"));
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let mut components: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" => return Err(format!("invoke path `{raw}` has an empty path segment")),
            "." => return Err(format!(
                "invoke path `{raw}` must be a plain repo-relative path (no `.` segments)"
            )),
            ".." => return Err(format!(
                "invoke path `{raw}` must not escape the repo tree with `..`"
            )),
            c => components.push(c),
        }
    }
    Ok(components.join("/"))
}

/// Hard cap on `invoke` nesting depth (ADR-0038 termination corollary). A chain
/// deeper than this is rejected at compile — real libraries nest 1–3 levels; the
/// cap is a backstop against pathological (or generated) recursion, alongside
/// cycle detection.
const MAX_INVOKE_DEPTH: usize = 8;

/// Inline every `invoke:` step (ADR-0038) into a flat step list with no invoke
/// steps left. **Recursive**: a library that itself invokes is expanded to full
/// depth, ids namespacing correctly at each level (`deploy/db/migrate`).
///
/// Termination (ADR-0038): a direct or indirect invoke **cycle** is rejected
/// with a diagnostic naming the offending path, and a hard **depth cap**
/// ([`MAX_INVOKE_DEPTH`]) bounds nesting.
fn inline_invokes(
    steps: &[StepSpec],
    libs: &BTreeMap<String, String>,
) -> Result<Vec<StepSpec>, Vec<String>> {
    let mut diagnostics = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let out = inline_level(steps, libs, &mut stack, &mut diagnostics);
    if diagnostics.is_empty() {
        Ok(out)
    } else {
        Err(diagnostics)
    }
}

/// Resolve every `invoke:` in `steps` into a flat list, ids relative to *this*
/// level (an outer caller applies its own prefix). `stack` holds the library
/// keys currently being expanded on this DFS path — used both for cycle
/// detection (a key already on the stack) and the depth cap (its length).
fn inline_level(
    steps: &[StepSpec],
    libs: &BTreeMap<String, String>,
    stack: &mut Vec<String>,
    diagnostics: &mut Vec<String>,
) -> Vec<StepSpec> {
    let mut out: Vec<StepSpec> = Vec::new();
    // invoke-step id -> the namespaced leaf ids that a `needs: [that id]` maps to.
    let mut seam: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Non-matrixed invoke id -> its exposed output names, for the output-reference
    // rewrite (ADR-0041): `outputs.<id>.<name>` -> `outputs["<id>/<name>"].<name>`.
    let mut output_aliases: Vec<(String, Vec<String>)> = Vec::new();

    for step in steps {
        let Some(raw_path) = &step.invoke else {
            // `with:` supplies inputs to a library — meaningless off an invoke step.
            if !step.with.is_empty() {
                diagnostics.push(format!(
                    "step `{}`: `with:` supplies inputs to an invoked library and is only valid on an `invoke` step",
                    step.id
                ));
            }
            out.push(step.clone());
            continue;
        };

        // An invoke step inlines a library; it launches nothing itself.
        if !step.image.is_empty() || !step.command.is_empty() {
            diagnostics.push(format!(
                "step `{}`: an invoke step inlines a library — it must not set an image or command",
                step.id
            ));
        }
        if step.gate.is_some() {
            diagnostics.push(format!(
                "step `{}`: a step is either a gate or an invoke, not both",
                step.id
            ));
        }
        if step.security.as_ref().is_some_and(|s| !s.is_baseline()) {
            diagnostics.push(format!(
                "step `{}`: an invoke step launches nothing — it must not set `security`",
                step.id
            ));
        }

        let key = match resolve_lib_path(raw_path) {
            Ok(k) => k,
            Err(msg) => {
                diagnostics.push(format!("step `{}`: {msg}", step.id));
                continue;
            }
        };
        // Cycle: this library is already being expanded on the current path.
        if let Some(pos) = stack.iter().position(|k| k == &key) {
            let mut path = stack[pos..].to_vec();
            path.push(key.clone());
            diagnostics.push(format!(
                "step `{}`: invoke cycle detected: {}",
                step.id,
                path.join(" -> ")
            ));
            continue;
        }
        // Depth cap: refuse to nest deeper than the backstop.
        if stack.len() >= MAX_INVOKE_DEPTH {
            diagnostics.push(format!(
                "step `{}`: invoke nesting exceeds the depth cap of {MAX_INVOKE_DEPTH} (at `{key}`)",
                step.id
            ));
            continue;
        }
        let Some(src) = libs.get(&key) else {
            diagnostics.push(format!(
                "step `{}`: no library found at `{key}` (invoke is local-only; read at the caller's ref)",
                step.id
            ));
            continue;
        };
        let lib: PipelineIr = match serde_yaml::from_str(src) {
            Ok(ir) => ir,
            Err(e) => {
                diagnostics.push(format!("step `{}`: library `{key}` failed to parse: {e}", step.id));
                continue;
            }
        };
        if lib.steps.is_empty() {
            diagnostics.push(format!("step `{}`: library `{key}` has no steps", step.id));
            continue;
        }

        // Interface (ADR-0038): validate the caller's `with:` inputs and the
        // library's declared/exposed outputs at compile — explicit over ambient.
        validate_interface(step, &key, &lib, steps, diagnostics);
        // Record exposed outputs for the reference rewrite (ADR-0041). A matrixed
        // invoke's outputs are unaddressable (validate_interface errors on any
        // reference), so it contributes no alias.
        if step.matrix.is_none() && !lib.interface.outputs.is_empty() {
            output_aliases.push((step.id.clone(), lib.interface.outputs.clone()));
        }

        // Recurse: resolve the library's own invokes first, so `resolved` is a
        // flat list in the library's namespace (ids may already contain `/`).
        // Coordinate-independent, so it runs once even under a matrix.
        stack.push(key.clone());
        let resolved = inline_level(&lib.steps, libs, stack, diagnostics);
        stack.pop();

        // `matrix` × `invoke` (ADR-0038): a matrix on the invoke step fans out N
        // copies of the whole subgraph, once per coordinate. `expand_step` gives
        // the coordinate instances (id `deploy[svc=api]`, `matrix_values` set,
        // `exclude` applied); a step without a matrix yields exactly one instance
        // whose id is the step id — the single-copy path falls out for free.
        let invoke_instances = match expand_step(step) {
            Ok(v) => v,
            Err(msg) => {
                diagnostics.push(msg);
                continue;
            }
        };

        let internal: BTreeSet<&str> = resolved.iter().map(|s| s.id.as_str()).collect();
        let needed_inside: BTreeSet<&str> = resolved
            .iter()
            .flat_map(|s| s.needs.0.iter().map(String::as_str))
            .collect();

        // A downstream `needs: [S.id]` fans onto every copy's leaves (exit seam),
        // keyed by the *authored* id since that is all a downstream can name.
        let mut all_leaves: Vec<String> = Vec::new();
        for inv in &invoke_instances {
            let prefix = &inv.id;
            let ns = |id: &str| format!("{prefix}/{id}");
            for ls in &resolved {
                if !needed_inside.contains(ls.id.as_str()) {
                    all_leaves.push(ns(&ls.id));
                }
            }
            for ls in &resolved {
                let mut inst = ls.clone();
                inst.id = ns(&ls.id);
                // Entry seam: a library root inherits the invoke step's upstreams;
                // an internal need is namespaced.
                inst.needs = if ls.needs.0.is_empty() {
                    inv.needs.clone()
                } else {
                    Needs(ls.needs.0.iter().map(|n| namespace_ref(n, &internal, &ns)).collect())
                };
                if let Some(inputs) = &ls.inputs {
                    inst.inputs =
                        Some(inputs.iter().map(|n| namespace_ref(n, &internal, &ns)).collect());
                }
                // Propagate the invoke coordinate so CEL inside the subgraph can
                // read it; a library step's own coordinate (added later by the
                // main matrix pass) wins on key conflict.
                for (k, v) in &inv.matrix_values {
                    inst.matrix_values.entry(k.clone()).or_insert_with(|| v.clone());
                }
                // Inject the caller's inputs as `SCARAB_PARAM_<NAME>` env
                // (ADR-0008 param convention), coerced to their declared types
                // and with optional defaults applied — the same typed resolution
                // the launch path uses (ADR-0043). Prepended so a library step's
                // own explicit env of the same name still wins (applied later).
                let supplied: BTreeMap<String, serde_json::Value> = step
                    .with
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                let param_env: Vec<(String, String)> =
                    match params::resolve_params(&lib.interface, &supplied) {
                        Ok(resolved) => resolved
                            .iter()
                            .map(|(k, v)| {
                                (format!("SCARAB_PARAM_{}", k.to_uppercase()), params::stringify(v))
                            })
                            .collect(),
                        // A resolution error was already reported by
                        // `validate_interface`; the compile fails, so inject nothing.
                        Err(_) => Vec::new(),
                    };
                if !param_env.is_empty() {
                    let mut env = param_env;
                    env.append(&mut inst.env);
                    inst.env = env;
                }
                out.push(inst);
            }
        }
        seam.insert(step.id.clone(), all_leaves);
    }

    // Second pass: rewrite every reference to an invoke-step id onto its leaves.
    for step in &mut out {
        step.needs = Needs(rewrite_through_seam(&step.needs.0, &seam));
        if let Some(inputs) = &step.inputs {
            step.inputs = Some(rewrite_through_seam(inputs, &seam));
        }
    }

    // Output-reference rewrite (ADR-0041): resolve `outputs.<invoke-id>.<name>`
    // to the concrete inlined backing step so the launch context stays generic
    // (just `outputs[<step-id>] = <that step's results>`). The backing step of a
    // non-matrixed invoke's exposed output `<name>` is `<invoke-id>/<name>`.
    for (invoke_id, exposed) in &output_aliases {
        for step in &mut out {
            step.image = rewrite_output_refs(&step.image, invoke_id, exposed);
            for c in &mut step.command {
                *c = rewrite_output_refs(c, invoke_id, exposed);
            }
            for (_, v) in &mut step.env {
                *v = rewrite_output_refs(v, invoke_id, exposed);
            }
        }
    }

    out
}

/// Rewrite `outputs.<invoke_id>.<name>` references (for each exposed `<name>`) to
/// `outputs["<invoke_id>/<name>"].<name>` — pointing at the concrete inlined
/// backing step (ADR-0041). Boundary-guarded so `url` does not match `urls`.
fn rewrite_output_refs(text: &str, invoke_id: &str, exposed: &[String]) -> String {
    let mut result = text.to_string();
    for name in exposed {
        let needle = format!("outputs.{invoke_id}.{name}");
        let replacement = format!("outputs[\"{invoke_id}/{name}\"].{name}");
        result = replace_token(&result, &needle, &replacement);
    }
    result
}

/// Replace each occurrence of `needle` in `text` with `replacement`, but only
/// where `needle` is not immediately followed by an identifier char (so
/// `outputs.x.url` does not match inside `outputs.x.urls`).
fn replace_token(text: &str, needle: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find(needle) {
        let start = i + rel;
        let end = start + needle.len();
        let boundary_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        out.push_str(&text[i..start]);
        if boundary_ok {
            out.push_str(replacement);
        } else {
            out.push_str(needle);
        }
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

/// Namespace one `needs`/`inputs` reference of a library step: prefix it if it
/// points at a sibling library step, leave it otherwise (a library is meant to
/// be self-contained; a stray external ref is left for `validate` to flag).
fn namespace_ref(r: &str, internal: &BTreeSet<&str>, ns: &impl Fn(&str) -> String) -> String {
    if internal.contains(r) {
        ns(r)
    } else {
        r.to_string()
    }
}

/// Rewrite a `needs`/`inputs` list through the invoke seam: a reference to an
/// invoke-step id becomes its library's leaf ids (de-duplicated, order-stable);
/// any other reference passes through unchanged.
fn rewrite_through_seam(refs: &[String], seam: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in refs {
        match seam.get(r) {
            Some(leaves) => {
                for leaf in leaves {
                    if !out.contains(leaf) {
                        out.push(leaf.clone());
                    }
                }
            }
            None => {
                if !out.contains(r) {
                    out.push(r.clone());
                }
            }
        }
    }
    out
}

/// Validate an invoke step against its library's reuse [`Interface`] (ADR-0038):
/// every required input present, no unknown extras, every exposed output naming a
/// real library step, and no sibling referencing an output the library does not
/// expose. Reports every problem it finds (does not short-circuit).
fn validate_interface(
    step: &StepSpec,
    key: &str,
    lib: &PipelineIr,
    siblings: &[StepSpec],
    diagnostics: &mut Vec<String>,
) {
    let iface = &lib.interface;

    // Declared param specs must be well-formed (env-safe names, sensible
    // required/default/choice, a parsable `validate:`) — ADR-0043 §2.
    params::validate_param_specs(&iface.inputs, &format!("library `{key}`"), diagnostics);

    // Every required input must be supplied; a supplied value must coerce to the
    // declared type (and be a valid choice / pass `validate:`); no unknown extras.
    // Optional inputs (a `default`) may be omitted — the default is injected.
    for p in &iface.inputs {
        match step.with.get(&p.name) {
            None => {
                if p.required {
                    diagnostics.push(format!(
                        "step `{}`: missing required input `{}` for library `{key}`",
                        step.id, p.name
                    ));
                }
            }
            Some(raw) => {
                // `with:` values are authored as strings; coerce to the type.
                let supplied = serde_json::Value::String(raw.clone());
                if let Err(e) = params::resolve_one(p, &supplied) {
                    diagnostics.push(format!(
                        "step `{}`: input `{}` for library `{key}`: {e}",
                        step.id, p.name
                    ));
                }
            }
        }
    }
    for k in step.with.keys() {
        if !iface.inputs.iter().any(|p| &p.name == k) {
            diagnostics.push(format!(
                "step `{}`: unknown input `{k}` (library `{key}` declares inputs: [{}])",
                step.id,
                iface.inputs.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    // Each exposed output must name a real library step.
    let lib_ids: BTreeSet<&str> = lib.steps.iter().map(|s| s.id.as_str()).collect();
    for name in &iface.outputs {
        if !lib_ids.contains(name.as_str()) {
            diagnostics.push(format!(
                "step `{}`: library `{key}` exposes output `{name}` but has no step `{name}`",
                step.id
            ));
        }
    }
    // Output references (ADR-0041): a sibling reads an exposed output as
    // `${{ outputs.<invoke-id>.<name> }}`. Validate each reference — undeclared
    // output, referencing without `needs`, or referencing a matrixed invoke
    // (ambiguous per-coordinate) — at compile.
    let exposed: BTreeSet<&str> = iface.outputs.iter().map(String::as_str).collect();
    let outputs_base = format!("outputs.{}", step.id);
    let matrixed = step.matrix.is_some();
    for t in siblings {
        if t.id == step.id {
            continue;
        }
        for text in interpolatable_strings(t) {
            for expr in cel::interpolations(text).unwrap_or_default() {
                for field in field_accesses(expr, &outputs_base) {
                    if matrixed {
                        diagnostics.push(format!(
                            "step `{}`: references output `outputs.{}.{field}` of a matrixed invoke — per-coordinate output references are not supported (ADR-0041)",
                            t.id, step.id
                        ));
                    } else if !exposed.contains(field.as_str()) {
                        diagnostics.push(format!(
                            "step `{}`: references undeclared output `outputs.{}.{field}` (library `{key}` exposes: [{}])",
                            t.id,
                            step.id,
                            iface.outputs.join(", ")
                        ));
                    } else if !t.needs.0.contains(&step.id) {
                        // The value is only guaranteed to exist at launch if `t`
                        // depends on the invoke (ADR-0041 §4).
                        diagnostics.push(format!(
                            "step `{}`: reads `outputs.{}.{field}` but does not `needs: [{}]`",
                            t.id, step.id, step.id
                        ));
                    }
                }
            }
        }
    }
}

/// The step strings that may carry `${{ … }}` interpolations (image, command,
/// env values) — the surfaces scanned for invoke-output references.
fn interpolatable_strings(step: &StepSpec) -> Vec<&str> {
    let mut out: Vec<&str> = vec![step.image.as_str()];
    out.extend(step.command.iter().map(String::as_str));
    out.extend(step.env.iter().map(|(_, v)| v.as_str()));
    out
}

/// The field names accessed on `base` in a CEL expression (`base.<field>`), where
/// `base` stands alone (not itself the tail of a longer `x.base` access). Used to
/// find `<invoke-id>.<output>` references.
fn field_accesses(expr: &str, base: &str) -> Vec<String> {
    let bytes = expr.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = expr[i..].find(base) {
        let start = i + rel;
        let end = start + base.len();
        // A standalone `base`: the char before is neither an identifier char nor
        // a `.` (which would make it `x.base`, a field of something else).
        let before_ok = start == 0 || {
            let prev = bytes[start - 1];
            !is_ident_byte(prev) && prev != b'.'
        };
        if before_ok && end < bytes.len() && bytes[end] == b'.' {
            let fstart = end + 1;
            let mut fend = fstart;
            while fend < bytes.len() && is_ident_byte(bytes[fend]) {
                fend += 1;
            }
            if fend > fstart {
                out.push(expr[fstart..fend].to_string());
            }
        }
        i = end.max(start + 1);
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Is `s` a valid identifier (`[A-Za-z_][A-Za-z0-9_]*`)? Used to keep declared
/// input names env-var-safe.
pub(crate) fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
            // Merge onto any coordinate the step already carries (an invoke
            // step's coordinate propagated onto an inlined library step, ADR-0038)
            // rather than replacing it; for an ordinary step the map starts empty.
            instance.matrix_values.extend(coord);
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
        // Compile invariants: matrices and invokes are resolved away by now.
        if step.matrix.is_some() {
            diagnostics.push(format!("step `{}`: matrix was not expanded", step.id));
        }
        if step.invoke.is_some() {
            diagnostics.push(format!("step `{}`: invoke was not inlined", step.id));
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
                // A gate launches no Pod, so a privilege request is meaningless.
                if step.security.as_ref().is_some_and(|s| !s.is_baseline()) {
                    diagnostics.push(format!(
                        "step `{}`: a gate step launches nothing — it must not set `security`",
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
                // Capability names must be non-empty (ADR-0039). They are compared
                // verbatim against the Environment whitelist at admission.
                if let Some(sec) = &step.security {
                    for cap in &sec.add_capabilities {
                        if cap.trim().is_empty() {
                            diagnostics.push(format!(
                                "step `{}`: `add_capabilities` entries must be non-empty capability names",
                                step.id
                            ));
                        }
                    }
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
        // Explicit inputs must be a subset of needs — a step can only consume the
        // workspace of a step it depends on (ADR-0007).
        if let Some(inputs) = &step.inputs {
            for input in inputs {
                if !step.needs.0.contains(input) {
                    diagnostics.push(format!(
                        "step `{}`: input `{input}` is not among its needs",
                        step.id
                    ));
                }
            }
        }
        // Explicit outputs must be workspace-relative, non-empty paths (ADR-0007):
        // no empty entry, no absolute path, no `..` traversal out of the workspace.
        if let Some(outputs) = &step.outputs {
            if outputs.is_empty() {
                diagnostics.push(format!(
                    "step `{}`: `outputs` must list at least one path (omit it for the whole workspace)",
                    step.id
                ));
            }
            for path in outputs {
                if path.is_empty() || path.starts_with('/') || path.split('/').any(|c| c == "..") {
                    diagnostics.push(format!(
                        "step `{}`: output path `{path}` must be workspace-relative (no leading `/`, no `..`)",
                        step.id
                    ));
                }
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

    // Launch-parameter interface (ADR-0043): each declared param spec must be
    // well-formed — env-safe unique names, coherent required/default, a
    // non-empty `options` for a choice, a default within those options, and a
    // parsable `validate:` predicate.
    params::validate_param_specs(&ir.interface.inputs, "interface", &mut diagnostics);

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

/// Return the ids of steps whose `when:` guard is **false** under `ctx` — the
/// steps to prune (ADR-0009, 0033). A step with no `when:` is always included.
///
/// The run keeps the *full* DAG (edges intact) and marks these steps `Skipped`,
/// so the engine's dep cascade transitively skips their descendants (ADR-0033) —
/// pruning here does not drop edges, which is what makes transitive skip work.
pub fn excluded_steps(
    ir: &PipelineIr,
    ctx: &serde_json::Value,
) -> Result<Vec<String>, PipelineError> {
    let mut excluded = Vec::new();
    for step in &ir.steps {
        let include = match &step.when {
            Some(When(expr)) => cel::eval_bool(expr, ctx)?,
            None => true,
        };
        if !include {
            excluded.push(step.id.clone());
        }
    }
    Ok(excluded)
}

/// Resolve `${{ … }}` interpolations in a step's launchable surfaces — `image`,
/// `command`, and `env` values — against `ctx` (ADR-0041). Pure: the engine
/// builds `ctx` at launch from the step's upstream results (`outputs`) and its
/// own matrix coordinate (`matrix`), then launches the returned spec.
///
/// **Fail-fast:** a bad reference — an unbound name, a type error in a guard —
/// is a hard error that fails the step; it never renders empty or degrades
/// silently (ADR-0041 §5). A surface with no `${{ … }}` is returned verbatim.
pub fn interpolate_spec(
    spec: &StepSpec,
    ctx: &serde_json::Value,
) -> Result<StepSpec, PipelineError> {
    let mut out = spec.clone();
    out.image = cel::interpolate(&spec.image, ctx)?;
    out.command = spec
        .command
        .iter()
        .map(|c| cel::interpolate(c, ctx))
        .collect::<Result<Vec<_>, _>>()?;
    out.env = spec
        .env
        .iter()
        .map(|(k, v)| cel::interpolate(v, ctx).map(|v| (k.clone(), v)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
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
    fn step_security_request_parses_and_round_trips() {
        let ir = compile(
            r#"
            ir_version: 1
            steps:
              - id: deploy
                image: ghcr.io/acme/deployer@sha256:aaaa
                security:
                  run_as_root: true
                  privileged: true
                  add_capabilities: [NET_ADMIN]
            "#,
        );
        let sec = ir.steps[0].security.as_ref().unwrap();
        assert!(sec.run_as_root && sec.privileged);
        assert_eq!(sec.add_capabilities, vec!["NET_ADMIN".to_string()]);
        // A baseline step omits `security` entirely (skipped on serialize).
        let ir2 = compile("steps: [{ id: a, image: busybox }]");
        assert!(ir2.steps[0].security.is_none());
        let json = serde_json::to_string(&ir2).unwrap();
        assert!(!json.contains("security"));
    }

    #[test]
    fn gate_may_not_request_security() {
        let errs = errors(
            r#"
            steps:
              - id: g
                gate: manual
                security: { privileged: true }
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("must not set `security`")));
    }

    #[test]
    fn empty_capability_name_is_rejected() {
        let errs = errors(
            r#"
            steps:
              - id: s
                image: busybox
                security: { add_capabilities: [""] }
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("non-empty capability")));
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
    fn excluded_steps_reports_when_false_steps() {
        let ir = compile(
            r#"
            steps:
              - { id: build, image: busybox }
              - { id: deploy, image: busybox, needs: [build], when: "event.branch == 'main'" }
              - { id: notify, image: busybox, needs: [deploy] }
            "#,
        );

        // On main the guard holds — nothing excluded.
        assert!(excluded_steps(&ir, &json!({ "event": { "branch": "main" } }))
            .unwrap()
            .is_empty());

        // Off main only `deploy` (the guarded step) is excluded; the DAG keeps its
        // edges so the engine transitively skips `notify` (ADR-0033).
        let excluded = excluded_steps(&ir, &json!({ "event": { "branch": "feat" } })).unwrap();
        assert_eq!(excluded, vec!["deploy".to_string()]);
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
    fn explicit_inputs_compile_and_must_be_a_subset_of_needs() {
        let ir = compile(
            r#"
            steps:
              - { id: b, image: busybox }
              - { id: c, image: busybox }
              - { id: d, image: busybox, needs: [b, c], inputs: [b] }
            "#,
        );
        let d = ir.steps.iter().find(|s| s.id == "d").unwrap();
        assert_eq!(d.inputs.as_deref(), Some(["b".to_string()].as_slice()));

        // An input that is not among the step's needs is rejected.
        let diags = errors(
            r#"
            steps:
              - { id: b, image: busybox }
              - { id: d, image: busybox, needs: [b], inputs: [c] }
            "#,
        );
        assert!(
            diags.iter().any(|d| d.contains("input `c` is not among its needs")),
            "got {diags:?}"
        );
    }

    #[test]
    fn explicit_outputs_compile_and_must_be_workspace_relative() {
        let ir = compile(r#"steps: [{ id: build, image: rust, outputs: [dist/, VERSION] }]"#);
        let b = ir.steps.iter().find(|s| s.id == "build").unwrap();
        assert_eq!(b.outputs.as_deref(), Some(["dist/".to_string(), "VERSION".to_string()].as_slice()));

        // Absolute paths, `..` traversal, and an empty list are rejected.
        assert!(errors(r#"steps: [{ id: b, image: rust, outputs: ["/etc/passwd"] }]"#)
            .iter()
            .any(|d| d.contains("workspace-relative")));
        assert!(errors(r#"steps: [{ id: b, image: rust, outputs: ["../x"] }]"#)
            .iter()
            .any(|d| d.contains("workspace-relative")));
        assert!(errors(r#"steps: [{ id: b, image: rust, outputs: [] }]"#)
            .iter()
            .any(|d| d.contains("at least one path")));
    }

    #[test]
    fn explicit_inputs_fan_in_over_a_matrixed_need() {
        let ir = compile(
            r#"
            steps:
              - id: build
                image: rust
                matrix: { dimensions: { os: [linux, mac] } }
              - { id: ship, image: busybox, needs: [build], inputs: [build] }
            "#,
        );
        let ship = ir.steps.iter().find(|s| s.id == "ship").unwrap();
        assert_eq!(
            ship.inputs.as_deref(),
            Some(["build[os=linux]".to_string(), "build[os=mac]".to_string()].as_slice()),
            "an input on a matrixed need fans to all its instances"
        );
    }

    // --- invoke / local reuse (ADR-0038) --------------------------------------

    fn libs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn compile_with(yaml: &str, libs: &BTreeMap<String, String>) -> PipelineIr {
        compile_yaml_with_libs(yaml, libs).expect("expected valid pipeline")
    }

    fn errors_with(yaml: &str, libs: &BTreeMap<String, String>) -> Vec<String> {
        match compile_yaml_with_libs(yaml, libs) {
            Err(PipelineError::Validation(d)) => d,
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn invoke_inlines_library_steps_namespaced_and_rewrites_the_seam() {
        let lib = r#"
            steps:
              - { id: build, image: rust }
              - { id: test, image: rust, needs: [build] }
        "#;
        let ir = compile_with(
            r#"
            steps:
              - { id: checkout, image: busybox }
              - { id: ci, invoke: .scarab/lib/ci.yaml, needs: [checkout] }
              - { id: publish, image: busybox, needs: [ci] }
            "#,
            &libs(&[(".scarab/lib/ci.yaml", lib)]),
        );

        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        // Library steps are inlined, id-namespaced by the invoke-step id; no
        // invoke step survives.
        assert!(ids.contains(&"ci/build"));
        assert!(ids.contains(&"ci/test"));
        assert!(!ids.contains(&"ci"), "the invoke step itself is gone");
        assert!(ir.steps.iter().all(|s| !s.is_invoke()));

        // Entry seam: the library root inherits the invoke step's upstream.
        let build = ir.steps.iter().find(|s| s.id == "ci/build").unwrap();
        assert_eq!(build.needs.0, vec!["checkout".to_string()]);
        // Internal edge is namespaced.
        let test = ir.steps.iter().find(|s| s.id == "ci/test").unwrap();
        assert_eq!(test.needs.0, vec!["ci/build".to_string()]);
        // Exit seam: `needs: [ci]` resolves to the library's leaf (`ci/test`).
        let publish = ir.steps.iter().find(|s| s.id == "publish").unwrap();
        assert_eq!(publish.needs.0, vec!["ci/test".to_string()]);

        // The result is a single valid flat DAG that round-trips.
        let back: PipelineIr =
            serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn invoke_seam_fans_multiple_roots_and_leaves() {
        // A library with two roots and two leaves (a diamond with split ends).
        let lib = r#"
            steps:
              - { id: a, image: busybox }
              - { id: b, image: busybox }
              - { id: c, image: busybox, needs: [a, b] }
              - { id: d, image: busybox, needs: [a] }
        "#;
        let ir = compile_with(
            r#"
            steps:
              - { id: prep, image: busybox }
              - { id: mod, invoke: .scarab/lib/x.yaml, needs: [prep] }
              - { id: after, image: busybox, needs: [mod] }
            "#,
            &libs(&[(".scarab/lib/x.yaml", lib)]),
        );
        // Both roots (a, b) inherit `prep`.
        for root in ["mod/a", "mod/b"] {
            let s = ir.steps.iter().find(|s| s.id == root).unwrap();
            assert_eq!(s.needs.0, vec!["prep".to_string()], "{root} inherits prep");
        }
        // Both leaves (c, d — nothing inside needs them) anchor the exit seam.
        let after = ir.steps.iter().find(|s| s.id == "after").unwrap();
        assert_eq!(after.needs.0, vec!["mod/c".to_string(), "mod/d".to_string()]);
    }

    #[test]
    fn invoke_composes_with_a_matrix_inside_the_library() {
        // matrix × invoke is a follow-up, but a matrix *inside* the library must
        // expand normally after inlining.
        let lib = r#"
            steps:
              - id: build
                image: rust
                matrix: { dimensions: { os: [linux, mac] } }
        "#;
        let ir = compile_with(
            "steps: [{ id: ci, invoke: .scarab/lib/m.yaml }]",
            &libs(&[(".scarab/lib/m.yaml", lib)]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"ci/build[os=linux]"));
        assert!(ids.contains(&"ci/build[os=mac]"));
        assert!(ir.steps.iter().all(|s| s.matrix.is_none()));
    }

    #[test]
    fn invoke_chains_through_the_seam() {
        // An invoke step whose `needs` is another invoke step resolves across
        // both seams (second-pass rewrite).
        let a = "steps: [{ id: x, image: busybox }]";
        let b = "steps: [{ id: y, image: busybox }]";
        let ir = compile_with(
            r#"
            steps:
              - { id: first, invoke: .scarab/lib/a.yaml }
              - { id: second, invoke: .scarab/lib/b.yaml, needs: [first] }
            "#,
            &libs(&[(".scarab/lib/a.yaml", a), (".scarab/lib/b.yaml", b)]),
        );
        let y = ir.steps.iter().find(|s| s.id == "second/y").unwrap();
        assert_eq!(y.needs.0, vec!["first/x".to_string()], "second's root chains onto first's leaf");
    }

    #[test]
    fn invoke_rejects_absolute_traversal_and_cross_repo_paths() {
        let lib = "steps: [{ id: a, image: busybox }]";
        let l = libs(&[(".scarab/lib/a.yaml", lib)]);
        for (path, needle) in [
            ("/etc/passwd", "no leading `/`"),
            ("../secrets.yaml", "escape the repo"),
            (".scarab/../../x.yaml", "escape the repo"),
            ("github.com/org/repo//lib@sha", "cross-repo"),
            ("https://evil.example/x.yaml", "cross-repo"),
        ] {
            let yaml = format!("steps: [{{ id: s, invoke: \"{path}\" }}]");
            let errs = errors_with(&yaml, &l);
            assert!(
                errs.iter().any(|e| e.contains(needle)),
                "path {path} should be rejected with `{needle}`, got {errs:?}"
            );
        }
    }

    #[test]
    fn invoke_of_a_missing_library_is_a_diagnostic_not_a_panic() {
        let errs = errors_with(
            "steps: [{ id: s, invoke: .scarab/lib/absent.yaml }]",
            &BTreeMap::new(),
        );
        assert!(
            errs.iter().any(|e| e.contains("no library found at `.scarab/lib/absent.yaml`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn nested_invoke_inlines_recursively_with_deep_namespacing() {
        // deploy.yaml invokes db.yaml (as step `db`); db.yaml has a `migrate`
        // step. Top-level invokes deploy.yaml as step `deploy`.
        let db = r#"
            steps:
              - { id: migrate, image: postgres }
        "#;
        let deploy = r#"
            steps:
              - { id: db, invoke: .scarab/lib/db.yaml }
              - { id: app, image: busybox, needs: [db] }
        "#;
        let ir = compile_with(
            "steps: [{ id: deploy, invoke: .scarab/lib/deploy.yaml }]",
            &libs(&[(".scarab/lib/deploy.yaml", deploy), (".scarab/lib/db.yaml", db)]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        // Two levels of namespacing compose: `deploy` / `db` / `migrate`.
        assert!(ids.contains(&"deploy/db/migrate"), "got {ids:?}");
        assert!(ids.contains(&"deploy/app"), "got {ids:?}");
        // The inner seam survives the outer inline: `app` depends on db's leaf.
        let app = ir.steps.iter().find(|s| s.id == "deploy/app").unwrap();
        assert_eq!(app.needs.0, vec!["deploy/db/migrate".to_string()]);
        assert!(ir.steps.iter().all(|s| !s.is_invoke()));
    }

    #[test]
    fn a_direct_invoke_cycle_is_rejected() {
        // A library that invokes itself.
        let selfref = "steps: [{ id: loop, invoke: .scarab/lib/self.yaml }]";
        let errs = errors_with(
            "steps: [{ id: s, invoke: .scarab/lib/self.yaml }]",
            &libs(&[(".scarab/lib/self.yaml", selfref)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("invoke cycle detected")
                && e.contains(".scarab/lib/self.yaml -> .scarab/lib/self.yaml")),
            "got {errs:?}"
        );
    }

    #[test]
    fn an_indirect_invoke_cycle_is_rejected_with_its_path() {
        // a -> b -> a.
        let a = "steps: [{ id: x, invoke: .scarab/lib/b.yaml }]";
        let b = "steps: [{ id: y, invoke: .scarab/lib/a.yaml }]";
        let errs = errors_with(
            "steps: [{ id: s, invoke: .scarab/lib/a.yaml }]",
            &libs(&[(".scarab/lib/a.yaml", a), (".scarab/lib/b.yaml", b)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("invoke cycle detected")
                && e.contains(".scarab/lib/a.yaml -> .scarab/lib/b.yaml -> .scarab/lib/a.yaml")),
            "got {errs:?}"
        );
    }

    #[test]
    fn a_diamond_of_invokes_is_not_a_cycle() {
        // top invokes A and B, both invoke C — C appears twice but on separate
        // DFS paths, so it must NOT be rejected as a cycle.
        let c = "steps: [{ id: leaf, image: busybox }]";
        let a = "steps: [{ id: x, invoke: .scarab/lib/c.yaml }]";
        let b = "steps: [{ id: y, invoke: .scarab/lib/c.yaml }]";
        let ir = compile_with(
            r#"
            steps:
              - { id: pa, invoke: .scarab/lib/a.yaml }
              - { id: pb, invoke: .scarab/lib/b.yaml }
            "#,
            &libs(&[
                (".scarab/lib/a.yaml", a),
                (".scarab/lib/b.yaml", b),
                (".scarab/lib/c.yaml", c),
            ]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"pa/x/leaf") && ids.contains(&"pb/y/leaf"), "got {ids:?}");
    }

    #[test]
    fn invoke_nesting_respects_the_depth_cap() {
        // Build a chain lib0 -> lib1 -> ... where libN invokes lib(N+1). The top
        // pipeline invokes lib0, so a chain of length L nests L levels deep.
        fn chain(len: usize) -> BTreeMap<String, String> {
            let mut m = BTreeMap::new();
            for i in 0..len {
                let src = if i + 1 < len {
                    format!("steps: [{{ id: s{i}, invoke: .scarab/lib/l{}.yaml }}]", i + 1)
                } else {
                    format!("steps: [{{ id: s{i}, image: busybox }}]")
                };
                m.insert(format!(".scarab/lib/l{i}.yaml"), src);
            }
            m
        }
        let top = "steps: [{ id: t, invoke: .scarab/lib/l0.yaml }]";

        // At the cap (stack never reaches MAX_INVOKE_DEPTH before the leaf) it
        // compiles; one deeper is rejected.
        let ok = compile_yaml_with_libs(top, &chain(MAX_INVOKE_DEPTH));
        assert!(ok.is_ok(), "a chain at the cap compiles: {ok:?}");

        let too_deep = errors_with(top, &chain(MAX_INVOKE_DEPTH + 1));
        assert!(
            too_deep.iter().any(|e| e.contains("exceeds the depth cap")),
            "got {too_deep:?}"
        );
    }

    #[test]
    fn invoke_step_must_not_carry_an_image() {
        let lib = "steps: [{ id: a, image: busybox }]";
        let with_image = errors_with(
            "steps: [{ id: s, invoke: .scarab/lib/a.yaml, image: busybox }]",
            &libs(&[(".scarab/lib/a.yaml", lib)]),
        );
        assert!(with_image.iter().any(|e| e.contains("must not set an image")), "got {with_image:?}");
    }

    #[test]
    fn matrix_on_an_invoke_fans_out_the_subgraph_per_coordinate() {
        let lib = r#"
            steps:
              - { id: build, image: rust }
              - { id: test, image: rust, needs: [build] }
        "#;
        let ir = compile_with(
            r#"
            steps:
              - id: svc
                invoke: .scarab/lib/ci.yaml
                matrix: { dimensions: { svc: [api, web] } }
              - { id: gate, image: busybox, needs: [svc] }
            "#,
            &libs(&[(".scarab/lib/ci.yaml", lib)]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        // Each copy's ids carry both the coordinate and the invoke namespace,
        // uniquely.
        for id in ["svc[svc=api]/build", "svc[svc=api]/test", "svc[svc=web]/build", "svc[svc=web]/test"] {
            assert!(ids.contains(&id), "missing {id} in {ids:?}");
        }
        // Internal edges rewrite per copy.
        let api_test = ir.steps.iter().find(|s| s.id == "svc[svc=api]/test").unwrap();
        assert_eq!(api_test.needs.0, vec!["svc[svc=api]/build".to_string()]);
        // The coordinate is visible to the inlined steps (for CEL interpolation).
        let api_build = ir.steps.iter().find(|s| s.id == "svc[svc=api]/build").unwrap();
        assert_eq!(api_build.matrix_values.get("svc").map(String::as_str), Some("api"));
        // Exit seam: `needs: [svc]` fans onto every copy's leaf.
        let gate = ir.steps.iter().find(|s| s.id == "gate").unwrap();
        assert_eq!(
            gate.needs.0,
            vec!["svc[svc=api]/test".to_string(), "svc[svc=web]/test".to_string()]
        );
    }

    #[test]
    fn matrix_on_an_invoke_is_two_dimensional_and_honours_exclude() {
        let lib = "steps: [{ id: run, image: busybox }]";
        let ir = compile_with(
            r#"
            steps:
              - id: m
                invoke: .scarab/lib/x.yaml
                matrix:
                  dimensions:
                    os: [linux, windows]
                    arch: [amd64, arm64]
                  exclude:
                    - "os == 'windows' && arch == 'arm64'"
            "#,
            &libs(&[(".scarab/lib/x.yaml", lib)]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        // 2x2 minus one excluded combination = 3 copies, each a unique id.
        assert_eq!(ir.steps.len(), 3, "got {ids:?}");
        assert!(!ids.contains(&"m[arch=arm64,os=windows]/run"), "excluded combo absent");
        assert!(ids.contains(&"m[arch=amd64,os=windows]/run"));
        assert!(ids.contains(&"m[arch=arm64,os=linux]/run"));
    }

    #[test]
    fn invoke_refs_lists_safe_fetchable_paths_only() {
        let yaml = r#"
            steps:
              - { id: a, invoke: ./.scarab/lib/one.yaml }
              - { id: b, invoke: .scarab/lib/two.yaml }
              - { id: c, invoke: ../escape.yaml }
              - { id: d, image: busybox }
        "#;
        // `./` is normalized; the traversal path is omitted (unsafe → not fetched).
        assert_eq!(
            invoke_refs(yaml),
            vec![".scarab/lib/one.yaml".to_string(), ".scarab/lib/two.yaml".to_string()]
        );
    }

    // --- invoke interface (ADR-0038 slice 3) ----------------------------------

    /// A library declaring required inputs and an exposed output.
    const IFACE_LIB: &str = r#"
        interface:
          inputs:  [region, replicas]
          outputs: [url]
        steps:
          - { id: plan, image: tf, command: ["plan"] }
          - { id: url, image: tf, command: ["apply"], needs: [plan] }
    "#;

    #[test]
    fn a_satisfied_interface_compiles_injects_params_and_rewrites_output_refs() {
        let ir = compile_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
              - { id: notify, image: busybox, command: ["echo", "${{ outputs.deploy.url }}"], needs: [deploy] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        // Inputs reach every inlined step as SCARAB_PARAM_* env (ADR-0008).
        let plan = ir.steps.iter().find(|s| s.id == "deploy/plan").unwrap();
        assert!(plan.env.contains(&("SCARAB_PARAM_REGION".to_string(), "us-east-1".to_string())));
        assert!(plan.env.contains(&("SCARAB_PARAM_REPLICAS".to_string(), "3".to_string())));
        // The invoke step is gone; the exposed-output reference compiled fine.
        assert!(ir.steps.iter().all(|s| !s.is_invoke()));
        assert!(ir.steps.iter().any(|s| s.id == "deploy/url"));
        // The output reference is rewritten to the concrete backing step (ADR-0041),
        // so the launch context stays generic (keyed by step id).
        let notify = ir.steps.iter().find(|s| s.id == "notify").unwrap();
        assert_eq!(notify.command[1], "${{ outputs[\"deploy/url\"].url }}");
    }

    #[test]
    fn a_missing_required_input_is_rejected() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/deploy.yaml
                with: { region: us-east-1 }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("missing required input `replicas`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn an_unknown_extra_input_is_rejected() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3", bogus: x }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(errs.iter().any(|e| e.contains("unknown input `bogus`")), "got {errs:?}");
    }

    #[test]
    fn a_reference_to_an_undeclared_output_is_a_compile_error() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
              - { id: notify, image: busybox, command: ["echo", "${{ outputs.deploy.secret_ip }}"], needs: [deploy] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("references undeclared output `outputs.deploy.secret_ip`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn reading_an_output_without_needing_the_invoke_is_a_compile_error() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
              - { id: notify, image: busybox, command: ["echo", "${{ outputs.deploy.url }}"] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("does not `needs: [deploy]`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn referencing_the_output_of_a_matrixed_invoke_is_rejected() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
                matrix: { dimensions: { region: [a, b] } }
              - { id: notify, image: busybox, command: ["${{ outputs.deploy.url }}"], needs: [deploy] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("matrixed invoke")),
            "got {errs:?}"
        );
    }

    #[test]
    fn interpolate_spec_resolves_outputs_and_matrix_and_fails_fast() {
        use serde_json::json;
        let mut spec = compile("steps: [{ id: notify, image: busybox }]").steps.pop().unwrap();
        spec.command = vec!["post".into(), r#"${{ outputs["deploy/url"].url }}"#.into()];
        spec.env = vec![("TARGET".into(), "${{ matrix.region }}".into())];

        let ctx = json!({
            "outputs": { "deploy/url": { "url": "https://svc.example" } },
            "matrix": { "region": "us-east-1" },
        });
        let out = interpolate_spec(&spec, &ctx).unwrap();
        assert_eq!(out.command[1], "https://svc.example");
        assert_eq!(out.env[0].1, "us-east-1");

        // Fail-fast: an unbound reference is a hard error, never empty.
        let bad = json!({ "outputs": {} });
        assert!(interpolate_spec(&spec, &bad).is_err());
    }

    #[test]
    fn an_exposed_output_that_is_not_a_step_is_rejected() {
        let bad_lib = r#"
            interface:
              outputs: [ghost]
            steps:
              - { id: run, image: busybox }
        "#;
        let errs = errors_with(
            "steps: [{ id: s, invoke: .scarab/lib/bad.yaml }]",
            &libs(&[(".scarab/lib/bad.yaml", bad_lib)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("exposes output `ghost` but has no step `ghost`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn with_on_a_non_invoke_step_is_rejected() {
        let errs = errors_with(
            r#"steps: [{ id: a, image: busybox, with: { x: y } }]"#,
            &BTreeMap::new(),
        );
        assert!(
            errs.iter().any(|e| e.contains("only valid on an `invoke` step")),
            "got {errs:?}"
        );
    }

    #[test]
    fn interface_round_trips_on_a_library_pipeline() {
        // A library authored with an interface parses and the interface survives
        // serialization when the pipeline is compiled on its own (as a top-level).
        let ir = compile(
            r#"
            interface: { inputs: [region], outputs: [url] }
            steps: [{ id: url, image: busybox }]
            "#,
        );
        // A bare-string input (ADR-0038 back-compat) becomes a required string
        // param (ADR-0043).
        assert_eq!(ir.interface.inputs.len(), 1);
        assert_eq!(ir.interface.inputs[0].name, "region");
        assert_eq!(ir.interface.inputs[0].r#type, ParamType::String);
        assert!(ir.interface.inputs[0].required);
        assert_eq!(ir.interface.outputs, vec!["url".to_string()]);
        let back: PipelineIr =
            serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);
    }

    // --- typed launch parameters (ADR-0043) -----------------------------------

    #[test]
    fn bare_string_inputs_deserialize_as_required_string_params() {
        // Back-compat (ADR-0038): `inputs: [region, replicas]` still parses.
        let ir = compile(
            r#"
            interface: { inputs: [region, replicas] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert_eq!(ir.interface.inputs.len(), 2);
        for p in &ir.interface.inputs {
            assert_eq!(p.r#type, ParamType::String);
            assert!(p.required);
            assert!(p.default.is_none());
        }
    }

    #[test]
    fn a_typed_param_map_compiles_and_survives_round_trip() {
        let ir = compile(
            r#"
            interface:
              inputs:
                - { name: region, type: string }
                - { name: replicas, type: number, required: false, default: 2 }
                - { name: env, type: choice, options: [staging, prod], required: false, default: staging }
                - { name: force, type: boolean, required: false, default: false, description: "skip checks" }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        let by = |n: &str| ir.interface.inputs.iter().find(|p| p.name == n).unwrap();
        assert_eq!(by("replicas").r#type, ParamType::Number);
        assert_eq!(by("env").options.as_ref().unwrap().len(), 2);
        assert_eq!(by("force").r#type, ParamType::Boolean);
        // Round-trips through JSON (serialize emits the map form).
        let back: PipelineIr =
            serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn required_param_with_a_default_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [{ name: x, type: string, required: true, default: d }] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("also declares a `default`")), "{errs:?}");
    }

    #[test]
    fn optional_param_without_a_default_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [{ name: x, type: string, required: false }] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("must declare a `default`")), "{errs:?}");
    }

    #[test]
    fn choice_without_options_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [{ name: x, type: choice }] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("non-empty `options`")), "{errs:?}");
    }

    #[test]
    fn duplicate_param_name_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [region, region] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("duplicate parameter")), "{errs:?}");
    }

    #[test]
    fn invoke_with_coerces_supplied_values_to_declared_types() {
        // A library declaring a number input; the caller supplies a string that
        // coerces, and it reaches inlined steps stringified (ADR-0043 typed path).
        let lib = r#"
            interface:
              inputs:
                - { name: replicas, type: number }
            steps: [{ id: run, image: busybox }]
        "#;
        let ir = compile_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/x.yaml
                with: { replicas: "3" }
            "#,
            &libs(&[(".scarab/lib/x.yaml", lib)]),
        );
        let run = ir.steps.iter().find(|s| s.id == "deploy/run").unwrap();
        assert!(run.env.contains(&("SCARAB_PARAM_REPLICAS".to_string(), "3".to_string())));
    }

    #[test]
    fn invoke_with_rejects_a_value_that_does_not_coerce() {
        let lib = r#"
            interface:
              inputs:
                - { name: replicas, type: number }
            steps: [{ id: run, image: busybox }]
        "#;
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: .scarab/lib/x.yaml
                with: { replicas: "not-a-number" }
            "#,
            &libs(&[(".scarab/lib/x.yaml", lib)]),
        );
        assert!(errs.iter().any(|e| e.contains("is not a number")), "{errs:?}");
    }

    #[test]
    fn compile_yaml_still_works_without_libraries() {
        // The no-library convenience entrypoint compiles an invoke-free pipeline.
        let ir = compile("steps: [{ id: a, image: busybox }]");
        assert_eq!(ir.steps.len(), 1);
    }

    #[test]
    fn malformed_yaml_is_a_parse_error() {
        match compile_yaml("steps: [ this is : not valid") {
            Err(PipelineError::Parse(_)) => {}
            other => panic!("expected parse error, got {other:?}"),
        }
    }
}
