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
    if let Err(why) = known_ir_version(db.run_ir(run).await?.as_ref()) {
        return Ok(Err(why));
    }
    // A spec stored as a `clone` Step, a kind ADR-0076 §9 removed, decodes
    // with no image and nothing to run instead.
    if spec.image.is_empty() && spec.build.is_none() {
        return Ok(Err(
            "this Step has no image to run: it was stored as a `clone` Step, a kind this \
             server no longer runs (ADR-0076 §9); start a new Run, which checks out with \
             `invoke: lib:thulasi-ram/clone`"
                .to_string(),
        ));
    }
    let params = db.run_params(run).await?;
    let event = db.run_event_facts(run).await?;
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
            event: &event,
        },
    );
    Ok(rendered.map(|r| StepSpec {
        image: r.image,
        command: r.command,
        env: r.env,
        ..spec
    }))
}

/// `Err(reason)` unless the stored IR's version is a whole number this
/// server knows (an IR without one predates the field).
fn known_ir_version(ir: Option<&serde_json::Value>) -> Result<(), String> {
    let Some(v) = ir.and_then(|ir| ir.get("ir_version")) else {
        return Ok(());
    };
    let max = scarab_pipeline::IR_VERSION_MAX;
    match v.as_u64() {
        Some(n) if n <= u64::from(max) => Ok(()),
        Some(n) => Err(format!(
            "this Run was compiled for IR version {n}; this server understands versions up \
             to {max}"
        )),
        None => Err(format!(
            "this Run's IR version `{v}` is not a whole number; this server understands \
             versions up to {max}"
        )),
    }
}

/// The compiled IR entry of `step` in `run`'s stored Pipeline.
pub(crate) fn ir_step<'a>(ir: &'a serde_json::Value, step: &StepId) -> Option<&'a serde_json::Value> {
    ir.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get("id").and_then(|i| i.as_str()) == Some(step.0.as_str()))
}

/// The inputs of plugin gate `step` of `run` (ADR-0076 §7), rendered once
/// when the gate is reached by the one renderer
/// ([`scarab_pipeline::render_inputs`]) with the Run's stored facts: its
/// launch parameters, its `event` values and the results of the gate's
/// `needs`. `Ok(Err(reason))` when they do not render, which fails the
/// gate; `Err` is the store failing.
pub async fn render_gate_inputs(
    db: &dyn Db,
    run: &RunId,
    step: &StepId,
    needs: &[StepId],
) -> Result<Result<serde_json::Value, String>, DbError> {
    let ir = db.run_ir(run).await?;
    if let Err(why) = known_ir_version(ir.as_ref()) {
        return Ok(Err(why));
    }
    let scope = match ir
        .as_ref()
        .and_then(|ir| ir_step(ir, step))
        .and_then(|s| s.get("scope"))
    {
        Some(raw) => match serde_json::from_value::<scarab_pipeline::Scope>(raw.clone()) {
            Ok(s) => s,
            Err(e) => return Ok(Err(format!("its inputs cannot be read: {e}"))),
        },
        None => return Ok(Err("its inputs are not in the Run's Pipeline".to_string())),
    };
    let params = db.run_params(run).await?;
    let event = db.run_event_facts(run).await?;
    let mut results: scarab_pipeline::Results = BTreeMap::new();
    for need in needs {
        let r = db.step_results(run, need).await?;
        if !r.is_empty() {
            results.insert(need.0.clone(), r);
        }
    }
    Ok(
        scarab_pipeline::render_inputs(&scope, &params, &results, &event)
            .map(|inputs| serde_json::Value::Object(inputs.into_iter().collect())),
    )
}

/// The outputs plugin gate `step` of `run` declares, from its stored IR
/// (ADR-0076 §7). `None` when the step is not a plugin gate there.
pub(crate) async fn gate_declared_outputs(
    db: &dyn Db,
    run: &RunId,
    step: &StepId,
) -> Result<Option<Vec<scarab_pipeline::OutputSpec>>, DbError> {
    let ir = db.run_ir(run).await?;
    Ok(ir
        .as_ref()
        .and_then(|ir| ir_step(ir, step))
        .and_then(|s| s.get("plugin_gate"))
        .and_then(|g| serde_json::from_value::<scarab_pipeline::PluginGateSpec>(g.clone()).ok())
        .map(|g| g.outputs))
}
