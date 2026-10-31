//! # scarab-pipeline — pipeline authoring & compilation
//!
//! Pure domain crate (serde / serde_json / serde_yaml / cel /
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
pub mod diagnostic;
pub mod event;
pub mod oci;
pub mod params;
pub mod render;
pub mod scope;

pub use render::{preview_image, render, render_inputs, Facts, Rendered, Results, Templates};
pub use scope::{Binding, BoundValue, Guard, OutputSource, Scope, ScopeInputs};

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

/// The IR version of a Pipeline with a Step that has a [`Scope`] (ADR-0076):
/// a Step inlined from a library, whose `inputs` are its own bindings and not
/// the Run's launch parameters. Recorded on the Run (`runs.ir_version`).
pub const IR_VERSION_SCOPED: u32 = 3;

/// The highest IR version this build understands. A Run stored with a higher
/// one was compiled by a newer server, whose Steps may render in a way this
/// one does not know: the engine refuses to launch them
/// (`scarab_engine::render::render_launch_spec`), and the compiler refuses a
/// Pipeline that declares one.
pub const IR_VERSION_MAX: u32 = IR_VERSION_SCOPED;

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
#[serde(deny_unknown_fields)]
pub struct PipelineIr {
    /// Schema version of the IR, for forward/backward compatibility.
    #[serde(default = "default_ir_version")]
    pub ir_version: u32,
    /// `kind: library` marks a file as a library (ADR-0076 §2): it is only
    /// ever inlined by an `invoke:`, has no `on:` triggers, and cannot run as
    /// a Pipeline. Absent means a Pipeline. A compiled IR never carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PipelineKind>,
    /// Optional human display name (`name:`) — what the UI shows for the run's
    /// pipeline. Absent means the caller falls back to the `.scarab/<file>` bare
    /// name (so `ci.yaml` reads as `ci`). Overriding it lets a workflow present a
    /// friendlier label without renaming the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// The Environment that governs this pipeline's runs, if any (ADR-0024,
    /// 0032, 0074 §2): its runs are checked against the Environment's protection
    /// rules at admission, get its scoped secrets, and opt out of newest-wins
    /// auto-cancel. Naming an Environment does not mark a run as a deploy; see
    /// [`PipelineIr::deploy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// `deploy: true` marks this pipeline's runs as **deploy Runs** (ADR-0074
    /// §2) — what the Runs list's deploy filter selects. Independent of
    /// `environment:`: an Environment-governed run may be a smoke test or a
    /// backup, and only the author knows which runs deploy. A Step can carry
    /// the mark instead ([`StepSpec::deploy`]); see [`PipelineIr::is_deploy`].
    /// Besides the filter, it changes one thing: a deploy run opts out of
    /// newest-wins auto-cancel, as an Environment-governed run does, so a newer
    /// push never cancels a deploy half-way. No admission or secrets.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deploy: bool,
    /// The operator [`RetentionProfile`] this pipeline's runs age under
    /// (ADR-0065 s2) — a NAME only, resolved against the operator registry at
    /// sweep time (so an operator retune applies retroactively, which is the
    /// point). Absent = the registry's `default` profile, else the flat env
    /// TTLs. Rides into `runs.ir` untouched; the author never defines values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_profile: Option<String>,
    /// The reuse **interface** of a Library pipeline (ADR-0038): the parameter
    /// names it requires and the output names it exposes to an `invoke:` caller.
    /// Only meaningful when this pipeline is invoked (it is authoring metadata for
    /// a `.scarab/lib/**` module); irrelevant on a top-level triggered pipeline.
    /// Distinct from the per-step workspace `inputs:`/`outputs:` (ADR-0007, 0035).
    #[serde(default, skip_serializing_if = "Interface::is_empty")]
    pub interface: Interface,
    /// Opt-in run budget in seconds (ADR-0047): the run fails once its
    /// **active** time (gate-suspended time excluded) exceeds this. No default
    /// — a run suspended for weeks on a gate is the core feature, not a hang; forward
    /// progress rests on step timeouts and gate expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
    /// Pipeline-level **shared services** (ADR-0058): Run-scoped standalone
    /// service instances a Step opts into with [`uses:`](StepSpec::uses). Unlike
    /// a per-Step [sidecar](StepSpec::services), a shared service is a standalone
    /// Pod with a cluster DNS name (`<name>:<port>`), born eagerly at Run start
    /// and torn down at the Run/Take terminal (namespace-per-run teardown, not a
    /// refcount) — a fresh instance per Take. It is **not** a `needs`-able DAG
    /// node and is explicitly *unfenced* external state (the double-effect
    /// contract is the author's, ADR-0021). Empty = no shared services.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<SharedServiceSpec>,
    pub steps: Vec<StepSpec>,
}

impl PipelineIr {
    /// Is a Run of this pipeline a **deploy Run** (ADR-0074 §2)? Yes when the
    /// pipeline is marked `deploy: true` or any of its Steps is. Static: read
    /// from the IR when the Run is created.
    pub fn is_deploy(&self) -> bool {
        self.deploy || self.steps.iter().any(|s| s.deploy)
    }

    /// The ids of the Steps marked `deploy: true`, in IR order — empty when
    /// only the pipeline is marked.
    pub fn deploy_steps(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter(|s| s.deploy)
            .map(|s| s.id.clone())
            .collect()
    }
}

/// The `kind:` of an authored file (ADR-0076 §2). `library` is the only value;
/// a file without `kind:` is a Pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineKind {
    /// A file only an `invoke:` can use.
    Library,
}

/// A pipeline's reuse / launch interface (ADR-0038, ADR-0043) — its explicit
/// contract with an `invoke:` caller *and* with a launch (`POST /v1/runs`,
/// manual dispatch). `inputs` are the **typed launch parameters** the caller
/// supplies (via `with:` for an invoke, or a launch `params` map); `outputs` are
/// the names the pipeline exposes, each of which must be one of its step ids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<ParamSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputSpec>,
}

impl Interface {
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.outputs.is_empty()
    }

    /// The declared output names, in order.
    pub fn output_names(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.name.clone()).collect()
    }
}

/// One output an [`Interface`] declares (ADR-0076 §7). A library names one of
/// its Step ids, whose results a caller reads; a plugin gate names a value
/// its plugin returns when it releases the gate, of the declared `type`. A
/// bare string is `{ name, type: string }`, and is written back as one, so a
/// library's interface reads as it always did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpec {
    pub name: String,
    /// The type a plugin gate's value must have: `string`, `boolean` or
    /// `number`. Only a plugin gate's outputs are typed; a library output
    /// is a Step's results and is always `string`.
    pub r#type: ParamType,
    pub description: Option<String>,
}

impl OutputSpec {
    /// A `string` output named `name`.
    pub fn named(name: impl Into<String>) -> Self {
        OutputSpec {
            name: name.into(),
            r#type: ParamType::String,
            description: None,
        }
    }
}

impl Serialize for OutputSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if self.r#type == ParamType::String && self.description.is_none() {
            return serializer.serialize_str(&self.name);
        }
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", &self.name)?;
        map.serialize_entry("type", &self.r#type)?;
        if let Some(d) = &self.description {
            map.serialize_entry("description", d)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for OutputSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            name: String,
            #[serde(rename = "type", default)]
            r#type: ParamType,
            #[serde(default)]
            description: Option<String>,
        }
        /// Dispatched on the shape, like [`ParamSpec`], so a misspelled key
        /// is named in the error.
        struct FormVisitor;
        impl<'de> serde::de::Visitor<'de> for FormVisitor {
            type Value = OutputSpec;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an output name, or an output map with a `name`")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<OutputSpec, E> {
                Ok(OutputSpec::named(v))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<OutputSpec, A::Error> {
                let f = Full::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(OutputSpec {
                    name: f.name,
                    r#type: f.r#type,
                    description: f.description,
                })
            }
        }
        deserializer.deserialize_any(FormVisitor)
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        #[serde(deny_unknown_fields)]
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

        /// Dispatched on the input's SHAPE rather than by `#[serde(untagged)]`,
        /// which is what the bare-string/map choice actually is. Untagged tries
        /// both arms and, on failure, reports only "data did not match any
        /// variant" — swallowing the `deny_unknown_fields` message that names
        /// the misspelled key, which is the whole point of denying. A string is
        /// a string and a map is a map; there is nothing to guess.
        struct FormVisitor;
        impl<'de> serde::de::Visitor<'de> for FormVisitor {
            type Value = ParamSpec;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a parameter name, or a parameter map with a `name`")
            }
            /// Backward-compat with ADR-0038's bare-name inputs.
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ParamSpec, E> {
                Ok(ParamSpec {
                    name: v.to_string(),
                    r#type: ParamType::String,
                    required: true,
                    default: None,
                    options: None,
                    validate: None,
                    description: None,
                })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<ParamSpec, A::Error> {
                let f = Full::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ParamSpec {
                    name: f.name,
                    r#type: f.r#type,
                    required: f.required,
                    default: f.default,
                    options: f.options,
                    validate: f.validate,
                    description: f.description,
                })
            }
        }
        deserializer.deserialize_any(FormVisitor)
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
#[serde(deny_unknown_fields)]
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
/// (approval), `timer` (wait a duration), `external` (release via API/webhook),
/// and gates can fail with a reason (ADR-0070 §9). `deployment` was a fourth
/// kind until ADR-0074 removed the Deployment record; the check refuses it
/// with [`RETIRED_DEPLOYMENTS`].
///
/// `plugin` (ADR-0076 §7) is released by a plugin installation and comes only
/// from `invoke: plugin:<installation>/<gate>`.
const GATE_KINDS: [&str; 4] = ["manual", "timer", "external", "plugin"];

/// The Pipeline check's answer to a `clone:` Step, removed by ADR-0076 §9:
/// it names the library that replaces it.
pub const RETIRED_CLONE: &str = "the `clone` Step kind was removed (ADR-0076 §9); check out \
    the repository with `invoke: lib:thulasi-ram/clone`, and set `depth`, `submodules` and \
    `lfs` under `with:`";

/// The Pipeline check's answer to `deployment_requests` and `gate: deployment`,
/// both removed by ADR-0074: it names what replaces them.
pub const RETIRED_DEPLOYMENTS: &str = "deployments are no longer recorded by Scarab \
    (ADR-0074); mark the step `deploy: true` and wait for your CD tool in a step (for \
    example `argocd app wait`)";

/// The env names Scarab itself sets in a Step's Pod or process: the
/// executors' fence, results, artifacts, OIDC and workspace variables, and
/// one `SCARAB_PARAM_<NAME>` per input (ADR-0008). A Step's own `env:`
/// entry under one of these names would be overwritten or would overwrite
/// Scarab's, so a templated value may not use one on a `forge_token` Step.
/// `scarab-executor-k8s` checks that every `SCARAB_*` name it sets is here.
pub const SCARAB_SET_ENV: [&str; 9] = [
    "SCARAB_RUN",
    "SCARAB_STEP",
    "SCARAB_ATTEMPT",
    "SCARAB_RESULTS",
    "SCARAB_RESULTS_URL",
    "SCARAB_RESULTS_TOKEN",
    "SCARAB_ARTIFACTS",
    "SCARAB_OIDC_TOKEN_FILE",
    "SCARAB_CACHE_ROOTS",
];

/// Prefixes of env names Scarab sets: `SCARAB_PARAM_<NAME>` per input, and
/// the workspace fetcher's `SCARAB_WORKSPACE_*` and `SCARAB_SNAPSHOT_*`.
pub const SCARAB_SET_ENV_PREFIXES: [&str; 3] =
    ["SCARAB_PARAM_", "SCARAB_WORKSPACE_", "SCARAB_SNAPSHOT_"];

/// Whether Scarab sets the env name `name` itself ([`SCARAB_SET_ENV`],
/// [`SCARAB_SET_ENV_PREFIXES`]).
pub fn scarab_sets_env(name: &str) -> bool {
    SCARAB_SET_ENV.contains(&name) || SCARAB_SET_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whether `name` is one a `forge_token` Step may give a templated value:
/// `SCARAB_` followed by upper-case letters, digits and `_`. No program
/// other than the Step's own reads a name in that space, so the value
/// stays data; `PATH`, `LD_PRELOAD`, `GIT_SSH_COMMAND` and the like make
/// their value code.
fn templated_env_name_ok(name: &str) -> bool {
    name.strip_prefix("SCARAB_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    pub id: String,
    /// OCI image the step runs in. Empty (and defaulted) for a [`gate`](StepSpec::gate)
    /// step, which launches nothing; required for every executed step (enforced
    /// by [`validate`]).
    #[serde(default)]
    pub image: String,
    /// If set, this is a **gate** step (ADR-0008's `kind: gate`): an image-less
    /// durable suspend point of this kind (`manual`/`timer`/`external`).
    /// It launches no unit; when its `needs` are satisfied it
    /// waits — holding only its dependents — until it resolves. The engine
    /// wiring is `Db::set_step_gate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Removed by ADR-0074 (a DeploymentGate's reference). Kept as raw data
    /// only so the Pipeline check can refuse it with [`RETIRED_DEPLOYMENTS`]
    /// instead of an unknown-field error, and so an IR stored before then
    /// still decodes. Nothing reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<serde_json::Value>,
    /// Removed by ADR-0076 §9: the `clone` Step kind, replaced by `invoke:
    /// lib:thulasi-ram/clone`. Kept as raw data only so the Pipeline check
    /// can refuse it with [`RETIRED_CLONE`] instead of an unknown-field
    /// error. Nothing reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone: Option<serde_json::Value>,
    /// The forge credential this Step asks for (ADR-0076 §9). Run creation
    /// refuses the Run when the Step may not have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_token: Option<ForgeToken>,
    /// If set, this is a **build** step (ADR-0018): a first-class rootless
    /// BuildKit image build. The engine runs the blessed BuildKit image
    /// (never the author's); the context/dockerfile are workspace-relative,
    /// so a build step normally `needs` the checkout. Image-less like
    /// `gate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildSpec>,
    /// If set, this is an **invoke** step (ADR-0038, ADR-0076 §1): the library
    /// it inlines *at compile time*, named with a prefix: `file:<path>` (a
    /// repo-relative library file, read at the caller's ref; no absolute
    /// paths, no `..`, no remote forms), `lib:<publisher>/<name>` (a library
    /// installed in the org) or `plugin:<installation>/<gate>` (not available
    /// yet). The step launches nothing itself; [`compile_with_libraries`]
    /// flattens the library's steps into the caller's DAG, id-namespaced by
    /// this step's id (`deploy/build`), with `needs` rewritten across the
    /// seam. After compile this field is always `None` (like `matrix`).
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
    /// For a `timer` gate, how long (seconds) to wait — measured from when its
    /// `needs` finished — before the gate auto-releases (ADR-0008). Required
    /// for `timer`; forbidden on any other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_after: Option<u32>,
    /// Opt-in gate **expiry** (ADR-0047), in seconds: fail the gate (and hence
    /// the run) if it is still unapproved this long after its `needs`
    /// finished (each gate keeps its own clock, whether or not the run is
    /// suspended).
    /// Distinct from [`gate_after`](StepSpec::gate_after) (a timer's
    /// auto-release). Default = indefinite — gates may wait forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_expires_after: Option<u32>,
    /// Artifact publication globs (ADR-0052): which files under
    /// `/scarab/artifacts/` this step publishes as artifacts of record.
    /// Default (empty) = everything the step wrote there. `*` matches within
    /// a path segment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// `deploy: true` marks this Step as the one that deploys (ADR-0074 §2):
    /// its Run is a deploy Run, and the Run page marks the Step. Not allowed
    /// on a gate step, which runs nothing. Same effect as
    /// [`PipelineIr::deploy`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub deploy: bool,
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
    /// Keyed directory Cache (ADR-0065 s1): workspace-relative directories
    /// restored at Step start and saved at Step end, keyed by the blob hashes
    /// of declared key files (typically a lockfile). Best-effort and
    /// warm-only: a miss is slower and never wrong, and a cache dir never
    /// flows downstream via the workspace (it is pruned from the published
    /// root and rides the cache instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheSpec>,
    /// Per-step execution deadline in **seconds** (ADR-0047). Absent = the
    /// global default (1h). Enforced primarily by the backend (kubelet
    /// `activeDeadlineSeconds` / the local kill-timer — it survives
    /// control-plane downtime), with an engine-side backstop. Exceeding it is
    /// a `Timeout` failure: terminal unless the step opted into `retry:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    /// Opt-in retry policy (ADR-0020/0047): `retry: { on: failure, max: N }`.
    ///
    /// ⚠ **At-least-once:** retry re-runs the whole step at-least-once; enable
    /// only if the step is idempotent or fenced against a cooperating sink
    /// (ADR-0021). Non-cooperating side effects (a bare POST, an email) *will*
    /// double-fire on retry — no fence token can prevent that.
    ///
    /// Setting this is the author's assertion "this step is safe to re-run":
    /// it gates retry of *post-start* failures (post-start infra, step
    /// verdict, timeout). Never-started infra failures (image pull,
    /// unschedulable) auto-retry independently of this field — no side effect
    /// is possible when the process never ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
    /// Authoring-only fan-out modifier; `None` on every compiled step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<Matrix>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    /// Placement targeting (ADR-0055): the names of the [`PlacementProfile`]s this
    /// step runs on, whose admin-defined k8s overlays are merged (in listed order)
    /// onto the step's Pod. Empty = the operator's `default` profile. The author
    /// **names profiles**, never raw k8s — so a pipeline carries no cluster
    /// topology (a bounded name-only reference) and no Kubernetes primitives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placement_profiles: Vec<String>,
    /// Requested compute resources (ADR-0055): exact `cpu`/`memory`, applied to the
    /// step container's requests/limits. Deliberately **not** named `size` tiers —
    /// state the specifics.
    #[serde(default)]
    pub resources: Resources,
    /// Governed placement escape hatch (ADR-0055): a raw pod-spec fragment
    /// strategic-merged **last** onto the generated Pod. Carries **no authority** —
    /// like a governed [`StepSecurity`] grant, it is admitted only if the run's
    /// target Environment permits raw overlays, else the run is rejected
    /// fail-closed. The `k8s_` prefix marks the backend-coupling (a pipeline using
    /// it will not run on the local/dev executor). For the rare *dynamic per-job*
    /// k8s need; static placement belongs in a [`PlacementProfile`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k8s_overlay: Option<serde_json::Value>,
    /// The concrete matrix coordinate this instance was expanded from (empty for
    /// steps authored without a matrix). Consumed later by CEL interpolation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub matrix_values: BTreeMap<String, String>,
    /// Sidecar services (ADR-0058): throwaway backing containers co-located in
    /// this Step's Pod, reachable at `localhost:<port>`. Each is a
    /// [`ServiceSpec`]; none is a `needs`-able DAG node. A matrixed Step's
    /// sidecars multiply per instance (each instance's Pod gets its own).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceSpec>,
    /// Shared-service opt-in (ADR-0058): the names of pipeline-level
    /// [`SharedServiceSpec`]s this Step reaches over the network. Each name must
    /// resolve to a declared pipeline service (enforced by [`validate`]). Opting
    /// in gets this Step three things: the NetworkPolicy hole to the service Pod,
    /// the cluster DNS path (`<name>:<port>`), and a scheduler **readiness gate**
    /// that holds the Step until the service's `ready:` probe passes. A Step with
    /// no `uses:` never waits on any shared service. Not a `needs` edge — a
    /// service is infrastructure *for* a Step, not a DAG node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// The names of the results this Step declares it writes (ADR-0041 results,
    /// declared per ADR-0070 §7): each is `/scarab/results/<name>.json`. A
    /// declaration, not a filter — a Step that declares nothing keeps the open
    /// ADR-0041 map. What a declaration buys is a compile-time check: every
    /// `${{ outputs.<this step>.<name> }}` read elsewhere in the Pipeline must
    /// name a result declared here, so a typo is rejected before any Run
    /// instead of failing the reading Step at launch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<String>,
    /// Removed by ADR-0074 (the Step's DeploymentRequests). Kept as raw data
    /// only so the Pipeline check can refuse it with [`RETIRED_DEPLOYMENTS`]
    /// and an IR stored before then still decodes. Nothing reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_requests: Option<serde_json::Value>,
    /// The invoke chain this Step (or gate) was inlined through, outermost
    /// first (ADR-0075 §2, ADR-0076 §1). Empty for a Step written in the
    /// Pipeline file itself. Written only by the compiler
    /// ([`compile_yaml_with_libs`]): an authored `origin` is a compile error,
    /// so a Pipeline cannot claim to come from somewhere it does not. The
    /// `run.create` PolicyCheckpoint reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin: Vec<OriginEntry>,
    /// What this Step's `${{ … }}` templates read (ADR-0076): set by the
    /// compiler on every Step and gate inlined from a library (its own
    /// inputs, a view of the library's Steps, its own coordinate), and on a
    /// Step in the Pipeline file that reads an invoke's exposed output.
    /// `None` renders `inputs` as the Run's launch parameters and `outputs`
    /// as the results of its direct needs. Written only by the compiler, like
    /// [`origin`](Self::origin): an authored `scope` is a compile error. It
    /// is read back from a stored IR, which stays self-describing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    /// For a Step or gate inlined from a library, the `when:`s that decide
    /// whether it runs, each with the scope it was written in (ADR-0076).
    /// Its own [`when`](Self::when) is kept as written and is one of them.
    /// Written only by the compiler, like [`scope`](Self::scope).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<Guard>,
    /// For a `gate: plugin` node, the plugin gate it waits on (ADR-0076 §7):
    /// the installation that releases it, the gate's name in that plugin's
    /// manifest, and the outputs the plugin must return. Written only by the
    /// compiler from `invoke: plugin:<installation>/<gate>`, like
    /// [`origin`](Self::origin): an authored one is a compile error. The
    /// gate's inputs are its [`scope`](Self::scope)'s bindings, rendered when
    /// the gate is reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_gate: Option<PluginGateSpec>,
}

/// What a `gate: plugin` node waits on (ADR-0076 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGateSpec {
    /// The installation, in the Run's org, whose plugin releases the gate.
    pub installation: String,
    /// The gate's name in the plugin's manifest.
    pub gate: String,
    /// What the plugin returns on a release, each value of its type. A
    /// release must hold exactly these ([`check_gate_outputs`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputSpec>,
}

impl PluginGateSpec {
    /// The Principal that may release the gate: `plugin:<installation>`.
    pub fn releaser(&self) -> String {
        format!("plugin:{}", self.installation)
    }
}

/// A gate a plugin installed in the caller's org declares (ADR-0076 §7), as
/// the caller hands it to the compiler: its interface, and the approved
/// manifest it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginGateDef {
    /// `inputs` are checked against the caller's `with:`; `outputs` are what
    /// the plugin returns.
    pub interface: Interface,
    /// SHA-256 of the installation's approved manifest, recorded as the
    /// gate's `origin` content hash.
    pub manifest_sha256: String,
}

/// The longest `gate_expires_after` a plugin gate may set: 30 days, in
/// seconds. A plugin gate must end (ADR-0076 §7); without a cap, the largest
/// value (about 136 years) would make its expiry optional in practice.
pub const MAX_PLUGIN_GATE_EXPIRES_AFTER_SECS: u32 = 30 * 24 * 60 * 60;

/// The most bytes of output one plugin gate release may carry, as JSON: the
/// cap on a Step's results (ADR-0041).
pub const MAX_GATE_OUTPUT_BYTES: usize = 64 * 1024;

/// Check a plugin's outputs against what its gate declares (ADR-0076 §7):
/// exactly the declared names, each of its declared type (`string`,
/// `boolean` or `number`; nothing is converted), at most
/// [`MAX_GATE_OUTPUT_BYTES`] in all. What is returned is stored as the
/// gate's results; a downstream Step reads it as `${{ outputs.<gate>.<name>
/// }}`, a value from outside the Pipeline ([`ValueSource::Outside`]).
pub fn check_gate_outputs(
    declared: &[OutputSpec],
    got: &serde_json::Map<String, serde_json::Value>,
) -> Result<BTreeMap<String, serde_json::Value>, String> {
    let size = serde_json::to_vec(got).map(|v| v.len()).unwrap_or(usize::MAX);
    if size > MAX_GATE_OUTPUT_BYTES {
        return Err(format!(
            "the outputs are {size} bytes; at most {MAX_GATE_OUTPUT_BYTES} are kept"
        ));
    }
    // Postgres `jsonb` cannot hold U+0000; refuse it here as a bad output
    // rather than fail the write.
    if let Some(k) = got.keys().find(|k| k.contains('\0')) {
        return Err(format!(
            "output name `{}` contains a NUL character",
            k.escape_debug()
        ));
    }
    let names = || {
        declared
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(extra) = got.keys().find(|k| !declared.iter().any(|o| &o.name == *k)) {
        return Err(format!(
            "output `{}` is not one the gate declares (it declares: [{}])",
            extra.escape_debug(),
            names()
        ));
    }
    let mut out = BTreeMap::new();
    for o in declared {
        let Some(v) = got.get(&o.name) else {
            return Err(format!(
                "output `{}` is missing; a release returns every output the gate declares \
                 ([{}])",
                o.name,
                names()
            ));
        };
        let ok = match o.r#type {
            ParamType::String => v.is_string(),
            ParamType::Boolean => v.is_boolean(),
            ParamType::Number => v.is_number(),
            ParamType::Choice => false,
        };
        if !ok {
            return Err(format!(
                "output `{}` must be a {}, got `{}`",
                o.name,
                param_type_name(o.r#type),
                json_kind(v)
            ));
        }
        if v.as_str().is_some_and(|s| s.contains('\0')) {
            return Err(format!("output `{}` contains a NUL character", o.name));
        }
        out.insert(o.name.clone(), v.clone());
    }
    Ok(out)
}

/// The wire name of a [`ParamType`].
fn param_type_name(t: ParamType) -> &'static str {
    match t {
        ParamType::String => "string",
        ParamType::Boolean => "boolean",
        ParamType::Number => "number",
        ParamType::Choice => "choice",
    }
}

/// The JSON kind of `v`, for a message.
fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Check a plugin gate's declared [`Interface`] (ADR-0076 §7): its inputs as
/// any launch parameter's ([`params::validate_param_specs`]); its outputs
/// named as a result is (`[A-Za-z0-9_-]`, 1-64), distinct, and of a type
/// a release can be checked against (`string`, `boolean`, `number`).
/// Each problem is pushed onto `diagnostics`, prefixed with `what`.
pub fn validate_gate_interface(iface: &Interface, what: &str, diagnostics: &mut Vec<String>) {
    params::validate_param_specs(&iface.inputs, what, diagnostics);
    for p in &iface.inputs {
        if p.default
            .as_ref()
            .is_some_and(|d| d.to_string().contains("${{"))
        {
            diagnostics.push(format!(
                "{what}: input `{}` has a default holding `${{{{ … }}}}`; a default is a plain \
                 value",
                p.name
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for o in &iface.outputs {
        let name_ok = !o.name.is_empty()
            && o.name.len() <= 64
            && o
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !name_ok {
            diagnostics.push(format!(
                "{what}: output `{}` must be 1-64 of `A-Z`, `a-z`, `0-9`, `_`, `-`",
                o.name.escape_debug()
            ));
        }
        if !seen.insert(o.name.as_str()) {
            diagnostics.push(format!("{what}: output `{}` is declared twice", o.name));
        }
        if o.r#type == ParamType::Choice {
            diagnostics.push(format!(
                "{what}: output `{}` is a `choice`; an output is a `string`, `boolean` or \
                 `number`",
                o.name
            ));
        }
    }
}

/// The kind of one link in an invoke chain (ADR-0076). The compiler writes
/// [`File`](OriginKind::File) for `invoke: file:` and
/// [`Library`](OriginKind::Library) for `invoke: lib:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    /// A library file in the same repository (`.scarab/lib/…`).
    File,
    /// An installed library (ADR-0076).
    Library,
    /// A plugin's gate (ADR-0076).
    Plugin,
}

/// One link of a compiled Step's [`origin`](StepSpec::origin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginEntry {
    pub kind: OriginKind,
    /// For a `file` link, the repo-relative library path; for a `library`
    /// link, the installed `<publisher>/<name>`.
    pub name: String,
    /// Where an installed library comes from (its git source and commit);
    /// `None` for a `file` link. Policies match a library by this, since the
    /// name is chosen at install (ADR-0076 §1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// SHA-256 of the library source, lowercase hex.
    pub content_hash: String,
}

/// SHA-256 of `bytes`, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

impl StepSpec {
    /// Is this an image-less gate step (a durable suspend point)?
    pub fn is_gate(&self) -> bool {
        self.gate.is_some()
    }

    /// Is this a build step (ADR-0018 rootless image build)?
    pub fn is_build(&self) -> bool {
        self.build.is_some()
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
#[serde(deny_unknown_fields)]
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

/// A **sidecar service** (ADR-0058): a throwaway backing container co-located
/// inside the declaring Step's Pod, reachable at `localhost:<port>`, alive only
/// for that one Step's execution. It reuses ADR-0042's native-sidecar machinery
/// and is **fenced by inheritance** — it shares the Step's Attempt identity and
/// teardown, dies with the Pod, and is re-created fresh on every Attempt. A
/// service is **not** a `needs`-able DAG node: it is infrastructure *for* a Step,
/// not a Step (no id, no exit code, no Attempt of its own). Its image is
/// author-supplied and no more trusted than a step image — it runs under the
/// ADR-0039 restricted baseline; the `run-as-root` self-service grant applies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    /// The OCI image (e.g. `postgres:16`). Serde-defaulted so an absent image
    /// surfaces as a validation diagnostic, not a parse error (cf. `BuildSpec`).
    #[serde(default)]
    pub image: String,
    /// Optional command (entrypoint) override — the image default when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Optional args appended after the command/entrypoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment for the service container, authored as a map
    /// (`env: { POSTGRES_PASSWORD: test }`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Container ports the service listens on (all reachable at `localhost:<p>`).
    /// The **first** port is the default target of a `tcp` readiness probe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    /// Optional readiness probe: gates the Step's **main** container start until
    /// the service is ready (ADR-0058). Absent = a TCP-connect on the first
    /// declared [`port`](ServiceSpec::ports); if no port is declared either, the
    /// main container starts immediately (no gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<ReadyProbe>,
    /// Pin the service image's built-in **non-root** uid (e.g. `999` for the
    /// official `postgres` image) so the service starts under the restricted
    /// baseline without any grant. Applied as the container
    /// `runAsUser`/`runAsGroup` AND the Pod-level `fsGroup`, so the service's
    /// `emptyDir` data volume is group-writable — the standard k8s non-root
    /// pattern (ADR-0058 governance). Ignored when [`run_as_root`](Self::run_as_root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as_user: Option<u32>,
    /// Self-service escape hatch (mirrors [`StepSecurity::run_as_root`]): run the
    /// service as uid 0. Sandbox-bound — caps-dropped, unprivileged,
    /// seccomp-confined — so root here does not escape. Default `false`: the
    /// restricted non-root baseline. Deliberately *not* the default path.
    #[serde(default, skip_serializing_if = "is_false")]
    pub run_as_root: bool,
}

/// A sidecar service's readiness probe (ADR-0058), authored as a one-key map:
/// `tcp` is the default form (`ready: { tcp: 5432 }`); `exec`
/// (`ready: { exec: [pg_isready] }`) and `http` (`ready: { http: { port, path } }`)
/// are also allowed. Exactly one form must be set (enforced by [`validate`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyProbe {
    /// Ready when a TCP connection to this port succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<u16>,
    /// Ready when this command, run inside the service container, exits 0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<String>,
    /// Ready on a 2xx/3xx from an HTTP GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpReady>,
}

impl ReadyProbe {
    /// How many probe forms this authored probe sets (must be exactly 1).
    fn forms_set(&self) -> usize {
        self.tcp.is_some() as usize
            + (!self.exec.is_empty()) as usize
            + self.http.is_some() as usize
    }
}

/// The `http` form of a [`ReadyProbe`] (ADR-0058).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpReady {
    /// Port to GET.
    pub port: u16,
    /// Request path (default `/`).
    #[serde(default = "default_http_path")]
    pub path: String,
}

fn default_http_path() -> String {
    "/".to_string()
}

/// A **shared service** (ADR-0058): a pipeline-level, Run-scoped standalone
/// service instance that Steps opt into via [`StepSpec::uses`]. It reuses the
/// slice-1 [`ServiceSpec`] shape (image/command/args/env/ports/`ready:`) and adds
/// the `name` that becomes both the opt-in key and the cluster DNS hostname
/// (`<name>:<port>`). Distinct from a per-Step sidecar: a shared service is a
/// standalone Pod + k8s Service reachable across Pods (scoped by NetworkPolicy to
/// the opt-in Pods only), born eagerly at Run start and torn down at Run/Take
/// terminal — a fresh instance per Take, never shared across Takes. It is **not**
/// a `needs`-able DAG node and is explicitly *unfenced* external state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SharedServiceSpec {
    /// The service name — the opt-in key a Step names in `uses:` and the cluster
    /// DNS hostname (`<name>:<port>`). Must be non-empty, unique across the
    /// pipeline's services, and DNS-label-safe (enforced by [`validate`]).
    #[serde(default)]
    pub name: String,
    /// The reused slice-1 service shape (image/command/args/env/ports/`ready:`).
    #[serde(flatten)]
    pub spec: ServiceSpec,
}

/// Hand-written because `flatten` and `deny_unknown_fields` are mutually
/// exclusive in serde, in a way that fails **open**: serde's flatten machinery
/// forwards only the keys the flattened struct declares, so a stray key on a
/// shared service never reaches [`ServiceSpec`]'s `deny_unknown_fields` and is
/// silently dropped. That is the exact silent facade the attribute exists to
/// stop, and this is the one level of the schema where the derive could not
/// deliver it.
///
/// So `name` is lifted off first and the remainder handed to [`ServiceSpec`]
/// itself, which keeps ONE authority for what a service may declare: a field
/// added there is accepted here the same day, with no second list to drift
/// (`a_shared_service_accepts_every_field_a_sidecar_service_declares` holds
/// that).
impl<'de> Deserialize<'de> for SharedServiceSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let name = {
            let map = value
                .as_object_mut()
                .ok_or_else(|| D::Error::custom("a shared service must be a map"))?;
            match map.remove("name") {
                // Absent stays a validation diagnostic ("service has no name"),
                // not a parse error — the same choice `ServiceSpec` makes for a
                // missing `image`.
                None | Some(serde_json::Value::Null) => String::new(),
                Some(serde_json::Value::String(s)) => s,
                Some(other) => {
                    return Err(D::Error::custom(format!(
                        "shared service `name` must be a string, found `{other}`"
                    )))
                }
            }
        };
        let spec = ServiceSpec::deserialize(value).map_err(D::Error::custom)?;
        Ok(SharedServiceSpec { name, spec })
    }
}

/// The forge credential a Step asks for (ADR-0076 §9): `forge_token: read`
/// or `forge_token: write`. Core mints it when each Attempt launches and
/// mounts it as two files, `/scarab/secrets/forge-token` and
/// `/scarab/secrets/forge-username`, never as an environment variable.
/// Run creation decides whether the Step may have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeToken {
    /// Read the Run's repository. A fork pull request always gets this.
    Read,
    /// Push to the Run's repository. Only a forge that mints short-lived
    /// tokens (GitHub App mode) can give it.
    Write,
}

impl ForgeToken {
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeToken::Read => "read",
            ForgeToken::Write => "write",
        }
    }
}

/// A step's keyed directory Cache declaration (ADR-0065 s1):
/// `cache: { dirs: [node_modules], key: [package-lock.json] }`.
///
/// `dirs` are the workspace-relative directories to restore/save; `key` names
/// the files whose **blob hashes** (from the input snapshots' tree entries —
/// no content reads) key the cache, scoped by project. Both lists are
/// required and validated by [`validate`]: workspace-relative, no `..`, no
/// `.git*`, no `=`/`,` in dir names (they ride a `dir=root,…` env), no mutual
/// prefix-overlap among dirs, and no overlap with the step's `outputs:` (a
/// cached dir is scratch — publishing it too would be a self-contradictory
/// label plan). `CARGO_HOME=/workspace/.cargo` is the documented pattern for
/// tool caches that default to `$HOME`; dirs outside the workspace are
/// deliberately unsupported in v1.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSpec {
    /// Workspace-relative directories to cache.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Workspace-relative files whose blob hashes form the cache key.
    #[serde(default)]
    pub key: Vec<String>,
}

/// Configuration of a `build` step (ADR-0018): what to build and where to
/// push. Registry credentials are NEVER authored here — they are a scoped
/// `REGISTRY_AUTH` secret (ADR-0037) or derived from the forge connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    /// Workspace-relative build context directory (default `.`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// Dockerfile name within the context (default `Dockerfile`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dockerfile: String,
    /// The image reference to build (and push), e.g. `ghcr.io/acme/app:v1`.
    /// Serde-defaulted so an absent tag surfaces as a validation diagnostic,
    /// not a parse error.
    #[serde(default)]
    pub image: String,
    /// Push the built image (default false — build-only validation).
    #[serde(default, skip_serializing_if = "is_false")]
    pub push: bool,
}

/// A step's opt-in retry policy (ADR-0020 syntax, ADR-0047 semantics).
///
/// ⚠ **At-least-once warning** (surfaced verbatim to authors): *retry re-runs
/// the whole step at-least-once; enable only if the step is idempotent or
/// fenced against a cooperating sink.*
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    /// Which failures the author's assertion covers. `failure` (the default
    /// and only value today) selects every *post-start* class: post-start
    /// infra, step verdict, and timeout. Never-started infra retries
    /// automatically regardless.
    #[serde(default)]
    pub on: RetryOn,
    /// The re-run budget: up to `max` additional attempts after the first.
    /// Bounded by [`validate`] to 1..=10 — a liveness bound, not a safety one
    /// (ADR-0047): side-effect safety remains at-least-once.
    pub max: u32,
}

