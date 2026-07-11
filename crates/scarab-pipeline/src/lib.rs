//! # scarab-pipeline — pipeline authoring & compilation
//!
//! Pure domain crate (serde / serde_json / thiserror only). Turns authored
//! YAML into a validated, versioned [`PipelineIr`]. All bodies are stubs.

pub mod cel;

use serde::{Deserialize, Serialize};

/// The compiled, versioned intermediate representation of a pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineIr {
    /// Schema version of the IR, for forward/backward compatibility.
    pub ir_version: u32,
    pub steps: Vec<StepSpec>,
}

/// One authored step. The step contract (ADR-0008) is an OCI `image` + a
/// `command`; the rest are DAG/placement modifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    pub id: String,
    /// OCI image the step runs in.
    pub image: String,
    /// Entrypoint/command (empty = the image default).
    #[serde(default)]
    pub command: Vec<String>,
    /// Environment overrides for the step.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub needs: Needs,
    pub matrix: Option<Matrix>,
    pub when: Option<When>,
    #[serde(default)]
    pub runs_on: RunsOn,
    #[serde(default)]
    pub resources: Resources,
}

/// The upstream steps this step depends on (its DAG edges).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Needs(pub Vec<String>);

/// A build matrix that fans a single spec into many concrete steps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matrix {
    pub dimensions: std::collections::BTreeMap<String, Vec<String>>,
}

/// A conditional guard, expressed as a CEL expression (kept as a raw string
/// for now; evaluated by the [`cel`] submodule).
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
    #[error("validation failed with {0} error(s)")]
    Validation(usize),
    #[error("not yet implemented")]
    NotImplemented,
}

/// Compile authored YAML into a [`PipelineIr`].
pub fn compile_yaml(_yaml: &str) -> Result<PipelineIr, PipelineError> {
    // TODO: parse + lower authored YAML into the versioned IR.
    Err(PipelineError::NotImplemented)
}

/// Validate a compiled [`PipelineIr`], returning all discovered problems.
pub fn validate(_ir: &PipelineIr) -> Result<(), Vec<String>> {
    // TODO: DAG cycle detection, needs resolution, matrix expansion checks…
    Err(vec!["validation not yet implemented".to_string()])
}
