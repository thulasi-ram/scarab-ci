//! Render a Step's launch spec (ADR-0041, ADR-0076): gather what its
//! templates read, from the store, and hand them to the one pure renderer,
//! [`scarab_pipeline::render`]. The scheduler calls this right before a
//! launch; the server's debug Pod calls it to start the image the Step ran.

use std::collections::BTreeMap;

use crate::ports::Db;
use crate::{DbError, RunId, StepId, StepSpec};

/// `spec` with its image, command and env rendered for `step` of `run`, and
/// its `SCARAB_PARAM_*` variables added: `Ok(Ok(spec))`. `Ok(Err(reason))`
/// when it does not render (a bad reference, a missing result, a caller's
/// value that fails its input's type or `validate:`), which is the Step's
/// failure, not the store's. `Err` is the store failing.
///
/// Only the results of the Step's own `needs` are read: a template can read
/// nothing else, whatever its scope names.
pub async fn render_launch_spec(
    db: &dyn Db,
    run: &RunId,
    step: &StepId,
    spec: StepSpec,
) -> Result<Result<StepSpec, String>, DbError> {
    // A Run compiled by a newer server may render in a way this one does
    // not know (a scope it cannot read, say): refuse it rather than render
    // it the old way. Every Run this server and its predecessors stored has
    // a version it knows.
    // The version must be a whole number this server knows: `"4"`, `4.0`
    // or `-1` is refused, not read as no version. A stored IR without the
    // field predates it.
    let ir_version = db
        .run_ir(run)
        .await?
        .and_then(|ir| ir.get("ir_version").cloned());
    if let Some(v) = ir_version {
        let max = scarab_pipeline::IR_VERSION_MAX;
        match v.as_u64() {
            Some(n) if n <= u64::from(max) => {}
            Some(n) => {
                return Ok(Err(format!(
                    "this Run was compiled for IR version {n}; this server understands \
                     versions up to {max}"
                )))
            }
            None => {
                return Ok(Err(format!(
                    "this Run's IR version `{v}` is not a whole number; this server \
                     understands versions up to {max}"
                )))
            }
        }
    }
    let params = db.run_params(run).await?;
    let has_template = spec.image.contains("${{")
        || spec.command.iter().any(|c| c.contains("${{"))
        || spec.env.iter().any(|(_, v)| v.contains("${{"));
    let mut results: scarab_pipeline::Results = BTreeMap::new();
    // A Step with no template and no scope reads no result: skip the reads.
    if has_template || spec.scope.is_some() {
        let needs = db
            .steps_of_run(run)
            .await?
            .into_iter()
            .find(|s| &s.step == step)
            .map(|s| s.needs)
            .unwrap_or_default();
        for need in &needs {
            let r = db.step_results(run, need).await?;
            if !r.is_empty() {
                results.insert(need.0.clone(), r);
            }
        }
    }
    let rendered = scarab_pipeline::render(
        scarab_pipeline::Templates {
            image: &spec.image,
            command: &spec.command,
            env: &spec.env,
        },
        spec.scope.as_ref(),
        &spec.matrix_values,
        scarab_pipeline::Facts::Launch {
            params: &params,
            results: &results,
        },
    );
    Ok(rendered.map(|r| StepSpec {
        image: r.image,
        command: r.command,
        env: r.env,
        ..spec
    }))
}