/// The failure selector of a [`Retry`] policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetryOn {
    /// Any post-start failure: post-start infra, step verdict, or timeout.
    #[default]
    Failure,
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
#[serde(deny_unknown_fields)]
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

/// A **PlacementProfile** (ADR-0055): an operator-owned, cluster-scoped named
/// bundle mapping a `name` → concrete Kubernetes placement (nodeSelector /
/// tolerations / runtimeClass / annotations — an *opaque overlay*). It lives in
/// Scarab operator config, **not** in a pipeline; a step references it *by name*
/// via [`StepSpec::placement_profiles`]. Kept here as the authoring-side shape so
/// the executor and config layers share one definition.
///
/// It is *where a step lands* — not an `Environment` (deploy governance) and not
/// a `Run` (a pipeline instance). One profile in the registry may be `default`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementProfile {
    /// The name a step references in `placement_profiles`.
    pub name: String,
    /// Marks the profile applied when a step names none. At most one per registry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// The concrete k8s placement this profile contributes — merged onto the Pod.
    /// An opaque JSON **Pod** fragment (`{"spec": {...}}`, not a bare pod-spec),
    /// so an admin can bake any static placement fact in without a Scarab schema
    /// change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k8s: Option<serde_json::Value>,
}

/// A **RetentionProfile** (ADR-0065 s2, git-bug 82c5775): an operator-owned,
/// cluster-scoped named bundle of per-class retention TTLs. The exact
/// [`PlacementProfile`] pattern: it lives in Scarab operator config
/// (`SCARAB_RETENTION_CONFIG_FILE`), **not** in a pipeline; a Pipeline may
/// *name* one via the top-level `retention_profile:` key but never defines
/// values — where the infrastructure is expensive, the system pays, not the author
/// (ADR-0061's governing principle).
///
/// **TTL-only** (2026-08-26 narrowing of ADR-0065's bundle): the warm space
/// budget, Cache-eligible directories and drop-and-re-derive thresholds are
/// deliberately NOT parsed — nothing consumes them yet, and an inert knob is
/// a silent lie. Each TTL is optional: an absent field falls back to the
/// operator's flat env default for that class (`SCARAB_RETENTION_*_DAYS`).
/// `pack_ttl_days` and `workspace_ttl_days` drive committed-fence expiry and
/// its reachability floor today; `log_ttl_days`/`artifact_ttl_days` are the
/// same classes the retention sweeper prunes, adopted there in a follow-up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionProfile {
    /// The name a pipeline references in `retention_profile:`.
    pub name: String,
    /// Marks the profile applied when a run names none. At most one per registry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// How long a terminal run's Depot packs are kept, in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_ttl_days: Option<u32>,
    /// How long a terminal run's logs are kept, in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_ttl_days: Option<u32>,
    /// How long a terminal run's artifacts are kept, in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ttl_days: Option<u32>,
    /// How long a terminal run's workspace CAS stays reachable, in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_ttl_days: Option<u32>,
}

/// What every operator profile registry entry answers — the shared machinery
/// ADR-0065 called for once a second profile type existed: lookup by name,
/// the at-most-one `default`, and registry validation live HERE rather than
/// being copy-pasted per profile kind.
pub trait NamedProfile {
    /// The name a pipeline/step references.
    fn profile_name(&self) -> &str;
    /// Whether this entry is the registry's default.
    fn is_default(&self) -> bool;
}

impl NamedProfile for PlacementProfile {
    fn profile_name(&self) -> &str {
        &self.name
    }
    fn is_default(&self) -> bool {
        self.default
    }
}

impl NamedProfile for RetentionProfile {
    fn profile_name(&self) -> &str {
        &self.name
    }
    fn is_default(&self) -> bool {
        self.default
    }
}

/// Look one profile up by name.
pub fn profile_named<'a, P: NamedProfile>(registry: &'a [P], name: &str) -> Option<&'a P> {
    registry.iter().find(|p| p.profile_name() == name)
}

/// The registry's `default`-flagged profile, if any.
pub fn default_profile<P: NamedProfile>(registry: &[P]) -> Option<&P> {
    registry.iter().find(|p| p.is_default())
}

/// Validate an operator profile registry: non-empty, unique names and at most
/// one `default`. `kind` names the registry in the message (a boot-failure
/// message must say WHICH gitops file to fix).
pub fn validate_profile_registry<P: NamedProfile>(
    registry: &[P],
    kind: &str,
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    let mut defaults = 0usize;
    for p in registry {
        if p.profile_name().trim().is_empty() {
            return Err(format!("{kind} registry contains an empty profile name"));
        }
        if !names.insert(p.profile_name()) {
            return Err(format!(
                "{kind} registry names `{}` more than once",
                p.profile_name()
            ));
        }
        if p.is_default() {
            defaults += 1;
        }
    }
    if defaults > 1 {
        return Err(format!(
            "{kind} registry marks {defaults} profiles as `default` — at most one"
        ));
    }
    Ok(())
}

/// The ceiling on a step's `timeout:`, in seconds — 23 hours (ticket 16a7768
/// item 3).
///
/// Not arbitrary: the workspace token a step Pod's drain authenticates with
/// expires at `min(timeout + grace, 24h)`
/// (`scarab_executor_k8s::workspace_token::expiry_for`; a compile-time assert
/// there ties the numbers). A timeout past the ceiling would let a step
/// **outlive its own drain credential**: the drain 401s, exits transient, and
/// the attempt loops without ever publishing — 24 hours into the run.
/// Validation refuses it here, where the author is still looking. The hour of
/// slack above 23h keeps the token's full post-timeout grace un-truncated.
pub const MAX_STEP_TIMEOUT_SECS: u32 = 23 * 60 * 60;

/// Requested compute resources for a step (ADR-0055). Exact `cpu`/`memory`, not
/// named `size` tiers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// An installed library (ADR-0076 §4) as the caller hands it to the compiler:
/// content already stored in Scarab, never fetched while compiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledLibrary {
    /// The library file's YAML.
    pub content: String,
    /// Where it was installed from, e.g.
    /// `github.com/acme/scarab-libs//deploy-helm.yaml@<commit>`. Recorded in
    /// each inlined Step's [`origin`](StepSpec::origin).
    pub source: String,
}

/// Everything an `invoke:` may name, pre-fetched by the caller (ADR-0038,
/// ADR-0076 §1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Libraries {
    /// `file:` targets: repo-relative path → source, read at the caller's
    /// ref. [`invoke_refs`] lists the paths to read.
    pub files: BTreeMap<String, String>,
    /// `lib:` targets: `<publisher>/<name>` → the library installed in the
    /// caller's org. A name missing here is "not installed in this org".
    pub installed: BTreeMap<String, InstalledLibrary>,
    /// `plugin:` targets: `<installation>/<gate>` → a gate a plugin
    /// installed in the caller's org declares, for an installation that may
    /// resolve gates (ADR-0076 §7). A name missing here is "not a gate of a
    /// plugin installed in this org".
    pub plugin_gates: BTreeMap<String, PluginGateDef>,
}

/// Compile authored YAML into a validated [`PipelineIr`], resolving `invoke:
/// file:` steps against a pre-fetched `{repo-relative path → source}` map and
/// with no installed libraries. A thin wrapper over
/// [`compile_with_libraries`].
pub fn compile_yaml_with_libs(
    yaml: &str,
    libs: &BTreeMap<String, String>,
) -> Result<PipelineIr, PipelineError> {
    compile_libs(
        yaml,
        LibsRef {
            files: libs,
            installed: &BTreeMap::new(),
            plugin_gates: &BTreeMap::new(),
            work: &Default::default(),
        },
    )
}

/// Compile authored YAML into a validated [`PipelineIr`], resolving each
/// `invoke:` against `libraries`: `file:` targets against
/// [`Libraries::files`], `lib:` targets against [`Libraries::installed`]
/// (ADR-0038, ADR-0076).
///
/// Compilation is **pure** (ADR-0031): the caller does the I/O (reading the
/// `file:` libraries at the caller's ref via `ForgePort`, and the org's
/// installed libraries from its store) and passes them in. This function
/// performs no I/O.
///
/// Pipeline: parse → **inline every `invoke:` step** by binding the library's
/// `${{ inputs.* }}` to the caller's `with:` and flattening its steps into the
/// DAG, id-namespaced by the invoke-step id and with `needs` rewritten across
/// the seam → statically expand every matrix into concrete steps (ADR-0023) →
/// fan each `needs` edge onto every expanded instance of its target → run full
/// [`validate`]. All problems are reported together via
/// [`PipelineError::Validation`].
pub fn compile_with_libraries(
    yaml: &str,
    libraries: &Libraries,
) -> Result<PipelineIr, PipelineError> {
    compile_libs(
        yaml,
        LibsRef {
            files: &libraries.files,
            installed: &libraries.installed,
            plugin_gates: &libraries.plugin_gates,
            work: &Default::default(),
        },
    )
}

/// A borrowed [`Libraries`], so [`compile_yaml_with_libs`] needs no copy,
/// with what one compile has parsed of them.
#[derive(Clone, Copy)]
struct LibsRef<'a> {
    files: &'a BTreeMap<String, String>,
    installed: &'a BTreeMap<String, InstalledLibrary>,
    plugin_gates: &'a BTreeMap<String, PluginGateDef>,
    work: &'a std::cell::RefCell<Work>,
}

/// The most bytes of library source one compile reads, counting each
/// library once however often it is invoked.
pub const MAX_LIBRARY_SOURCE_BYTES: usize = 16 << 20;

/// What one compile has done so far, so its work is bounded: each library
/// source it has read, by its target as written (`file:…`, `lib:…`), parsed
/// once however often it is invoked; and the matrices it has expanded.
#[derive(Default)]
struct Work {
    /// The library, its content hash, or why it cannot be used.
    full: BTreeMap<String, Result<(std::rc::Rc<PipelineIr>, String), String>>,
    /// The outputs its interface exposes, if it reads.
    exposed: BTreeMap<String, Option<Vec<String>>>,
    /// The library sources read, counted against
    /// [`MAX_LIBRARY_SOURCE_BYTES`].
    admitted: BTreeSet<String>,
    bytes: usize,
    matrix: MatrixWork,
}

impl Work {
    /// Whether the library `written` may be read: it was, or its source
    /// `src` fits under [`MAX_LIBRARY_SOURCE_BYTES`] with those that were.
    fn admit(&mut self, written: &str, src: &str) -> bool {
        if self.admitted.contains(written) {
            return true;
        }
        if self.bytes.saturating_add(src.len()) > MAX_LIBRARY_SOURCE_BYTES {
            return false;
        }
        self.bytes += src.len();
        self.admitted.insert(written.to_string());
        true
    }
}

impl LibsRef<'_> {
    /// The library `written` names, with source `src`, parsed once per
    /// compile, and its content hash; or why it cannot be used ("failed to
    /// parse: …").
    fn parse(&self, written: &str, src: &str) -> Result<(std::rc::Rc<PipelineIr>, String), String> {
        let mut parsed = self.work.borrow_mut();
        if let Some(done) = parsed.full.get(written) {
            return done.clone();
        }
        let done = if !parsed.admit(written, src) {
            Err(format!(
                "is past the limit on library source: the libraries one Pipeline invokes may \
                 hold {MAX_LIBRARY_SOURCE_BYTES} bytes (16 MiB) in all, each counted once"
            ))
        } else {
            serde_yaml::from_str::<PipelineIr>(src)
                .map(|ir| (std::rc::Rc::new(ir), sha256_hex(src.as_bytes())))
                .map_err(|e| format!("failed to parse: {e}"))
        };
        parsed.full.insert(written.to_string(), done.clone());
        done
    }

    /// The outputs the interface of the library `written` exposes (a
    /// shallow read of its `interface:` only), read once per compile.
    fn exposed(&self, written: &str, src: &str) -> Option<Vec<String>> {
        #[derive(Deserialize)]
        struct Shallow {
            #[serde(default)]
            interface: Interface,
        }
        let mut parsed = self.work.borrow_mut();
        if let Some(done) = parsed.exposed.get(written) {
            return done.clone();
        }
        let done = parsed
            .admit(written, src)
            .then(|| serde_yaml::from_str::<Shallow>(src).ok())
            .flatten()
            .map(|l| l.interface.output_names());
        parsed.exposed.insert(written.to_string(), done.clone());
        done
    }
}

fn compile_libs(yaml: &str, libs: LibsRef<'_>) -> Result<PipelineIr, PipelineError> {
    if yaml.len() > MAX_SOURCE_BYTES {
        return Err(PipelineError::Validation(vec![source_too_large(
            "the pipeline file",
            yaml.len(),
        )]));
    }
    let authored: PipelineIr =
        serde_yaml::from_str(yaml).map_err(|e| PipelineError::Parse(e.to_string()))?;

    // A library is only ever inlined (ADR-0076 §2).
    if authored.kind == Some(PipelineKind::Library) {
        return Err(PipelineError::Validation(vec![
            "this file is a library (`kind: library`) and cannot run as a Pipeline; use it \
             from a Pipeline with `invoke: file:<its path>`"
                .to_string(),
        ]));
    }

    // Inline `invoke:` steps first (ADR-0038): the result is a flat step list
    // with no invoke steps, over which matrix expansion and validation run
    // unchanged. A library's own matrices thus expand normally.
    let authored_steps =
        inline_invokes(&authored.steps, libs).map_err(PipelineError::Validation)?;

    let mut diagnostics = Vec::new();
    let mut expanded: Vec<StepSpec> = Vec::new();
    // authored step id -> the concrete instance ids it expanded into.
    let mut instances: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // The stored size of `expanded` so far: each instance is about the size
    // of the Step it was expanded from.
    let mut bytes: usize = 0;
    for step in &authored_steps {
        match expand_step(step, &mut libs.work.borrow_mut().matrix) {
            Ok(concrete) => {
                let ids = concrete.iter().map(|s| s.id.clone()).collect();
                // A duplicate authored id shadows an earlier expansion; the
                // resulting duplicate concrete ids are caught by validate().
                instances.insert(step.id.clone(), ids);
                let each = json_len(step, MAX_COMPILED_IR_BYTES).unwrap_or(usize::MAX);
                bytes = bytes.saturating_add(each.saturating_mul(concrete.len()));
                expanded.extend(concrete);
                if expanded.len() > MAX_COMPILED_STEPS {
                    diagnostics.push(too_many_steps(&format!("step `{}`", step.id)));
                    break;
                }
                if bytes > MAX_COMPILED_IR_BYTES {
                    diagnostics.push(ir_too_large(&format!("step `{}`", step.id)));
                    break;
                }
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
        // A Pipeline Step given a scope for an invoke's outputs reads every
        // other need by its id, as a Step with no scope does.
        if let Some(scope) = &mut step.scope {
            if scope.inputs == ScopeInputs::Run {
                for n in &step.needs.0 {
                    scope
                        .outputs
                        .entry(n.clone())
                        .or_insert_with(|| OutputSource::Step(n.clone()));
                }
            }
        }
        // Explicit `inputs:` fan in the same way, so a subset of a matrixed need
        // resolves to that need's instances.
        if let Some(inputs) = &step.inputs {
            step.inputs = Some(fan_in(inputs));
        }
    }

    // A Step with a scope renders differently from one without: a server
    // that predates scopes must not launch it (see [`IR_VERSION_SCOPED`]).
    let ir_version = if expanded.iter().any(|s| s.scope.is_some()) {
        authored.ir_version.max(IR_VERSION_SCOPED)
    } else {
        authored.ir_version
    };
    let ir = PipelineIr {
        ir_version,
        kind: None,
        name: authored.name,
        triggers: authored.triggers,
        concurrency: authored.concurrency,
        environment: authored.environment,
        deploy: authored.deploy,
        retention_profile: authored.retention_profile,
        interface: authored.interface,
        budget: authored.budget,
        // Pipeline-level shared services (ADR-0058) are not DAG nodes and are not
        // matrix-expanded — they pass through compilation unchanged.
        services: authored.services,
        steps: expanded,
    };

    if json_len(&ir, MAX_COMPILED_IR_BYTES).is_none() {
        return Err(PipelineError::Validation(vec![ir_too_large(
            "the pipeline",
        )]));
    }
    validate(&ir).map_err(PipelineError::Validation)?;
    Ok(ir)
}

/// The repository paths an authored pipeline's `invoke: file:` steps name, so
/// an I/O edge (the server trigger path) can pre-fetch their sources before
/// the pure compile (ADR-0038). Only `file:` targets are read from the
/// repository (ADR-0076 §1); `lib:` and `plugin:` targets are not listed.
/// Returns the normalized, safety-checked repo-relative keys (the keys the
/// compiler looks up in [`Libraries::files`]), in declaration order and
/// de-duplicated.
///
/// Best-effort: yaml that does not parse, and targets that fail the prefix
/// or path-safety check, are silently omitted here — the compiler is the sole
/// authority that turns those into diagnostics. This function only answers
/// "what is safe to fetch?".
pub fn invoke_refs(yaml: &str) -> Vec<String> {
    let Ok(authored) = serde_yaml::from_str::<PipelineIr>(yaml) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for step in &authored.steps {
        if let Some(raw) = &step.invoke {
            if let Ok(InvokeTarget::File(key)) = parse_invoke_target(raw) {
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        }
    }
    out
}

/// Is this authored file a library (`kind: library`, ADR-0076 §2)? A shallow
/// read of the `kind:` field only, so pipeline discovery can skip a library
/// without compiling it. `false` for a file that does not parse: the compile
/// reports that.
pub fn is_library(yaml: &str) -> bool {
    #[derive(Deserialize)]
    struct ShallowKind {
        #[serde(default)]
        kind: Option<PipelineKind>,
    }
    serde_yaml::from_str::<ShallowKind>(yaml).is_ok_and(|k| k.kind == Some(PipelineKind::Library))
}

/// For a check with no server (`scarab validate` offline): a stand-in for
/// each `lib:` target that `sources` (the Pipeline and its `file:`
/// libraries) invoke and `installed` does not have. A stand-in declares the
/// inputs the callers pass, as optional strings, and a Step for each output
/// they read (an exposed output names a library Step), so the rest of the
/// Pipeline can be checked; what the library itself
/// checks is not. The names of the targets stood in for are the keys.
pub fn unresolved_library_stubs<'a>(
    sources: impl IntoIterator<Item = &'a str>,
    installed: &BTreeMap<String, InstalledLibrary>,
) -> BTreeMap<String, InstalledLibrary> {
    // name -> (input names, output names)
    let mut wanted: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for src in sources {
        let Ok(authored) = serde_yaml::from_str::<PipelineIr>(src) else {
            continue;
        };
        for step in &authored.steps {
            let Some(Ok(InvokeTarget::Lib(name))) = step.invoke.as_deref().map(parse_invoke_target)
            else {
                continue;
            };
            if installed.contains_key(&name) {
                continue;
            }
            let entry = wanted.entry(name).or_default();
            entry.0.extend(step.with.keys().cloned());
            // `outputs.<invoke id>.<name>` read anywhere in the file.
            let needle = format!("outputs.{}.", step.id);
            let mut rest = src;
            while let Some(at) = rest.find(&needle) {
                rest = &rest[at + needle.len()..];
                let out: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if !out.is_empty() {
                    entry.1.insert(out);
                }
            }
        }
    }
    wanted
        .into_iter()
        .map(|(name, (inputs, outputs))| {
            // An exposed output names a library Step.
            let image = format!("unresolved.invalid/library@sha256:{}", "0".repeat(64));
            let ids: Vec<&str> = if outputs.is_empty() {
                vec!["unresolved"]
            } else {
                outputs.iter().map(String::as_str).collect()
            };
            let steps: Vec<serde_json::Value> = ids
                .iter()
                .map(|id| serde_json::json!({ "id": id, "image": image }))
                .collect();
            let stub = serde_json::json!({
                "kind": "library",
                "interface": {
                    "inputs": inputs.iter().map(|i| serde_json::json!({
                        "name": i, "type": "string", "required": false, "default": ""
                    })).collect::<Vec<_>>(),
                    "outputs": outputs,
                },
                "steps": steps,
            });
            let content = serde_yaml::to_string(&stub).unwrap_or_default();
            (
                name,
                InstalledLibrary {
                    content,
                    source: String::new(),
                },
            )
        })
        .collect()
}

/// For a check with no server (`scarab validate` offline): a stand-in for
/// each `plugin:` target that `sources` invoke, like
/// [`unresolved_library_stubs`]. A stand-in declares the inputs the callers
/// pass, as optional strings, and a `string` output for each output they
/// read, so the rest of the Pipeline can be checked; the gate's own types are
/// not. The names of the targets stood in for are the keys.
pub fn unresolved_plugin_gate_stubs<'a>(
    sources: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, PluginGateDef> {
    let mut wanted: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for src in sources {
        let Ok(authored) = serde_yaml::from_str::<PipelineIr>(src) else {
            continue;
        };
        for step in &authored.steps {
            let Some(Ok(InvokeTarget::Plugin(name))) =
                step.invoke.as_deref().map(parse_invoke_target)
            else {
                continue;
            };
            let entry = wanted.entry(name).or_default();
            entry.0.extend(step.with.keys().cloned());
            let needle = format!("outputs.{}.", step.id);
            let mut rest = src;
            while let Some(at) = rest.find(&needle) {
                rest = &rest[at + needle.len()..];
                let out: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if !out.is_empty() {
                    entry.1.insert(out);
                }
            }
        }
    }
    wanted
        .into_iter()
        .map(|(name, (inputs, outputs))| {
            let def = PluginGateDef {
                interface: Interface {
                    inputs: inputs
                        .into_iter()
                        .map(|i| ParamSpec {
                            name: i,
                            r#type: ParamType::String,
                            required: false,
                            default: Some(serde_json::Value::String(String::new())),
                            options: None,
                            validate: None,
                            description: None,
                        })
                        .collect(),
                    outputs: outputs.into_iter().map(OutputSpec::named).collect(),
                },
                manifest_sha256: "0".repeat(64),
            };
            (name, def)
        })
        .collect()
}

/// Check that `content` can be installed as `lib:<qualified>` (ADR-0076 §4):
/// it is a library (`kind: library`, no `on:`), it is closed (it invokes only
/// `lib:` and `plugin:` targets), every `lib:` it invokes is in `installed`,
/// and it compiles when invoked. Each input the library requires with no
/// default is bound to a template, as a caller's `${{ … }}` would be, so the
/// check needs no values for them; defaults are checked as written.
///
/// `installed` is the org's other installations; `content` is checked under
/// `qualified` in their place, so an upgrade is checked against the rest of
/// the org as it is.
pub fn check_installed_library(
    qualified: &str,
    content: &str,
    installed: &BTreeMap<String, InstalledLibrary>,
) -> Result<(), Vec<String>> {
    if content.len() > MAX_SOURCE_BYTES {
        return Err(vec![source_too_large("the library", content.len())]);
    }
    let lib: PipelineIr = serde_yaml::from_str(content)
        .map_err(|e| vec![format!("the library failed to parse: {e}")])?;
    if lib.kind != Some(PipelineKind::Library) {
        return Err(vec![
            "the file is not a library; a library starts with `kind: library`".to_string(),
        ]);
    }
    let mut inputs = Vec::new();
    let mut with = serde_json::Map::new();
    for p in &lib.interface.inputs {
        if p.required && p.default.is_none() {
            inputs.push(serde_json::json!({ "name": p.name, "type": "string" }));
            with.insert(
                p.name.clone(),
                serde_json::Value::String(format!("${{{{ inputs.{} }}}}", p.name)),
            );
        }
    }
    let wrapper = serde_json::json!({
        "interface": { "inputs": inputs },
        "steps": [{ "id": "library", "invoke": format!("lib:{qualified}"), "with": with }],
    });
    let yaml = serde_yaml::to_string(&wrapper).map_err(|e| vec![e.to_string()])?;
    let mut all = installed.clone();
    all.insert(
        qualified.to_string(),
        InstalledLibrary {
            content: content.to_string(),
            source: String::new(),
        },
    );
    match compile_with_libraries(
        &yaml,
        &Libraries {
            files: BTreeMap::new(),
            installed: all,
            plugin_gates: Default::default(),
        },
    ) {
        Ok(_) => Ok(()),
        Err(PipelineError::Validation(d)) => Err(d),
        Err(e) => Err(vec![e.to_string()]),
    }
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

/// What an `invoke:` names (ADR-0076 §1), parsed from its prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InvokeTarget {
    /// `file:<path>`: a library file in the repository, by its normalized
    /// repo-relative path.
    File(String),
    /// `lib:<publisher>/<name>`: a library installed in the caller's org.
    Lib(String),
    /// `plugin:<installation>/<gate>`: a gate a plugin resolves. Parsed, and
    /// refused until plugin gates exist (ADR-0076 §7).
    Plugin(String),
}

impl InvokeTarget {
    /// The target as an author writes it, for diagnostics and cycle paths.
    fn written(&self) -> String {
        match self {
            InvokeTarget::File(p) => format!("file:{p}"),
            InvokeTarget::Lib(n) => format!("lib:{n}"),
            InvokeTarget::Plugin(n) => format!("plugin:{n}"),
        }
    }
}

/// Whether `raw` is an `invoke: plugin:…` target, which inlines one gate node
/// with the invoke step's own id.
fn is_plugin_invoke(raw: &str) -> bool {
    matches!(parse_invoke_target(raw), Ok(InvokeTarget::Plugin(_)))
}

/// Parse an `invoke:` value into its [`InvokeTarget`] (ADR-0076 §1). Every
/// target has a prefix: a bare path is an error that names `file:`, an
/// unknown prefix is an error, and `url:` is reserved and refused (remote
/// `invoke` stays refused, ADR-0038).
fn parse_invoke_target(raw: &str) -> Result<InvokeTarget, String> {
    let text = raw.trim();
    // A prefix is the word before the first `:`. Anything else (`./x.yaml`,
    // `.scarab/lib/x.yaml`) is a bare path.
    let prefixed = text
        .split_once(':')
        .filter(|(p, _)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphabetic()));
    if let Some((prefix, rest)) = prefixed {
        // Prefixes are lowercase: `LIB:` and `File:` are unknown, not paths.
        if prefix.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(format!(
                "`invoke: {text}` has an unknown prefix `{prefix}:`; use `file:`, `lib:` or \
                 `plugin:` (prefixes are lowercase)"
            ));
        }
        // No space between the prefix and the target, for every prefix.
        if rest.starts_with(char::is_whitespace) {
            return Err(format!(
                "`invoke: {text}` has a space after `{prefix}:`; write the target right after \
                 the prefix"
            ));
        }
    }
    let Some((prefix, rest)) = prefixed else {
        return Err(if resolve_lib_path(text).is_ok() {
            format!("`invoke: {text}` needs a prefix: write `invoke: file:{text}`")
        } else {
            format!(
                "`invoke: {text}` needs a prefix: `file:<path>`, `lib:<publisher>/<name>` or \
                 `plugin:<installation>/<gate>`"
            )
        });
    };
    match prefix {
        "file" => resolve_lib_path(rest).map(InvokeTarget::File),
        "lib" => two_segment_name(rest, "lib", "<publisher>/<name>").map(InvokeTarget::Lib),
        "plugin" => {
            two_segment_name(rest, "plugin", "<installation>/<gate>").map(InvokeTarget::Plugin)
        }
        "url" => Err(format!(
            "`invoke: {text}`: `url:` is reserved and Scarab does not invoke from a URL \
             (ADR-0038); copy the file into the repository and use `file:`, or have an org \
             admin install it and use `lib:`"
        )),
        other => Err(format!(
            "`invoke: {text}` has an unknown prefix `{other}:`; use `file:`, `lib:` or `plugin:`"
        )),
    }
}

/// Check a `<a>/<b>` target name: one `/`, each segment non-empty, made of
/// `[A-Za-z0-9._-]`, and not starting with `.` or `-`.
fn two_segment_name(rest: &str, prefix: &str, shape: &str) -> Result<String, String> {
    let segment_ok = |s: &str| {
        !s.is_empty()
            && !s.starts_with(['.', '-'])
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    match rest.split_once('/') {
        Some((a, b)) if segment_ok(a) && segment_ok(b) => Ok(rest.to_string()),
        _ => Err(format!(
            "`invoke: {prefix}:{rest}` must be `{prefix}:{shape}`: two names made of letters, \
             digits, `.`, `_` and `-`, joined by one `/`, neither starting with `.` or `-`"
        )),
    }
}

/// Normalize and safety-check the repo-relative path of an `invoke: file:`
/// target (ADR-0038), returning the canonical key used to look its source up
/// in [`Libraries::files`].
///
/// The path must be repo-relative and resolve inside the repo tree. Absolute
/// paths, `..` traversal, and cross-repo / remote references are rejected —
/// cross-repo *causation* is `on: upstream`, not `invoke`. A single leading
/// `./` is accepted and stripped.
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
        return Err(format!(
            "invoke path `{raw}` must be repo-relative (no leading `/`)"
        ));
    }
    if path.contains('\\') {
        return Err(format!("invoke path `{raw}` must use `/` path separators"));
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let mut components: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" => return Err(format!("invoke path `{raw}` has an empty path segment")),
            "." => {
                return Err(format!(
                    "invoke path `{raw}` must be a plain repo-relative path (no `.` segments)"
                ))
            }
            ".." => {
                return Err(format!(
                    "invoke path `{raw}` must not escape the repo tree with `..`"
                ))
            }
            c => components.push(c),
        }
    }
    Ok(components.join("/"))
}

/// Hard cap on `invoke` nesting depth (ADR-0038 termination corollary). A chain
/// deeper than this is rejected at compile — real libraries nest 1–3 levels; the
/// cap is a backstop against pathological (or generated) recursion, alongside
/// cycle detection. A Step's `origin` chain has at most this many links;
/// `scarab_policy::run_create::MAX_ORIGIN` is this constant.
pub const MAX_INVOKE_DEPTH: usize = 8;

/// The most Steps and gates one Pipeline may compile to, after `invoke:`
/// inlining and matrix expansion. Twice `scarab_policy`'s per-Run caps (512
/// Steps and 32 gates), so every Run a `run.create` Policy can check compiles,
/// with room for Projects that store no Policy, where those caps are not
/// applied. The compile runs on the server for each webhook, and a few small
/// libraries that each invoke the next several times multiply: the compiler
/// stops as soon as a count passes this, instead of building the whole list.
pub const MAX_COMPILED_STEPS: usize = 1024;

/// The most combinations one `matrix:` may have before `exclude` removes
/// any. Kept combinations count against [`MAX_COMPILED_STEPS`]; this bounds
/// the work of building and filtering the rest.
pub const MAX_MATRIX_COMBINATIONS: usize = 65_536;

/// The most `exclude` predicates one `matrix:` may have.
pub const MAX_MATRIX_EXCLUDES: usize = 256;

/// The most checks of a combination against an `exclude` predicate one
/// compile may make: for each matrix, its combinations before `exclude`
/// times its predicates. A predicate is evaluated once per distinct value
/// of the dimensions it reads, so this bounds the evaluations too.
pub const MAX_MATRIX_EXCLUDE_CHECKS: usize = 1 << 18;

/// The most combinations, before `exclude`, of all the matrices one
/// compile expands.
pub const MAX_MATRIX_COMBINATIONS_PER_COMPILE: usize = 1 << 20;

/// The matrix work one compile has done, against
/// [`MAX_MATRIX_COMBINATIONS_PER_COMPILE`] and
/// [`MAX_MATRIX_EXCLUDE_CHECKS`].
#[derive(Default)]
struct MatrixWork {
    combinations: usize,
    checks: usize,
}

/// The diagnostic for a Pipeline over [`MAX_COMPILED_STEPS`].
fn too_many_steps(at: &str) -> String {
    format!(
        "{at}: the pipeline compiles to more than {MAX_COMPILED_STEPS} Steps and gates after \
         inlining `invoke:` steps and expanding matrices; the limit is {MAX_COMPILED_STEPS}"
    )
}

/// The largest Pipeline or library file the compiler reads, in bytes (1 MiB).
pub const MAX_SOURCE_BYTES: usize = 1 << 20;

/// The largest compiled Pipeline, as stored JSON, in bytes (16 MiB). A
/// Run stores its compiled Pipeline, and each Step stores its own launch
/// spec, scope included. A library's bound inputs are stored on every Step
/// it inlines, and a matrix copies a Step, so a small file can compile to a
/// large Pipeline: the compiler stops as soon as a list it builds passes
/// this, instead of building the whole Pipeline.
pub const MAX_COMPILED_IR_BYTES: usize = 16 << 20;

/// The diagnostic for a file over [`MAX_SOURCE_BYTES`].
fn source_too_large(what: &str, len: usize) -> String {
    format!(
        "{what} is {len} bytes; the limit is {MAX_SOURCE_BYTES} bytes (1 MiB) per Pipeline or \
         library file"
    )
}

/// The diagnostic for a Pipeline over [`MAX_COMPILED_IR_BYTES`].
fn ir_too_large(at: &str) -> String {
    format!(
        "{at}: the pipeline compiles to more than {MAX_COMPILED_IR_BYTES} bytes (16 MiB) after \
         inlining `invoke:` steps and expanding matrices; the limit is {MAX_COMPILED_IR_BYTES} \
         bytes. Each Step a library inlines stores the library's inputs, and each matrix leg \
         stores a copy of its Step"
    )
}

/// The length of `value` as JSON, or `None` when it is longer than `limit`:
/// serializing stops there, so the work is bounded by `limit`.
fn json_len(value: &impl Serialize, limit: usize) -> Option<usize> {
    struct Count {
        len: usize,
        limit: usize,
    }
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.len += buf.len();
            if self.len > self.limit {
                return Err(std::io::Error::other("over the limit"));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { len: 0, limit };
    serde_json::to_writer(&mut count, value)
        .ok()
        .map(|()| count.len)
}

/// Inline every `invoke:` step (ADR-0038, ADR-0076) into a flat step list
/// with no invoke steps left. **Recursive**: a library that itself invokes is
/// expanded to full depth, ids namespacing correctly at each level
/// (`deploy/db/migrate`).
///
/// Termination (ADR-0038): a direct or indirect invoke **cycle** is rejected
/// with a diagnostic naming the offending path, and a hard **depth cap**
/// ([`MAX_INVOKE_DEPTH`]) bounds nesting.
///
/// Work is bounded too (see [`Budget`]): every `invoke:` step expanded counts,
/// failing ones included, and the compiler stops after
/// [`MAX_INLINE_DIAGNOSTICS`] problems.
fn inline_invokes(steps: &[StepSpec], libs: LibsRef<'_>) -> Result<Vec<StepSpec>, Vec<String>> {
    let mut diagnostics = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut budget = Budget::default();
    // The Pipeline file's own scope: `inputs` are the Run's launch parameters.
    let root = Scope::run();
    let late = BTreeSet::new();
    let level = Level {
        scope: &root,
        library: None,
        late: &late,
    };
    let out = inline_level(
        steps,
        libs,
        &level,
        &mut stack,
        &mut budget,
        &mut diagnostics,
    );
    if diagnostics.is_empty() {
        Ok(out)
    } else {
        Err(diagnostics)
    }
}

/// The most `invoke:` steps one compile expands, at every depth, counting a
/// library each time it is invoked and counting invokes that fail or inline
/// nothing (a matrix whose `exclude` removes every combination). A Pipeline
/// under [`MAX_COMPILED_STEPS`] whose invokes each inline at least one Step
/// expands fewer: each such invoke is an ancestor of a compiled Step, and a
/// Step has at most [`MAX_INVOKE_DEPTH`] of them.
const MAX_INVOKE_EXPANSIONS: usize = MAX_INVOKE_DEPTH * MAX_COMPILED_STEPS;

/// The most problems `invoke:` inlining reports before the compiler stops.
const MAX_INLINE_DIAGNOSTICS: usize = 100;

/// What one compile may still spend on `invoke:` inlining.
#[derive(Default)]
struct Budget {
    /// `invoke:` steps expanded so far, failing ones included.
    expansions: usize,
    /// Set, with one diagnostic, once a limit is passed; every level then
    /// returns at once.
    stopped: bool,
}

impl Budget {
    /// Stop, saying why, unless already stopped.
    fn stop(&mut self, why: String, diagnostics: &mut Vec<String>) {
        if !self.stopped {
            self.stopped = true;
            diagnostics.push(why);
        }
    }

    /// Whether to stop before the next step: already stopped, or
    /// [`MAX_INLINE_DIAGNOSTICS`] problems found.
    fn spent(&mut self, diagnostics: &mut Vec<String>) -> bool {
        if diagnostics.len() >= MAX_INLINE_DIAGNOSTICS {
            self.stop(
                format!(
                    "the compiler stopped after {MAX_INLINE_DIAGNOSTICS} problems; fix these and \
                     compile again to see any others"
                ),
                diagnostics,
            );
        }
        self.stopped
    }
}

/// One library on the current invoke path.
struct Frame {
    /// The target as written (`file:…`, `lib:…`): the cycle key.
    written: String,
    /// How messages name it: a file by its path, an installed library by
    /// `lib:<publisher>/<name>`.
    label: String,
    /// An installed (`lib:`) library, which may invoke only `lib:` and
    /// `plugin:` targets (ADR-0076 §1).
    installed: bool,
}

/// The Step fields an invoke step may set (ADR-0076 §1), besides `id`,
/// `invoke` and `with`. Each other field that is set is a compile error
/// naming it, so nothing on an invoke step is dropped without a word.
fn invoke_step_extra_fields(step: &StepSpec) -> Vec<&'static str> {
    let set = [
        ("image", !step.image.is_empty()),
        ("command", !step.command.is_empty()),
        ("env", !step.env.is_empty()),
        ("secrets", !step.secrets.is_empty()),
        ("security", step.security.is_some()),
        ("gate", step.gate.is_some()),
        ("gate_after", step.gate_after.is_some()),
        ("clone", step.clone.is_some()),
        ("forge_token", step.forge_token.is_some()),
        ("build", step.build.is_some()),
        ("artifacts", !step.artifacts.is_empty()),
        ("inputs", step.inputs.is_some()),
        ("outputs", step.outputs.is_some()),
        ("cache", step.cache.is_some()),
        ("retry", step.retry.is_some()),
        ("k8s_overlay", step.k8s_overlay.is_some()),
        ("matrix_values", !step.matrix_values.is_empty()),
        ("services", !step.services.is_empty()),
        ("uses", !step.uses.is_empty()),
        ("results", !step.results.is_empty()),
    ];
    set.into_iter()
        .filter(|(_, s)| *s)
        .map(|(f, _)| f)
        .collect()
}

