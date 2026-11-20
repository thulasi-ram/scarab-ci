//! The `{step}` segment of a step route (`/v1/runs/{id}/steps/{step}/…`,
//! `/v1/runs/{id}/gates/{step}/…`).
//!
//! A client names a step by its **key** ([`scarab_engine::StepRun::key`]):
//! 12 characters of lowercase base32, random, unique within the run. Every
//! Scarab client does (the web UI, the CLI, the results sidecar), because an
//! authored step id can hold `/` (`amd64/image`) and `[`, `,` and `=` (a
//! matrix step), and a proxy in front of the server may turn a `%2F` back
//! into `/` (Envoy Gateway does by default). An authored id, percent-encoded
//! as one segment, still works: a segment that is not a key of a step of the
//! run is taken as an authored id, and the handler answers 404 when no step
//! has it.

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use scarab_engine::{RunId, StepId};

use crate::{ApiError, AppState};

/// A step route's run id and the step its `{step}` segment names, as the
/// authored step id: the segment resolved by [`resolve_step`].
#[derive(Debug, Clone)]
pub struct RunStep {
    pub run: String,
    pub step: String,
}

impl FromRequestParts<AppState> for RunStep {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Response> {
        let Path((run, segment)) = Path::<(String, String)>::from_request_parts(parts, st)
            .await
            .map_err(IntoResponse::into_response)?;
        let step = resolve_step(st, &RunId(run.clone()), segment)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(RunStep { run, step: step.0 })
    }
}

/// The step of `run` that a `{step}` path segment names: the step whose key
/// it is, else the segment itself as an authored step id. A key of another
/// run's step is not a key of this run, so it is taken as an id and names no
/// step.
pub(crate) async fn resolve_step(
    st: &AppState,
    run: &RunId,
    segment: String,
) -> Result<StepId, ApiError> {
    if scarab_engine::is_step_key_shape(&segment) {
        if let Some(step) = st.db.step_by_key(run, &segment).await? {
            return Ok(step);
        }
    }
    Ok(StepId(segment))
}
