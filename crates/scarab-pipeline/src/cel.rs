//! Minimal CEL (Common Expression Language) evaluation surface.
//!
//! For now expressions are stored as raw strings and evaluated against a
//! JSON context. Body is a stub.

use crate::PipelineError;

/// Evaluate a CEL `expr` against a JSON `ctx`, yielding a JSON value.
pub fn eval(
    _expr: &str,
    _ctx: &serde_json::Value,
) -> Result<serde_json::Value, PipelineError> {
    // TODO: wire a real CEL evaluator.
    Err(PipelineError::NotImplemented)
}