/// Resolve every `invoke:` in `steps` into a flat list, ids relative to *this*
/// level (an outer caller applies its own prefix). `stack` holds the libraries
/// currently being expanded on this DFS path — used for cycle detection (a
/// target already on the stack), the depth cap (its length) and the closed
/// rule for installed libraries (its last frame). `budget` stops the compile,
/// with one diagnostic, once a level's list passes [`MAX_COMPILED_STEPS`];
/// every level then returns at once. A level's list is part of the final list
/// at least once, so the check never refuses a Pipeline under the cap.
///
/// Each library invocation is a scope (ADR-0076): a library Step gets a
/// [`Scope`] whose `inputs` are the invocation's bindings, whose `outputs`
/// name the library's own Steps, and whose caller is the scope of the invoke
/// Step that wrote the `with:`. Nothing a caller writes is inserted into a
/// library's text.
fn inline_level(
    steps: &[StepSpec],
    libs: LibsRef<'_>,
    level: &Level<'_>,
    stack: &mut Vec<Frame>,
    budget: &mut Budget,
    diagnostics: &mut Vec<String>,
) -> Vec<StepSpec> {
    let mut out: Vec<StepSpec> = Vec::new();
    // invoke-step id -> the namespaced leaf ids that a `needs: [that id]` maps to.
    let mut seam: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // What each invoke Step at this level exposes, so a sibling read before
    // the invoke is inlined resolves too.
    let exposed = exposed_outputs(steps, libs);

    // An invoke Step's id names its library's Steps (`<id>/…`) and its
    // outputs (`outputs.<id>`): it must not be any other Step's id here.
    for (i, s) in steps.iter().enumerate() {
        if s.invoke.is_some() && steps[..i].iter().any(|t| t.id == s.id) {
            diagnostics.push(format!("duplicate step id `{}`", s.id));
        }
    }

    // Stop when the list passes a cap: its length, or its stored size, which
    // only grows as outer levels namespace it.
    let mut bytes: usize = 0;
    let mut capped =
        |out: &[StepSpec], at: &str, budget: &mut Budget, diagnostics: &mut Vec<String>| {
            if out.len() > MAX_COMPILED_STEPS {
                budget.stop(too_many_steps(at), diagnostics);
            } else if let Some(last) = out.last() {
                let left = MAX_COMPILED_IR_BYTES - bytes;
                match json_len(last, left) {
                    Some(n) => bytes += n,
                    None => budget.stop(ir_too_large(at), diagnostics),
                }
            }
            budget.stopped
        };

    for step in steps {
        if budget.spent(diagnostics) {
            return out;
        }
        // `origin` is the compiler's record of where a Step came from
        // (ADR-0075 §2); an authored one would let a Pipeline pass a
        // library Policy by claiming another source.
        if !step.origin.is_empty() {
            let place = match stack.last() {
                Some(lib) => format!(" in library `{}`", lib.label),
                None => String::new(),
            };
            diagnostics.push(format!(
                "step `{}`{place}: `origin` is written by the compiler and cannot be set in a \
                 pipeline file",
                step.id
            ));
        }
        // A Step's scope decides what its templates read (ADR-0076); an
        // authored one would let a library read the Run's parameters.
        for (field, set) in [
            ("scope", step.scope.is_some()),
            ("guards", !step.guards.is_empty()),
            ("plugin_gate", step.plugin_gate.is_some()),
        ] {
            if set {
                let place = match stack.last() {
                    Some(lib) => format!(" in library `{}`", lib.label),
                    None => String::new(),
                };
                diagnostics.push(format!(
                    "step `{}`{place}: `{field}` is written by the compiler and cannot be set \
                     in a pipeline file",
                    step.id
                ));
            }
        }
        let Some(raw_target) = &step.invoke else {
            // `with:` supplies inputs to a library — meaningless off an invoke step.
            if !step.with.is_empty() {
                diagnostics.push(format!(
                    "step `{}`: `with:` supplies inputs to an invoked library and is only valid on an `invoke` step",
                    step.id
                ));
            }
            let mut step = step.clone();
            match level.library {
                Some((label, _)) => {
                    library_step_scope(&mut step, level, label, steps, &exposed, diagnostics)
                }
                None => invoke_output_scope(&mut step, &exposed),
            }
            out.push(step);
            if capped(
                &out,
                &format!("step `{}`", out[out.len() - 1].id),
                budget,
                diagnostics,
            ) {
                return out;
            }
            continue;
        };
        // Every expansion counts, so invokes that fail cannot fan out
        // without a bound.
        budget.expansions += 1;
        if budget.expansions > MAX_INVOKE_EXPANSIONS {
            budget.stop(
                format!(
                    "step `{}`: the pipeline expands more than {MAX_INVOKE_EXPANSIONS} `invoke:` \
                     steps, counting a library each time it is invoked; the limit is \
                     {MAX_INVOKE_EXPANSIONS}",
                    step.id
                ),
                diagnostics,
            );
            return out;
        }

        // An invoke step inlines a library and launches nothing itself: only
        // the fields that carry onto what it inlines are allowed (ADR-0076 §1).
        for field in invoke_step_extra_fields(step) {
            diagnostics.push(format!(
                "step `{}`: an invoke step cannot set `{field}`; set it on the library's Steps",
                step.id
            ));
        }
        if step.deployment_requests.is_some() || step.deployment.is_some() {
            diagnostics.push(format!("step `{}`: {RETIRED_DEPLOYMENTS}", step.id));
        }

        let target = match parse_invoke_target(raw_target) {
            Ok(t) => t,
            Err(msg) => {
                diagnostics.push(format!("step `{}`: {msg}", step.id));
                continue;
            }
        };
        let written = target.written();
        if step.gate_expires_after.is_some() && !matches!(target, InvokeTarget::Plugin(_)) {
            diagnostics.push(format!(
                "step `{}`: `gate_expires_after` is allowed on an invoke step only for a \
                 `plugin:` gate",
                step.id
            ));
        }
        // Installed libraries are closed (ADR-0076 §1): a repository cannot
        // replace Steps inside an approved library.
        if let Some(parent) = stack.last().filter(|f| f.installed) {
            if matches!(target, InvokeTarget::File(_)) {
                diagnostics.push(format!(
                    "step `{}` in library `{}`: an installed library can invoke only `lib:` and \
                     `plugin:` targets, not `{written}`",
                    step.id, parent.label
                ));
                continue;
            }
        }
        // Where the library comes from, and how messages name it: a file by
        // its path, an installed library by `lib:<publisher>/<name>`.
        let (src, kind, name, source, label) = match &target {
            InvokeTarget::File(key) => match libs.files.get(key) {
                Some(src) => (src.as_str(), OriginKind::File, key, None, key.clone()),
                None => {
                    diagnostics.push(format!(
                        "step `{}`: no library found at `{key}` (a `file:` library is read from \
                         the repository at the caller's ref)",
                        step.id
                    ));
                    continue;
                }
            },
            InvokeTarget::Lib(lib_name) => match libs.installed.get(lib_name) {
                Some(l) => (
                    l.content.as_str(),
                    OriginKind::Library,
                    lib_name,
                    Some(l.source.clone()),
                    written.clone(),
                ),
                None => {
                    diagnostics.push(format!(
                        "step `{}`: `{written}` is not installed in this org",
                        step.id
                    ));
                    continue;
                }
            },
            InvokeTarget::Plugin(target) => {
                let Some(def) = libs.plugin_gates.get(target) else {
                    diagnostics.push(format!(
                        "step `{}`: `{written}` is not a gate of a plugin installed in this org \
                         (an org admin installs the plugin, with the `resolve_gates` \
                         capability, under the installation name before the `/`)",
                        step.id
                    ));
                    continue;
                };
                let gates = plugin_gate_nodes(
                    step,
                    target,
                    def,
                    level,
                    steps,
                    &exposed,
                    &mut libs.work.borrow_mut().matrix,
                    diagnostics,
                );
                if step.matrix.is_some() {
                    seam.insert(
                        step.id.clone(),
                        gates.iter().map(|g| g.id.clone()).collect(),
                    );
                }
                for gate in gates {
                    out.push(gate);
                    if capped(&out, &format!("step `{}`", step.id), budget, diagnostics) {
                        return out;
                    }
                }
                continue;
            }
        };
        // Cycle: this library is already being expanded on the current path.
        if let Some(pos) = stack.iter().position(|f| f.written == written) {
            let mut path: Vec<&str> = stack[pos..].iter().map(|f| f.written.as_str()).collect();
            path.push(&written);
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
                "step `{}`: invoke nesting exceeds the depth cap of {MAX_INVOKE_DEPTH} (at `{written}`)",
                step.id
            ));
            continue;
        }
        if src.len() > MAX_SOURCE_BYTES {
            diagnostics.push(format!(
                "step `{}`: {}",
                step.id,
                source_too_large(&format!("library `{label}`"), src.len())
            ));
            continue;
        }
        let (lib, content_hash) = match libs.parse(&written, src) {
            Ok(parsed) => parsed,
            Err(why) => {
                diagnostics.push(format!("step `{}`: library `{label}` {why}", step.id));
                continue;
            }
        };
        // Every invoke target is marked as a library (ADR-0076 §2): inside
        // one, `${{ inputs.* }}` reads the caller's `with:`, which is not what
        // it means in a Pipeline.
        if lib.kind != Some(PipelineKind::Library) {
            diagnostics.push(format!(
                "step `{}`: `{written}` is not a library; add `kind: library` to it",
                step.id
            ));
            continue;
        }
        if !lib.triggers.is_empty() {
            diagnostics.push(format!(
                "step `{}`: library `{label}` has `on:` triggers; a library runs only where it \
                 is invoked",
                step.id
            ));
        }
        // A library is only Steps and their interface. A Pipeline-level field
        // would be dropped when it is inlined, so it is refused instead.
        for (field, set) in [
            ("environment", lib.environment.is_some()),
            ("concurrency", lib.concurrency.is_some()),
            ("services", !lib.services.is_empty()),
            ("budget", lib.budget.is_some()),
            ("retention_profile", lib.retention_profile.is_some()),
        ] {
            if set {
                diagnostics.push(format!(
                    "step `{}`: library `{label}` sets `{field}`, which only a Pipeline can \
                     set; set it on the Pipeline that invokes the library",
                    step.id
                ));
            }
        }
        if lib.steps.is_empty() {
            diagnostics.push(format!(
                "step `{}`: library `{label}` has no steps",
                step.id
            ));
            continue;
        }

        // Interface (ADR-0038): validate the caller's `with:` inputs and the
        // library's declared/exposed outputs at compile — explicit over ambient.
        let before_interface = diagnostics.len();
        validate_interface(step, &label, &lib, steps, diagnostics);

        // Bind the library's inputs (ADR-0076): a literal `with:` value or a
        // default, converted to the declared type now; a caller's template,
        // kept as written, rendered in this level's scope when a Step of the
        // library launches. A `with:` template is checked like any template
        // written here.
        let BoundWith {
            bindings,
            late,
            caller_view,
            with_needs,
        } = bind_with(
            step,
            &lib.interface.inputs,
            &format!("library `{label}`"),
            level,
            steps,
            &exposed,
            before_interface,
            diagnostics,
        );
        // A library's invoke `when` is decided at Run creation, in this
        // level's scope (the Pipeline file's is unchanged).
        if let (Some((l, _)), Some(When(w))) = (level.library, &step.when) {
            let mut errors = Vec::new();
            check_reads(
                w,
                Written::Guard,
                level,
                "when",
                &mut Vec::new(),
                &mut errors,
            );
            diagnostics.extend(
                errors
                    .into_iter()
                    .map(|e| format!("library `{l}`, step `{}`: {e}", step.id)),
            );
        }
        let mut caller = level.scope.clone();
        caller.outputs = caller_view;
        let lib_scope = Scope {
            inputs: ScopeInputs::Bound(bindings),
            outputs: BTreeMap::new(),
            matrix: BTreeMap::new(),
            caller: Some(Box::new(caller)),
        };

        // Recurse: resolve the library's own invokes first, so `resolved` is a
        // flat list in the library's namespace (ids may already contain `/`).
        // Coordinate-independent, so it runs once even under a matrix.
        stack.push(Frame {
            written: written.clone(),
            label: label.clone(),
            installed: kind == OriginKind::Library,
        });
        let resolved = inline_level(
            &lib.steps,
            libs,
            &Level {
                scope: &lib_scope,
                library: Some((&label, &lib.interface.inputs)),
                late: &late,
            },
            stack,
            budget,
            diagnostics,
        );
        stack.pop();
        if budget.stopped {
            return out;
        }
        // The fields an invoke step carries apply only to non-gate Steps; with
        // none to receive them they would be dropped, so they are refused.
        if resolved.iter().all(StepSpec::is_gate) {
            for (field, set) in [
                ("timeout", step.timeout.is_some()),
                (
                    "resources",
                    step.resources.cpu_millis.is_some() || step.resources.memory_mib.is_some(),
                ),
                ("placement_profiles", !step.placement_profiles.is_empty()),
            ] {
                if set {
                    diagnostics.push(format!(
                        "step `{}`: `{field}` applies to the Steps an invoke inlines, and \
                         library `{label}` inlines only gates",
                        step.id
                    ));
                }
            }
        }
        // This library is the outermost link so far of every Step it
        // inlines: the nested levels already recorded theirs.
        let link = OriginEntry {
            kind,
            name: name.clone(),
            source,
            content_hash,
        };

        // `matrix` × `invoke` (ADR-0038): a matrix on the invoke step fans out N
        // copies of the whole subgraph, once per coordinate. `expand_step` gives
        // the coordinate instances (id `deploy[svc=api]`, `matrix_values` set,
        // `exclude` applied); a step without a matrix yields exactly one instance
        // whose id is the step id — the single-copy path falls out for free.
        let invoke_instances = match expand_step(step, &mut libs.work.borrow_mut().matrix) {
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
                inst.origin.insert(0, link.clone());
                // `deploy: true` on the invoke step, or on the library
                // pipeline, marks every Step it inlines that runs something
                // (ADR-0074 §2); a gate can never carry the mark.
                if (step.deploy || lib.deploy) && !inst.is_gate() {
                    inst.deploy = true;
                }
                carry_invoke_fields(inv, &mut inst);
                // Entry seam: a library root inherits the invoke step's upstreams;
                // an internal need is namespaced.
                inst.needs = if ls.needs.0.is_empty() {
                    inv.needs.clone()
                } else {
                    Needs(
                        ls.needs
                            .0
                            .iter()
                            .map(|n| namespace_ref(n, &internal, &ns))
                            .collect(),
                    )
                };
                if let Some(inputs) = &ls.inputs {
                    inst.inputs = Some(
                        inputs
                            .iter()
                            .map(|n| namespace_ref(n, &internal, &ns))
                            .collect(),
                    );
                }
                // The invoke coordinate is recorded on the Step (its id and
                // `matrix_values` show it); a library Step's own coordinate
                // (added later by the main matrix pass) wins on key conflict.
                // A library does not read it: its scope's `matrix` is its own.
                for (k, v) in &inv.matrix_values {
                    inst.matrix_values
                        .entry(k.clone())
                        .or_insert_with(|| v.clone());
                }
                for n in &with_needs {
                    if !inst.needs.0.contains(n) {
                        inst.needs.0.push(n.clone());
                    }
                }
                // The scopes inside the library name its Steps as the library
                // does; from here they are `<invoke>/<id>`. The scope `hops`
                // callers out is this invoke Step's: its `matrix` is this
                // copy's coordinate, and its `when` reads it.
                let hops = inst.origin.len();
                if let Some(scope) = &mut inst.scope {
                    for hop in 0..hops {
                        if let Some(s) = scope.at_mut(hop) {
                            namespace_view(s, &ns);
                        }
                    }
                    if let Some(s) = scope.at_mut(hops) {
                        s.matrix = inv.matrix_values.clone();
                    }
                }
                if let Some(When(w)) = &inv.when {
                    inst.guards.push(Guard {
                        caller_hops: hops,
                        when: w.clone(),
                    });
                }
                out.push(inst);
                if capped(&out, &format!("step `{}`", step.id), budget, diagnostics) {
                    return out;
                }
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

    out
}

/// The gate nodes `invoke: plugin:<installation>/<gate>` inlines (ADR-0076
/// §7): one `gate: plugin` node with the invoke step's id, or one per
/// coordinate of its `matrix`. Its releaser is the installation; its inputs
/// are bound from `with:` like a library's ([`bind_with`]) and render in
/// the invoke step's scope when the gate is reached; its `gate_expires_after`
/// is required, since a gate otherwise waits forever.
#[allow(clippy::too_many_arguments)]
fn plugin_gate_nodes(
    step: &StepSpec,
    target: &str,
    def: &PluginGateDef,
    level: &Level<'_>,
    siblings: &[StepSpec],
    exposed: &BTreeMap<String, Option<Vec<String>>>,
    matrix_work: &mut MatrixWork,
    diagnostics: &mut Vec<String>,
) -> Vec<StepSpec> {
    let (installation, gate) = target.split_once('/').unwrap_or((target, ""));
    let what = format!("plugin gate `plugin:{target}`");
    if step.gate_expires_after.is_none() {
        diagnostics.push(format!(
            "step `{}`: a plugin gate must set `gate_expires_after` (seconds): nothing else \
             ends the wait when the plugin does not answer",
            step.id
        ));
    }
    // What an invoke step carries applies to the Steps it inlines; a plugin
    // gate is a gate, and a gate runs nothing.
    for (field, set) in [
        ("timeout", step.timeout.is_some()),
        (
            "resources",
            step.resources.cpu_millis.is_some() || step.resources.memory_mib.is_some(),
        ),
        ("placement_profiles", !step.placement_profiles.is_empty()),
        ("deploy", step.deploy),
    ] {
        if set {
            diagnostics.push(format!(
                "step `{}`: `{field}` cannot be set on a plugin gate, which runs nothing",
                step.id
            ));
        }
    }
    let before = diagnostics.len();
    params::validate_param_specs(&def.interface.inputs, &what, diagnostics);
    check_with(step, &what, &def.interface.inputs, diagnostics);
    let bound = bind_with(
        step,
        &def.interface.inputs,
        &what,
        level,
        siblings,
        exposed,
        before,
        diagnostics,
    );
    // A plugin gate's inputs are sent to the plugin: a secret or the
    // server's environment never renders into them. Refused here rather
    // than when the gate is reached.
    for (name, raw) in &step.with {
        for i in cel::scan_interpolations(raw).0 {
            let Ok(reads) = cel::reads(raw[i.inner].trim()) else {
                continue;
            };
            for var in ["secrets", "env"] {
                if reads.iter().any(|r| r.var == var) {
                    diagnostics.push(format!(
                        "step `{}`: `with.{name}` reads `{var}`; the inputs of {what} are sent \
                         to the plugin, so they cannot read `{var}`",
                        step.id
                    ));
                }
            }
        }
    }
    if let (Some((l, _)), Some(When(w))) = (level.library, &step.when) {
        let mut errors = Vec::new();
        check_reads(
            w,
            Written::Guard,
            level,
            "when",
            &mut Vec::new(),
            &mut errors,
        );
        diagnostics.extend(
            errors
                .into_iter()
                .map(|e| format!("library `{l}`, step `{}`: {e}", step.id)),
        );
    }
    let instances = match expand_step(step, matrix_work) {
        Ok(v) => v,
        Err(msg) => {
            diagnostics.push(msg);
            return Vec::new();
        }
    };
    let link = OriginEntry {
        kind: OriginKind::Plugin,
        name: installation.to_string(),
        source: None,
        content_hash: def.manifest_sha256.clone(),
    };
    instances
        .into_iter()
        .map(|inv| {
            let mut node = inv.clone();
            node.invoke = None;
            node.with = BTreeMap::new();
            node.when = None;
            node.gate = Some("plugin".to_string());
            node.plugin_gate = Some(PluginGateSpec {
                installation: installation.to_string(),
                gate: gate.to_string(),
                outputs: def.interface.outputs.clone(),
            });
            for n in &bound.with_needs {
                if !node.needs.0.contains(n) {
                    node.needs.0.push(n.clone());
                }
            }
            // The `with:` templates render in the invoke step's scope: this
            // level's, with the outputs they read and this copy's
            // coordinate.
            let mut caller = level.scope.clone();
            caller.outputs = bound.caller_view.clone();
            caller.matrix = inv.matrix_values.clone();
            node.scope = Some(Scope {
                inputs: ScopeInputs::Bound(bound.bindings.clone()),
                outputs: BTreeMap::new(),
                matrix: BTreeMap::new(),
                caller: Some(Box::new(caller)),
            });
            node.origin = vec![link.clone()];
            if let Some(When(w)) = &inv.when {
                node.guards.push(Guard {
                    caller_hops: 1,
                    when: w.clone(),
                });
            }
            node
        })
        .collect()
}

/// A library's or a plugin gate's inputs, bound by an invoke step's `with:`
/// (ADR-0076): see [`bind_with`].
struct BoundWith {
    bindings: BTreeMap<String, Binding>,
    /// The inputs bound to a template that reads `outputs`.
    late: BTreeSet<String>,
    /// The `outputs` view of the caller scope the templates render in.
    caller_view: BTreeMap<String, OutputSource>,
    /// The Steps the templates read, which every inlined node needs.
    with_needs: Vec<String>,
}

/// Bind the inputs `inputs` (declared by `what`) from `step`'s `with:`: a
/// literal `with:` value or a default, converted to the declared type now;
/// a caller's template, kept as written, rendered in this level's scope
/// when a Step of the library launches, or when a plugin gate is reached. A
/// `with:` template is checked like any template written here.
/// `before_interface` is the diagnostics count before the target's
/// interface was checked.
#[allow(clippy::too_many_arguments)]
fn bind_with(
    step: &StepSpec,
    inputs: &[ParamSpec],
    what: &str,
    level: &Level<'_>,
    siblings: &[StepSpec],
    exposed: &BTreeMap<String, Option<Vec<String>>>,
    before_interface: usize,
    diagnostics: &mut Vec<String>,
) -> BoundWith {
    let at = match level.library {
        Some((l, _)) => format!("library `{l}`, step `{}`:", step.id),
        None => format!("step `{}`:", step.id),
    };
    let who = format!("{at} `with:`");
    let mut bindings: BTreeMap<String, Binding> = BTreeMap::new();
    let mut late: BTreeSet<String> = BTreeSet::new();
    let mut with_reads: OutputNames = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for p in inputs {
        let value = match step.with.get(&p.name) {
            Some(raw) if raw.contains("${{") => {
                if let Err(e) = cel::check_interpolation(raw) {
                    diagnostics.push(format!("{at} `with.{}` {e}", p.name));
                    continue;
                }
                let mut reads = Vec::new();
                let mut errors = Vec::new();
                check_reads(
                    raw,
                    Written::Template,
                    level,
                    &format!("with.{}", p.name),
                    &mut reads,
                    &mut errors,
                );
                diagnostics.extend(errors.into_iter().map(|e| format!("{at} {e}")));
                if !reads.is_empty() || reads_late_inputs(raw, level.late) {
                    late.insert(p.name.clone());
                }
                with_reads.extend(reads);
                BoundValue::Template(raw.clone())
            }
            Some(raw) => {
                match params::resolve_one(p, &serde_json::Value::String(raw.clone())) {
                    Ok(v) => BoundValue::Literal(v),
                    Err(e) => {
                        unresolved.push(format!("`{}`: {e}", p.name));
                        continue;
                    }
                }
            }
            None => match &p.default {
                Some(d) => match params::resolve_one(p, d) {
                    Ok(v) => BoundValue::Literal(v),
                    Err(e) => {
                        unresolved.push(format!("`{}` (default): {e}", p.name));
                        continue;
                    }
                },
                // A missing required input, reported by validate_interface.
                None => continue,
            },
        };
        let binding = match value {
            BoundValue::Literal(v) => Binding::literal(v),
            BoundValue::Template(t) => Binding::template(t, p),
        };
        bindings.insert(p.name.clone(), binding);
    }
    // `validate_interface` usually said why an input does not resolve;
    // when it did not, the resolve error is the diagnostic.
    if !unresolved.is_empty() && diagnostics.len() == before_interface {
        diagnostics.push(format!(
            "step `{}`: inputs for {what} do not resolve: {}",
            step.id,
            unresolved.join("; ")
        ));
    }
    // The Steps the `with:` templates read: every Step the library
    // inlines renders them, so each needs them (the engine gives a Step
    // the results of its own needs only). A library root already needs
    // what the invoke Step needs, so the schedule does not change.
    let mut with_needs: Vec<String> = Vec::new();
    let caller_view = output_view(
        &with_reads,
        siblings,
        exposed,
        &step.needs.0,
        &who,
        level,
        diagnostics,
        &mut with_needs,
    );
    BoundWith {
        bindings,
        late,
        caller_view,
        with_needs,
    }
}

/// The scope that the templates written at one level of `invoke:` nesting
/// read (ADR-0076).
struct Level<'a> {
    /// Its `inputs` and its callers; `outputs` and `matrix` are set for each
    /// Step or `with:` that reads them.
    scope: &'a Scope,
    /// The library inlined at this level and the inputs it declares; `None`
    /// for the Pipeline file.
    library: Option<(&'a str, &'a [ParamSpec])>,
    /// This level's inputs bound to a caller's template that reads
    /// `outputs`, directly or through the caller's own inputs: they have no
    /// value until a Step launches, so a `when:` cannot read them.
    late: &'a BTreeSet<String>,
}

/// The `outputs.<name>` reads of templates written at one level: each name
/// with the result read from it, when that is a constant.
type OutputNames = Vec<(String, Option<String>)>;

/// What is being checked by [`check_reads`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    /// A `${{ … }}` template, rendered when a Step launches.
    Template,
    /// A `when:` expression, decided when the Run is created.
    Guard,
}

/// Check what a template (or a `when:` expression) written at `level` reads,
/// by the CEL parser (ADR-0076). Pushes the static `outputs.<name>` reads
/// onto `outputs` and each problem onto `errors`, naming `field`:
///
/// - inside a library, an `inputs.<name>` the library does not declare;
/// - an `outputs` read whose Step is computed (`outputs[k]`): the compiler
///   must know the Step, to give the reading Step its view and its `needs`;
/// - in a library `when:`, any `outputs` read, and a read of an input bound
///   to a caller's template that reads `outputs`: no Step has run when the
///   Run is created.
///
/// An expression that does not parse is left to [`validate`] (a `with:`
/// value is checked by the caller).
fn check_reads(
    text: &str,
    written: Written,
    level: &Level<'_>,
    field: &str,
    outputs: &mut OutputNames,
    errors: &mut Vec<String>,
) {
    let exprs: Vec<&str> = match written {
        Written::Guard => vec![text],
        Written::Template => cel::scan_interpolations(text)
            .0
            .into_iter()
            .map(|i| text[i.inner].trim())
            .collect(),
    };
    for expr in exprs {
        let Ok(reads) = cel::reads(expr) else {
            continue;
        };
        for r in reads {
            match r.var.as_str() {
                "inputs" => {
                    if let (Some((label, declared)), Some(name)) = (level.library, &r.first) {
                        if !declared.iter().any(|p| &p.name == name) {
                            errors.push(format!(
                                "`{field}` reads `inputs.{name}`, which library `{label}` does \
                                 not declare in `interface.inputs`"
                            ));
                        }
                    }
                    if written == Written::Guard {
                        let late = match &r.first {
                            Some(name) => level.late.contains(name).then(|| name.clone()),
                            None => level.late.first().cloned(),
                        };
                        if let Some(name) = late {
                            errors.push(format!(
                                "`{field}` reads `inputs.{name}`, which the caller binds to a \
                                 template that reads `outputs`; a `when:` is decided when the \
                                 Run is created, before any Step has run"
                            ));
                        }
                    }
                }
                "outputs" => {
                    if written == Written::Guard {
                        errors.push(format!(
                            "`{field}` reads `outputs`; a `when:` in a library is decided when \
                             the Run is created, before any Step has run"
                        ));
                        continue;
                    }
                    match r.first {
                        Some(name) => outputs.push((name, r.second)),
                        None => errors.push(format!(
                            "`{field}` reads `outputs` in a form the compiler cannot resolve to \
                             a Step; read a Step's outputs as `outputs.<id>` or \
                             `outputs[\"<id>\"]`"
                        )),
                    }
                }
                _ => {}
            }
        }
    }
}

/// Whether a template reads an input of `late` (or `inputs` whole while any
/// input is late).
fn reads_late_inputs(template: &str, late: &BTreeSet<String>) -> bool {
    if late.is_empty() {
        return false;
    }
    cel::scan_interpolations(template).0.iter().any(|i| {
        cel::reads(template[i.inner.clone()].trim()).is_ok_and(|reads| {
            reads.iter().any(|r| {
                r.var == "inputs" && r.first.as_ref().is_none_or(|name| late.contains(name))
            })
        })
    })
}

/// What each invoke Step in `steps` exposes: its id → its exposed output
/// names, or `None` when they cannot be read (a matrixed invoke, or a
/// library that does not resolve; both are reported where the invoke is
/// inlined). A shallow read of the library's `interface:` only.
fn exposed_outputs(steps: &[StepSpec], libs: LibsRef<'_>) -> BTreeMap<String, Option<Vec<String>>> {
    steps
        .iter()
        .filter_map(|s| {
            let raw = s.invoke.as_ref()?;
            // A plugin gate is one node with the invoke's id: it is read as
            // a Step, not through a view.
            if is_plugin_invoke(raw) {
                return None;
            }
            let outputs = (s.matrix.is_none())
                .then(|| {
                    let target = parse_invoke_target(raw).ok()?;
                    let src = match &target {
                        InvokeTarget::File(key) => libs.files.get(key)?.as_str(),
                        InvokeTarget::Lib(name) => libs.installed.get(name)?.content.as_str(),
                        InvokeTarget::Plugin(_) => return None,
                    };
                    if src.len() > MAX_SOURCE_BYTES {
                        return None;
                    }
                    libs.exposed(&target.written(), src)
                })
                .flatten();
            Some((s.id.clone(), outputs))
        })
        .collect()
}

/// The `outputs` view of a template's reads at one level (ADR-0076): each
/// name read → a sibling Step, or a sibling invoke's exposed outputs. A
/// name that is neither is refused. A sibling Step read must be among
/// `needs`, which the reader wrote; each Step the view reads is pushed onto
/// `data_needs` (an invoke's backing Steps by their ids at this level,
/// `<invoke>/<output>`). `who` begins each message.
#[allow(clippy::too_many_arguments)] // one resolution, called from two places
fn output_view(
    reads: &OutputNames,
    siblings: &[StepSpec],
    exposed: &BTreeMap<String, Option<Vec<String>>>,
    needs: &[String],
    who: &str,
    level: &Level<'_>,
    diagnostics: &mut Vec<String>,
    data_needs: &mut Vec<String>,
) -> BTreeMap<String, OutputSource> {
    let push = |n: String, data_needs: &mut Vec<String>| {
        if !data_needs.contains(&n) {
            data_needs.push(n);
        }
    };
    let mut view = BTreeMap::new();
    for (name, field) in reads {
        if let Some(outs) = exposed.get(name) {
            // An invoke here: its exposed outputs. A read of an output it
            // does not expose, or without `needs: [<invoke>]`, is reported
            // by validate_interface.
            let Some(outs) = outs else { continue };
            for out in outs {
                if field.as_ref().is_none_or(|f| f == out) {
                    push(format!("{name}/{out}"), data_needs);
                }
            }
            view.insert(
                name.clone(),
                OutputSource::Invoke(
                    outs.iter()
                        .map(|o| (o.clone(), format!("{name}/{o}")))
                        .collect(),
                ),
            );
            continue;
        }
        // A Step here, by its id or, for a matrixed Step, one instance's id
        // (`build[os=linux]`), which its authored id fans onto.
        let sibling = siblings.iter().find(|s| {
            s.invoke.as_deref().is_none_or(is_plugin_invoke)
                && (*name == s.id
                    || name
                        .strip_prefix(s.id.as_str())
                        .is_some_and(|r| r.starts_with('[')))
        });
        match sibling {
            Some(s) => {
                if !needs.contains(&s.id) {
                    diagnostics.push(format!(
                        "{who} reads `outputs.{}` but does not `needs: [{}]`",
                        s.id, s.id
                    ));
                }
                push(s.id.clone(), data_needs);
                view.insert(name.clone(), OutputSource::Step(name.clone()));
            }
            None => diagnostics.push(format!(
                "{who} reads `outputs.{name}`, which is not a Step {}",
                match level.library {
                    Some((l, _)) => format!("of library `{l}`"),
                    None => "of this Pipeline".to_string(),
                }
            )),
        }
    }
    view
}

/// Give a Step written in a library its scope (ADR-0076): this level's
/// `inputs` and callers, an `outputs` view of the Steps it reads, and its
/// own `when` as a guard. Each template is checked against the library's
/// interface; a Step that reads another Step's outputs must need it.
fn library_step_scope(
    step: &mut StepSpec,
    level: &Level<'_>,
    label: &str,
    siblings: &[StepSpec],
    exposed: &BTreeMap<String, Option<Vec<String>>>,
    diagnostics: &mut Vec<String>,
) {
    let prefix = format!("library `{label}`, step `{}`", step.id);
    let mut reads = Vec::new();
    let mut errors = Vec::new();
    let mut fields: Vec<(String, &str)> = vec![("image".to_string(), step.image.as_str())];
    fields.extend(
        step.command
            .iter()
            .enumerate()
            .map(|(i, c)| (format!("command[{i}]"), c.as_str())),
    );
    fields.extend(
        step.env
            .iter()
            .enumerate()
            .map(|(i, (_, v))| (format!("env[{i}][1]"), v.as_str())),
    );
    for (field, text) in fields {
        check_reads(
            text,
            Written::Template,
            level,
            &field,
            &mut reads,
            &mut errors,
        );
    }
    if let Some(When(w)) = &step.when {
        check_reads(
            w,
            Written::Guard,
            level,
            "when",
            &mut Vec::new(),
            &mut errors,
        );
        step.guards.push(Guard {
            caller_hops: 0,
            when: w.clone(),
        });
    }
    errors.extend(uninterpolated_input_reads(step));
    diagnostics.extend(errors.into_iter().map(|e| format!("{prefix}: {e}")));
    let mut data_needs = Vec::new();
    let view = output_view(
        &reads,
        siblings,
        exposed,
        &step.needs.0,
        &format!("{prefix}:"),
        level,
        diagnostics,
        &mut data_needs,
    );
    // An invoke's backing Steps: `needs: [<invoke>]` reaches only its leaves.
    for n in data_needs {
        if !step.needs.0.contains(&n) {
            step.needs.0.push(n);
        }
    }
    let mut scope = level.scope.clone();
    scope.outputs = view;
    step.scope = Some(scope);
}

/// Every `${{ … }}` of a library Step that reads `inputs` in a field that is
/// not rendered when the Step launches (a sidecar's image or env,
/// `results`, `artifacts`, `secrets`, cache keys, ...): it would reach the
/// Step as text, so it is refused. An expression that mentions `inputs` and
/// does not parse is refused too, since what it reads cannot be checked.
fn uninterpolated_input_reads(step: &StepSpec) -> Vec<String> {
    /// The fields that render (or, for `when`, are evaluated), and what the
    /// compiler writes.
    const RENDERED: [&str; 8] = [
        "image", "command", "env", "with", "when", "scope", "guards", "origin",
    ];
    fn walk(value: &serde_json::Value, path: &str, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => {
                let (found, unterminated) = cel::scan_interpolations(text);
                let mut exprs: Vec<&str> =
                    found.iter().map(|i| text[i.inner.clone()].trim()).collect();
                // An unterminated `${{` is checked to the end of the text.
                if let Some(at) = unterminated {
                    exprs.push(text[at + 3..].trim());
                }
                for expr in exprs.into_iter().filter(|e| e.contains("inputs")) {
                    let top = path.split(['.', '[']).next().unwrap_or(path);
                    match cel::reads(expr) {
                        Ok(reads) if !reads.iter().any(|r| r.var == "inputs") => {}
                        Ok(_) => out.push(format!(
                            "`{path}` reads `inputs`, but `{top}` is not interpolated when the \
                             Step launches; a library can read its inputs only in image, \
                             command, env, when and a nested invoke's with:"
                        )),
                        Err(e) => out.push(format!(
                            "`{path}` mentions `inputs` and does not parse, so its reads cannot \
                             be checked: {e}"
                        )),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{i}]"), out);
                }
            }
            serde_json::Value::Object(fields) => {
                for (k, v) in fields {
                    if path.is_empty() && RENDERED.contains(&k.as_str()) {
                        continue;
                    }
                    let key = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    walk(&serde_json::Value::String(k.clone()), &key, out);
                    walk(v, &key, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    match serde_json::to_value(step) {
        Ok(value) => walk(&value, "", &mut out),
        Err(e) => out.push(format!("cannot be checked for `inputs` reads: {e}")),
    }
    out
}

/// Namespace the Step ids a scope's `outputs` view names: they were ids in
/// a library, and are `<invoke>/<id>` in its caller.
fn namespace_view(scope: &mut Scope, ns: &impl Fn(&str) -> String) {
    for source in scope.outputs.values_mut() {
        match source {
            OutputSource::Step(id) => *id = ns(id),
            OutputSource::Invoke(exposed) => {
                for id in exposed.values_mut() {
                    *id = ns(id);
                }
            }
        }
    }
}

/// Give a Step written in the Pipeline file a scope when it reads an
/// invoke's exposed output (ADR-0041 §2, ADR-0076): `outputs.<invoke>` is
/// a view of the invoke's backing Steps, `<invoke>/<output>`. Its `inputs`
/// stay the Run's launch parameters, and every other Step it reads is named
/// by its own id, as for a Step with no scope ([`compile_libs`] adds those
/// once its `needs` are final). The engine gives a Step the results of its
/// direct needs only, and `needs: [<invoke>]` reaches the library's leaves,
/// so the Step also needs each backing Step it reads, which need not be a
/// leaf. A read of an output the invoke does not expose, or without `needs:
/// [<invoke>]`, is reported by `validate_interface`.
fn invoke_output_scope(step: &mut StepSpec, exposed: &BTreeMap<String, Option<Vec<String>>>) {
    let mut view = BTreeMap::new();
    let mut backing = Vec::new();
    for text in interpolatable_strings(step) {
        for i in cel::scan_interpolations(text).0 {
            let Ok(reads) = cel::reads(text[i.inner].trim()) else {
                continue;
            };
            for r in reads.into_iter().filter(|r| r.var == "outputs") {
                let Some(name) = r.first else { continue };
                let Some(Some(outs)) = exposed.get(&name) else {
                    continue;
                };
                for out in outs {
                    if r.second.as_ref().is_none_or(|f| f == out) {
                        backing.push(format!("{name}/{out}"));
                    }
                }
                view.insert(
                    name.clone(),
                    OutputSource::Invoke(
                        outs.iter()
                            .map(|o| (o.clone(), format!("{name}/{o}")))
                            .collect(),
                    ),
                );
            }
        }
    }
    if view.is_empty() {
        return;
    }
    for b in backing {
        if !step.needs.0.contains(&b) {
            step.needs.0.push(b);
        }
    }
    step.scope = Some(Scope {
        outputs: view,
        ..Scope::run()
    });
}

/// What an invoke step carries onto each Step and gate it inlines
/// (ADR-0076 §1). Its `timeout`, `resources` and `placement_profiles` apply to
/// a Step that is not a gate, only where the library Step leaves the field
/// unset: the library's own value wins (`resources` per field, `cpu_millis`
/// and `memory_mib`). Its `when` becomes a [`Guard`] of each, read in the
/// invoke Step's own scope. `env` never carries, and `placement_profiles`
/// never carries onto a Step with `forge_token` (ADR-0076 §1): a placement
/// profile is an arbitrary Pod patch, so a caller could otherwise place a
/// trusted Step next to its own.
fn carry_invoke_fields(invoke: &StepSpec, inst: &mut StepSpec) {
    if !inst.is_gate() {
        if inst.timeout.is_none() {
            inst.timeout = invoke.timeout;
        }
        if inst.resources.cpu_millis.is_none() {
            inst.resources.cpu_millis = invoke.resources.cpu_millis;
        }
        if inst.resources.memory_mib.is_none() {
            inst.resources.memory_mib = invoke.resources.memory_mib;
        }
        if inst.placement_profiles.is_empty() && inst.forge_token.is_none() {
            inst.placement_profiles = invoke.placement_profiles.clone();
        }
    }
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
    // A default is a plain value. Only the caller's `with:` holds a template
    // the compiler renders; a `${{` in a default would reach the Step as
    // text, which is never what its author meant.
    for p in &iface.inputs {
        if p.default
            .as_ref()
            .is_some_and(|d| d.to_string().contains("${{"))
        {
            diagnostics.push(format!(
                "library `{key}`: input `{}` has a default holding `${{{{ … }}}}`; a default \
                 is a plain value, and only the caller's `with:` may hold an expression",
                p.name
            ));
        }
    }

    // An input that reaches a Step with `forge_token` must be a value the
    // library can check (ADR-0076 §2): a free string would let the caller
    // write into a Step that holds a forge credential.
    for (name, lib_step) in forge_token_input_reads(lib) {
        let Some(p) = iface.inputs.iter().find(|p| p.name == name) else {
            continue; // an undeclared input is reported where it is read
        };
        if p.r#type == ParamType::String && p.validate.is_none() {
            diagnostics.push(format!(
                "library `{key}`: input `{name}` reaches step `{lib_step}`, which has \
                 `forge_token`; declare it as a `boolean`, a `number`, a `choice`, or a \
                 `string` with a `validate:` rule"
            ));
        }
    }

    check_with(step, &format!("library `{key}`"), &iface.inputs, diagnostics);
    // Each exposed output must name a real library step.
    let lib_ids: BTreeSet<&str> = lib.steps.iter().map(|s| s.id.as_str()).collect();
    for out in &iface.outputs {
        let name = &out.name;
        // A library output is a Step's results: nothing checks a type on
        // it, so declaring one would promise what is not kept.
        if out.r#type != ParamType::String {
            diagnostics.push(format!(
                "step `{}`: library `{key}` declares output `{name}` as `{}`; a library \
                 output names a Step and is not typed (only a plugin gate's outputs are)",
                step.id,
                serde_json::to_value(out.r#type)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            ));
        }
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
    let exposed: BTreeSet<&str> = iface.outputs.iter().map(|o| o.name.as_str()).collect();
    let matrixed = step.matrix.is_some();
    for t in siblings {
        if t.id == step.id {
            continue;
        }
        for text in interpolatable_strings(t) {
            for i in cel::scan_interpolations(text).0 {
                let Ok(reads) = cel::reads(text[i.inner].trim()) else {
                    continue;
                };
                let fields = reads
                    .into_iter()
                    .filter(|r| r.var == "outputs" && r.first.as_deref() == Some(step.id.as_str()))
                    .filter_map(|r| r.second);
                for field in fields {
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
                            iface.output_names().join(", ")
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

/// Check an invoke step's `with:` against the inputs its target declares
/// (`what` names the target in messages): every required input present, a
/// literal that converts to its type, and no unknown extras.
fn check_with(step: &StepSpec, what: &str, inputs: &[ParamSpec], diagnostics: &mut Vec<String>) {
    // Every required input must be supplied; a literal value must coerce to
    // the declared type (and be a valid choice / pass `validate:`); no unknown
    // extras. A template is converted and checked when a Step launches.
    // Optional inputs (a `default`) may be omitted — the default is bound.
    for p in inputs {
        match step.with.get(&p.name) {
            None => {
                if p.required {
                    diagnostics.push(format!(
                        "step `{}`: missing required input `{}` for {what}",
                        step.id, p.name
                    ));
                }
            }
            Some(raw) if raw.contains("${{") => {}
            Some(raw) => {
                // `with:` values are authored as strings; coerce to the type.
                let supplied = serde_json::Value::String(raw.clone());
                if let Err(e) = params::resolve_one(p, &supplied) {
                    diagnostics.push(format!(
                        "step `{}`: input `{}` for {what}: {e}",
                        step.id, p.name
                    ));
                }
            }
        }
    }
    for k in step.with.keys() {
        if !inputs.iter().any(|p| &p.name == k) {
            diagnostics.push(format!(
                "step `{}`: unknown input `{k}` ({what} declares inputs: [{}])",
                step.id,
                inputs
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
}

/// Each `(input, step)` where a library Step with `forge_token` reads
/// `inputs.<input>` in its image, command or env, once per pair. A read of
/// `inputs` whole counts as a read of every input.
fn forge_token_input_reads(lib: &PipelineIr) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for step in lib.steps.iter().filter(|s| s.forge_token.is_some()) {
        let mut texts: Vec<&str> = vec![step.image.as_str()];
        texts.extend(step.command.iter().map(String::as_str));
        texts.extend(step.env.iter().map(|(_, v)| v.as_str()));
        for text in texts {
            for i in cel::scan_interpolations(text).0 {
                let Ok(reads) = cel::reads(text[i.inner].trim()) else {
                    continue;
                };
                for r in reads.into_iter().filter(|r| r.var == "inputs") {
                    match r.first {
                        Some(name) => {
                            out.insert((name, step.id.clone()));
                        }
                        None => {
                            for p in &lib.interface.inputs {
                                out.insert((p.name.clone(), step.id.clone()));
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// The step strings that may carry `${{ … }}` interpolations (image, command,
/// env values, and an invoke step's `with:` values, which its library's Steps
/// render) — the surfaces scanned for invoke-output references.
fn interpolatable_strings(step: &StepSpec) -> Vec<&str> {
    let mut out: Vec<&str> = vec![step.image.as_str()];
    out.extend(step.command.iter().map(String::as_str));
    out.extend(step.env.iter().map(|(_, v)| v.as_str()));
    out.extend(step.with.values().map(String::as_str));
    out
}

/// Is `s` an RFC-1123 DNS label (lowercase alnum + interior `-`, ≤63 chars)? A
/// shared service's name is used verbatim as a k8s Service name / cluster DNS
/// hostname (ADR-0058), so it must be a valid label.
fn is_dns_label(s: &str) -> bool {
    if s.is_empty() || s.len() > 63 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Validate a service `ready:` probe (ADR-0058), shared by the per-Step sidecar
/// and pipeline-level shared-service paths. `ctx` is the diagnostic prefix (e.g.
/// `` step `test` `` or `` service `db` ``). Absent probe = nothing to check.
fn validate_ready_probe(ready: &Option<ReadyProbe>, ctx: &str, diagnostics: &mut Vec<String>) {
    let Some(ready) = ready else { return };
    // Exactly one of tcp/exec/http (a probe with none set is meaningless; more
    // than one is ambiguous).
    if ready.forms_set() != 1 {
        diagnostics.push(format!(
            "{ctx}: a service `ready` probe must set exactly one of `tcp`/`exec`/`http`"
        ));
    }
    if ready.tcp == Some(0) {
        diagnostics.push(format!(
            "{ctx}: a service `ready.tcp` port must be greater than zero"
        ));
    }
    if ready.http.as_ref().is_some_and(|h| h.port == 0) {
        diagnostics.push(format!(
            "{ctx}: a service `ready.http.port` must be greater than zero"
        ));
    }
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
fn expand_step(step: &StepSpec, work: &mut MatrixWork) -> Result<Vec<StepSpec>, String> {
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
    // Refuse a product over the bound before building it. The cap on Steps
    // applies to the combinations `exclude` keeps, below.
    let combinations = matrix
        .dimensions
        .values()
        .try_fold(1usize, |n, v| n.checked_mul(v.len()));
    if combinations.is_none_or(|n| n > MAX_MATRIX_COMBINATIONS) {
        return Err(format!(
            "step `{}`: the matrix has more than {MAX_MATRIX_COMBINATIONS} combinations before \
             `exclude`; the limit is {MAX_MATRIX_COMBINATIONS}",
            step.id
        ));
    }

    let n = combinations.unwrap_or(0);
    // Every matrix of the compile (a library's, each time it is inlined)
    // counts against one bound.
    work.combinations = work.combinations.saturating_add(n);
    if work.combinations > MAX_MATRIX_COMBINATIONS_PER_COMPILE {
        return Err(format!(
            "step `{}`: the pipeline's matrices have more than \
             {MAX_MATRIX_COMBINATIONS_PER_COMPILE} combinations in all before `exclude`, \
             counting a library's each time it is inlined; the limit is \
             {MAX_MATRIX_COMBINATIONS_PER_COMPILE}",
            step.id
        ));
    }
    if matrix.exclude.len() > MAX_MATRIX_EXCLUDES {
        return Err(format!(
            "step `{}`: the matrix has {} `exclude` predicates; the limit is \
             {MAX_MATRIX_EXCLUDES}",
            step.id,
            matrix.exclude.len()
        ));
    }
    work.checks = work
        .checks
        .saturating_add(n.saturating_mul(matrix.exclude.len()));
    if work.checks > MAX_MATRIX_EXCLUDE_CHECKS {
        return Err(format!(
            "step `{}`: the pipeline's matrices need more than {MAX_MATRIX_EXCLUDE_CHECKS} \
             `exclude` checks in all (each matrix's combinations times its predicates, counting \
             a library's each time it is inlined); the limit is {MAX_MATRIX_EXCLUDE_CHECKS}",
            step.id
        ));
    }

    // A combination is a number: each dimension's value index, in the
    // (sorted) dimension order, the last changing fastest. Nothing is copied
    // per combination until the kept ones are known to fit.
    let dims: Vec<(&String, &Vec<String>)> = matrix.dimensions.iter().collect();
    let mut stride = vec![1usize; dims.len()];
    for k in (0..dims.len().saturating_sub(1)).rev() {
        stride[k] = stride[k + 1] * dims[k + 1].1.len();
    }
    let index = |c: usize, k: usize| (c / stride[k]) % dims[k].1.len();
    let values: Vec<Vec<std::sync::Arc<String>>> = dims
        .iter()
        .map(|(_, v)| v.iter().map(|s| std::sync::Arc::new(s.clone())).collect())
        .collect();

    // Drop combinations excluded by a CEL predicate (evaluated against the
    // combination's own coordinate, each dimension bound as a variable). A
    // predicate is parsed once and evaluated once per distinct value of the
    // dimensions it reads, among the combinations still kept.
    let mut keep = vec![true; n];
    for pred in &matrix.exclude {
        let reads = cel::reads(pred).map_err(|e| format!("step `{}`: {e}", step.id))?;
        let read: BTreeSet<&str> = reads.iter().map(|r| r.var.as_str()).collect();
        let mut fixed = Vec::new();
        let mut varying = Vec::new();
        for (k, (name, v)) in dims.iter().enumerate() {
            if !read.contains(name.as_str()) {
                continue;
            }
            if v.len() == 1 {
                fixed.push((name.as_str(), values[k][0].clone()));
            } else {
                varying.push(k);
            }
        }
        // A point: the read dimensions' value indexes, the last fastest.
        let point = |c: usize| {
            varying
                .iter()
                .fold(0usize, |p, &k| p * dims[k].1.len() + index(c, k))
        };
        let size: usize = varying.iter().map(|&k| dims[k].1.len()).product();
        let mut wanted = vec![false; size];
        let mut points = Vec::new();
        for c in (0..n).filter(|&c| keep[c]) {
            let p = point(c);
            if !wanted[p] {
                wanted[p] = true;
                points.push(p);
            }
        }
        let read_dims: Vec<cel::Dim<'_>> = varying
            .iter()
            .map(|&k| cel::Dim {
                name: dims[k].0,
                values: &values[k],
            })
            .collect();
        let excluded = cel::eval_bool_product(pred, &fixed, &read_dims, &points)
            .map_err(|e| format!("step `{}`: {e}", step.id))?;
        let mut table = vec![false; size];
        for (p, out) in points.into_iter().zip(excluded) {
            table[p] = out;
        }
        for (c, k) in keep.iter_mut().enumerate() {
            if *k && table[point(c)] {
                *k = false;
            }
        }
    }
    let kept: Vec<usize> = (0..n).filter(|&c| keep[c]).collect();
    if kept.len() > MAX_COMPILED_STEPS {
        return Err(too_many_steps(&format!("step `{}`", step.id)));
    }
    // Each kept combination is a copy of the Step plus its coordinate,
    // written three times (id, `matrix_values`, the scope's `matrix`):
    // refuse before copying.
    let each = json_len(step, MAX_COMPILED_IR_BYTES).unwrap_or(usize::MAX);
    let coordinate_len = |c: usize| -> usize {
        (0..dims.len())
            .map(|k| dims[k].0.len() + dims[k].1[index(c, k)].len() + 8)
            .sum()
    };
    let total = kept.iter().try_fold(0usize, |t, &c| {
        let next = t
            .saturating_add(each)
            .saturating_add(coordinate_len(c).saturating_mul(3));
        (next <= MAX_COMPILED_IR_BYTES).then_some(next)
    });
    if total.is_none() {
        return Err(ir_too_large(&format!("step `{}`", step.id)));
    }

    Ok(kept
        .into_iter()
        .map(|c| {
            let coord: BTreeMap<String, String> = (0..dims.len())
                .map(|k| (dims[k].0.clone(), dims[k].1[index(c, k)].clone()))
                .collect();
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
            instance.matrix_values.extend(coord.clone());
            // A library Step's `matrix` is its own coordinate only (ADR-0076).
            if let Some(scope) = &mut instance.scope {
                scope.matrix = coord;
            }
            instance
        })
        .collect())
}

/// Whether an invoke link is the clone library (ADR-0076 §9): an installed
/// library named `clone`, under any publisher (`lib:thulasi-ram/clone`, or a
/// mirror's). The missing-checkout lint reads it. A name proves nothing about
/// where a library came from; this is a warning, and which Steps may hold a
/// forge credential is decided at Run creation from the installation's
/// content and source, never from its name.
pub fn is_clone_library(link: &OriginEntry) -> bool {
    link.kind == OriginKind::Library
        && link
            .name
            .rsplit_once('/')
            .is_some_and(|(_, name)| name == "clone")
}

/// Validate a compiled [`PipelineIr`], returning **all** discovered problems at
/// once. Checks: unique step ids, no unexpanded matrix (a compile-invariant),
/// `needs` resolve to real steps, and the `needs` graph is acyclic.
/// Non-fatal lint diagnostics over a compiled pipeline (ADR-0045) — the rules
/// behind `scarab lint`, also surfaced (as warnings, never failures) wherever
/// a pipeline compiles. First rule: a `push`/`pull_request` pipeline with no
/// checkout almost certainly forgot its source; some triggered pipelines
/// legitimately need none, hence a lint and not a hard error. A checkout is a
/// Step inlined from the clone library ([`is_clone_library`]). Repo-less
/// triggers (`cron`, `upstream`, …) never warn. Second rule: a Step inlined
/// from a library runs an image pinned only by tag (ADR-0076 §2).
pub fn lint(ir: &PipelineIr) -> Vec<String> {
    let mut warnings = Vec::new();
    let source_triggered = ir
        .triggers
        .0
        .keys()
        .any(|k| k == "push" || k == "pull_request");
    let has_checkout = ir
        .steps
        .iter()
        .any(|s| s.origin.iter().any(is_clone_library));
    if source_triggered && !has_checkout {
        warnings.push(
            "pipeline triggers on push/pull_request but does not check out the repository — \
             its steps will run without source; add `- { id: checkout, invoke: \
             lib:thulasi-ram/clone }` and depend on it via `needs`"
                .to_string(),
        );
    }
    // A `${{ … }}` in a shell script (ADR-0076): the value becomes code.
    warnings.extend(
        script_templates(ir)
            .into_iter()
            .filter(|(severity, _)| *severity == Severity::Warning)
            .map(|(_, finding)| finding),
    );
    // A library pins the images it runs by digest (ADR-0076 §2). One warning
    // per library and image, however many Steps or matrix legs use it.
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for step in &ir.steps {
        let Some(lib) = step.origin.last() else {
            continue;
        };
        let images = std::iter::once(step.image.as_str())
            .chain(step.services.iter().map(|svc| svc.image.as_str()));
        for image in images {
            // Empty (a gate, clone or build) or computed at launch: nothing
            // to check here.
            if image.is_empty() || image.contains("${{") || image.contains("@sha256:") {
                continue;
            }
            if seen.insert((lib.name.as_str(), image)) {
                warnings.push(format!(
                    "library `{}` runs image `{image}` pinned only by tag (step `{}`); pin it \
                     by digest (`{image}@sha256:…`) so the library runs the same image every \
                     time",
                    lib.name, step.id
                ));
            }
        }
    }
    warnings
}

/// Where a value that a template reads comes from (ADR-0076), for the checks
/// that treat a value from outside differently from the Pipeline's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueSource {
    /// Written in the Pipeline or a library file: a literal, a `matrix`
    /// coordinate, a literal `with:` value or an input's default.
    Pipeline,
    /// From outside those files: a Run's launch parameters, which whoever
    /// dispatches the Run supplies, a Step's results, a trigger event's
    /// fields, and a library input whose caller's `with:` template reads any
    /// of these, however many callers out.
    Outside,
}

/// How the Pipeline check reports a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A lint warning; the Pipeline still compiles.
    Warning,
    /// A compile error.
    Error,
}

/// What the Pipeline check does about a `${{ … }}` in a script, by where the
/// value comes from: the Pipeline's own value is a warning, a value from
/// outside is an error.
pub fn script_template_severity(source: ValueSource) -> Severity {
    match source {
        ValueSource::Pipeline => Severity::Warning,
        ValueSource::Outside => Severity::Error,
    }
}

/// The last path segment of a program: `sh` for `/bin/sh`.
fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// The index of the program a command runs once the wrappers in front of it
/// are skipped: `env` (with its options and `NAME=value` arguments),
/// `busybox`, `timeout` (with its duration), `nice`, `nohup`, `stdbuf`,
/// `setsid`, `sudo` and `exec`, each with its options and their arguments.
fn skip_wrappers(command: &[String]) -> usize {
    let mut i = 0;
    while let Some(program) = command.get(i) {
        // The options that take the next argument, whether `NAME=value`
        // arguments may follow, and how many operands come before the
        // program the wrapper runs.
        let (takes_arg, assigns, operands): (&[&str], bool, usize) = match basename(program) {
            "env" => (
                &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
                true,
                0,
            ),
            "sudo" => (
                &[
                    "-u",
                    "-g",
                    "-h",
                    "-p",
                    "-C",
                    "-D",
                    "-r",
                    "-t",
                    "-U",
                    "-T",
                    "-R",
                    "--user",
                    "--group",
                    "--host",
                    "--prompt",
                    "--close-from",
                    "--chdir",
                    "--role",
                    "--type",
                    "--other-user",
                    "--command-timeout",
                    "--chroot",
                ],
                true,
                0,
            ),
            "timeout" => (&["-s", "-k", "--signal", "--kill-after"], false, 1),
            "nice" => (&["-n", "--adjustment"], false, 0),
            "stdbuf" => (
                &["-i", "-o", "-e", "--input", "--output", "--error"],
                false,
                0,
            ),
            "exec" => (&["-a"], false, 0),
            "busybox" | "nohup" | "setsid" => (&[], false, 0),
            _ => return i,
        };
        i += 1;
        while let Some(arg) = command.get(i) {
            let arg = arg.as_str();
            if arg == "--" {
                i += 1;
                break;
            }
            if arg.starts_with('-') {
                i += if takes_arg.contains(&arg) { 2 } else { 1 };
            } else if assigns && arg.contains('=') {
                i += 1;
            } else {
                break;
            }
        }
        while assigns
            && command
                .get(i)
                .is_some_and(|a| a.contains('=') && !a.starts_with('-'))
        {
            i += 1;
        }
        i += operands;
    }
    i
}

/// The index of the script in a shell command (`sh`, `bash`, `dash`, ...):
/// the first argument after the options when one of them holds `c` (`-c`,
/// `-ec`, `-c -e`). `-o`, `-O`, `--rcfile` and `--init-file` take an
/// argument; an argument that is not an option, without `-c`, is a script
/// file.
fn shell_script(command: &[String], start: usize) -> Option<usize> {
    let mut has_c = false;
    let mut i = start;
    while let Some(arg) = command.get(i) {
        let arg = arg.as_str();
        match arg {
            "--" | "-" => {
                i += 1;
                break;
            }
            "--rcfile" | "--init-file" => i += 2,
            _ if arg.starts_with("--") => i += 1,
            _ if arg.len() > 1 && (arg.starts_with('-') || arg.starts_with('+')) => {
                let flags = &arg[1..];
                has_c |= flags.contains('c');
                // Each `o` or `O` in a group takes the next argument.
                i += 1 + flags.chars().filter(|c| matches!(c, 'o' | 'O')).count();
            }
            _ => break,
        }
    }
    (has_c && i < command.len()).then_some(i)
}

/// How an interpreter that is not a shell takes a script on its command
/// line (`python -c`, `node -e`, `perl -e`, `ruby -e`).
struct ScriptFlags {
    /// The short options whose argument is the script.
    script: &'static [char],
    /// Long options whose argument is the script (`--eval`), next or after
    /// `=`.
    long_script: &'static [&'static str],
    /// Short options that take an argument: the rest of their group, or the
    /// next argument when the group ends with them.
    takes_arg: &'static [char],
    /// Long options that take the next argument.
    long_takes_arg: &'static [&'static str],
    /// Short options after which no script follows (`python -m`).
    stop: &'static [char],
    /// Whether a short option's argument may be written in its own group
    /// (`python -c'…'`). `node` reads none so: `-pe` is `-p -e`, and the
    /// script is the next argument.
    attached: bool,
}

const PYTHON: ScriptFlags = ScriptFlags {
    script: &['c'],
    long_script: &[],
    takes_arg: &['W', 'X'],
    long_takes_arg: &[],
    stop: &['m'],
    attached: true,
};
const NODE: ScriptFlags = ScriptFlags {
    script: &['e', 'p'],
    long_script: &["--eval", "--print"],
    takes_arg: &['r', 'C'],
    long_takes_arg: &[
        "--require",
        "--conditions",
        "--import",
        "--loader",
        "--experimental-loader",
    ],
    stop: &[],
    attached: false,
};
const PERL: ScriptFlags = ScriptFlags {
    script: &['e', 'E'],
    long_script: &[],
    takes_arg: &['I', 'M', 'm', 'i', 'x', 'd', 'D'],
    long_takes_arg: &[],
    stop: &[],
    attached: true,
};
const RUBY: ScriptFlags = ScriptFlags {
    script: &['e'],
    long_script: &[],
    takes_arg: &['I', 'r', 'C', 'E', 'F', 'x', 'K', 'W', 'T', '0'],
    long_takes_arg: &[],
    stop: &[],
    attached: true,
};

/// The index of the script in an interpreter's command (see
/// [`ScriptFlags`]). A script written in the same argument as its option
/// (`python -c'…'`, `node --eval=…`) is that argument. An argument that is
/// not an option is a script file.
fn flag_script(command: &[String], start: usize, flags: &ScriptFlags) -> Option<usize> {
    let next = |i: usize| (i + 1 < command.len()).then_some(i + 1);
    let mut i = start;
    while let Some(arg) = command.get(i) {
        let arg = arg.as_str();
        if arg == "-" || arg == "--" || !arg.starts_with('-') {
            return None;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            let name = &arg[..2 + name.len()];
            if flags.long_script.contains(&name) {
                return if long.contains('=') { Some(i) } else { next(i) };
            }
            i += if !long.contains('=') && flags.long_takes_arg.contains(&name) {
                2
            } else {
                1
            };
            continue;
        }
        let group = &arg[1..];
        let mut skip = 1;
        for (at, c) in group.char_indices() {
            let rest = &group[at + c.len_utf8()..];
            if flags.stop.contains(&c) {
                return None;
            }
            if flags.script.contains(&c) {
                return if rest.is_empty() || !flags.attached {
                    next(i)
                } else {
                    Some(i)
                };
            }
            if flags.takes_arg.contains(&c) {
                if rest.is_empty() {
                    skip = 2;
                }
                break;
            }
        }
        i += skip;
    }
    None
}

/// The index of the script in a Step's command, after any wrappers: what a
/// shell reads with `-c`, `python -c`, `node -e`/`-p`, `perl -e`/`-E` and
/// `ruby -e` run. The interpreter is named by name or path (`/bin/sh`,
/// `python3.12`).
fn script_arg(command: &[String]) -> Option<(usize, &str)> {
    let at = skip_wrappers(command);
    let program = basename(command.get(at)?);
    let start = at + 1;
    let found = match program {
        "sh" | "bash" | "dash" | "zsh" | "ash" | "ksh" | "mksh" | "yash" | "busybox-sh" => {
            shell_script(command, start)
        }
        "node" | "nodejs" => flag_script(command, start, &NODE),
        "perl" => flag_script(command, start, &PERL),
        "ruby" => flag_script(command, start, &RUBY),
        _ if program
            .strip_prefix("python")
            .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.')) =>
        {
            flag_script(command, start, &PYTHON)
        }
        _ => None,
    }?;
    Some((found, program))
}

/// Each `${{ … }}` in a script a Step runs (ADR-0076), with its severity: the
/// rendered value is pasted into the script, so a value that holds `"; curl
/// …` runs. One finding per script text, at the severity of the value from
/// furthest outside that any Step running it reads.
fn script_templates(ir: &PipelineIr) -> Vec<(Severity, String)> {
    // Script text -> the finding, so a library Step inlined twice reports
    // once, at the severity of its worse caller.
    let mut found: Vec<(Severity, String)> = Vec::new();
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for step in &ir.steps {
        let Some((at, program)) = script_arg(&step.command) else {
            continue;
        };
        let script = step.command[at].as_str();
        let (spans, _) = cel::scan_interpolations(script);
        if spans.is_empty() {
            continue;
        }
        let sources = scope::InputSources::of(step.scope.as_ref(), &ir.interface.inputs);
        let each: Vec<(ValueSource, &str)> = spans
            .iter()
            .map(|i| {
                (
                    sources
                        .of_expr(script[i.inner.clone()].trim())
                        .value_source(),
                    &script[i.outer.clone()],
                )
            })
            .collect();
        let source = each
            .iter()
            .map(|(s, _)| *s)
            .max()
            .unwrap_or(ValueSource::Pipeline);
        let severity = script_template_severity(source);
        // The template named is the first one from as far outside.
        let shown = each
            .iter()
            .find(|(s, _)| *s == source)
            .map(|(_, t)| *t)
            .unwrap_or_default();
        let why = match source {
            ValueSource::Pipeline => "so the value becomes part of the script".to_string(),
            ValueSource::Outside => "which reads a value from outside the Pipeline (a Run \
                                     parameter, a Step's results or an event field), so that \
                                     value would run as part of the script"
                .to_string(),
        };
        let finding = format!(
            "step `{}`: `command[{at}]` is a script that `{program}` runs, with `{shown}` in it, \
             {why}; pass it in `env:` (`env: [[VALUE, \"{shown}\"]]`) and read it from the \
             environment (\"$VALUE\" in a shell script)",
            step.id
        );
        match index.get(script) {
            Some(&k) if found[k].0 >= severity => {}
            Some(&k) => found[k] = (severity, finding),
            None => {
                index.insert(script, found.len());
                found.push((severity, finding));
            }
        }
    }
    found
}

/// Each `${{ … }}` in a Step's `command` that reads free text from outside
/// the Pipeline (ADR-0076, 2026-10-30): a value that is not a `choice`,
/// boolean or number input, nor an `event` field of a fixed shape
/// ([`event::FIXED`]). The launch refuses it (see [`render`]); this
/// says so when the Pipeline compiles, naming the Step and the argument, and
/// for a library Step, the library and the caller's `with:` that binds the
/// input. One finding per template and caller, however many matrix legs or
/// Steps share it.
fn command_values(ir: &PipelineIr) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: BTreeSet<(String, usize, String)> = BTreeSet::new();
    for step in &ir.steps {
        if step.command.iter().all(|c| !c.contains("${{")) {
            continue;
        }
        let sources = scope::InputSources::of(step.scope.as_ref(), &ir.interface.inputs);
        for (index, arg) in step.command.iter().enumerate() {
            let (spans, _) = cel::scan_interpolations(arg);
            for i in spans {
                let expr = arg[i.inner.clone()].trim();
                if sources.of_expr(expr) != scope::Source::Free {
                    continue;
                }
                let shown = &arg[i.outer.clone()];
                // For a library Step: the input the caller binds to free text.
                let bound = step.scope.as_ref().and_then(|sc| {
                    let ScopeInputs::Bound(bindings) = &sc.inputs else {
                        return None;
                    };
                    cel::reads(expr).ok()?.into_iter().find_map(|r| {
                        let name = r.first.filter(|_| r.var == "inputs")?;
                        match &bindings.get(&name)?.value {
                            BoundValue::Template(t) if sources.input(&name) == scope::Source::Free => {
                                Some((name, t.clone()))
                            }
                            _ => None,
                        }
                    })
                });
                let (at, caller) = match (&bound, step.origin.last()) {
                    (Some((name, template)), Some(lib)) => {
                        let (invoke, own) = step.id.rsplit_once('/').unwrap_or(("", &step.id));
                        (
                            format!("library `{}`, step `{own}`", lib.name),
                            format!(
                                ", which step `{invoke}` binds with `with.{name}: {template}`",
                            ),
                        )
                    }
                    _ => (format!("step `{}`", step.id), String::new()),
                };
                // Matrix legs share a finding: the key leaves out each `[…]`.
                if !seen.insert((without_legs(&at) + &caller, index, shown.to_string())) {
                    continue;
                }
                found.push(format!(
                    "{at}: `command[{index}]` reads `{shown}`{caller}, a value from outside the \
                     Pipeline that is not of a fixed shape (a `choice`, boolean or number input, \
                     or `event.sha`, `event.number`, `event.issue`, `event.fork` or \
                     `event.kind`); Scarab puts no such value into a command. Pass it in `env:` \
                     (`env: [[VALUE, \"{shown}\"]]`) \
                     and read it from the environment (`\"$VALUE\"` in a shell script)"
                ));
            }
        }
    }
    found
}

/// `text` without its `[…]` parts: a matrix leg's id as its Step's.
fn without_legs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Validate one step's [`CacheSpec`] (ADR-0065 s1). All diagnostics are
/// author-fixable submit-time errors; nothing here is enforced at runtime
/// except the fetcher's source-wins backstop (which this validation exists to
/// make unreachable).
fn validate_cache(
    step_id: &str,
    cache: &CacheSpec,
    outputs: Option<&[String]>,
    diagnostics: &mut Vec<String>,
) {
    if cache.dirs.is_empty() {
        diagnostics.push(format!(
            "step `{step_id}`: `cache.dirs` must list at least one directory"
        ));
    }
    if cache.key.is_empty() {
        diagnostics.push(format!(
            "step `{step_id}`: `cache.key` must list at least one key file \
             (typically a lockfile) — a keyless cache would never invalidate"
        ));
    }
    for dir in &cache.dirs {
        let d = dir.trim_end_matches('/');
        // `=` and `,` are the separators of the `SCARAB_CACHE_ROOTS`
        // `dir=root,dir=root` env the fetcher parses; a dir name carrying
        // either would be unparseable on the other side.
        if d.contains('=') || d.contains(',') {
            diagnostics.push(format!(
                "step `{step_id}`: cache dir `{dir}` must not contain `=` or `,`"
            ));
            continue;
        }
        if d.is_empty() || dir.starts_with('/') {
            diagnostics.push(format!(
                "step `{step_id}`: cache dir `{dir}` must be workspace-relative \
                 (no leading `/`, non-empty)"
            ));
            continue;
        }
        // `.` would make the whole workspace a cache dir (clobbering the
        // clone, which lands at the workspace root); `..` escapes it; `.git*`
        // components would clobber source control state on restore.
        if d.split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c.starts_with(".git"))
        {
            diagnostics.push(format!(
                "step `{step_id}`: cache dir `{dir}` must not contain empty, \
                 `.`, `..`, or `.git*` path components"
            ));
        }
    }
    // Mutual prefix-overlap among cache dirs: restoring `a` and `a/b`
    // double-writes the same tree region in an order nothing defines.
    for (i, a) in cache.dirs.iter().enumerate() {
        for b in cache.dirs.iter().skip(i + 1) {
            if paths_overlap(a, b) {
                diagnostics.push(format!(
                    "step `{step_id}`: cache dirs `{a}` and `{b}` overlap — \
                     one contains the other"
                ));
            }
        }
    }
    // outputs ∩ cache.dirs (binding amendment #4): a path that is both
    // published downstream and cache-labelled scratch is a self-contradictory
    // label plan; refuse it at submit rather than resolving it silently.
    if let Some(outputs) = outputs {
        for out in outputs {
            for dir in &cache.dirs {
                if paths_overlap(out, dir) {
                    diagnostics.push(format!(
                        "step `{step_id}`: output `{out}` overlaps cache dir \
                         `{dir}` — a cached directory is scratch and cannot \
                         also be published (drop it from `outputs:` or from \
                         `cache.dirs`)"
                    ));
                }
            }
        }
    }
    for path in &cache.key {
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|c| c == "..") {
            diagnostics.push(format!(
                "step `{step_id}`: cache key file `{path}` must be \
                 workspace-relative (no leading `/`, no `..`)"
            ));
        }
    }
}

/// Do two workspace-relative paths name overlapping tree regions? True when
/// one's components are a prefix of the other's, where a component containing
/// `*` (outputs may be globs) conservatively matches anything.
fn paths_overlap(a: &str, b: &str) -> bool {
    let ac: Vec<&str> = a.trim_end_matches('/').split('/').collect();
    let bc: Vec<&str> = b.trim_end_matches('/').split('/').collect();
    ac.iter()
        .zip(bc.iter())
        .all(|(x, y)| x == y || x.contains('*') || y.contains('*'))
}

/// A Step's declared `results:` (ADR-0070 §7, kept by ADR-0074 §3), checked
/// at pipeline check:
///
/// - result names are non-empty, unique, and a plain file stem (no `/`, no
///   whitespace): each is `/scarab/results/<name>.json`;
/// - a gate writes no results;
/// - the fields ADR-0074 removed (`deployment_requests`, a gate's
///   `deployment:` reference) are refused with [`RETIRED_DEPLOYMENTS`].
fn validate_results(step: &StepSpec, diagnostics: &mut Vec<String>) {
    let mut declared = BTreeSet::new();
    for name in &step.results {
        if name.trim().is_empty() || name.contains('/') || name.chars().any(char::is_whitespace) {
            diagnostics.push(format!(
                "step `{}`: result name `{name}` must be a non-empty file stem (no `/`, no \
                 whitespace) — it is `/scarab/results/<name>.json`",
                step.id
            ));
        }
        if !declared.insert(name.as_str()) {
            diagnostics.push(format!(
                "step `{}`: result `{name}` is declared twice",
                step.id
            ));
        }
    }
    if step.is_gate() && !step.results.is_empty() {
        diagnostics.push(format!(
            "step `{}`: a gate step launches nothing — it must not declare `results`",
            step.id
        ));
    }
    if step.deployment_requests.is_some() {
        diagnostics.push(format!(
            "step `{}`: `deployment_requests`: {RETIRED_DEPLOYMENTS}",
            step.id
        ));
    }
    // `gate: deployment` reports once, from the gate-kind check; a stray
    // `deployment:` on any other step reports here.
    if step.deployment.is_some() && step.gate.as_deref() != Some("deployment") {
        diagnostics.push(format!(
            "step `{}`: `deployment`: {RETIRED_DEPLOYMENTS}",
            step.id
        ));
    }
}

/// Every `${{ outputs.<step>.<name> }}` read of a Step that declares
/// `results:` must name a declared result (ADR-0070 §7, kept by ADR-0074 §3),
/// so a typo fails the Pipeline check instead of the reading Step at launch.
/// A Step that declares nothing keeps the open ADR-0041 map and is not
/// checked. A read names a Step through the reader's scope (ADR-0076): a
/// library Step's `outputs.b` is its own library's `b`, never a Step of the
/// same name elsewhere.
fn validate_output_reads(ir: &PipelineIr, diagnostics: &mut Vec<String>) {
    // A plugin gate's results are its declared outputs (ADR-0076 §7), even
    // when it declares none.
    let declared: BTreeMap<&str, Vec<String>> = ir
        .steps
        .iter()
        .filter_map(|s| match &s.plugin_gate {
            Some(g) => Some((
                s.id.as_str(),
                g.outputs.iter().map(|o| o.name.clone()).collect(),
            )),
            None => (!s.results.is_empty()).then(|| (s.id.as_str(), s.results.clone())),
        })
        .collect();
    if declared.is_empty() {
        return;
    }
    for t in &ir.steps {
        // The templates written on `t`, with the `outputs` view they read
        // through (`None`: Step ids as written).
        let mut written: Vec<(&str, Option<&BTreeMap<String, OutputSource>>)> =
            interpolatable_strings(t)
                .into_iter()
                .map(|text| (text, t.scope.as_ref().map(|s| &s.outputs)))
                .collect();
        // A plugin gate's `with:` templates are kept as its input bindings
        // and render in the invoke step's scope when the gate is reached.
        if t.plugin_gate.is_some() {
            if let Some(scope) = &t.scope {
                if let (ScopeInputs::Bound(bindings), Some(caller)) =
                    (&scope.inputs, &scope.caller)
                {
                    for b in bindings.values() {
                        if let BoundValue::Template(text) = &b.value {
                            written.push((text.as_str(), Some(&caller.outputs)));
                        }
                    }
                }
            }
        }
        for (text, view) in written {
            for i in cel::scan_interpolations(text).0 {
                let Ok(reads) = cel::reads(text[i.inner].trim()) else {
                    continue;
                };
                for r in reads.into_iter().filter(|r| r.var == "outputs") {
                    let (Some(name), Some(field)) = (r.first, r.second) else {
                        continue;
                    };
                    let step = match view {
                        None => name.clone(),
                        Some(outputs) => match outputs.get(&name) {
                            Some(OutputSource::Step(id)) => id.clone(),
                            Some(OutputSource::Invoke(exposed)) => match exposed.get(&field) {
                                Some(id) => id.clone(),
                                None => continue,
                            },
                            None => continue,
                        },
                    };
                    if let Some(results) = declared.get(step.as_str()) {
                        if !results.contains(&field) {
                            diagnostics.push(format!(
                                "step `{}`: reads `outputs.{name}.{field}`, which step `{step}` \
                                 does not declare (it declares results: [{}])",
                                t.id,
                                results.join(", ")
                            ));
                        }
                    }
                }
            }
        }
    }
}

pub fn validate(ir: &PipelineIr) -> Result<(), Vec<String>> {
    let mut diagnostics = Vec::new();

    if ir.ir_version > IR_VERSION_MAX {
        diagnostics.push(format!(
            "`ir_version` {} is newer than this server understands ({IR_VERSION_MAX})",
            ir.ir_version
        ));
    }

    // Run budget (ADR-0047): opt-in, active-time-only; zero is nonsense.
    if ir.budget == Some(0) {
        diagnostics.push("`budget` must be greater than zero seconds".to_string());
    }

    // Retention (ADR-0065 s2): a profile is named, never defined, and an
    // empty name would silently resolve as "no profile" downstream.
    if ir
        .retention_profile
        .as_deref()
        .is_some_and(|p| p.trim().is_empty())
    {
        diagnostics
            .push("`retention_profile` must be a non-empty operator profile name".to_string());
    }

    // `${{ event.* }}` reads only the fields Scarab can check (ADR-0076 §9).
    event::check(ir, &mut diagnostics);

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
        // The `clone` kind is gone (ADR-0076 §9): say what replaces it.
        if step.clone.is_some() {
            diagnostics.push(format!("step `{}`: {RETIRED_CLONE}", step.id));
        }
        // Step kinds are mutually exclusive: a step is a gate, a build, or an
        // ordinary executed step — never two at once.
        if step.gate.is_some() && step.build.is_some() {
            diagnostics.push(format!(
                "step `{}`: `gate` and `build` are mutually exclusive step kinds",
                step.id
            ));
        }
        // A forge credential is for a Step that runs a Pod with an image the
        // Pipeline names: a gate runs nothing, and a build runs BuildKit,
        // which reads its registry credential, not the forge's (ADR-0076 §9).
        if step.forge_token.is_some() && (step.is_gate() || step.is_build()) {
            diagnostics.push(format!(
                "step `{}`: `forge_token` is only valid on a step that runs an image (not a \
                 gate or build step)",
                step.id
            ));
        }
        // The image and command of a Step that holds a forge credential are
        // written where the Step is written (ADR-0076 §2): a `${{ … }}` in
        // either would let a value from outside (a caller's input, a Step's
        // result, the trigger) choose what runs beside the token. Outside
        // values reach it only through `env:`.
        if step.forge_token.is_some() {
            let mut templated: Vec<String> = Vec::new();
            if step.image.contains("${{") {
                templated.push("`image`".to_string());
            }
            for (i, c) in step.command.iter().enumerate() {
                if c.contains("${{") {
                    templated.push(format!("`command[{i}]`"));
                }
            }
            for field in templated {
                diagnostics.push(format!(
                    "step `{}`: {field} holds `${{{{ … }}}}`, and the step has `forge_token`; \
                     its image and command must be written out in full, and a value from an \
                     input, a result or the trigger may reach it only through `env:`",
                    step.id
                ));
            }
            // An env value is data only under a name no program treats as
            // code: git runs `GIT_SSH_COMMAND`, the loader reads
            // `LD_PRELOAD`, a shell sources `BASH_ENV`. A value from outside
            // may therefore reach only a `SCARAB_*` name the Step's own
            // program reads, and not one Scarab sets itself.
            for (name, value) in &step.env {
                if !value.contains("${{") {
                    continue;
                }
                if !templated_env_name_ok(name) {
                    diagnostics.push(format!(
                        "step `{}`: env `{name}` holds `${{{{ … }}}}`, and the step has \
                         `forge_token`; a value from an input, a result or the trigger may \
                         reach only an env name of the form `SCARAB_<NAME>` (upper-case \
                         letters, digits and `_`), because other names (`PATH`, \
                         `LD_PRELOAD`, `GIT_SSH_COMMAND`, …) can make the value run as code",
                        step.id
                    ));
                } else if scarab_sets_env(name) {
                    diagnostics.push(format!(
                        "step `{}`: env `{name}` holds `${{{{ … }}}}`, and the step has \
                         `forge_token`; Scarab sets `{name}` itself, so pick another \
                         `SCARAB_<NAME>`",
                        step.id
                    ));
                }
            }
            // A Cache restores files an earlier Run's Steps saved under the
            // same key, and those may be the repository's own Steps.
            if step.cache.is_some() {
                diagnostics.push(format!(
                    "step `{}`: `cache` is not allowed on a step with `forge_token`: a cache \
                     restores files other Steps saved, and a forge credential is mounted beside \
                     them",
                    step.id
                ));
            }
        }
        // Placement (ADR-0055): profile names must be non-empty, and a raw
        // `k8s_overlay`, if present, must be a JSON object (a pod-spec fragment) —
        // never a scalar/array — so it can strategic-merge onto the Pod.
        if step.placement_profiles.iter().any(|p| p.trim().is_empty()) {
            diagnostics.push(format!(
                "step `{}`: `placement_profiles` contains an empty profile name",
                step.id
            ));
        }
        if let Some(overlay) = &step.k8s_overlay {
            if !overlay.is_object() {
                diagnostics.push(format!(
                    "step `{}`: `k8s_overlay` must be a mapping (a pod-spec fragment)",
                    step.id
                ));
            }
        }
        // Sidecar services (ADR-0058): a service co-locates in the *executed*
        // Step's Pod, so it is only meaningful on an ordinary step — never on a
        // gate (no Pod) or a build (rootless BuildKit).
        if !step.services.is_empty() && (step.is_gate() || step.is_build()) {
            diagnostics.push(format!(
                "step `{}`: `services` is only valid on an ordinary executed step \
                 (not a gate or build step)",
                step.id
            ));
        }
        for svc in &step.services {
            if svc.image.trim().is_empty() {
                diagnostics.push(format!(
                    "step `{}`: a `services` entry must name an `image`",
                    step.id
                ));
            }
            // A sidecar's `ready:` probe is validated by the shared helper (the
            // same rules a shared service's probe obeys).
            validate_ready_probe(&svc.ready, &format!("step `{}`", step.id), &mut diagnostics);
            // No probe + no port = nothing to gate on; still valid (the main
            // container simply starts immediately alongside the sidecar).
        }
        // Shared-service opt-in (ADR-0058): every `uses:` name must resolve to a
        // pipeline-level service. A gate launches no Pod, so it can reach nothing.
        if !step.uses.is_empty() && step.is_gate() {
            diagnostics.push(format!(
                "step `{}`: `uses` is only valid on a step that runs a Pod (not a gate)",
                step.id
            ));
        }
        for name in &step.uses {
            if !ir.services.iter().any(|s| &s.name == name) {
                diagnostics.push(format!(
                    "step `{}`: `uses` names unknown shared service `{name}` \
                     (declare it under pipeline-level `services:`)",
                    step.id
                ));
            }
        }
        // A build step runs the blessed rootless BuildKit image (ADR-0018):
        // the author names no image/command and needs no privilege (rootless
        // by construction).
        if let Some(build) = &step.build {
            if build.image.is_empty() {
                diagnostics.push(format!(
                    "step `{}`: a build step must name the `image:` to build",
                    step.id
                ));
            }
            if !step.image.is_empty() || !step.command.is_empty() {
                diagnostics.push(format!(
                    "step `{}`: a build step runs the blessed BuildKit image — it must not \
                     set an image or command",
                    step.id
                ));
            }
            if step.security.as_ref().is_some_and(|s| !s.is_baseline()) {
                diagnostics.push(format!(
                    "step `{}`: a build step is rootless by construction — it must not request \
                     privilege escalation",
                    step.id
                ));
            }
        }
        // Step kind: a gate launches nothing (image-less); every other step must
        // name an image. A gate with an image/command is a contradiction.
        match &step.gate {
            Some(kind) => {
                if kind == "deployment" {
                    diagnostics.push(format!(
                        "step `{}`: `gate: deployment`: {RETIRED_DEPLOYMENTS}",
                        step.id
                    ));
                } else if !GATE_KINDS.contains(&kind.as_str()) {
                    diagnostics.push(format!(
                        "step `{}`: unknown gate kind `{kind}` (expected one of {})",
                        step.id,
                        GATE_KINDS.join(", ")
                    ));
                }
                // A plugin gate names its releaser, which only the compiler
                // writes (ADR-0076 §7), and must end: nothing else answers
                // for a plugin that does not.
                match (kind.as_str(), &step.plugin_gate) {
                    ("plugin", None) => diagnostics.push(format!(
                        "step `{}`: a plugin gate comes only from `invoke: \
                         plugin:<installation>/<gate>`",
                        step.id
                    )),
                    ("plugin", Some(_)) if step.gate_expires_after.is_none() => {
                        diagnostics.push(format!(
                            "step `{}`: a plugin gate must set `gate_expires_after` (seconds)",
                            step.id
                        ))
                    }
                    ("plugin", Some(_))
                        if step
                            .gate_expires_after
                            .is_some_and(|s| s > MAX_PLUGIN_GATE_EXPIRES_AFTER_SECS) =>
                    {
                        diagnostics.push(format!(
                            "step `{}`: a plugin gate's `gate_expires_after` is at most \
                             {MAX_PLUGIN_GATE_EXPIRES_AFTER_SECS} seconds (30 days)",
                            step.id
                        ))
                    }
                    (other, Some(_)) if other != "plugin" => diagnostics.push(format!(
                        "step `{}`: `plugin_gate` belongs to a `gate: plugin` node only",
                        step.id
                    )),
                    _ => {}
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
                // A gate deploys nothing; the mark belongs on the Step that
                // makes the change (ADR-0074 §2).
                if step.deploy {
                    diagnostics.push(format!(
                        "step `{}`: a gate step launches nothing — it must not set `deploy` \
                         (mark the step that deploys)",
                        step.id
                    ));
                }
                // A gate executes nothing, so there is nothing to re-run.
                if step.retry.is_some() {
                    diagnostics.push(format!(
                        "step `{}`: a gate step launches nothing — it must not set `retry`",
                        step.id
                    ));
                }
                // A gate has no execution to deadline (`gate_after` covers
                // timer gates; gate *expiry* is `gate_expires_after`).
                if step.timeout.is_some() {
                    diagnostics.push(format!(
                        "step `{}`: a gate step launches nothing — it must not set `timeout`",
                        step.id
                    ));
                }
                // Gate expiry (ADR-0047): opt-in, positive.
                if step.gate_expires_after == Some(0) {
                    diagnostics.push(format!(
                        "step `{}`: `gate_expires_after` must be greater than zero seconds",
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
                if step.plugin_gate.is_some() {
                    diagnostics.push(format!(
                        "step `{}`: `plugin_gate` belongs to a `gate: plugin` node only",
                        step.id
                    ));
                }
                if step.image.is_empty() && !step.is_build() && step.clone.is_none() {
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
                // Gate expiry is only meaningful on a gate.
                if step.gate_expires_after.is_some() {
                    diagnostics.push(format!(
                        "step `{}`: `gate_expires_after` is only valid on a gate step",
                        step.id
                    ));
                }
                // A zero deadline would kill every step instantly (ADR-0047).
                if step.timeout == Some(0) {
                    diagnostics.push(format!(
                        "step `{}`: `timeout` must be greater than zero seconds",
                        step.id
                    ));
                }
                // And a deadline past [`MAX_STEP_TIMEOUT_SECS`] would outlive
                // the step's own drain credential (ticket 16a7768 item 3) —
                // refused at authoring time, not discovered 24h into the run.
                if step.timeout.is_some_and(|t| t > MAX_STEP_TIMEOUT_SECS) {
                    diagnostics.push(format!(
                        "step `{}`: `timeout` must be at most {MAX_STEP_TIMEOUT_SECS} seconds \
                         (23 hours) — the workspace token the step's drain authenticates \
                         with is capped at 24 hours, and a longer step would expire its own \
                         credential before its workspace could publish",
                        step.id
                    ));
                }
                // Retry budget bounds (ADR-0047): a liveness bound. Zero re-runs
                // is "no retry" (omit the field); an unbounded budget hides
                // flakiness and burns minutes on doomed code.
                if let Some(retry) = &step.retry {
                    if !(1..=10).contains(&retry.max) {
                        diagnostics.push(format!(
                            "step `{}`: `retry.max` must be between 1 and 10 (got {}) — retry re-runs \
                             the whole step at-least-once; enable only if the step is idempotent or \
                             fenced against a cooperating sink",
                            step.id, retry.max
                        ));
                    }
                }
            }
        }
        // Declared results (ADR-0070 §7) and the fields ADR-0074 removed.
        validate_results(step, &mut diagnostics);
        // Dangling needs.
        for need in &step.needs.0 {
            if !ids.contains(need.as_str()) {
                diagnostics.push(format!("step `{}`: needs unknown step `{need}`", step.id));
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
        // Keyed directory Cache (ADR-0065 s1): dirs and key files must be
        // workspace-relative and well-formed, dirs must not clobber each
        // other (mutual prefix-overlap) or the source (`.git*`, `.`), must
        // not carry `=`/`,` (they ride a `dir=root,…` env var), and must not
        // overlap the step's `outputs:` (a cached dir is scratch by
        // definition — also publishing it is a self-contradictory label plan).
        if let Some(cache) = &step.cache {
            validate_cache(&step.id, cache, step.outputs.as_deref(), &mut diagnostics);
        }
        // Submit-time CEL: `when:` guards and every `${{ … }}` interpolation
        // must parse now, not fail mid-run (ADR-0009).
        if let Some(When(expr)) = &step.when {
            if let Err(e) = cel::check(expr) {
                diagnostics.push(format!("step `{}`: when {e}", step.id));
            }
        }
        // An invoke Step's `when`, carried onto what it inlines (its own
        // `when` was checked where it is the Step's own).
        for g in step.guards.iter().filter(|g| g.caller_hops > 0) {
            if let Err(e) = cel::check(&g.when) {
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
        } else {
            // The group is fixed when the Run is created, from its launch
            // inputs and its `event` values: no Step has run, so there are
            // no `outputs`, and nothing else is there to read.
            let mut seen = BTreeSet::new();
            for i in cel::scan_interpolations(&c.group).0 {
                let Ok(reads) = cel::reads(c.group[i.inner].trim()) else {
                    continue;
                };
                for r in reads {
                    if matches!(r.var.as_str(), "inputs" | "event") || !seen.insert(r.var.clone())
                    {
                        continue;
                    }
                    diagnostics.push(if r.var == "outputs" {
                        "concurrency: group reads `outputs`; the group is fixed when the Run \
                         is created, before any Step has run"
                            .to_string()
                    } else {
                        format!(
                            "concurrency: group reads `{}`; a group reads only `inputs` and \
                             `event`",
                            r.var
                        )
                    });
                }
            }
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

    // Pipeline-level shared services (ADR-0058): each needs a unique,
    // DNS-label-safe name (it is also the cluster DNS hostname), an image, and a
    // well-formed `ready:` probe (the same rules a sidecar's probe obeys).
    let mut service_names = BTreeSet::new();
    for svc in &ir.services {
        if svc.name.trim().is_empty() {
            diagnostics.push("a pipeline-level `services` entry must set a `name`".to_string());
        } else {
            if !is_dns_label(&svc.name) {
                diagnostics.push(format!(
                    "service `{}`: name must be a DNS label (lowercase letters, digits, `-`; \
                     it is the cluster DNS hostname)",
                    svc.name
                ));
            }
            if !service_names.insert(svc.name.as_str()) {
                diagnostics.push(format!("duplicate shared service name `{}`", svc.name));
            }
        }
        if svc.spec.image.trim().is_empty() {
            diagnostics.push(format!(
                "service `{}`: a shared service must name an `image`",
                svc.name
            ));
        }
        validate_ready_probe(
            &svc.spec.ready,
            &format!("service `{}`", svc.name),
            &mut diagnostics,
        );
    }

    // Launch-parameter interface (ADR-0043): each declared param spec must be
    // well-formed — env-safe unique names, coherent required/default, a
    // non-empty `options` for a choice, a default within those options, and a
    // parsable `validate:` predicate.
    params::validate_param_specs(&ir.interface.inputs, "interface", &mut diagnostics);

    // `${{ outputs.<step>.<name> }}` reads against declared `results:`.
    validate_output_reads(ir, &mut diagnostics);

    // A `${{ … }}` in a shell script whose value the check refuses there.
    diagnostics.extend(
        script_templates(ir)
            .into_iter()
            .filter(|(severity, _)| *severity == Severity::Error)
            .map(|(_, finding)| finding),
    );
    // Free text from outside in a `command` (ADR-0076, 2026-10-30).
    diagnostics.extend(command_values(ir));

    // Cycle detection over needs edges (Kahn's algorithm). Only run when the
    // graph is well-formed enough to be meaningful (no dangling edges).
    if diagnostics.is_empty() {
        if let Some(cycle) = find_cycle(&ir.steps) {
            diagnostics.push(format!(
                "dependency cycle among steps: {}",
                cycle.join(" -> ")
            ));
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
            dependents
                .entry(need.as_str())
                .or_default()
                .push(step.id.as_str());
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
/// [`excluded_steps_with_params`] with no launch parameters (a webhook Run).
///
/// The run keeps the *full* DAG (edges intact) and marks these steps `Skipped`,
/// so the engine's dep cascade transitively skips their descendants (ADR-0033) —
/// pruning here does not drop edges, which is what makes transitive skip work.
pub fn excluded_steps(
    ir: &PipelineIr,
    ctx: &serde_json::Value,
) -> Result<Vec<String>, PipelineError> {
    excluded_steps_with_params(ir, ctx, &BTreeMap::new())
}

/// [`excluded_steps`] for a Run created with launch parameters `params`.
///
/// A Step written in the Pipeline file is decided by its `when:` against
/// `ctx` (the trigger event), as it always was. A Step or gate inlined from a
/// library is decided by its [`guards`](StepSpec::guards), outermost first,
/// each in the scope it was written in (ADR-0076): an invoke Step's `when` in
/// the Pipeline file against `ctx`, any other in its library scope, where a
/// caller's `with:` template renders with `params`.
pub fn excluded_steps_with_params(
    ir: &PipelineIr,
    ctx: &serde_json::Value,
    params: &BTreeMap<String, serde_json::Value>,
) -> Result<Vec<String>, PipelineError> {
    let mut excluded = Vec::new();
    for step in &ir.steps {
        let include = if step.origin.is_empty() {
            match &step.when {
                Some(When(expr)) => cel::eval_bool(expr, ctx)?,
                None => true,
            }
        } else {
            guards_hold(step, ctx, params)?
        };
        if !include {
            excluded.push(step.id.clone());
        }
    }
    Ok(excluded)
}

/// Whether every guard of an inlined Step holds, outermost first (an inner
/// guard is not evaluated once an outer one is false).
fn guards_hold(
    step: &StepSpec,
    ctx: &serde_json::Value,
    params: &BTreeMap<String, serde_json::Value>,
) -> Result<bool, PipelineError> {
    for g in step.guards.iter().rev() {
        let scope = step
            .scope
            .as_ref()
            .and_then(|s| s.at(g.caller_hops))
            .ok_or_else(|| {
                PipelineError::Cel(format!(
                    "step `{}`: a `when:` has no scope to be read in",
                    step.id
                ))
            })?;
        let holds = match scope.inputs {
            // Written in the Pipeline file: the trigger context, as a
            // Pipeline Step's own `when` reads.
            ScopeInputs::Run => cel::eval_bool(&g.when, ctx)?,
            ScopeInputs::Bound(_) => render::guard_holds(&g.when, scope, params)
                .map_err(|e| PipelineError::Cel(format!("step `{}`: {e}", step.id)))?,
        };
        if !holds {
            return Ok(false);
        }
    }
    Ok(true)
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
    fn parse_error(yaml: &str) -> String {
        match compile_yaml(yaml) {
            Err(PipelineError::Parse(msg)) => msg,
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    /// Zero, misplaced, and duplicated values are refused at authoring time,
    /// each with its own diagnostic.
    #[test]
    fn zero_misplaced_and_duplicate_fields_are_refused() {
        let cases = [
            (
                "budget: 0\nsteps: [{id: a, image: alpine}]",
                "`budget` must be greater than zero",
            ),
            (
                "concurrency: {group: ''}\nsteps: [{id: a, image: alpine}]",
                "group must not be empty",
            ),
            (
                "steps: [{id: a, image: alpine, timeout: 0}]",
                "`timeout` must be greater than zero",
            ),
            (
                "steps: [{id: a, image: alpine, gate_expires_after: 60}]",
                "only valid on a gate step",
            ),
            (
                "steps: [{id: g, gate: manual, timeout: 60}]",
                "must not set `timeout`",
            ),
            (
                "steps: [{id: g, gate: manual, gate_expires_after: 0}]",
                "`gate_expires_after` must be greater",
            ),
            (
                "steps: [{id: g, gate: manual, results: [r]}]",
                "must not declare `results`",
            ),
            (
                "steps: [{id: a, image: alpine, results: [r, r]}]",
                "declared twice",
            ),
            (
                "steps: [{id: a, image: alpine, results: ['a/b']}]",
                "non-empty file stem",
            ),
        ];
        for (yaml, want) in cases {
            let errs = errors(yaml);
            assert!(errs.iter().any(|e| e.contains(want)), "{yaml}: {errs:?}");
        }
    }

    /// The timeout ceiling (ticket 16a7768 item 3): a step past 23h would
    /// outlive its own 24h-capped workspace drain token — refused at
    /// authoring time. The ceiling itself is valid: the token still keeps its
    /// full post-timeout grace there.
    #[test]
    fn a_timeout_past_the_drain_token_ceiling_is_refused_and_the_ceiling_is_not() {
        let ds = errors(&format!(
            "steps: [{{ id: a, image: busybox, timeout: {} }}]",
            MAX_STEP_TIMEOUT_SECS + 1
        ));
        assert!(
            ds.iter()
                .any(|d| d.contains("at most") && d.contains("drain")),
            "the refusal must say WHY (the drain credential), got: {ds:?}"
        );
        let ir = compile(&format!(
            "steps: [{{ id: a, image: busybox, timeout: {MAX_STEP_TIMEOUT_SECS} }}]"
        ));
        assert_eq!(ir.steps[0].timeout, Some(MAX_STEP_TIMEOUT_SECS));
    }

    #[test]
    fn optional_name_parses_and_defaults_to_none() {
        let named = compile(
            r#"
            name: Continuous Integration
            steps:
              - id: build
                image: ghcr.io/acme/build@sha256:aaaa
            "#,
        );
        assert_eq!(named.name.as_deref(), Some("Continuous Integration"));

        let unnamed = compile(
            r#"
            steps:
              - id: build
                image: ghcr.io/acme/build@sha256:aaaa
            "#,
        );
        assert_eq!(
            unnamed.name, None,
            "absent name → caller falls back to the file name"
        );
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
    fn placement_fields_parse_and_round_trip() {
        let ir = compile(
            r#"
            ir_version: 1
            steps:
              - id: heavy
                image: busybox
                placement_profiles: [arm64, critical]
                resources: { cpu_millis: 8000, memory_mib: 16384 }
                k8s_overlay:
                  spec:
                    schedulerName: my-scheduler
            "#,
        );
        let s = &ir.steps[0];
        assert_eq!(
            s.placement_profiles,
            vec!["arm64".to_string(), "critical".to_string()]
        );
        assert_eq!(s.resources.cpu_millis, Some(8000));
        assert_eq!(s.resources.memory_mib, Some(16384));
        assert_eq!(
            s.k8s_overlay.as_ref().unwrap()["spec"]["schedulerName"],
            json!("my-scheduler")
        );
        // A step with no placement omits the fields on serialize.
        let bare = compile("steps: [{ id: a, image: busybox }]");
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("placement_profiles"));
        assert!(!json.contains("k8s_overlay"));
    }

    #[test]
    fn k8s_overlay_must_be_a_mapping() {
        let errs = errors("steps: [{ id: a, image: busybox, k8s_overlay: [1, 2] }]");
        assert!(errs
            .iter()
            .any(|e| e.contains("`k8s_overlay` must be a mapping")));
    }

    #[test]
    fn empty_placement_profile_name_is_rejected() {
        let errs = errors(r#"steps: [{ id: a, image: busybox, placement_profiles: ["", "x"] }]"#);
        assert!(errs.iter().any(|e| e.contains("empty profile name")));
    }

    /// ADR-0065 s2: the pipeline-level `retention_profile:` NAME rides into
    /// the IR untouched (that is how it reaches `runs.ir` for sweep-time
    /// resolution), is omitted from the serialized IR when absent, and an
    /// empty name is rejected (it would silently resolve as "no profile").
    #[test]
    fn retention_profile_rides_into_the_ir_and_an_empty_name_is_rejected() {
        let ir = compile(
            r#"
            retention_profile: keep-longer
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert_eq!(ir.retention_profile.as_deref(), Some("keep-longer"));
        let json = serde_json::to_string(&ir).unwrap();
        assert!(
            json.contains(r#""retention_profile":"keep-longer""#),
            "the name must survive into the stored IR: {json}"
        );
        let bare = compile("steps: [{ id: a, image: busybox }]");
        assert!(
            !serde_json::to_string(&bare)
                .unwrap()
                .contains("retention_profile"),
            "absent means absent — no null key in every stored IR"
        );
        let errs = errors("retention_profile: \"\"\nsteps: [{ id: a, image: busybox }]");
        assert!(errs.iter().any(|e| e.contains("retention_profile")));
    }

    /// The shared named-registry machinery (ADR-0065 consequence): one
    /// helper serves both operator profile kinds — lookup, the default flag,
    /// and registry validation (empty/duplicate names, two defaults).
    #[test]
    fn the_shared_profile_registry_helper_serves_both_profile_kinds() {
        let placement = vec![
            PlacementProfile {
                name: "arm64".into(),
                default: false,
                k8s: None,
            },
            PlacementProfile {
                name: "big".into(),
                default: true,
                k8s: None,
            },
        ];
        assert_eq!(
            profile_named(&placement, "arm64").map(|p| &*p.name),
            Some("arm64")
        );
        assert!(profile_named(&placement, "nope").is_none());
        assert_eq!(default_profile(&placement).map(|p| &*p.name), Some("big"));
        assert!(validate_profile_registry(&placement, "placement").is_ok());

        let retention = vec![
            RetentionProfile {
                name: "keep".into(),
                default: true,
                ..Default::default()
            },
            RetentionProfile {
                name: "keep".into(),
                default: true,
                ..Default::default()
            },
        ];
        let err = validate_profile_registry(&retention, "retention")
            .expect_err("duplicate names must be refused");
        assert!(
            err.contains("retention"),
            "the message names the registry: {err}"
        );
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
            diags
                .iter()
                .any(|d| d.contains("dimension `os` has no values")),
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

    /// `exclude` is evaluated once per distinct value of the dimensions a
    /// predicate reads, with a dimension of one value bound too, and only
    /// where an earlier predicate kept the combination; its length and its
    /// checks are capped.
    #[test]
    fn matrix_exclude_reads_its_dimensions_and_is_capped() {
        let ir = compile(
            r#"
            steps:
              - id: t
                image: rust
                matrix:
                  dimensions:
                    a: ["1", "2", "3"]
                    b: [x, y]
                    c: [k]
                  exclude:
                    - "a == '1'"
                    - "c == 'k' && b == 'y' && int(a) > 2"
                    - "int(a) == 2 && b == 'x'"
            "#,
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["t[a=2,b=y,c=k]", "t[a=3,b=x,c=k]"]);
        assert_eq!(ir.steps[0].matrix_values["c"], "k");

        // A predicate that errors where an earlier one excluded everything
        // is never evaluated; one that is not a boolean is an error.
        compile(
            r#"steps: [{ id: t, image: rust, matrix: { dimensions: { a: [x] }, exclude: ["true", "int(a) > 0"] } }]"#,
        );
        let e = errors(
            r#"steps: [{ id: t, image: rust, matrix: { dimensions: { a: [x] }, exclude: ["a"] } }]"#,
        );
        assert!(e[0].contains("expected a boolean"), "{e:?}");

        let many = ["\"a == 'z'\""; MAX_MATRIX_EXCLUDES + 1].join(", ");
        let e = errors(&format!(
            "steps: [{{ id: t, image: rust, matrix: {{ dimensions: {{ a: [x] }}, exclude: [{many}] }} }}]"
        ));
        assert!(
            e[0].contains(&format!("the limit is {MAX_MATRIX_EXCLUDES}")),
            "{e:?}"
        );

        let dims: Vec<String> = (0..16).map(|d| format!("d{d}: [a, b]")).collect();
        let preds = ["\"d0 == 'z'\""; MAX_MATRIX_EXCLUDE_CHECKS / 65_536 + 1].join(", ");
        let e = errors(&format!(
            "steps: [{{ id: t, image: rust, matrix: {{ dimensions: {{ {} }}, exclude: [{preds}] }} }}]",
            dims.join(", ")
        ));
        assert!(
            e[0].contains(&format!("the limit is {MAX_MATRIX_EXCLUDE_CHECKS}")),
            "{e:?}"
        );

        // The bounds are per compile: matrices that each fit, and exclude
        // every combination, still add up.
        let step = |i: usize, exclude: &str| {
            format!(
                "  - {{ id: s{i}, image: rust, matrix: {{ dimensions: {{ {} }}, exclude: [{exclude}] }} }}\n",
                dims.join(", ")
            )
        };
        let per = MAX_MATRIX_EXCLUDE_CHECKS / 65_536;
        let yaml: String = (0..=per).map(|i| step(i, "\"true\"")).collect();
        let e = errors(&format!("steps:\n{yaml}"));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].starts_with(&format!("step `s{per}`: the pipeline's matrices need more")),
            "{e:?}"
        );
        let per = MAX_MATRIX_COMBINATIONS_PER_COMPILE / 65_536;
        let yaml: String = (0..=per).map(|i| step(i, "")).collect();
        let e = errors(&format!("steps:\n{yaml}"));
        assert!(
            e.last()
                .unwrap()
                .starts_with(&format!("step `s{per}`: the pipeline's matrices have more")),
            "{e:?}"
        );
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
        assert!(
            excluded_steps(&ir, &json!({ "event": { "branch": "main" } }))
                .unwrap()
                .is_empty()
        );

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
        assert!(
            !matches_trigger(&ir, "push", &on_dev).unwrap(),
            "ref filtered out"
        );
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
        assert!(
            diags.iter().any(|d| d.contains("trigger `push`")),
            "got {diags:?}"
        );
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
        assert!(
            diags.iter().any(|d| d.contains("duplicate step id `a`")),
            "got {diags:?}"
        );
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
        let back: PipelineIr = serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
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
            diags
                .iter()
                .any(|d| d.contains("unknown policy `nonsense`")),
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
    fn a_concurrency_group_reads_only_inputs_and_event() {
        compile(
            r#"
            concurrency: { group: "deploy-${{ inputs.env }}-${{ event.branch }}" }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        for (group, want) in [
            ("deploy-${{ outputs.a.x }}", "reads `outputs`"),
            ("deploy-${{ secrets.X }}", "reads `secrets`"),
            ("deploy-${{ matrix.os }}", "reads `matrix`"),
        ] {
            let diags = errors(&format!(
                "concurrency: {{ group: \"{group}\" }}\nsteps: [{{ id: a, image: busybox }}]\n"
            ));
            assert!(diags.iter().any(|d| d.contains(want)), "{group}: {diags:?}");
        }
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
        let back: PipelineIr = serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
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
            diags
                .iter()
                .any(|d| d.contains("environment: target must not be empty")),
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
            unknown
                .iter()
                .any(|d| d.contains("unknown gate kind `whenever`")),
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
                errors(yaml)
                    .iter()
                    .any(|d| d.contains("only valid on a timer gate")),
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
            diags
                .iter()
                .any(|d| d.contains("input `c` is not among its needs")),
            "got {diags:?}"
        );
    }

    #[test]
    fn explicit_outputs_compile_and_must_be_workspace_relative() {
        let ir = compile(r#"steps: [{ id: build, image: rust, outputs: [dist/, VERSION] }]"#);
        let b = ir.steps.iter().find(|s| s.id == "build").unwrap();
        assert_eq!(
            b.outputs.as_deref(),
            Some(["dist/".to_string(), "VERSION".to_string()].as_slice())
        );

        // Absolute paths, `..` traversal, and an empty list are rejected.
        assert!(
            errors(r#"steps: [{ id: b, image: rust, outputs: ["/etc/passwd"] }]"#)
                .iter()
                .any(|d| d.contains("workspace-relative"))
        );
        assert!(
            errors(r#"steps: [{ id: b, image: rust, outputs: ["../x"] }]"#)
                .iter()
                .any(|d| d.contains("workspace-relative"))
        );
        assert!(errors(r#"steps: [{ id: b, image: rust, outputs: [] }]"#)
            .iter()
            .any(|d| d.contains("at least one path")));
    }

    /// ADR-0065 s1: a well-formed `cache:` compiles through; the malformed
    /// shapes are submit-time diagnostics, never runtime surprises.
    #[test]
    fn cache_compiles_and_its_validation_rejects_the_dangerous_shapes() {
        let ir = compile(
            r#"steps: [{ id: b, image: node, cache: { dirs: [node_modules, .cargo],
               key: [package-lock.json] } }]"#,
        );
        let b = ir.steps.iter().find(|s| s.id == "b").unwrap();
        let cache = b.cache.as_ref().expect("cache compiled through");
        assert_eq!(cache.dirs, vec!["node_modules", ".cargo"]);
        assert_eq!(cache.key, vec!["package-lock.json"]);

        // Empty dirs / empty key.
        assert!(
            errors(r#"steps: [{ id: b, image: n, cache: { dirs: [], key: [l] } }]"#)
                .iter()
                .any(|d| d.contains("at least one directory"))
        );
        assert!(
            errors(r#"steps: [{ id: b, image: n, cache: { dirs: [d], key: [] } }]"#)
                .iter()
                .any(|d| d.contains("at least one key file"))
        );
        // Dir-clobber shapes (amendment #2): `.`, `..`, `.git*`, absolute.
        for bad in [".", "a/..", ".git", ".github/x", "/abs"] {
            assert!(
                !errors(&format!(
                    r#"steps: [{{ id: b, image: n, cache: {{ dirs: ["{bad}"], key: [l] }} }}]"#
                ))
                .is_empty(),
                "cache dir {bad:?} must be rejected"
            );
        }
        // `=`/`,` would break the `dir=root,…` env the fetcher parses.
        for bad in ["a=b", "a,b"] {
            assert!(
                errors(&format!(
                    r#"steps: [{{ id: b, image: n, cache: {{ dirs: ["{bad}"], key: [l] }} }}]"#
                ))
                .iter()
                .any(|d| d.contains("`=` or `,`")),
                "cache dir {bad:?} must be rejected"
            );
        }
        // Mutual prefix-overlap among dirs.
        assert!(
            errors(r#"steps: [{ id: b, image: n, cache: { dirs: [a, a/b], key: [l] } }]"#)
                .iter()
                .any(|d| d.contains("overlap"))
        );
        // outputs ∩ cache.dirs (amendment #4), both directions + glob:
        // equality, an output INSIDE a cache dir, an output that is a PREFIX
        // of a cache dir, and a glob that covers it.
        for (outputs, dirs) in [
            ("[node_modules]", "[node_modules]"),
            ("[node_modules/pkg]", "[node_modules]"),
            ("[dist]", "[dist/cache]"),
            ("[\"*\"]", "[node_modules]"),
        ] {
            assert!(
                errors(&format!(
                    r#"steps: [{{ id: b, image: n, outputs: {outputs},
                       cache: {{ dirs: {dirs}, key: [l] }} }}]"#
                ))
                .iter()
                .any(|d| d.contains("overlaps cache dir")),
                "outputs {outputs} must conflict with cache dirs {dirs}"
            );
        }
        // Disjoint outputs COEXIST with a cache dir — the amendment rejects
        // overlap, never coexistence. (`compile`, not `errors`: the helper
        // panics on a pipeline that compiles, which is the point here.)
        let ok = compile(
            r#"steps: [{ id: b, image: n, outputs: [dist],
               cache: { dirs: [node_modules], key: [l] } }]"#,
        );
        let b = ok.steps.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(b.outputs.as_deref(), Some(["dist".to_string()].as_slice()));
        assert!(b.cache.is_some(), "the disjoint cache survives compile");
        // Key files are workspace-relative.
        assert!(
            errors(r#"steps: [{ id: b, image: n, cache: { dirs: [d], key: ["../x"] } }]"#)
                .iter()
                .any(|d| d.contains("workspace-relative"))
        );
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

    /// The `file:` libraries map. Each source is marked `kind: library`
    /// (ADR-0076 §2) unless it sets `kind:` itself; [`raw_libs`] does not
    /// mark them.
    fn libs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), as_library(v)))
            .collect()
    }

    fn raw_libs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// `src` with `kind: library` added at its own indentation, unless it
    /// already sets `kind:`.
    fn as_library(src: &str) -> String {
        if src.lines().any(|l| l.trim_start().starts_with("kind:")) {
            return src.to_string();
        }
        let indent: String = src
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.chars().take_while(|c| *c == ' ').collect())
            .unwrap_or_default();
        format!("{indent}kind: library\n{}", src.trim_start_matches('\n'))
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
              - { id: ci, invoke: file:.scarab/lib/ci.yaml, needs: [checkout] }
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
        let back: PipelineIr = serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn inlined_steps_and_gates_carry_their_origin_chain_outermost_first() {
        let outer = r#"
            steps:
              - { id: approve, gate: manual }
              - { id: db, invoke: file:.scarab/lib/db.yaml, needs: [approve] }
        "#;
        let inner = "steps: [{ id: migrate, image: busybox }]";
        let ir = compile_with(
            r#"
            steps:
              - { id: checkout, image: busybox }
              - { id: deploy, invoke: file:./.scarab/lib/deploy.yaml, needs: [checkout] }
            "#,
            &libs(&[
                (".scarab/lib/deploy.yaml", outer),
                (".scarab/lib/db.yaml", inner),
            ]),
        );
        let step = |id: &str| ir.steps.iter().find(|s| s.id == id).unwrap();
        let file = |name: &str, src: &str| OriginEntry {
            kind: OriginKind::File,
            name: name.to_string(),
            source: None,
            content_hash: sha256_hex(as_library(src).as_bytes()),
        };

        assert!(
            step("checkout").origin.is_empty(),
            "written in the file itself"
        );
        // A gate inlined from a library carries the library too.
        assert_eq!(
            step("deploy/approve").origin,
            vec![file(".scarab/lib/deploy.yaml", outer)]
        );
        assert_eq!(
            step("deploy/db/migrate").origin,
            vec![
                file(".scarab/lib/deploy.yaml", outer),
                file(".scarab/lib/db.yaml", inner)
            ]
        );
        // SHA-256 of the source text, as `sha256sum` prints it.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Stored IR keeps it.
        let back: PipelineIr = serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn an_authored_origin_is_a_compile_error_in_the_file_or_a_library() {
        let spoof =
            "origin: [{ kind: library, name: x, source: github.com/acme/ok, content_hash: '00' }]";
        let top = format!("steps: [{{ id: a, image: busybox, {spoof} }}]");
        let errs = errors_with(&top, &BTreeMap::new());
        assert!(
            errs.iter().any(
                |e| e.contains("step `a`") && e.contains("`origin` is written by the compiler")
            ),
            "{errs:?}"
        );

        let lib = format!("steps: [{{ id: b, image: busybox, {spoof} }}]");
        let errs = errors_with(
            "steps: [{ id: ci, invoke: file:.scarab/lib/ci.yaml }]",
            &libs(&[(".scarab/lib/ci.yaml", &lib)]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("step `b` in library `.scarab/lib/ci.yaml`")
                    && e.contains("`origin` is written by the compiler")),
            "{errs:?}"
        );
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
              - { id: mod, invoke: file:.scarab/lib/x.yaml, needs: [prep] }
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
        assert_eq!(
            after.needs.0,
            vec!["mod/c".to_string(), "mod/d".to_string()]
        );
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
            "steps: [{ id: ci, invoke: file:.scarab/lib/m.yaml }]",
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
              - { id: first, invoke: file:.scarab/lib/a.yaml }
              - { id: second, invoke: file:.scarab/lib/b.yaml, needs: [first] }
            "#,
            &libs(&[(".scarab/lib/a.yaml", a), (".scarab/lib/b.yaml", b)]),
        );
        let y = ir.steps.iter().find(|s| s.id == "second/y").unwrap();
        assert_eq!(
            y.needs.0,
            vec!["first/x".to_string()],
            "second's root chains onto first's leaf"
        );
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
            let yaml = format!("steps: [{{ id: s, invoke: \"file:{path}\" }}]");
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
            "steps: [{ id: s, invoke: file:.scarab/lib/absent.yaml }]",
            &BTreeMap::new(),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("no library found at `.scarab/lib/absent.yaml`")),
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
              - { id: db, invoke: file:.scarab/lib/db.yaml }
              - { id: app, image: busybox, needs: [db] }
        "#;
        let ir = compile_with(
            "steps: [{ id: deploy, invoke: file:.scarab/lib/deploy.yaml }]",
            &libs(&[
                (".scarab/lib/deploy.yaml", deploy),
                (".scarab/lib/db.yaml", db),
            ]),
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
        let selfref = "steps: [{ id: loop, invoke: file:.scarab/lib/self.yaml }]";
        let errs = errors_with(
            "steps: [{ id: s, invoke: file:.scarab/lib/self.yaml }]",
            &libs(&[(".scarab/lib/self.yaml", selfref)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("invoke cycle detected")
                && e.contains("file:.scarab/lib/self.yaml -> file:.scarab/lib/self.yaml")),
            "got {errs:?}"
        );
    }

    #[test]
    fn an_indirect_invoke_cycle_is_rejected_with_its_path() {
        // a -> b -> a.
        let a = "steps: [{ id: x, invoke: file:.scarab/lib/b.yaml }]";
        let b = "steps: [{ id: y, invoke: file:.scarab/lib/a.yaml }]";
        let errs = errors_with(
            "steps: [{ id: s, invoke: file:.scarab/lib/a.yaml }]",
            &libs(&[(".scarab/lib/a.yaml", a), (".scarab/lib/b.yaml", b)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("invoke cycle detected")
                && e.contains(
                    "file:.scarab/lib/a.yaml -> file:.scarab/lib/b.yaml -> file:.scarab/lib/a.yaml"
                )),
            "got {errs:?}"
        );
    }

    #[test]
    fn a_diamond_of_invokes_is_not_a_cycle() {
        // top invokes A and B, both invoke C — C appears twice but on separate
        // DFS paths, so it must NOT be rejected as a cycle.
        let c = "steps: [{ id: leaf, image: busybox }]";
        let a = "steps: [{ id: x, invoke: file:.scarab/lib/c.yaml }]";
        let b = "steps: [{ id: y, invoke: file:.scarab/lib/c.yaml }]";
        let ir = compile_with(
            r#"
            steps:
              - { id: pa, invoke: file:.scarab/lib/a.yaml }
              - { id: pb, invoke: file:.scarab/lib/b.yaml }
            "#,
            &libs(&[
                (".scarab/lib/a.yaml", a),
                (".scarab/lib/b.yaml", b),
                (".scarab/lib/c.yaml", c),
            ]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(&"pa/x/leaf") && ids.contains(&"pb/y/leaf"),
            "got {ids:?}"
        );
    }

    #[test]
    fn invoke_nesting_respects_the_depth_cap() {
        // Build a chain lib0 -> lib1 -> ... where libN invokes lib(N+1). The top
        // pipeline invokes lib0, so a chain of length L nests L levels deep.
        fn chain(len: usize) -> BTreeMap<String, String> {
            let mut m = BTreeMap::new();
            for i in 0..len {
                let src = if i + 1 < len {
                    format!(
                        "kind: library\nsteps: [{{ id: s{i}, invoke: file:.scarab/lib/l{}.yaml }}]",
                        i + 1
                    )
                } else {
                    format!("kind: library\nsteps: [{{ id: s{i}, image: busybox }}]")
                };
                m.insert(format!(".scarab/lib/l{i}.yaml"), src);
            }
            m
        }
        let top = "steps: [{ id: t, invoke: file:.scarab/lib/l0.yaml }]";

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

    // --- libraries through `invoke` (ADR-0076 step 1) --------------------------

    fn installed(entries: &[(&str, &str, &str)]) -> BTreeMap<String, InstalledLibrary> {
        entries
            .iter()
            .map(|(name, source, content)| {
                (
                    name.to_string(),
                    InstalledLibrary {
                        content: as_library(content),
                        source: source.to_string(),
                    },
                )
            })
            .collect()
    }

    fn library_errors(yaml: &str, libraries: &Libraries) -> Vec<String> {
        match compile_with_libraries(yaml, libraries) {
            Err(PipelineError::Validation(d)) => d,
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn invoke_targets_are_refused_with_a_reason() {
        let libraries = Libraries {
            files: libs(&[(".scarab/lib/a.yaml", "steps: [{ id: a, image: busybox }]")]),
            installed: BTreeMap::new(),
            plugin_gates: Default::default(),
        };
        for (target, needle) in [
            (
                ".scarab/lib/a.yaml",
                "`invoke: .scarab/lib/a.yaml` needs a prefix: write `invoke: file:.scarab/lib/a.yaml`",
            ),
            ("../a.yaml", "needs a prefix: `file:<path>`, `lib:<publisher>/<name>`"),
            ("https://evil.example/x.yaml", "unknown prefix `https:`"),
            ("git:acme/x", "unknown prefix `git:`"),
            ("url:https://evil.example/x.yaml", "`url:` is reserved"),
            ("file:", "must not be empty"),
            ("lib:acme", "must be `lib:<publisher>/<name>`"),
            ("lib:acme/x/y", "must be `lib:<publisher>/<name>`"),
            ("lib:/x", "must be `lib:<publisher>/<name>`"),
            ("lib:-acme/x", "must be `lib:<publisher>/<name>`"),
            ("lib:acme/.x", "must be `lib:<publisher>/<name>`"),
            ("lib:ac me/x", "must be `lib:<publisher>/<name>`"),
            ("lib:acme/missing", "`lib:acme/missing` is not installed in this org"),
            ("plugin:argocd", "must be `plugin:<installation>/<gate>`"),
            ("plugin:argocd/wait-live", "is not a gate of a plugin installed in this org"),
        ] {
            let errs = library_errors(
                &format!("steps: [{{ id: s, invoke: \"{target}\" }}]"),
                &libraries,
            );
            assert!(
                errs.iter().any(|e| e.starts_with("step `s`: ") && e.contains(needle)),
                "`{target}` should be refused with `{needle}`, got {errs:?}"
            );
        }
        // `gate_expires_after` belongs to a plugin gate only.
        let errs = library_errors(
            "steps: [{ id: s, invoke: file:.scarab/lib/a.yaml, gate_expires_after: 60 }]",
            &libraries,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("`gate_expires_after` is allowed on an invoke step only")),
            "{errs:?}"
        );
    }

    #[test]
    fn an_invoke_step_refuses_each_field_it_cannot_carry() {
        let l = libs(&[(".scarab/lib/a.yaml", "steps: [{ id: a, image: busybox }]")]);
        for (field, yaml) in [
            ("image", "image: busybox"),
            ("command", "command: [echo]"),
            ("env", "env: [[A, b]]"),
            ("secrets", "secrets: [TOKEN]"),
            ("security", "security: {}"),
            ("gate", "gate: manual"),
            ("gate_after", "gate_after: 5"),
            ("artifacts", "artifacts: ['*.tar']"),
            ("inputs", "inputs: []"),
            ("outputs", "outputs: [dist]"),
            ("cache", "cache: { key: [Cargo.lock], dirs: [target] }"),
            ("retry", "retry: { on: failure, max: 2 }"),
            ("k8s_overlay", "k8s_overlay: { spec: {} }"),
            ("services", "services: [{ image: postgres:16 }]"),
            ("uses", "uses: [db]"),
            ("results", "results: [v]"),
        ] {
            let errs = errors_with(
                &format!("steps: [{{ id: s, invoke: file:.scarab/lib/a.yaml, {yaml} }}]"),
                &l,
            );
            assert!(
                errs.iter()
                    .any(|e| e.contains(&format!("step `s`: an invoke step cannot set `{field}`"))),
                "`{field}` should be refused, got {errs:?}"
            );
        }
        // The fields it carries compile.
        compile_with(
            r#"
            steps:
              - { id: pre, image: busybox }
              - id: s
                invoke: file:.scarab/lib/a.yaml
                needs: [pre]
                matrix: { dimensions: { r: [eu, us] } }
                when: "true"
                deploy: true
                timeout: 60
                resources: { cpu_millis: 100 }
                placement_profiles: [big]
            "#,
            &l,
        );
    }

    #[test]
    fn a_library_file_is_not_a_pipeline() {
        // Compiled on its own, a library names the way to use it.
        let errs = errors("kind: library\nsteps: [{ id: a, image: busybox }]");
        assert!(
            errs.iter().any(|e| e.contains("cannot run as a Pipeline")
                && e.contains("`invoke: file:<its path>`")),
            "{errs:?}"
        );
        assert!(is_library("kind: library\nsteps: []"));
        assert!(!is_library("steps: []"));
        // A file that does not parse is not a library here; the compile says why.
        assert!(!is_library("steps: ["));
        // `library` is the only kind.
        assert!(parse_error("kind: pipeline\nsteps: []").contains("unknown variant"));

        // Every invoke target is marked as a library.
        let errs = errors_with(
            "steps: [{ id: s, invoke: file:.scarab/lib/a.yaml }]",
            &raw_libs(&[(".scarab/lib/a.yaml", "steps: [{ id: a, image: busybox }]")]),
        );
        assert!(
            errs.iter().any(|e| e
                .contains("`file:.scarab/lib/a.yaml` is not a library; add `kind: library` to it")),
            "{errs:?}"
        );
        // A library has no triggers.
        let errs = errors_with(
            "steps: [{ id: s, invoke: file:.scarab/lib/a.yaml }]",
            &libs(&[(
                ".scarab/lib/a.yaml",
                "on: { push: {} }\nsteps: [{ id: a, image: busybox }]",
            )]),
        );
        assert!(
            errs.iter().any(|e| e.contains("has `on:` triggers")),
            "{errs:?}"
        );
    }

    /// Launch `step` as the engine does: the one renderer, with the Run's
    /// launch parameters and the results of the Step's needs.
    fn launch(
        step: &StepSpec,
        params: serde_json::Value,
        results: serde_json::Value,
    ) -> Result<Rendered, String> {
        let params: BTreeMap<String, serde_json::Value> = serde_json::from_value(params).unwrap();
        let results: Results = serde_json::from_value(results).unwrap();
        render(
            Templates {
                image: &step.image,
                command: &step.command,
                env: &step.env,
            },
            step.scope.as_ref(),
            &step.matrix_values,
            Facts::Launch {
                params: &params,
                declared: &[],
                results: &results,
                event: &serde_json::Map::new(),
            },
        )
    }

    #[test]
    fn library_inputs_bind_to_with_through_nested_invokes() {
        let outer = r#"
            interface:
              inputs:
                - { name: region, type: string }
                - { name: canary, type: boolean, required: false, default: false }
            steps:
              - id: a
                image: "registry/app:${{ inputs.region }}"
                command: [echo, "${{ inputs.region }}"]
                env: [[R, "r-${{ inputs.region }}"]]
                when: "inputs.region == 'eu' && !inputs.canary"
              - id: inner
                invoke: file:.scarab/lib/inner.yaml
                needs: [a]
                with: { zone: "${{ inputs.region }}-1" }
        "#;
        let inner = r#"
            interface:
              inputs:
                - { name: zone, type: string }
                - { name: count, type: number, required: false, default: 3 }
            steps:
              - id: b
                image: busybox
                command: ["${{ inputs.zone }}", "${{ inputs.count + 1 }}", "${{ 'z=' + inputs.zone }}"]
        "#;
        let ir = compile_with(
            r#"
            interface:
              inputs: [{ name: region, type: string }]
            steps:
              - { id: top, image: busybox, env: [[R, "${{ inputs.region }}"]] }
              - { id: d, invoke: file:.scarab/lib/outer.yaml, with: { region: eu } }
            "#,
            &libs(&[
                (".scarab/lib/outer.yaml", outer),
                (".scarab/lib/inner.yaml", inner),
            ]),
        );
        assert_eq!(ir.ir_version, IR_VERSION_SCOPED);
        let step = |id: &str| ir.steps.iter().find(|s| s.id == id).unwrap();
        // The Pipeline's own `inputs` are its launch parameters: untouched,
        // and no scope.
        let top = step("top");
        assert_eq!(top.scope, None);
        let run = json!({ "region": "RUN" });
        assert_eq!(
            launch(top, run.clone(), json!({})).unwrap().env.last(),
            Some(&("R".to_string(), "RUN".to_string()))
        );

        // Nothing is pasted: the library's text is as written.
        let a = step("d/a");
        assert_eq!(a.image, "registry/app:${{ inputs.region }}");
        assert_eq!(
            a.when,
            Some(When("inputs.region == 'eu' && !inputs.canary".into()))
        );
        let ScopeInputs::Bound(bound) = &a.scope.as_ref().unwrap().inputs else {
            panic!("a library Step's inputs are its bindings");
        };
        assert_eq!(bound["region"].value, BoundValue::Literal(json!("eu")));
        assert_eq!(bound["canary"].value, BoundValue::Literal(json!(false)));
        let r = launch(a, run.clone(), json!({})).unwrap();
        assert_eq!(r.image, "registry/app:eu");
        assert_eq!(r.command, ["echo", "eu"]);
        assert_eq!(
            r.env,
            [
                ("SCARAB_PARAM_CANARY".to_string(), "false".to_string()),
                ("SCARAB_PARAM_REGION".to_string(), "eu".to_string()),
                ("R".to_string(), "r-eu".to_string()),
            ]
        );

        // The nested `with:` is rendered in `d`'s scope, then typed.
        let b = step("d/inner/b");
        assert_eq!(b.command[0], "${{ inputs.zone }}");
        let r = launch(b, run, json!({})).unwrap();
        assert_eq!(r.command, ["eu-1", "4", "z=eu-1"]);
        assert_eq!(
            r.env,
            [
                ("SCARAB_PARAM_COUNT".to_string(), "3".to_string()),
                ("SCARAB_PARAM_ZONE".to_string(), "eu-1".to_string()),
            ]
        );
        // The guard is decided at Run creation, in the library's scope.
        assert!(excluded_steps(&ir, &json!({ "event": {} }))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_caller_value_is_data_in_the_library() {
        let lib = r#"
            interface:
              inputs: [{ name: msg, type: string }]
            steps:
              - { id: a, image: busybox, command: ["${{ inputs.msg + '' }}"], when: "inputs.msg != ''" }
        "#;
        for msg in ["a\"b\\c}}d\ne{x}\u{1}", "' || true || '", "}} ${{ true"] {
            let caller = format!(
                "steps: [{{ id: s, invoke: file:.scarab/lib/a.yaml, with: {{ msg: {} }} }}]",
                serde_json::to_string(msg).unwrap()
            );
            match compile_yaml_with_libs(&caller, &libs(&[(".scarab/lib/a.yaml", lib)])) {
                // A value holding `${{` is the caller's template.
                Err(e) => assert!(msg.contains("${{"), "{msg}: {e}"),
                Ok(ir) => {
                    let a = ir.steps.iter().find(|s| s.id == "s/a").unwrap();
                    let r = launch(a, json!({ "msg": "RUN", "secret": "RUN" }), json!({}));
                    assert_eq!(r.unwrap().command[0], msg);
                }
            }
        }
    }

    #[test]
    fn a_caller_output_is_read_in_the_callers_scope() {
        let lib = r#"
            interface:
              inputs: [{ name: image, type: string }]
            steps:
              - { id: run, image: "${{ inputs.image }}" }
              - { id: tag, image: "busybox", needs: [run], env: [[T, "${{ inputs.image + ':1' }}"]] }
        "#;
        let ir = compile_with(
            r#"
            steps:
              - { id: build, image: busybox, results: [image] }
              - id: deploy
                invoke: file:.scarab/lib/run.yaml
                needs: [build]
                with: { image: "${{ outputs.build.image }}" }
            "#,
            &libs(&[(".scarab/lib/run.yaml", lib)]),
        );
        let run = ir.steps.iter().find(|s| s.id == "deploy/run").unwrap();
        assert_eq!(run.image, "${{ inputs.image }}");
        assert_eq!(run.needs.0, vec!["build".to_string()]);
        let caller = run.scope.as_ref().unwrap().at(1).unwrap();
        assert_eq!(
            caller.outputs,
            BTreeMap::from([("build".into(), OutputSource::Step("build".into()))])
        );
        let results = json!({ "build": { "image": "registry/app@sha256:1" } });
        assert_eq!(
            launch(run, json!({}), results.clone()).unwrap().image,
            "registry/app@sha256:1"
        );
        // Read inside a larger expression, it is a value like any other.
        let tag = ir.steps.iter().find(|s| s.id == "deploy/tag").unwrap();
        assert!(tag.needs.0.contains(&"build".to_string()));
        assert_eq!(
            launch(tag, json!({}), results).unwrap().env.last(),
            Some(&("T".to_string(), "registry/app@sha256:1:1".to_string()))
        );

        // Reading the output of an invoke sibling through `with:` needs it.
        let errs = errors_with(
            r#"
            steps:
              - { id: build, invoke: file:.scarab/lib/b.yaml }
              - id: deploy
                invoke: file:.scarab/lib/run.yaml
                with: { image: "${{ outputs.build.image }}" }
            "#,
            &libs(&[
                (".scarab/lib/run.yaml", lib),
                (
                    ".scarab/lib/b.yaml",
                    "interface: { outputs: [image] }\nsteps: [{ id: image, image: busybox }]",
                ),
            ]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("reads `outputs.build.image` but does not `needs: [build]`")),
            "{errs:?}"
        );
        // A name that is no Step of the caller is refused.
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/run.yaml
                with: { image: "${{ outputs.ghost.image }}" }
            "#,
            &libs(&[(".scarab/lib/run.yaml", lib)]),
        );
        assert!(
            errs.iter().any(|e| e
                == "step `deploy`: `with:` reads `outputs.ghost`, which is not a Step of this \
                    Pipeline"),
            "{errs:?}"
        );
    }

    #[test]
    fn an_inputs_read_in_a_field_that_does_not_render_is_refused() {
        let step: StepSpec = serde_yaml::from_str(
            r#"
            id: s
            image: "${{ .inputs.x }}"
            command: ["${{ [1].map(inputs, inputs) }}"]
            when: "x.y || inputs.z"
            services: [{ image: "${{ 'inputs' }}" }]
            artifacts: ["${{ inputs.a"]
            results: ["${{ inputs. }}"]
            "#,
        )
        .unwrap();
        assert_eq!(
            uninterpolated_input_reads(&step),
            vec![
                "`artifacts[0]` reads `inputs`, but `artifacts` is not interpolated when the \
                 Step launches; a library can read its inputs only in image, command, env, when \
                 and a nested invoke's with:"
                    .to_string(),
                format!(
                    "`results[0]` mentions `inputs` and does not parse, so its reads cannot be \
                     checked: {}",
                    cel::check("inputs.").unwrap_err()
                ),
            ]
        );
    }

    #[test]
    fn a_caller_launch_parameter_reaches_a_library_through_with() {
        // In the Pipeline, `inputs` is the Run's: the caller may hand one to
        // a library, through two levels; the library never reads the Run's.
        let ir = compile_with(
            r#"
            interface: { inputs: [{ name: p, type: string }] }
            steps:
              - { id: c, invoke: file:l.yaml, with: { v: "${{ inputs.p }}" } }
            "#,
            &libs(&[
                (
                    "l.yaml",
                    r#"
                interface: { inputs: [{ name: v, type: string }] }
                steps:
                  - { id: a, image: busybox, env: [[V, "${{ inputs.v }}"]] }
                  - id: n
                    invoke: file:m.yaml
                    with: { w: "${{ inputs.v }}" }
                "#,
                ),
                (
                    "m.yaml",
                    r#"
                interface: { inputs: [{ name: w, type: string }] }
                steps: [{ id: b, image: busybox, env: [[V, "${{ inputs.w }}"]] }]
                "#,
                ),
            ]),
        );
        for id in ["c/a", "c/n/b"] {
            let s = ir.steps.iter().find(|s| s.id == id).unwrap();
            let r = launch(s, json!({ "p": "RUN-P", "v": "NO", "w": "NO" }), json!({}));
            assert_eq!(
                r.unwrap().env.last(),
                Some(&("V".to_string(), "RUN-P".to_string())),
                "{id}"
            );
        }
    }

    /// The command boundary at compile (ADR-0076, 2026-10-30): free text
    /// from outside in a `command` is an error naming the Step and the
    /// argument; for a library Step, the library and the caller's `with:`.
    /// A listed `choice`, a boolean, a number and the Pipeline's own values
    /// compile.
    #[test]
    fn free_text_from_outside_in_a_command_is_a_compile_error() {
        let pipeline = |command: &str| {
            format!(
                "interface: {{ inputs: [{{ name: ref, type: string }}, {{ name: region, type: choice, options: [eu, us] }}, {{ name: n, type: number }}, {{ name: b, type: boolean }}] }}\n\
                 steps:\n  - {{ id: build, image: busybox, results: [url] }}\n  \
                 - {{ id: s, image: busybox, needs: [build], command: {command}, matrix: {{ dimensions: {{ x: [one, two] }} }} }}"
            )
        };
        for (command, at, shown) in [
            (r#"[git, checkout, "${{ inputs.ref }}"]"#, 2, "${{ inputs.ref }}"),
            (r#"[deploy, "--ref=${{ inputs.ref }}"]"#, 1, "${{ inputs.ref }}"),
            (r#"[echo, "${{ outputs.build.url }}"]"#, 1, "${{ outputs.build.url }}"),
            (r#"[echo, "${{ size(inputs.ref) }}"]"#, 1, "${{ size(inputs.ref) }}"),
        ] {
            let errs = errors(&pipeline(command));
            let found: Vec<&String> = errs.iter().filter(|e| e.contains("`command[")).collect();
            // One finding for both matrix legs.
            assert_eq!(found.len(), 1, "{command}: {errs:?}");
            assert!(
                found[0].starts_with("step `s[x=one]`: ")
                    && found[0].contains(&format!(
                        "`command[{at}]` reads `{shown}`, a value from outside"
                    ))
                    && found[0].contains("Pass it in `env:`"),
                "{command}: {errs:?}"
            );
        }
        compile(&pipeline(
            r#"[deploy, "${{ inputs.region }}", "n=${{ inputs.n }}", "${{ inputs.b }}", "${{ inputs.n > 80 }}", "${{ matrix.x }}"], env: [[REF, "${{ inputs.ref }}"]]"#,
        ));

        // A library: whether it compiles depends on what its caller binds.
        let lib = r#"
            kind: library
            interface: { inputs: [{ name: v, type: string }, { name: k, type: number }] }
            steps: [{ id: a, image: busybox, command: [tool, "${{ inputs.v }}", "${{ inputs.k }}"] }]
        "#;
        let files = libs(&[("l.yaml", lib)]);
        let caller = |with: &str| {
            format!(
                "interface: {{ inputs: [{{ name: ref, type: string }}, {{ name: n, type: number }}] }}\n\
                 steps: [{{ id: c, invoke: file:l.yaml, with: {with} }}]"
            )
        };
        let errs = errors_with(&caller(r#"{ v: "${{ inputs.ref }}", k: "${{ inputs.n }}" }"#), &files);
        assert_eq!(
            errs.iter().filter(|e| e.contains("`command[")).collect::<Vec<_>>(),
            [&"library `l.yaml`, step `a`: `command[1]` reads `${{ inputs.v }}`, which step `c` \
               binds with `with.v: ${{ inputs.ref }}`, a value from outside the Pipeline that is \
               not of a fixed shape (a `choice`, boolean or number input, or `event.sha`, \
               `event.number`, `event.issue`, `event.fork` or `event.kind`); Scarab puts no such \
               value into a command. Pass it in `env:` (`env: [[VALUE, \"${{ inputs.v }}\"]]`) \
               and read it from the environment (`\"$VALUE\"` in a shell script)"
                .to_string()],
        );
        // A literal `with:`, or a caller's number: compiles.
        compile_with(&caller(r#"{ v: main, k: "${{ inputs.n }}" }"#), &files);
    }

    #[test]
    fn a_library_reads_only_the_inputs_it_declares() {
        for (field, yaml) in [
            ("command[0]", r#"command: ["${{ inputs.ghost }}"]"#),
            ("when", r#"when: "inputs.ghost == 'x'""#),
            ("env[0][1]", r#"env: [[A, "${{ 'x' + inputs['ghost'] }}"]]"#),
        ] {
            let lib = format!(
                "interface: {{ inputs: [{{ name: known, type: string }}] }}\n\
                 steps: [{{ id: a, image: busybox, {yaml} }}]"
            );
            let errs = errors_with(
                "steps: [{ id: s, invoke: file:.scarab/lib/a.yaml, with: { known: k } }]",
                &libs(&[(".scarab/lib/a.yaml", &lib)]),
            );
            assert!(
                errs.iter().any(|e| e
                    == &format!(
                        "library `.scarab/lib/a.yaml`, step `a`: `{field}` reads `inputs.ghost`, \
                         which library `.scarab/lib/a.yaml` does not declare in \
                         `interface.inputs`"
                    )),
                "{field}: {errs:?}"
            );
        }
        // `inputs` used whole reads the library's own inputs.
        let ir = compile_with(
            "steps: [{ id: s, invoke: file:.scarab/lib/a.yaml, with: { x: X } }]",
            &libs(&[(
                ".scarab/lib/a.yaml",
                r#"
                interface: { inputs: [{ name: x, type: string }] }
                steps: [{ id: a, image: busybox, command: ["${{ inputs['x'] + string(size(inputs)) }}"] }]
                "#,
            )]),
        );
        let a = ir.steps.iter().find(|s| s.id == "s/a").unwrap();
        assert_eq!(
            launch(a, json!({ "x": "RUN", "y": 1 }), json!({}))
                .unwrap()
                .command,
            ["X1"]
        );
    }

    #[test]
    fn a_library_when_reads_only_what_exists_when_the_run_is_created() {
        let lib = |when: &str| {
            format!(
                "interface: {{ inputs: [{{ name: a, type: string }}, {{ name: b, type: string }}] }}\n\
                 steps:\n  - {{ id: x, image: busybox, results: [r] }}\n  - {{ id: s, image: busybox, needs: [x], when: {when:?} }}\n"
            )
        };
        let caller = r#"
            interface: { inputs: [{ name: p, type: string }] }
            steps:
              - { id: up, image: busybox, results: [v] }
              - { id: c, invoke: file:l.yaml, needs: [up], with: { a: "${{ outputs.up.v }}", b: "${{ inputs.p }}" } }
        "#;
        for (when, refused) in [
            ("outputs.x.r == 'y'", "`when` reads `outputs`"),
            (
                "inputs.a == 'y'",
                "`when` reads `inputs.a`, which the caller binds",
            ),
            (
                "size(inputs) > 0",
                "`when` reads `inputs.a`, which the caller binds",
            ),
        ] {
            let errs = errors_with(caller, &libs(&[("l.yaml", &lib(when))]));
            assert!(errs.iter().any(|e| e.contains(refused)), "{when}: {errs:?}");
        }
        // An input bound to a launch parameter is known at creation.
        let ir = compile_with(caller, &libs(&[("l.yaml", &lib("inputs.b == 'go'"))]));
        let params = BTreeMap::from([("p".to_string(), json!("go"))]);
        let ctx = json!({ "event": {} });
        assert!(excluded_steps_with_params(&ir, &ctx, &params)
            .unwrap()
            .is_empty());
        let params = BTreeMap::from([("p".to_string(), json!("stop"))]);
        assert_eq!(
            excluded_steps_with_params(&ir, &ctx, &params).unwrap(),
            ["c/s"]
        );
    }

    #[test]
    fn a_template_in_a_script_warns_for_the_pipelines_own_value_and_refuses_an_outside_one() {
        let yaml = |command: &str| {
            format!(
                "interface: {{ inputs: [{{ name: p, type: string }}] }}\n\
                 steps:\n  - {{ id: b, image: busybox, results: [v] }}\n  \
                 - {{ id: a, image: busybox, needs: [b], command: {command}, \
                 matrix: {{ dimensions: {{ x: [one] }} }} }}"
            )
        };
        // The Pipeline's own value (a matrix coordinate, a literal): a
        // warning, in every form of script.
        for command in [
            r#"[sh, -c, "echo ${{ matrix.x }}"]"#,
            r#"[/bin/bash, -ec, "echo ${{ matrix.x }}"]"#,
            r#"[bash, -o, pipefail, -xc, "echo ${{ matrix.x }}"]"#,
            r#"[dash, --norc, +e, -c, "echo ${{ 'lit' }}"]"#,
            r#"[bash, -c, -e, "echo ${{ matrix.x }}"]"#,
            r#"[env, -i, A=b, --, sh, -c, "echo ${{ matrix.x }}"]"#,
            r#"[sudo, -u, root, A=b, bash, -c, "echo ${{ matrix.x }}"]"#,
            r#"[nice, -n, "5", stdbuf, -oL, setsid, nohup, sh, -c, "echo ${{ matrix.x }}"]"#,
            r#"[timeout, -s, KILL, 60s, exec, -a, x, sh, -c, "echo ${{ matrix.x }}"]"#,
            r#"[python3.12, -B, -W, ignore, -c, "print('${{ matrix.x }}')"]"#,
            r#"[python, "-uc", "print('${{ matrix.x }}')"]"#,
            r#"[python, "-c print('${{ matrix.x }}')"]"#,
            r#"[node, -r, m, -e, "console.log('${{ matrix.x }}')"]"#,
            r#"[node, "--eval=console.log('${{ matrix.x }}')"]"#,
            r#"[nodejs, --print, "'${{ matrix.x }}'"]"#,
            r#"[perl, -Ilib, -ne, "print '${{ matrix.x }}'"]"#,
            r#"[ruby, -r, json, -e, "puts '${{ matrix.x }}'"]"#,
        ] {
            let ir = compile(&yaml(command));
            let w = lint(&ir);
            assert!(
                w.iter().any(|w| w.contains("`command[")
                    && w.contains("is a script that `")
                    && w.contains("pass it in `env:`")
                    && w.contains("\"$VALUE\"")),
                "{command}: {w:?}"
            );
        }
        // A value from outside: a Run parameter or a Step's results.
        for command in [
            r#"[sh, -c, "echo ${{ inputs.p }}"]"#,
            r#"[sh, -c, "echo ${{ matrix.x }} ${{ outputs.b.v }}"]"#,
            r#"[python3, -c, "${{ size(inputs) }}"]"#,
        ] {
            let errs = errors(&yaml(command));
            assert!(
                errs.iter()
                    .any(|e| e.contains("from outside the Pipeline")
                        && e.contains("pass it in `env:`")),
                "{command}: {errs:?}"
            );
        }
        // Not a script, a script file, a script with no template, a module.
        for command in [
            r#"[echo, "${{ matrix.x }}"]"#,
            r#"[sh, script.sh, "${{ matrix.x }}"]"#,
            r#"[sh, -c, "echo \"$VALUE\"", sh, "${{ matrix.x }}"]"#,
            r#"[sh, -e]"#,
            r#"[sh, -c]"#,
            r#"[python, -m, "${{ matrix.x }}"]"#,
            r#"[python, script.py, -c, "${{ matrix.x }}"]"#,
            r#"[node, --, "${{ matrix.x }}"]"#,
            r#"[perl, -pi, "${{ matrix.x }}"]"#,
            r#"[env, "${{ matrix.x }}"]"#,
        ] {
            let ir = compile(&yaml(command));
            assert!(
                !lint(&ir).iter().any(|w| w.contains("is a script")),
                "{command}"
            );
        }
        // A script given in `env:` is not run from the command line.
        compile(&yaml(r#"[sh, -c, "$S"], env: [[S, "${{ inputs.p }}"]]"#));
        assert_eq!(
            script_template_severity(ValueSource::Outside),
            Severity::Error
        );
        assert_eq!(
            script_template_severity(ValueSource::Pipeline),
            Severity::Warning
        );
    }

    /// A library script's value is from outside when its caller's `with:`
    /// reads a Run parameter, however many callers out; a literal `with:`
    /// value or a default is the Pipeline's own. One script inlined from two
    /// callers is reported at the worse caller's severity.
    #[test]
    fn a_library_scripts_value_is_classified_through_its_callers() {
        let inner = "kind: library\ninterface: { inputs: [{ name: v, type: string }, { name: d, type: string, required: false, default: x }] }\nsteps: [{ id: s, image: busybox, command: [sh, -c, \"echo ${{ inputs.v }}\"] }, { id: t, image: busybox, command: [sh, -c, \"echo ${{ inputs.d }}\"] }]\n";
        let outer = "kind: library\ninterface: { inputs: [{ name: w, type: string }] }\nsteps: [{ id: n, invoke: file:inner.yaml, with: { v: \"${{ inputs.w }}-x\" } }]\n";
        let files = libs(&[("inner.yaml", inner), ("outer.yaml", outer)]);
        let pipeline = |w: &str| {
            format!(
                "interface: {{ inputs: [{{ name: p, type: string }}] }}\nsteps:\n  - {{ id: lit, invoke: file:outer.yaml, with: {{ w: lit }} }}\n  - {{ id: run, invoke: file:outer.yaml, with: {{ w: \"{w}\" }} }}\n"
            )
        };
        // Both callers literal: warnings only.
        let ir = compile_with(&pipeline("also-lit"), &files);
        let w = lint(&ir);
        assert!(w.iter().any(|w| w.contains("lit/n/s")), "{w:?}");
        assert!(w.iter().any(|w| w.contains("lit/n/t")), "{w:?}");
        // One caller passes a Run parameter two levels down: an error, while
        // the default read by the other script stays a warning.
        let errs = errors_with(&pipeline("${{ inputs.p }}"), &files);
        assert!(
            errs.iter()
                .any(|e| e.contains("run/n/s") && e.contains("from outside the Pipeline")),
            "{errs:?}"
        );
        assert!(!errs.iter().any(|e| e.contains("/n/t")), "{errs:?}");
    }

    /// A Pipeline or library file over 1 MiB is refused, and so is a small
    /// file that compiles to more than 16 MiB: a library's default is stored
    /// on each Step it inlines, and a matrix copies its Step.
    #[test]
    fn source_and_compiled_sizes_are_capped() {
        let big = "x".repeat(MAX_SOURCE_BYTES);
        let errs = errors(&format!(
            "steps: [{{ id: a, image: busybox, command: [\"{big}\"] }}]"
        ));
        assert!(
            errs.iter()
                .any(|e| e.contains("the pipeline file is") && e.contains("(1 MiB)")),
            "{errs:?}"
        );
        let lib = format!("kind: library\n# {big}\nsteps: [{{ id: s, image: busybox }}]\n");
        let errs = errors_with(
            "steps: [{ id: c, invoke: file:l.yaml }]",
            &raw_libs(&[("l.yaml", &lib)]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("step `c`: library `l.yaml` is") && e.contains("(1 MiB)")),
            "{errs:?}"
        );

        let large = "d".repeat(600_000);
        let too_large = |errs: &[String]| {
            errs.iter()
                .any(|e| e.contains("more than 16777216 bytes (16 MiB)"))
        };
        // A 600 KB default, stored on each of 40 inlined Steps.
        let steps: String = (0..40)
            .map(|i| format!("  - {{ id: s{i}, image: busybox }}\n"))
            .collect();
        let lib = format!(
            "kind: library\ninterface: {{ inputs: [{{ name: v, type: string, required: false, default: \"{large}\" }}] }}\nsteps:\n{steps}"
        );
        let errs = errors_with(
            "steps: [{ id: c, invoke: file:l.yaml }]",
            &raw_libs(&[("l.yaml", &lib)]),
        );
        assert!(too_large(&errs), "{errs:?}");
        // A 600 KB Step with 32 matrix legs.
        let legs = (0..32)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let matrixed = format!(
            "kind: library\nsteps: [{{ id: s, image: busybox, command: [\"{large}\"], matrix: {{ dimensions: {{ n: [{legs}] }} }} }}]\n"
        );
        let errs = errors_with(
            "steps: [{ id: c, invoke: file:l.yaml }]",
            &raw_libs(&[("l.yaml", &matrixed)]),
        );
        assert!(too_large(&errs), "{errs:?}");
        // Two such Steps of 16 legs each: under the cap one at a time, over it
        // together.
        let legs = (0..16)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let half = format!(
            "kind: library\nsteps: [{{ id: s, image: busybox, command: [\"{large}\"], matrix: {{ dimensions: {{ n: [{legs}] }} }} }}]\n"
        );
        let errs = errors_with(
            "steps: [{ id: a, invoke: file:a.yaml }, { id: b, invoke: file:b.yaml }]",
            &raw_libs(&[("a.yaml", &half), ("b.yaml", &half)]),
        );
        assert!(too_large(&errs), "{errs:?}");
        // One of them compiles.
        compile_with(
            "steps: [{ id: a, invoke: file:a.yaml }]",
            &raw_libs(&[("a.yaml", &half)]),
        );
    }

    #[test]
    fn an_ir_version_newer_than_this_server_is_refused() {
        let errs = errors(&format!(
            "ir_version: {}\nsteps: [{{ id: a, image: busybox }}]",
            IR_VERSION_MAX + 1
        ));
        assert!(
            errs.iter()
                .any(|e| e.contains("newer than this server understands")),
            "{errs:?}"
        );
        // A Pipeline without a scoped Step keeps the version it had.
        assert_eq!(
            compile("steps: [{ id: a, image: busybox }]").ir_version,
            IR_VERSION
        );
    }

    #[test]
    fn a_rendered_image_must_be_an_image_reference() {
        let mut step = compile("steps: [{ id: a, image: busybox }]")
            .steps
            .pop()
            .unwrap();
        step.image = "${{ inputs.img }}".into();
        let e = launch(&step, json!({ "img": "busybox --privileged" }), json!({})).unwrap_err();
        assert!(e.contains("is not an OCI image reference"), "{e}");
        assert_eq!(
            launch(&step, json!({ "img": "busybox:1.36" }), json!({}))
                .unwrap()
                .image,
            "busybox:1.36"
        );
    }

    #[test]
    fn scope_and_guards_are_written_only_by_the_compiler() {
        for field in [
            "scope: { inputs: run }",
            "guards: [{ caller_hops: 0, when: 'true' }]",
        ] {
            let errs = errors(&format!("steps: [{{ id: a, image: busybox, {field} }}]"));
            let name = field.split(':').next().unwrap();
            assert!(
                errs.iter().any(|e| e.contains(&format!(
                    "step `a`: `{name}` is written by the compiler and cannot be set"
                ))),
                "{errs:?}"
            );
            let errs = errors_with(
                "steps: [{ id: c, invoke: file:l.yaml }]",
                &libs(&[(
                    "l.yaml",
                    &format!("steps: [{{ id: a, image: busybox, {field} }}]"),
                )]),
            );
            assert!(
                errs.iter().any(|e| e.contains(&format!(
                    "step `a` in library `l.yaml`: `{name}` is written by the compiler"
                ))),
                "{errs:?}"
            );
        }
    }

    #[test]
    fn an_invoke_step_carries_when_timeout_resources_and_placement() {
        let lib = r#"
            steps:
              - { id: x, image: busybox }
              - id: y
                image: busybox
                needs: [x]
                timeout: 60
                resources: { cpu_millis: 500 }
                placement_profiles: [gpu]
                when: "true"
              - { id: g, gate: manual, needs: [y] }
        "#;
        let ir = compile_with(
            r#"
            steps:
              - id: d
                invoke: file:.scarab/lib/a.yaml
                timeout: 1800
                resources: { cpu_millis: 1000, memory_mib: 256 }
                placement_profiles: [big]
                when: "event.branch == 'main'"
            "#,
            &libs(&[(".scarab/lib/a.yaml", lib)]),
        );
        let step = |id: &str| ir.steps.iter().find(|s| s.id == id).unwrap();
        let guard = "event.branch == 'main'";

        // Unset on the library Step: the invoke step's value applies.
        let x = step("d/x");
        assert_eq!(x.timeout, Some(1800));
        assert_eq!(
            x.resources,
            Resources {
                cpu_millis: Some(1000),
                memory_mib: Some(256)
            }
        );
        assert_eq!(x.placement_profiles, vec!["big"]);
        // The invoke `when` is a guard read in the invoke Step's scope (one
        // caller out); the library Step's own `when` stays as written.
        let outer = Guard {
            caller_hops: 1,
            when: guard.to_string(),
        };
        assert_eq!(x.when, None);
        assert_eq!(x.guards, vec![outer.clone()]);

        // Set on the library Step: the library wins, field by field; each
        // `when` is its own guard.
        let y = step("d/y");
        assert_eq!(y.timeout, Some(60));
        assert_eq!(
            y.resources,
            Resources {
                cpu_millis: Some(500),
                memory_mib: Some(256)
            }
        );
        assert_eq!(y.placement_profiles, vec!["gpu"]);
        assert_eq!(y.when, Some(When("true".to_string())));
        assert_eq!(
            y.guards,
            vec![
                Guard {
                    caller_hops: 0,
                    when: "true".into()
                },
                outer.clone()
            ]
        );

        // A gate gets only the `when`.
        let g = step("d/g");
        assert_eq!(g.timeout, None);
        assert!(g.placement_profiles.is_empty());
        assert_eq!(g.resources, Resources::default());
        assert_eq!(g.guards, vec![outer]);

        // The invoke Step's guard prunes the whole library.
        let ctx = json!({ "event": { "branch": "dev" } });
        let mut skipped = excluded_steps(&ir, &ctx).unwrap();
        skipped.sort();
        assert_eq!(skipped, vec!["d/g", "d/x", "d/y"]);
    }

    #[test]
    fn installed_libraries_are_closed_and_recorded_by_source() {
        let outer_src = "github.com/acme/scarab-libs//deploy.yaml@1111111";
        let inner_src = "github.com/acme/scarab-libs//helm.yaml@2222222";
        let outer = r#"
            steps:
              - { id: approve, gate: manual }
              - { id: helm, invoke: "lib:acme/helm", needs: [approve] }
        "#;
        let inner = "steps: [{ id: upgrade, image: alpine/helm@sha256:00 }]";
        let libraries = Libraries {
            files: libs(&[(
                ".scarab/lib/local.yaml",
                "steps: [{ id: deploy, invoke: \"lib:acme/deploy\" }]",
            )]),
            installed: installed(&[
                ("acme/deploy", outer_src, outer),
                ("acme/helm", inner_src, inner),
            ]),
            plugin_gates: Default::default(),
        };
        let ir = compile_with_libraries(
            "steps: [{ id: d, invoke: file:.scarab/lib/local.yaml }]",
            &libraries,
        )
        .expect("a file library may invoke installed ones");
        let lib_link = |name: &str, source: &str, content: &str| OriginEntry {
            kind: OriginKind::Library,
            name: name.to_string(),
            source: Some(source.to_string()),
            content_hash: sha256_hex(as_library(content).as_bytes()),
        };
        let local = OriginEntry {
            kind: OriginKind::File,
            name: ".scarab/lib/local.yaml".to_string(),
            source: None,
            content_hash: sha256_hex(libraries.files[".scarab/lib/local.yaml"].as_bytes()),
        };
        let step = |id: &str| ir.steps.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            step("d/deploy/helm/upgrade").origin,
            vec![
                local.clone(),
                lib_link("acme/deploy", outer_src, outer),
                lib_link("acme/helm", inner_src, inner)
            ]
        );
        assert_eq!(
            step("d/deploy/approve").origin,
            vec![local, lib_link("acme/deploy", outer_src, outer)]
        );

        // An installed library cannot invoke a repository file.
        let libraries = Libraries {
            files: libs(&[(".scarab/lib/x.yaml", "steps: [{ id: x, image: busybox }]")]),
            installed: installed(&[(
                "acme/open",
                outer_src,
                "steps: [{ id: inner, invoke: file:.scarab/lib/x.yaml }]",
            )]),
            plugin_gates: Default::default(),
        };
        let errs = library_errors("steps: [{ id: d, invoke: \"lib:acme/open\" }]", &libraries);
        assert!(
            errs.iter().any(|e| e.contains(
                "step `inner` in library `lib:acme/open`: an installed library can invoke only \
                 `lib:` and `plugin:` targets, not `file:.scarab/lib/x.yaml`"
            )),
            "{errs:?}"
        );

        // The thin wrapper sees no installed libraries.
        let errs = errors_with(
            "steps: [{ id: d, invoke: \"lib:acme/deploy\" }]",
            &BTreeMap::new(),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("`lib:acme/deploy` is not installed in this org")),
            "{errs:?}"
        );
    }

    #[test]
    fn lint_warns_on_a_library_image_pinned_only_by_tag() {
        let lib = r#"
            steps:
              - id: a
                image: alpine:3.20
                matrix: { dimensions: { n: ["1", "2"] } }
                services: [{ image: postgres:16 }]
              - { id: b, image: "alpine:3.20@sha256:0000" }
              - { id: c, image: "${{ 'alpine' }}" }
        "#;
        let ir = compile_with(
            r#"
            steps:
              - { id: top, image: busybox }
              - { id: d, invoke: file:.scarab/lib/a.yaml }
            "#,
            &libs(&[(".scarab/lib/a.yaml", lib)]),
        );
        let warnings = lint(&ir);
        let tagged: Vec<&String> = warnings
            .iter()
            .filter(|w| w.contains("pinned only by tag"))
            .collect();
        assert_eq!(
            tagged.len(),
            2,
            "one per library image, matrix legs folded: {warnings:?}"
        );
        assert!(tagged[0].contains("library `.scarab/lib/a.yaml` runs image `alpine:3.20`"));
        assert!(tagged[1].contains("`postgres:16`"));
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
                invoke: file:.scarab/lib/ci.yaml
                matrix: { dimensions: { svc: [api, web] } }
              - { id: gate, image: busybox, needs: [svc] }
            "#,
            &libs(&[(".scarab/lib/ci.yaml", lib)]),
        );
        let ids: Vec<&str> = ir.steps.iter().map(|s| s.id.as_str()).collect();
        // Each copy's ids carry both the coordinate and the invoke namespace,
        // uniquely.
        for id in [
            "svc[svc=api]/build",
            "svc[svc=api]/test",
            "svc[svc=web]/build",
            "svc[svc=web]/test",
        ] {
            assert!(ids.contains(&id), "missing {id} in {ids:?}");
        }
        // Internal edges rewrite per copy.
        let api_test = ir
            .steps
            .iter()
            .find(|s| s.id == "svc[svc=api]/test")
            .unwrap();
        assert_eq!(api_test.needs.0, vec!["svc[svc=api]/build".to_string()]);
        // The coordinate is visible to the inlined steps (for CEL interpolation).
        let api_build = ir
            .steps
            .iter()
            .find(|s| s.id == "svc[svc=api]/build")
            .unwrap();
        assert_eq!(
            api_build.matrix_values.get("svc").map(String::as_str),
            Some("api")
        );
        // Exit seam: `needs: [svc]` fans onto every copy's leaf.
        let gate = ir.steps.iter().find(|s| s.id == "gate").unwrap();
        assert_eq!(
            gate.needs.0,
            vec![
                "svc[svc=api]/test".to_string(),
                "svc[svc=web]/test".to_string()
            ]
        );
    }

    #[test]
    fn matrix_on_an_invoke_is_two_dimensional_and_honours_exclude() {
        let lib = "steps: [{ id: run, image: busybox }]";
        let ir = compile_with(
            r#"
            steps:
              - id: m
                invoke: file:.scarab/lib/x.yaml
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
        assert!(
            !ids.contains(&"m[arch=arm64,os=windows]/run"),
            "excluded combo absent"
        );
        assert!(ids.contains(&"m[arch=amd64,os=windows]/run"));
        assert!(ids.contains(&"m[arch=arm64,os=linux]/run"));
    }

    #[test]
    fn invoke_refs_lists_safe_fetchable_paths_only() {
        let yaml = r#"
            steps:
              - { id: a, invoke: file:./.scarab/lib/one.yaml }
              - { id: b, invoke: file:.scarab/lib/two.yaml }
              - { id: c, invoke: file:../escape.yaml }
              - { id: d, image: busybox }
              - { id: e, invoke: .scarab/lib/bare.yaml }
              - { id: f, invoke: lib:acme/deploy }
              - { id: g, invoke: plugin:argocd/wait-live }
        "#;
        // `./` is normalized; the traversal path, the bare path and the
        // `lib:`/`plugin:` targets are omitted: only `file:` targets are read
        // from the repository (ADR-0076 §1).
        assert_eq!(
            invoke_refs(yaml),
            vec![
                ".scarab/lib/one.yaml".to_string(),
                ".scarab/lib/two.yaml".to_string()
            ]
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
    fn a_satisfied_interface_compiles_injects_params_and_reads_output_refs() {
        let ir = compile_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
              - { id: notify, image: busybox, env: [[URL, "${{ outputs.deploy.url }}"]], needs: [deploy] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        // Inputs reach every inlined step as SCARAB_PARAM_* env (ADR-0008),
        // added when it launches.
        let plan = ir.steps.iter().find(|s| s.id == "deploy/plan").unwrap();
        assert!(plan.env.is_empty());
        let env = launch(plan, json!({}), json!({})).unwrap().env;
        assert!(env.contains(&("SCARAB_PARAM_REGION".to_string(), "us-east-1".to_string())));
        assert!(env.contains(&("SCARAB_PARAM_REPLICAS".to_string(), "3".to_string())));
        // The invoke step is gone; the exposed-output reference compiled fine.
        assert!(ir.steps.iter().all(|s| !s.is_invoke()));
        assert!(ir.steps.iter().any(|s| s.id == "deploy/url"));
        // The output reference is kept as written (ADR-0076): the reading
        // Step's scope names the backing Step, which it needs.
        let notify = ir.steps.iter().find(|s| s.id == "notify").unwrap();
        assert_eq!(notify.env[0].1, "${{ outputs.deploy.url }}");
        assert!(notify.needs.0.contains(&"deploy/url".to_string()));
        let r = launch(
            notify,
            json!({}),
            json!({ "deploy/url": { "url": "https://svc" }, "deploy/plan": { "url": "no" } }),
        )
        .unwrap();
        assert_eq!(
            r.env.last(),
            Some(&("URL".to_string(), "https://svc".to_string()))
        );
    }

    #[test]
    fn a_missing_required_input_is_rejected() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
                with: { region: us-east-1 }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("missing required input `replicas`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn an_unknown_extra_input_is_rejected() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3", bogus: x }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("unknown input `bogus`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn a_reference_to_an_undeclared_output_is_a_compile_error() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
              - { id: notify, image: busybox, command: ["echo", "${{ outputs.deploy.secret_ip }}"], needs: [deploy] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("references undeclared output `outputs.deploy.secret_ip`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn reading_an_output_without_needing_the_invoke_is_a_compile_error() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
                with: { region: us-east-1, replicas: "3" }
              - { id: notify, image: busybox, command: ["echo", "${{ outputs.deploy.url }}"] }
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", IFACE_LIB)]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("does not `needs: [deploy]`")),
            "got {errs:?}"
        );
    }

    #[test]
    fn referencing_the_output_of_a_matrixed_invoke_is_rejected() {
        let errs = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
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
    fn a_step_renders_outputs_and_matrix_and_fails_fast() {
        let mut spec = compile("steps: [{ id: notify, image: busybox }]")
            .steps
            .pop()
            .unwrap();
        spec.command = vec!["post".into(), "${{ matrix.region }}".into()];
        spec.env = vec![
            ("TARGET".into(), "${{ matrix.region }}".into()),
            ("URL".into(), r#"${{ outputs["deploy/url"].url }}"#.into()),
        ];
        spec.matrix_values = BTreeMap::from([("region".into(), "us-east-1".into())]);

        let out = launch(
            &spec,
            json!({}),
            json!({ "deploy/url": { "url": "https://svc.example" } }),
        )
        .unwrap();
        assert_eq!(out.command[1], "us-east-1");
        assert_eq!(out.env[0].1, "us-east-1");
        assert_eq!(out.env[1].1, "https://svc.example");

        // Fail-fast: an unbound reference is a hard error, never empty.
        assert!(launch(&spec, json!({}), json!({})).is_err());
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
            "steps: [{ id: s, invoke: file:.scarab/lib/bad.yaml }]",
            &libs(&[(".scarab/lib/bad.yaml", bad_lib)]),
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("exposes output `ghost` but has no step `ghost`")),
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
            errs.iter()
                .any(|e| e.contains("only valid on an `invoke` step")),
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
        assert_eq!(ir.interface.outputs, vec![OutputSpec::named("url")]);
        let back: PipelineIr = serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
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
        let back: PipelineIr = serde_json::from_str(&serde_json::to_string(&ir).unwrap()).unwrap();
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
        assert!(
            errs.iter().any(|e| e.contains("also declares a `default`")),
            "{errs:?}"
        );
    }

    #[test]
    fn optional_param_without_a_default_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [{ name: x, type: string, required: false }] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(
            errs.iter().any(|e| e.contains("must declare a `default`")),
            "{errs:?}"
        );
    }

    #[test]
    fn choice_without_options_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [{ name: x, type: choice }] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(
            errs.iter().any(|e| e.contains("non-empty `options`")),
            "{errs:?}"
        );
    }

    #[test]
    fn duplicate_param_name_is_a_compile_error() {
        let errs = errors(
            r#"
            interface: { inputs: [region, region] }
            steps: [{ id: a, image: busybox }]
            "#,
        );
        assert!(
            errs.iter().any(|e| e.contains("duplicate parameter")),
            "{errs:?}"
        );
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
                invoke: file:.scarab/lib/x.yaml
                with: { replicas: "3" }
            "#,
            &libs(&[(".scarab/lib/x.yaml", lib)]),
        );
        let run = ir.steps.iter().find(|s| s.id == "deploy/run").unwrap();
        assert!(launch(run, json!({}), json!({}))
            .unwrap()
            .env
            .contains(&("SCARAB_PARAM_REPLICAS".to_string(), "3".to_string())));
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
                invoke: file:.scarab/lib/x.yaml
                with: { replicas: "not-a-number" }
            "#,
            &libs(&[(".scarab/lib/x.yaml", lib)]),
        );
        assert!(
            errs.iter().any(|e| e.contains("is not a number")),
            "{errs:?}"
        );
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

    // --- retry: {on, max} (ADR-0020 syntax, ADR-0047 semantics) -------------

    #[test]
    fn retry_parses_and_lands_in_the_compiled_ir() {
        let ir = compile(
            r#"
            steps:
              - id: flaky
                image: busybox
                retry: { on: failure, max: 3 }
            "#,
        );
        assert_eq!(
            ir.steps[0].retry,
            Some(Retry {
                on: RetryOn::Failure,
                max: 3
            })
        );
    }

    #[test]
    fn retry_on_defaults_to_failure() {
        let ir = compile(
            r#"
            steps:
              - id: flaky
                image: busybox
                retry: { max: 2 }
            "#,
        );
        assert_eq!(
            ir.steps[0].retry,
            Some(Retry {
                on: RetryOn::Failure,
                max: 2
            })
        );
    }

    #[test]
    fn no_retry_is_the_default() {
        let ir = compile("steps: [{ id: a, image: busybox }]");
        assert_eq!(ir.steps[0].retry, None);
        // And the compiled IR round-trips without a retry key at all.
        let json = serde_json::to_value(&ir).unwrap();
        assert!(json["steps"][0].get("retry").is_none());
    }

    #[test]
    fn retry_max_bounds_are_validated() {
        for max in [0, 11] {
            let yaml = format!("steps: [{{ id: a, image: busybox, retry: {{ max: {max} }} }}]");
            match compile_yaml(&yaml) {
                Err(PipelineError::Validation(errs)) => {
                    // The bound error carries the at-least-once warning at the
                    // opt-in point (ADR-0047: never over-promise safety).
                    assert!(
                        errs.iter()
                            .any(|e| e.contains("retry.max") && e.contains("at-least-once")),
                        "max={max}: {errs:?}"
                    );
                }
                other => panic!("max={max}: expected validation error, got {other:?}"),
            }
        }
    }

    #[test]
    fn retry_on_a_gate_step_is_rejected() {
        match compile_yaml(
            r#"
            steps:
              - id: approve
                gate: manual
                retry: { max: 1 }
            "#,
        ) {
            Err(PipelineError::Validation(errs)) => {
                assert!(
                    errs.iter().any(|e| e.contains("must not set `retry`")),
                    "{errs:?}"
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    // --- deploy marker (ADR-0074 §2) ----------------------------------------

    #[test]
    fn deploy_marks_a_run_from_the_pipeline_or_any_step() {
        let unmarked = compile("steps:\n  - { id: test, image: rust }\n");
        assert!(!unmarked.is_deploy());
        // An Environment alone does not make a deploy Run.
        let governed = compile("environment: production\nsteps:\n  - { id: smoke, image: curl }\n");
        assert!(!governed.is_deploy());

        let pipeline = compile("deploy: true\nsteps:\n  - { id: ship, image: helm }\n");
        assert!(pipeline.is_deploy());
        assert!(pipeline.deploy_steps().is_empty(), "no Step is marked");

        let step = compile(
            r#"
            steps:
              - { id: build, image: rust }
              - { id: ship, image: helm, needs: [build], deploy: true }
            "#,
        );
        assert!(step.is_deploy());
        assert_eq!(step.deploy_steps(), ["ship"]);
        // Unmarked fields stay out of the stored IR.
        let stored = serde_json::to_value(&step).unwrap();
        assert!(stored.get("deploy").is_none());
        assert!(stored["steps"][0].get("deploy").is_none());
        assert_eq!(stored["steps"][1]["deploy"], json!(true));
    }

    #[test]
    fn deploy_on_a_gate_step_is_rejected() {
        match compile_yaml(
            r#"
            steps:
              - { id: approve, gate: manual, deploy: true }
            "#,
        ) {
            Err(PipelineError::Validation(errs)) => {
                assert!(
                    errs.iter().any(|e| e.contains("must not set `deploy`")),
                    "{errs:?}"
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn deploy_on_an_invoke_marks_the_inlined_steps_but_not_gates() {
        let lib = r#"
            steps:
              - { id: approve, gate: manual }
              - { id: ship, image: helm, needs: [approve] }
        "#;
        let marked_lib = lib.replacen("steps:", "deploy: true\n            steps:", 1);
        for (caller, libs) in [
            (
                "steps:\n  - { id: prod, invoke: file:.scarab/lib/ship.yaml, deploy: true }\n",
                libs(&[(".scarab/lib/ship.yaml", lib)]),
            ),
            (
                "steps:\n  - { id: prod, invoke: file:.scarab/lib/ship.yaml }\n",
                libs(&[(".scarab/lib/ship.yaml", &marked_lib)]),
            ),
        ] {
            let ir = compile_with(caller, &libs);
            assert_eq!(ir.deploy_steps(), ["prod/ship"], "{caller}");
        }
        let plain = compile_with(
            "steps:\n  - { id: prod, invoke: file:.scarab/lib/ship.yaml }\n",
            &libs(&[(".scarab/lib/ship.yaml", lib)]),
        );
        assert!(!plain.is_deploy());
    }

    // --- the clone library replaces the clone kind (ADR-0076 §9) ------------

    /// `libs/clone.yaml`, as the release ships it.
    const CLONE_LIBRARY: &str = include_str!("../../../libs/clone.yaml");

    fn with_clone() -> Libraries {
        Libraries {
            files: BTreeMap::new(),
            installed: installed(&[(
                "thulasi-ram/clone",
                "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111",
                CLONE_LIBRARY,
            )]),
            plugin_gates: Default::default(),
        }
    }

    #[test]
    fn a_clone_step_is_refused_naming_the_library() {
        for yaml in [
            "steps: [{ id: checkout, clone: {} }]",
            "steps: [{ id: checkout, clone: { depth: full } }]",
        ] {
            let errs = errors(yaml);
            assert!(
                errs.iter()
                    .any(|e| e.contains("step `checkout`") && e.contains("invoke: lib:thulasi-ram/clone")),
                "{errs:?}"
            );
        }
    }

    #[test]
    fn the_clone_library_checks_out_and_needs_cross_the_seam() {
        let ir = compile_with_libraries(
            r#"
            on: { push: {} }
            steps:
              - id: checkout
                invoke: lib:thulasi-ram/clone
                with: { depth: full, lfs: "true" }
                timeout: 1800
                placement_profiles: [big]
              - { id: build, image: busybox, needs: [checkout] }
            "#,
            &with_clone(),
        )
        .unwrap();
        let git = ir.steps.iter().find(|s| s.id == "checkout/git").unwrap();
        assert_eq!(git.forge_token, Some(ForgeToken::Read));
        assert_eq!(git.timeout, Some(1800), "timeout carries");
        assert!(
            git.placement_profiles.is_empty(),
            "placement_profiles never carries onto a forge_token Step"
        );
        assert!(git.origin.last().is_some_and(is_clone_library));
        assert!(git.image.contains("@sha256:"), "{}", git.image);
        let build = ir.steps.iter().find(|s| s.id == "build").unwrap();
        assert_eq!(build.needs.0, vec!["checkout/git".to_string()]);
        // It reads the Run's commit and clone URL, and nothing else.
        assert_eq!(
            event::reads(&ir),
            BTreeSet::from(["sha".to_string(), "repo.clone_url".to_string()])
        );
        assert!(lint(&ir).is_empty(), "{:?}", lint(&ir));
    }

    #[test]
    fn forge_token_is_for_a_step_that_runs_an_image() {
        let errs = errors("steps: [{ id: g, gate: manual, forge_token: read }]");
        assert!(errs.iter().any(|e| e.contains("`forge_token` is only valid")), "{errs:?}");
        let errs = errors(r#"steps: [{ id: b, build: { image: "a/b:1" }, forge_token: read }]"#);
        assert!(errs.iter().any(|e| e.contains("`forge_token` is only valid")), "{errs:?}");
        let ir = compile("steps: [{ id: push, image: alpine/git, forge_token: write }]");
        assert_eq!(ir.steps[0].forge_token, Some(ForgeToken::Write));
        let errs = library_errors(
            "steps: [{ id: c, invoke: lib:thulasi-ram/clone, forge_token: write }]",
            &with_clone(),
        );
        assert!(
            errs.iter().any(|e| e.contains("cannot set `forge_token`")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_forge_token_step_has_a_literal_image_and_command_and_no_cache() {
        let errs = errors(
            r#"steps: [{ id: g, image: "a/${{ matrix.x }}", matrix: { dimensions: { x: [b] } }, forge_token: read }]"#,
        );
        assert!(errs.iter().any(|e| e.contains("`image` holds")), "{errs:?}");
        let errs = errors(
            r#"steps: [{ id: g, image: alpine/git, command: [git, "${{ event.sha }}"], forge_token: read }]"#,
        );
        assert!(errs.iter().any(|e| e.contains("`command[1]` holds")), "{errs:?}");
        let errs = errors(
            r#"steps: [{ id: g, image: alpine/git, cache: { dirs: [d], key: [k] }, forge_token: read }]"#,
        );
        assert!(errs.iter().any(|e| e.contains("`cache` is not allowed")), "{errs:?}");
        // The same values through `env:` compile.
        compile(
            r#"steps: [{ id: g, image: alpine/git, env: [[SCARAB_S, "${{ event.sha }}"]], forge_token: read }]"#,
        );
    }

    #[test]
    fn a_templated_env_value_of_a_forge_token_step_reaches_only_a_scarab_name() {
        for name in ["S", "PATH", "GIT_SSH_COMMAND", "scarab_x", "SCARAB_", "SCARAB_a"] {
            let errs = errors(&format!(
                r#"steps: [{{ id: g, image: alpine/git, env: [["{name}", "${{{{ event.sha }}}}"]], forge_token: read }}]"#
            ));
            assert!(
                errs.iter().any(|e| e.contains(&format!("env `{name}` holds"))),
                "{name}: {errs:?}"
            );
        }
        for name in ["SCARAB_RUN", "SCARAB_PARAM_X", "SCARAB_WORKSPACE_URL"] {
            let errs = errors(&format!(
                r#"steps: [{{ id: g, image: alpine/git, env: [["{name}", "${{{{ event.sha }}}}"]], forge_token: read }}]"#
            ));
            assert!(
                errs.iter().any(|e| e.contains(&format!("Scarab sets `{name}`"))),
                "{name}: {errs:?}"
            );
        }
        // A literal value on any name is the author's, and compiles.
        compile(r#"steps: [{ id: g, image: alpine/git, env: [[GIT_TERMINAL_PROMPT, "0"]], forge_token: read }]"#);
        // Without `forge_token` the rule does not apply.
        compile(r#"steps: [{ id: g, image: alpine/git, env: [[S, "${{ event.sha }}"]] }]"#);
    }

    #[test]
    fn a_free_string_input_may_not_reach_a_forge_token_step() {
        let lib = r#"
            interface:
              inputs:
                - { name: url, type: string }
                - { name: depth, type: string, validate: 'value.matches("^[0-9]+$")' }
                - { name: lfs, type: boolean, required: false, default: false }
            steps:
              - id: git
                image: "alpine/git@sha256:0000000000000000000000000000000000000000000000000000000000000000"
                forge_token: read
                env:
                  - [SCARAB_U, "${{ inputs.url }}"]
                  - [SCARAB_D, "${{ inputs.depth }}"]
                  - [SCARAB_L, "${{ inputs.lfs }}"]
            "#;
        let libraries = Libraries {
            files: BTreeMap::new(),
            installed: installed(&[("acme/git", "github.com/acme/l//git.yaml@1", lib)]),
            plugin_gates: Default::default(),
        };
        let errs = library_errors(
            "steps: [{ id: c, invoke: lib:acme/git, with: { url: x, depth: '1' } }]",
            &libraries,
        );
        assert!(
            errs.iter().any(|e| e.contains("input `url` reaches step `git`")),
            "{errs:?}"
        );
        assert!(!errs.iter().any(|e| e.contains("input `depth`")), "{errs:?}");
        assert!(!errs.iter().any(|e| e.contains("input `lfs`")), "{errs:?}");
    }

    #[test]
    fn a_template_reads_only_the_allowlisted_event_fields() {
        for (field, want) in [
            ("event.actor", "may not read"),
            ("event.body", "may not read"),
            ("event", "whole"),
            ("event.repo", "whole"),
        ] {
            let yaml = format!("steps: [{{ id: a, image: busybox, env: [[X, \"${{{{ {field} }}}}\"]] }}]");
            let errs = errors(&yaml);
            assert!(
                errs.iter().any(|e| e.contains("step `a`") && e.contains(want)),
                "{field}: {errs:?}"
            );
        }
        let ir = compile(r#"steps: [{ id: a, image: busybox, env: [[B, "${{ event.branch }}"]] }]"#);
        assert_eq!(event::reads(&ir), BTreeSet::from(["branch".to_string()]));
    }

    #[test]
    fn a_library_step_renders_the_runs_event_values() {
        let ir = compile_with_libraries(
            "steps: [{ id: checkout, invoke: lib:thulasi-ram/clone }]",
            &with_clone(),
        )
        .unwrap();
        let git = &ir.steps[0];
        let sha = "a".repeat(40);
        let event = json!({ "sha": sha, "repo": { "clone_url": "https://github.com/acme/web.git" } });
        let r = render(
            Templates {
                image: &git.image,
                command: &git.command,
                env: &git.env,
            },
            git.scope.as_ref(),
            &git.matrix_values,
            Facts::Launch {
                params: &BTreeMap::new(),
                declared: &[],
                results: &Results::new(),
                event: event.as_object().unwrap(),
            },
        )
        .unwrap();
        let env: BTreeMap<_, _> = r.env.into_iter().collect();
        assert_eq!(env["SCARAB_CLONE_SHA"], sha);
        assert_eq!(env["SCARAB_CLONE_URL"], "https://github.com/acme/web.git");
        assert_eq!(env["SCARAB_CLONE_DEPTH"], "1");
        assert_eq!(env["SCARAB_CLONE_LFS"], "false");
    }

    /// Render `command` of a Pipeline-file Step at launch, against stored
    /// `event` values.
    fn launch_with_event(command: &[&str], event: serde_json::Value) -> Result<Vec<String>, String> {
        let command: Vec<String> = command.iter().map(|c| c.to_string()).collect();
        render(
            Templates {
                image: "alpine/git",
                command: &command,
                env: &[],
            },
            None,
            &BTreeMap::new(),
            Facts::Launch {
                params: &BTreeMap::new(),
                declared: &[],
                results: &Results::new(),
                event: event.as_object().unwrap(),
            },
        )
        .map(|r| r.command)
    }

    /// Free-text event fields (ADR-0076, 2026-10-30) reach a Step only
    /// through `env:`: a compile error in a command, and a launch refusal
    /// for a command patched into a compiled IR, however plain the value.
    #[test]
    fn a_free_text_event_value_reaches_a_command_only_through_env() {
        for field in [
            "ref",
            "branch",
            "tag",
            "run",
            "repo.owner",
            "repo.name",
            "repo.clone_url",
        ] {
            let errs = errors(&format!(
                r#"steps: [{{ id: a, image: busybox, command: [git, fetch, origin, "${{{{ event.{field} }}}}"] }}]"#
            ));
            let want = format!("`command[3]` reads `${{{{ event.{field} }}}}`");
            assert!(
                errs.iter().any(|e| e.contains("step `a`") && e.contains(&want)),
                "{field}: {errs:?}"
            );
        }
        // Built into a larger argument, or read beside a fixed field.
        for arg in [
            "--branch=${{ event.branch }}",
            "${{ event.sha + event.branch }}",
            "${{ event.repo['name'] }}",
        ] {
            let errs = errors(&format!(
                r#"steps: [{{ id: a, image: busybox, command: [git, "{arg}"] }}]"#
            ));
            assert!(
                errs.iter().any(|e| e.contains("`command[1]` reads")),
                "{arg}: {errs:?}"
            );
        }
        // Through `env:` it compiles.
        compile(r#"steps: [{ id: a, image: busybox, env: [[B, "${{ event.branch }}"]] }]"#);
        // At launch: refused before the value is read.
        for (template, event) in [
            ("${{ event.branch }}", json!({ "branch": "main" })),
            ("${{ event.ref }}", json!({ "ref": "refs/heads/main" })),
            ("x-${{ event.repo.clone_url }}", json!({ "repo": { "clone_url": "https://h/a.git" } })),
        ] {
            let err = launch_with_event(&["git", "fetch", template], event).unwrap_err();
            assert!(err.starts_with("`command[2]`"), "{template}: {err}");
        }
    }

    /// `event.sha`, `event.number`, `event.issue`, `event.fork` and
    /// `event.kind` are fixed shapes (ADR-0076, 2026-10-30): they reach a
    /// command, each checked against its shape when the Step launches.
    #[test]
    fn a_fixed_event_value_reaches_a_command_checked_at_launch() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let ir = compile(
            r#"steps: [{ id: a, image: alpine/git, command: [git, checkout, "${{ event.sha }}"] }]"#,
        );
        let s = &ir.steps[0];
        let cmd: Vec<&str> = s.command.iter().map(String::as_str).collect();
        assert_eq!(
            launch_with_event(&cmd, json!({ "sha": sha })).unwrap(),
            ["git", "checkout", sha]
        );
        let sha256 = "a".repeat(64);
        assert_eq!(
            launch_with_event(&cmd, json!({ "sha": sha256 })).unwrap(),
            ["git", "checkout", sha256.as_str()]
        );
        compile(
            r#"steps: [{ id: a, image: busybox, command: [x, "pr-${{ event.number }}", "${{ event.issue }}", "${{ event.fork }}", "${{ event.kind }}", "${{ event.number + 1 }}"] }]"#,
        );
        let all = [
            "x",
            "pr-${{ event.number }}",
            "${{ event.issue }}",
            "${{ event.fork }}",
            "${{ event.kind }}",
            "${{ event.number + 1 }}",
        ];
        assert_eq!(
            launch_with_event(
                &all,
                json!({ "number": 7, "issue": 12, "fork": false, "kind": "pull_request" })
            )
            .unwrap(),
            ["x", "pr-7", "12", "false", "pull_request", "8"]
        );
        // A stored value that is not of its shape (a store written around
        // Run creation's check) fails the Step at launch, naming it.
        let bad_sha = "the text `0123456789ABCDEF0123456789ABCDEF01234567`";
        for (command, event, what) in [
            (&cmd[..], json!({ "sha": sha.to_uppercase() }), bad_sha),
            (&cmd[..], json!({ "sha": "--upload-pack=touch /tmp/x" }), "which is not a commit SHA"),
            (&cmd[..], json!({ "sha": "main" }), "which is not a commit SHA"),
            (&cmd[..], json!({ "sha": &sha[..39] }), "which is not a commit SHA"),
            (&cmd[..], json!({ "sha": format!("{sha}0") }), "which is not a commit SHA"),
            (&cmd[..], json!({ "sha": 12345 }), "the number `12345`, which is not a commit SHA"),
            (&cmd[..], json!({ "sha": null }), "null, which is not a commit SHA"),
            (&["x", "y", "${{ event.number }}"], json!({ "number": "7" }), "not a whole number"),
            (&["x", "y", "${{ event.number }}"], json!({ "number": 1.5 }), "not a whole number"),
            (&["x", "y", "${{ event.issue }}"], json!({ "issue": -1 }), "not a whole number"),
            (&["x", "y", "${{ event.fork }}"], json!({ "fork": "true" }), "not true or false"),
            (&["x", "y", "${{ event.kind }}"], json!({ "kind": "-x" }), "not a trigger kind"),
            (&["x", "y", "${{ event.kind }}"], json!({ "kind": "nightly" }), "not a trigger kind"),
            // Text built from a fixed field is not of a fixed shape.
            (&["x", "y", "${{ event.sha + 'x' }}"], json!({ "sha": sha }), "text that is not"),
            (&["x", "y", "${{ event.kind + '' }}"], json!({ "kind": "push" }), "text that is not"),
        ] {
            let err = launch_with_event(command, event).unwrap_err();
            assert!(
                err.starts_with("`command[2]`") && err.contains("renders to") && err.contains(what),
                "{command:?}: {err}"
            );
        }
        // The script check is unchanged: a fixed value is still from outside.
        let errs = errors(
            r#"steps: [{ id: a, image: alpine/git, command: [sh, -c, "git checkout ${{ event.sha }}"] }]"#,
        );
        assert!(errs.iter().any(|e| e.contains("step `a`")), "{errs:?}");
    }

    /// Run creation's preview applies the same check to the stored values.
    #[test]
    fn a_preview_checks_the_event_values_it_knows() {
        let command = vec!["git".to_string(), "checkout".to_string(), "${{ event.sha }}".to_string()];
        let preview = |event: serde_json::Value| {
            render(
                Templates {
                    image: "",
                    command: &command,
                    env: &[],
                },
                None,
                &BTreeMap::new(),
                Facts::Preview {
                    params: &BTreeMap::new(),
                    declared: &[],
                    event: event.as_object().unwrap(),
                },
            )
        };
        let sha = "f".repeat(40);
        assert_eq!(preview(json!({ "sha": sha })).unwrap().command[2], sha);
        let e = preview(json!({ "sha": "nothex" })).unwrap_err();
        assert!(e.contains("not a commit SHA"), "{e}");
        let branch = vec!["git".to_string(), "${{ event.branch }}".to_string()];
        let e = render(
            Templates {
                image: "",
                command: &branch,
                env: &[],
            },
            None,
            &BTreeMap::new(),
            Facts::Preview {
                params: &BTreeMap::new(),
                declared: &[],
                event: json!({ "branch": "main" }).as_object().unwrap(),
            },
        )
        .unwrap_err();
        assert!(e.starts_with("`command[1]`"), "{e}");
    }

    // --- build step kind (ADR-0018) -----------------------------------------

    #[test]
    fn build_compiles_to_a_distinct_kind_with_defaults() {
        let ir = compile(
            r#"
            steps:
              - { id: checkout, image: alpine/git }
              - id: image
                needs: [checkout]
                build:
                  image: ghcr.io/acme/app:v1
                  push: true
            "#,
        );
        let image = &ir.steps[1];
        assert!(image.is_build());
        assert!(!image.is_gate());
        let b = image.build.as_ref().unwrap();
        assert_eq!(b.image, "ghcr.io/acme/app:v1");
        assert!(b.push);
        assert!(
            b.context.is_empty() && b.dockerfile.is_empty(),
            "defaults resolve at persist"
        );
    }

    #[test]
    fn build_step_rejects_image_command_privilege_and_missing_tag() {
        // An authored image/command contradicts the blessed BuildKit image.
        let err = errors(
            r#"
            steps:
              - id: image
                image: busybox
                build: { image: "ghcr.io/a/b:1" }
            "#,
        );
        assert!(
            err.iter()
                .any(|d| d.contains("must not") && d.contains("image")),
            "{err:?}"
        );

        // The image to build is mandatory.
        let err = errors(
            r#"
            steps:
              - id: image
                build: {}
            "#,
        );
        assert!(
            err.iter().any(|d| d.contains("must name the `image:`")),
            "{err:?}"
        );

        // Build steps are rootless by construction — no escalation.
        let err = errors(
            r#"
            steps:
              - id: image
                build: { image: "ghcr.io/a/b:1" }
                security: { run_as_root: true }
            "#,
        );
        assert!(err.iter().any(|d| d.contains("rootless")), "{err:?}");

        // Kinds are mutually exclusive.
        let err = errors(
            r#"
            steps:
              - id: image
                gate: manual
                build: { image: "ghcr.io/a/b:1" }
            "#,
        );
        assert!(
            err.iter().any(|d| d.contains("mutually exclusive")),
            "{err:?}"
        );
    }

    #[test]
    fn missing_clone_on_push_pipeline_lints_but_compiles() {
        // The lint fires — and compilation still SUCCEEDS (non-fatal).
        let ir = compile(
            r#"
            on: { push: {} }
            steps: [{ id: build, image: busybox }]
            "#,
        );
        let warnings = lint(&ir);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("without source"), "{warnings:?}");

        // pull_request triggers warn too.
        let ir = compile(
            r#"
            on: { pull_request: {} }
            steps: [{ id: test, image: busybox }]
            "#,
        );
        assert_eq!(lint(&ir).len(), 1);
    }

    #[test]
    fn lint_is_quiet_with_a_clone_or_on_repo_less_triggers() {
        // A push pipeline WITH a checkout: quiet.
        let ir = compile_with_libraries(
            r#"
            on: { push: {} }
            steps:
              - { id: checkout, invoke: lib:thulasi-ram/clone }
              - { id: build, image: busybox, needs: [checkout] }
            "#,
            &with_clone(),
        )
        .unwrap();
        assert!(lint(&ir).is_empty());

        // Repo-less triggers (cron/upstream) never warn — they legitimately
        // may have no source.
        let ir = compile(
            r#"
            on: { cron: {} }
            steps: [{ id: sweep, image: busybox }]
            "#,
        );
        assert!(lint(&ir).is_empty());

        // No triggers at all (API/manual-only): quiet.
        let ir = compile("steps: [{ id: a, image: busybox }]");
        assert!(lint(&ir).is_empty());
    }

    #[test]
    fn retry_survives_matrix_expansion() {
        let ir = compile(
            r#"
            steps:
              - id: test
                image: busybox
                retry: { max: 2 }
                matrix:
                  dimensions:
                    os: [linux, mac]
            "#,
        );
        assert_eq!(ir.steps.len(), 2);
        for step in &ir.steps {
            assert_eq!(
                step.retry,
                Some(Retry {
                    on: RetryOn::Failure,
                    max: 2
                })
            );
        }
    }

    // --- sidecar services (ADR-0058) ----------------------------------------

    #[test]
    fn sidecar_services_parse_with_a_ready_probe() {
        let ir = compile(
            r#"
            steps:
              - id: test
                image: rust:1
                command: [cargo, test]
                services:
                  - image: postgres:16
                    env: { POSTGRES_PASSWORD: test }
                    ports: [5432]
                    ready: { tcp: 5432 }
            "#,
        );
        let svcs = &ir.steps[0].services;
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].image, "postgres:16");
        assert_eq!(svcs[0].ports, vec![5432]);
        assert_eq!(svcs[0].ready.as_ref().unwrap().tcp, Some(5432));
        assert_eq!(
            svcs[0].env.get("POSTGRES_PASSWORD").map(String::as_str),
            Some("test")
        );
    }

    #[test]
    fn a_service_without_an_image_is_rejected() {
        match compile_yaml(
            r#"
            steps:
              - id: test
                image: rust:1
                services:
                  - env: { A: b }
            "#,
        ) {
            Err(PipelineError::Validation(errs)) => assert!(
                errs.iter().any(|e| e.contains("must name an `image`")),
                "{errs:?}"
            ),
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn services_on_a_gate_step_are_rejected() {
        match compile_yaml(
            r#"
            steps:
              - id: approve
                gate: manual
                services:
                  - image: postgres:16
            "#,
        ) {
            Err(PipelineError::Validation(errs)) => assert!(
                errs.iter()
                    .any(|e| e.contains("`services` is only valid on an ordinary executed step")),
                "{errs:?}"
            ),
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn shared_services_parse_and_steps_opt_in_with_uses() {
        let ir = compile(
            r#"
            services:
              - name: db
                image: postgres:16
                env: { POSTGRES_PASSWORD: test }
                ready: { tcp: 5432 }
            steps:
              - id: migrate
                image: migrate:latest
                uses: [db]
                command: [migrate, up]
              - id: test
                image: rust:1
                uses: [db]
                needs: [migrate]
            "#,
        );
        assert_eq!(ir.services.len(), 1);
        assert_eq!(ir.services[0].name, "db");
        assert_eq!(ir.services[0].spec.image, "postgres:16");
        assert_eq!(ir.services[0].spec.ready.as_ref().unwrap().tcp, Some(5432));
        assert_eq!(ir.steps[0].uses, vec!["db".to_string()]);
        assert_eq!(ir.steps[1].uses, vec!["db".to_string()]);
        // Round-trips through serde_json unchanged (self-describing IR).
        let json = serde_json::to_string(&ir).unwrap();
        let back: PipelineIr = serde_json::from_str(&json).unwrap();
        assert_eq!(ir, back);
    }

    #[test]
    fn uses_an_undeclared_service_is_rejected() {
        let errs = errors(
            r#"
            services:
              - name: db
                image: postgres:16
            steps:
              - id: test
                image: rust:1
                uses: [cache]
            "#,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("unknown shared service `cache`")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_shared_service_without_name_or_image_is_rejected() {
        let errs = errors(
            r#"
            services:
              - image: postgres:16
              - name: cache
            steps:
              - { id: a, image: busybox }
            "#,
        );
        assert!(
            errs.iter().any(|e| e.contains("must set a `name`")),
            "{errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("service `cache`: a shared service must name an `image`")),
            "{errs:?}"
        );
    }

    #[test]
    fn duplicate_shared_service_names_are_rejected() {
        let errs = errors(
            r#"
            services:
              - { name: db, image: postgres:16 }
              - { name: db, image: mysql:8 }
            steps:
              - { id: a, image: busybox }
            "#,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("duplicate shared service name `db`")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_non_dns_label_service_name_is_rejected() {
        let errs = errors(
            r#"
            services:
              - { name: My_DB, image: postgres:16 }
            steps:
              - { id: a, image: busybox }
            "#,
        );
        assert!(
            errs.iter().any(|e| e.contains("must be a DNS label")),
            "{errs:?}"
        );
    }

    #[test]
    fn uses_on_a_gate_step_is_rejected() {
        let errs = errors(
            r#"
            services:
              - { name: db, image: postgres:16 }
            steps:
              - { id: g, gate: manual, uses: [db] }
            "#,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("`uses` is only valid on a step that runs a Pod")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_shared_service_bad_ready_probe_is_rejected() {
        let errs = errors(
            r#"
            services:
              - name: db
                image: postgres:16
                ready: { tcp: 5432, exec: [pg_isready] }
            steps:
              - { id: a, image: busybox }
            "#,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("service `db`: a service `ready` probe must set exactly one")),
            "{errs:?}"
        );
    }

    // --- kitchen-sink sample guard --------------------------------------------

    /// The repo's OWN CI pipeline must compile, and its trigger must stay
    /// exactly `push:` with NO filter. `dogfood.yaml` was guarded here and
    /// `ci.yaml` was not — yet this is the one that runs on every push.
    ///
    /// THE UNFILTERED `push:` IS LOAD-BEARING, and this test exists because it
    /// was broken once. `.github/workflows/demo-keepalive.yml` force-pushes an
    /// empty commit to the `demo-keepalive` branch every four hours, and this
    /// pipeline compiling at that commit is what keeps the public demo's Oracle
    /// Always Free box from being reclaimed for idleness — nothing schedules
    /// `on: cron:` yet, so that push is its only recurring run. Narrowing this
    /// to `event.branch == 'main'` makes those pushes produce NO run, silently,
    /// and the box disappears about a week later. There is no failing build in
    /// that story, which is why it needs an assertion rather than a comment.
    ///
    /// `pull_request:` must stay ABSENT: GitHub delivers both a `push` and a
    /// `pull_request` for one push to a branch with an open PR, so having both
    /// ran this pipeline twice per PR push.
    #[test]
    fn the_repos_own_ci_pipeline_compiles_and_keeps_an_unfiltered_push_trigger() {
        let ci = include_str!("../../../.scarab/ci.yaml");
        let ir = compile_with_libraries(ci, &with_clone())
            .expect("the repo's own .scarab/ci.yaml must compile to IR");

        assert!(
            ir.steps.iter().any(|s| s.origin.iter().any(is_clone_library)),
            "checks out the repository"
        );

        assert_eq!(
            ir.triggers.0.keys().collect::<Vec<_>>(),
            vec!["push"],
            "`push` ONLY — adding `pull_request` back runs this twice per PR push"
        );
        let push = ir.triggers.0.get("push").expect("runs on push");
        assert!(
            push.when.is_none(),
            "the push trigger must stay UNFILTERED: a branch predicate here kills \
             the demo-keepalive run (see this test's doc comment) with no failing \
             build to notice, got {:?}",
            push.when
        );
    }

    /// `.scarab/dogfood.yaml` is DISPATCH-ONLY on purpose. It is sized for a build
    /// farm, not for the demo box that also runs the control plane, and it shares
    /// the `scarab` commit-status context with `ci.yaml` — so when it ran on every
    /// PR push its contention failures overwrote ci.yaml's green.
    #[test]
    fn the_dogfood_pipeline_is_dispatch_only() {
        let dogfood = include_str!("../../../.scarab/dogfood.yaml");
        let notify = include_str!("../../../.scarab/lib/notify.yaml");
        let ir = compile_with_libraries(
            dogfood,
            &Libraries {
                files: libs(&[(".scarab/lib/notify.yaml", notify)]),
                ..with_clone()
            },
        )
        .expect("the dogfood sample must compile");
        assert_eq!(
            ir.triggers.0.keys().collect::<Vec<_>>(),
            vec!["manual"],
            "manual dispatch ONLY — adding `pull_request` back re-creates the \
             status-overwrite race on the demo box"
        );
    }

    /// The repo's `.scarab/dogfood.yaml` kitchen-sink sample (and the library it
    /// invokes) must always compile to IR — this guards the teaching sample in CI
    /// so a schema change can never leave it stale/broken. The invoke is resolved
    /// against the inlined library source, mirroring the server trigger path.
    #[test]
    fn dogfood_sample_compiles() {
        let dogfood = include_str!("../../../.scarab/dogfood.yaml");
        let notify = include_str!("../../../.scarab/lib/notify.yaml");
        let libraries = Libraries {
            files: libs(&[(".scarab/lib/notify.yaml", notify)]),
            ..with_clone()
        };
        let ir = compile_with_libraries(dogfood, &libraries)
            .expect("the dogfood kitchen-sink sample must compile to IR");

        // A few features must have survived compilation.
        assert!(
            ir.steps.iter().any(|s| s.origin.iter().any(is_clone_library)),
            "checks out the repository"
        );
        assert!(
            !ir.services.is_empty(),
            "has pipeline-level shared services"
        );
        assert_eq!(ir.environment.as_deref(), Some("production"));
        assert!(ir.interface.inputs.iter().any(|p| p.name == "deploy_env"));
        // Matrix expanded (2 x 2 minus one exclude = 3 instances), invokes inlined.
        assert_eq!(
            ir.steps
                .iter()
                .filter(|s| s.id.starts_with("test["))
                .count(),
            3,
            "matrix expanded with the excluded combo dropped"
        );
        assert!(ir.steps.iter().all(|s| !s.is_invoke()), "invokes inlined");
        let post = ir.steps.iter().find(|s| s.id == "notify/post");
        let post = post.expect("library step inlined");
        // The library's `${{ inputs.channel }}` is the caller's `with:` value.
        let launched = launch(post, json!({}), json!({})).unwrap();
        assert!(
            launched
                .env
                .contains(&("CHANNEL".to_string(), "deploys".to_string())),
            "{:?}",
            launched.env
        );

        // The sample is not just a compile fixture — it is the LOCAL DOGFOOD
        // TARGET, dispatched against this repo, so the paths it names must
        // actually exist here. Compilation cannot catch that: a `build:` naming
        // an absent Dockerfile only fails in-cluster, after the expensive
        // lint/test legs it depends on have already run.
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for step in ir.steps.iter() {
            let Some(build) = step.build.as_ref() else {
                continue;
            };
            let context = repo.join(&build.context);
            assert!(
                context.is_dir(),
                "step `{}` builds context `{}`, which is not a directory in this repo",
                step.id,
                build.context
            );
            // BuildKit resolves `filename` INSIDE the context dir.
            assert!(
                context.join(&build.dockerfile).is_file(),
                "step `{}` names dockerfile `{}`, absent from context `{}`",
                step.id,
                build.dockerfile,
                build.context
            );
        }

        // The results channel (ADR-0042) drains `/scarab/results/<name>.json`
        // and NOTHING else. A step that redirects into a bare
        // `/scarab/results/tag` publishes nothing, logs only "no results to
        // drain", and STILL EXITS 0 — so the pipeline goes green while
        // `${{ outputs.<step>.<name> }}` silently resolves empty downstream.
        // This sample shipped that exact bug. Only a check like this catches it.
        for step in ir.steps.iter() {
            for arg in step.command.iter() {
                // Redirect targets only: the surrounding prose mentions the
                // wrong form on purpose, to document it.
                for (i, _) in arg.match_indices('>') {
                    let target: String = arg[i + 1..]
                        .trim_start()
                        .chars()
                        .take_while(|c| !c.is_whitespace() && *c != ';' && *c != '|')
                        .collect();
                    if let Some(name) = target.strip_prefix("/scarab/results/") {
                        assert!(
                            name.ends_with(".json"),
                            "step `{}` writes result `{}` — the sidecar drains \
                             `<name>.json` only, so this publishes nothing while \
                             the step still passes",
                            step.id,
                            target
                        );
                    }
                }
            }
        }
    }

    /// The `steps[i].resources` typo that started this: `cpu_milis` for
    /// `cpu_millis`. Before `deny_unknown_fields` this compiled clean and
    /// `validate` reported "ok: 2 step(s)" — while the step silently requested
    /// no CPU at all. A resource request dropped on the floor is worse than a
    /// rejected one, because nothing ever says so.
    #[test]
    fn a_misspelled_key_is_a_parse_error_not_a_silent_no_op() {
        let yaml = r#"
            steps:
              - id: build
                image: rust:1
                resources:
                  cpu_milis: 500
            "#;
        let err = parse_error(yaml);
        assert!(err.contains("cpu_milis"), "{err}");
        assert!(err.contains("cpu_millis"), "names the key it meant: {err}");
        // And the correct spelling still lands.
        let ir = compile(&yaml.replace("cpu_milis", "cpu_millis"));
        assert_eq!(ir.steps[0].resources.cpu_millis, Some(500));
    }

    /// One case per nesting level of the authored schema, because the attribute
    /// is PER-STRUCT: a level that is missed fails open and silently, so the
    /// coverage has to be enumerated rather than assumed.
    #[test]
    fn an_unknown_key_is_rejected_at_every_level_of_the_schema() {
        let cases: &[(&str, &str)] = &[
            ("top level", "stepz: []\nsteps: [{id: a, image: alpine}]\n"),
            ("step", "steps: [{id: a, image: alpine, dependsOn: [b]}]\n"),
            (
                "interface input",
                "interface: {inputs: [{name: v, requried: false}]}\nsteps: [{id: a, image: alpine}]\n",
            ),
            (
                "concurrency",
                "concurrency: {group: g, polciy: queue}\nsteps: [{id: a, image: alpine}]\n",
            ),
            (
                "trigger filter",
                "on: {push: {whn: \"true\"}}\nsteps: [{id: a, image: alpine}]\n",
            ),
            (
                "retry",
                "steps: [{id: a, image: alpine, retry: {max: 2, onn: failure}}]\n",
            ),
            (
                "build",
                "steps: [{id: a, build: {image: x, dockerfle: D}}]\n",
            ),
            (
                "cache",
                "steps: [{id: a, image: alpine, cache: {dirs: [t], keys: [f]}}]\n",
            ),
            (
                "security",
                "steps: [{id: a, image: alpine, security: {run_as_rot: true}}]\n",
            ),
            (
                "matrix",
                "steps: [{id: a, image: alpine, matrix: {dimensions: {os: [l]}, excludes: []}}]\n",
            ),
            (
                "sidecar service",
                "steps: [{id: a, image: alpine, services: [{image: postgres:16, portz: [5432]}]}]\n",
            ),
            (
                "sidecar ready probe",
                "steps: [{id: a, image: alpine, services: [{image: postgres:16, ready: {tcpp: 5432}}]}]\n",
            ),
            (
                "http ready probe",
                "steps: [{id: a, image: alpine, services: [{image: n, ready: {http: {port: 80, pth: /}}}]}]\n",
            ),
            // The shared service is the one level the derive could NOT cover:
            // it flattens `ServiceSpec`, and serde's flatten forwards only the
            // keys the flattened struct declares, so `deny_unknown_fields`
            // never sees the stray one. Hence the hand-written `Deserialize`.
            (
                "shared service",
                "services: [{name: db, image: postgres:16, portz: [5432]}]\nsteps: [{id: a, image: alpine}]\n",
            ),
        ];
        for (level, yaml) in cases {
            let err = parse_error(yaml);
            assert!(
                err.contains("unknown field"),
                "{level}: expected an unknown-field parse error, got: {err}"
            );
        }
    }

    /// A shared service must accept exactly what a sidecar service accepts,
    /// plus `name`. The hand-written `Deserialize` delegates to [`ServiceSpec`]
    /// precisely so there is no second field list to drift, and this holds it:
    /// every field a fully-populated `ServiceSpec` serializes must survive the
    /// round-trip through the shared-service form.
    #[test]
    fn a_shared_service_accepts_every_field_a_sidecar_service_declares() {
        // An exhaustive literal on purpose: adding a field to `ServiceSpec`
        // breaks THIS line, which is the reminder to re-check the delegation.
        let spec = ServiceSpec {
            image: "postgres:16".to_string(),
            command: vec!["docker-entrypoint.sh".to_string()],
            args: vec!["postgres".to_string()],
            env: BTreeMap::from([("POSTGRES_PASSWORD".to_string(), "test".to_string())]),
            ports: vec![5432],
            ready: Some(ReadyProbe {
                tcp: Some(5432),
                exec: Vec::new(),
                http: None,
            }),
            run_as_user: Some(999),
            run_as_root: false,
        };
        let shared = SharedServiceSpec {
            name: "db".to_string(),
            spec: spec.clone(),
        };

        let shared_keys: BTreeSet<String> = serde_json::to_value(&shared)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut expected: BTreeSet<String> = serde_json::to_value(&spec)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        expected.insert("name".to_string());
        assert_eq!(shared_keys, expected);

        let back: SharedServiceSpec =
            serde_json::from_value(serde_json::to_value(&shared).unwrap()).unwrap();
        assert_eq!(back, shared);
    }

    /// `name` is absent-tolerant (that becomes a `validate` diagnostic, matching
    /// what `ServiceSpec` does with a missing `image`) but not type-tolerant.
    #[test]
    fn a_shared_service_name_must_be_a_string_when_present() {
        let err = parse_error("services: [{name: 7, image: n}]\nsteps: [{id: a, image: alpine}]\n");
        assert!(err.contains("`name` must be a string"), "{err}");
        let errs = errors("services: [{image: n}]\nsteps: [{id: a, image: alpine}]\n");
        assert!(
            errs.iter().any(|e| e.contains("name")),
            "expected a name diagnostic, got {errs:?}"
        );
    }

    // --- ADR-0074: the Deployment record's pipeline fields are refused -------

    const REPLACEMENT: &str = "mark the step `deploy: true` and wait for your CD tool";

    #[test]
    fn deployment_requests_are_refused_naming_the_replacement() {
        let ds = errors(
            r#"
            environment: prod
            steps:
              - id: bump
                image: busybox
                results: [version]
                deployment_requests:
                  - { target: { source: argocd, id: web }, version: version }
            "#,
        );
        assert!(
            ds.iter().any(|d| d.contains("`bump`")
                && d.contains("deployment_requests")
                && d.contains("no longer recorded by Scarab (ADR-0074)")
                && d.contains(REPLACEMENT)),
            "got: {ds:?}"
        );
    }

    #[test]
    fn a_deployment_gate_is_refused_once_naming_the_replacement() {
        let ds = errors(
            r#"
            environment: prod
            steps:
              - { id: bump, image: busybox }
              - id: live
                gate: deployment
                needs: [bump]
                deployment: { step: bump }
            "#,
        );
        let hits: Vec<_> = ds.iter().filter(|d| d.contains("`live`")).collect();
        assert_eq!(hits.len(), 1, "one diagnostic for the gate, got: {ds:?}");
        assert!(
            hits[0].contains("gate: deployment")
                && hits[0].contains("ADR-0074")
                && hits[0].contains(REPLACEMENT),
            "got: {ds:?}"
        );
    }

    #[test]
    fn deployment_fields_on_an_invoke_step_are_refused() {
        let lib = "steps: [{ id: bump, image: busybox }]";
        let ds = errors_with(
            r#"
            steps:
              - id: deploy
                invoke: file:.scarab/lib/deploy.yaml
                deployment_requests: []
            "#,
            &libs(&[(".scarab/lib/deploy.yaml", lib)]),
        );
        assert!(
            ds.iter()
                .any(|d| d.contains("`deploy`") && d.contains(REPLACEMENT)),
            "got: {ds:?}"
        );
    }

    /// An IR stored before ADR-0074 still decodes, so an old Run's pipeline
    /// can be read back; only the Pipeline check refuses the fields.
    #[test]
    fn a_stored_ir_with_the_removed_fields_still_decodes() {
        let stored = serde_json::json!({
            "steps": [
                { "id": "bump", "image": "busybox", "results": ["v"],
                  "deployment_requests": [{ "target": { "source": "s", "id": "t" }, "version": "v" }] },
                { "id": "live", "gate": "deployment", "needs": ["bump"],
                  "deployment": { "step": "bump" } }
            ]
        });
        let ir: PipelineIr = serde_json::from_value(stored).expect("old IR decodes");
        assert!(ir.steps[0].deployment_requests.is_some());
        assert!(ir.steps[1].deployment.is_some());
    }

    // --- declared `results:` checks every `${{ outputs }}` read ---------------

    #[test]
    fn a_read_of_a_declared_result_compiles() {
        compile(
            r#"
            steps:
              - { id: bump, image: busybox, results: [images, version] }
              - id: wait
                image: argoproj/argocd
                needs: [bump]
                command: ["argocd", "app", "wait"]
                env: [[I, "${{ outputs.bump.images }}"], [V, "${{ outputs.bump.version }}"]]
            "#,
        );
    }

    #[test]
    fn a_read_of_an_undeclared_result_is_a_compile_error() {
        for surface in [
            r#"{ id: wait, image: busybox, needs: [bump], command: ["${{ outputs.bump.imgaes }}"] }"#,
            r#"{ id: wait, image: busybox, needs: [bump], env: [[I, "${{ outputs.bump.imgaes }}"]] }"#,
            r#"{ id: wait, image: "reg/${{ outputs.bump.imgaes }}", needs: [bump] }"#,
        ] {
            let yaml = format!(
                "steps:\n  - {{ id: bump, image: busybox, results: [images] }}\n  - {surface}\n"
            );
            let ds = errors(&yaml);
            assert!(
                ds.iter().any(|d| d.contains("`wait`")
                    && d.contains("outputs.bump.imgaes")
                    && d.contains("does not declare")
                    && d.contains("[images]")),
                "{surface}: got {ds:?}"
            );
        }
    }

    /// A Step that declares no `results:` keeps the open ADR-0041 map.
    #[test]
    fn a_step_without_results_is_not_checked() {
        compile(
            r#"
            steps:
              - { id: bump, image: busybox }
              - { id: wait, image: busybox, needs: [bump], env: [[A, "${{ outputs.bump.anything }}"]] }
            "#,
        );
    }

    /// A library step's declared results check reads of it through the
    /// reader's view of the invoke: an exposed output `images` is library
    /// step `images`'s result `images`.
    #[test]
    fn a_read_through_an_invoke_seam_is_checked_against_declared_results() {
        let lib = |declared: &str| {
            format!(
                "interface:\n  outputs: [images]\nsteps:\n  - {{ id: images, image: busybox, results: [{declared}] }}\n"
            )
        };
        let caller = r#"
            steps:
              - { id: deploy, invoke: file:.scarab/lib/deploy.yaml }
              - { id: wait, image: busybox, needs: [deploy], env: [[I, "${{ outputs.deploy.images }}"]] }
            "#;
        let ds = errors_with(
            caller,
            &libs(&[(".scarab/lib/deploy.yaml", &lib("digest"))]),
        );
        assert!(
            ds.iter().any(|d| d.contains("`wait`")
                && d.contains("reads `outputs.deploy.images`, which step `deploy/images`")
                && d.contains("does not declare")),
            "got: {ds:?}"
        );
        compile_with(
            caller,
            &libs(&[(".scarab/lib/deploy.yaml", &lib("images"))]),
        );
    }

    /// The compiler must know which Step every `outputs` read names inside a
    /// library and in a `with:` value; a computed one is refused.
    #[test]
    fn an_outputs_read_with_a_computed_step_is_refused() {
        let refused = "reads `outputs` in a form the compiler cannot resolve to a Step";
        let lib = |command: &str| {
            format!(
                "interface: {{ inputs: [{{ name: v, type: string, required: false, default: d }}] }}\nsteps:\n  - {{ id: s, image: busybox, command: [{command:?}] }}\n"
            )
        };
        let ds = errors_with(
            "steps: [{ id: c, invoke: file:a.yaml }]",
            &libs(&[("a.yaml", &lib("${{ outputs[matrix.k].v }}"))]),
        );
        assert!(
            ds.iter().any(
                |d| d.starts_with("library `a.yaml`, step `s`: `command[0]` ")
                    && d.contains(refused)
            ),
            "{ds:?}"
        );
        let ds = errors_with(
            r#"steps: [{ id: c, invoke: file:a.yaml, with: { v: "${{ outputs.exists(k, true) }}" } }]"#,
            &libs(&[("a.yaml", &lib("${{ inputs.v }}"))]),
        );
        assert!(
            ds.iter()
                .any(|d| d.starts_with("step `c`: `with.v` ") && d.contains(refused)),
            "{ds:?}"
        );
    }

    /// The libraries one compile reads are bounded in all, each counted once
    /// however often it is invoked. (Each source here fails to parse at its
    /// first byte, so the test reads little.)
    #[test]
    fn library_source_is_bounded_per_compile() {
        let big = format!("@{}", "x".repeat(MAX_SOURCE_BYTES - 1));
        let n = MAX_LIBRARY_SOURCE_BYTES / MAX_SOURCE_BYTES;
        let names: Vec<String> = (0..=n).map(|i| format!("l{i}.yaml")).collect();
        let files: BTreeMap<String, String> =
            names.iter().map(|k| (k.clone(), big.clone())).collect();
        // The same library invoked many times counts once.
        let mut yaml = String::from("steps:\n");
        for i in 0..50 {
            yaml.push_str(&format!("  - {{ id: a{i}, invoke: file:l0.yaml }}\n"));
        }
        let ds = errors_with(&yaml, &files);
        assert!(ds.iter().all(|d| d.contains("failed to parse")), "{ds:?}");
        // One more library than fits is refused, by name.
        let mut yaml = String::from("steps:\n");
        for (i, name) in names.iter().enumerate() {
            yaml.push_str(&format!("  - {{ id: b{i}, invoke: file:{name} }}\n"));
        }
        let ds = errors_with(&yaml, &files);
        assert!(
            ds.iter().any(|d| d.starts_with(&format!(
                "step `b{n}`: library `l{n}.yaml` is past the limit"
            ))),
            "{ds:?}"
        );
    }

    /// Every `invoke:` expansion counts against a bound, including one whose
    /// matrix `exclude` removes every combination, which adds no Step and no
    /// problem.
    #[test]
    fn invoke_expansions_are_bounded_when_nothing_is_inlined() {
        let fan = |target: &str| -> String {
            (0..30)
                .map(|k| {
                    format!(
                        "  - {{ id: k{k}, invoke: \"{target}\", matrix: {{ dimensions: {{ a: [x] }}, exclude: [\"true\"] }} }}\n"
                    )
                })
                .collect()
        };
        let l1 = format!("steps:\n{}", fan("file:l2.yaml"));
        let l2 = format!("steps:\n{}", fan("file:l3.yaml"));
        let l3 = "steps: [{ id: s, image: busybox }]";
        let ds = errors_with(
            &format!("steps:\n{}", fan("file:l1.yaml")),
            &libs(&[("l1.yaml", &l1), ("l2.yaml", &l2), ("l3.yaml", l3)]),
        );
        assert_eq!(
            ds,
            vec![format!(
                "step `k29`: the pipeline expands more than {MAX_INVOKE_EXPANSIONS} `invoke:` \
                 steps, counting a library each time it is invoked; the limit is \
                 {MAX_INVOKE_EXPANSIONS}"
            )]
        );
    }
}
