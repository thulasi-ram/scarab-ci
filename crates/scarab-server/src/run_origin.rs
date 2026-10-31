//! Where a Run's Steps come from, as the Run page shows it (ADR-0076):
//!
//! - each Step's **origin**, the invoke chain the compiler recorded, with a
//!   mark on every library version yanked in the Run's org since;
//! - the Run's **yanked libraries**, so the page can say why a Rerun is
//!   refused before anyone clicks it;
//! - a plugin gate's **view**: who releases it, when it expires, the inputs
//!   rendered when it was reached, and its typed outputs;
//! - `GET /v1/runs/{id}/steps/{step}/origin`: the chain, the inputs bound in
//!   the Step's invocation, and the Step's image, command and env rendered as
//!   its launch renders them.
//!
//! Everything is read from the stored IR, leniently: a field this release
//! does not know is ignored, so a Run created by a newer server still shows.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_engine::{RunId, StepId, StepRun};
use scarab_identity::Action;

use crate::{authorize_scoped, run_scope, ApiError, AppState};

// ---------------------------------------------------------------------------
// The stored IR, as far as these views read it
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct Ir {
    #[serde(default)]
    steps: Vec<IrStep>,
}

#[derive(Deserialize)]
struct IrStep {
    id: String,
    #[serde(default)]
    origin: Vec<IrOrigin>,
    #[serde(default)]
    plugin_gate: Option<IrPluginGate>,
    #[serde(default)]
    gate_expires_after: Option<u64>,
    #[serde(default)]
    scope: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
struct IrOrigin {
    kind: String,
    name: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    content_hash: String,
}

#[derive(Deserialize)]
struct IrPluginGate {
    installation: String,
    gate: String,
    #[serde(default)]
    outputs: Vec<IrOutput>,
}

#[derive(Deserialize)]
struct IrOutput {
    name: String,
    #[serde(default = "string_type")]
    r#type: String,
}

fn string_type() -> String {
    "string".into()
}

async fn read_ir(st: &AppState, run: &RunId) -> Result<Ir, ApiError> {
    Ok(st
        .db
        .run_ir(run)
        .await?
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// One link of a Step's invoke chain, outermost first.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OriginLinkDto {
    /// `file`, `library` or `plugin`.
    pub kind: String,
    /// A repo-relative path (`file`), `<publisher>/<name>` (`library`) or the
    /// plugin installation and gate (`plugin`).
    pub name: String,
    /// Where an installed library comes from, at its commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// SHA-256 of the library source (or of the plugin's approved manifest).
    pub content_hash: String,
    /// The version is yanked in the Run's org now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub yanked: bool,
}

/// A library version a Run uses that is yanked in its org.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RunYankDto {
    /// `<publisher>/<name>`.
    pub name: String,
    pub content_hash: String,
    pub reason: String,
    pub yanked_by: String,
    pub yanked_at: i64,
    /// The Steps of the Run inlined from it.
    pub steps: Vec<String>,
}

/// One output a plugin gate returns on release.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GateOutputDto {
    pub name: String,
    /// `string`, `boolean` or `number`.
    pub r#type: String,
    /// The value the plugin released the gate with; absent until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub value: Option<serde_json::Value>,
}

/// What a `gate: plugin` node waits on, and where its wait stands.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PluginGateViewDto {
    /// The Principal that may release it: `plugin:<installation>`.
    pub releaser: String,
    pub installation: String,
    /// The gate's name in the plugin's manifest.
    pub gate: String,
    /// `enabled` or `killed`; absent when the installation is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_at: Option<i64>,
    /// `gate_expires_after`, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_after: Option<u64>,
    /// When the latest wait began (Unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_since: Option<i64>,
    /// When the latest wait expires (Unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The inputs rendered when the gate was reached; absent before.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub inputs: Option<serde_json::Value>,
    pub outputs: Vec<GateOutputDto>,
}

/// The per-step facts [`step_views`] adds to `GET /v1/runs/{id}`.
#[derive(Default)]
pub(crate) struct StepOriginView {
    pub origin: Vec<OriginLinkDto>,
    pub plugin_gate: Option<PluginGateViewDto>,
}

/// One input in the scope of a Step's invocation.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BoundInputDto {
    pub name: String,
    /// `literal` (a value known at compile: the caller's literal or the
    /// input's default) or `template` (the caller's `${{ … }}`, rendered at
    /// launch).
    pub kind: String,
    /// The literal value, or the template text.
    #[schema(value_type = Object)]
    pub bound: serde_json::Value,
}

/// A Step's image, command and env as its launch renders them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RenderedSpecDto {
    pub image: String,
    pub command: Vec<String>,
    /// `[name, value]` pairs.
    pub env: Vec<(String, String)>,
}

/// `GET /v1/runs/{id}/steps/{step}/origin`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StepOriginDto {
    pub step: String,
    /// The Pipeline the Run was created from, by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    /// The invoke chain, outermost first. Empty for a Step written in the
    /// Pipeline file itself.
    pub chain: Vec<OriginLinkDto>,
    /// The inputs bound in the Step's own invocation. Empty for a Step
    /// written in the Pipeline file.
    pub inputs: Vec<BoundInputDto>,
    /// The Step rendered as its launch renders it; absent before its first
    /// launch, for a gate, or when it does not render.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered: Option<RenderedSpecDto>,
    /// Why `rendered` is absent. Not set for a gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_note: Option<String>,
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// The yanks in the Run's org among the library versions its Steps use.
async fn yanks_of(
    st: &AppState,
    run: &RunId,
    ir: &Ir,
) -> Result<BTreeMap<String, scarab_policy::library::LibraryYank>, ApiError> {
    let Some(store) = st.libraries.as_deref() else {
        return Ok(BTreeMap::new());
    };
    let hashes: BTreeSet<String> = ir
        .steps
        .iter()
        .flat_map(|s| s.origin.iter())
        .filter(|o| o.kind == "library")
        .map(|o| o.content_hash.clone())
        .collect();
    if hashes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let Some((org, _)) = st.db.run_tenant(run).await? else {
        return Ok(BTreeMap::new());
    };
    let hashes: Vec<String> = hashes.into_iter().collect();
    let yanks = store
        .yanked(&org, &hashes)
        .await
        .map_err(crate::libraries::store_api_error)?;
    Ok(yanks
        .into_iter()
        .map(|y| (y.content_hash.clone(), y))
        .collect())
}

fn link(o: &IrOrigin, yanks: &BTreeMap<String, scarab_policy::library::LibraryYank>) -> OriginLinkDto {
    OriginLinkDto {
        kind: o.kind.clone(),
        name: o.name.clone(),
        source: o.source.clone(),
        content_hash: o.content_hash.clone(),
        yanked: o.kind == "library" && yanks.contains_key(&o.content_hash),
    }
}

/// The origin and plugin-gate view of every step of `run`, by step id, and
/// the Run's yanked library versions.
pub(crate) async fn step_views(
    st: &AppState,
    run: &RunId,
    steps: &[StepRun],
) -> Result<(HashMap<String, StepOriginView>, Vec<RunYankDto>), ApiError> {
    let ir = read_ir(st, run).await?;
    let yanks = yanks_of(st, run, &ir).await?;
    let attempts: HashMap<&str, &StepRun> = steps.iter().map(|s| (s.step.0.as_str(), s)).collect();
    let mut views = HashMap::new();
    let mut yanked: BTreeMap<String, RunYankDto> = BTreeMap::new();
    for s in &ir.steps {
        let origin: Vec<OriginLinkDto> = s.origin.iter().map(|o| link(o, &yanks)).collect();
        for o in s.origin.iter().filter(|o| o.kind == "library") {
            if let Some(y) = yanks.get(&o.content_hash) {
                yanked
                    .entry(o.content_hash.clone())
                    .or_insert_with(|| RunYankDto {
                        name: o.name.clone(),
                        content_hash: y.content_hash.clone(),
                        reason: y.reason.clone(),
                        yanked_by: y.yanked_by.clone(),
                        yanked_at: y.yanked_at,
                        steps: Vec::new(),
                    })
                    .steps
                    .push(s.id.clone());
            }
        }
        let plugin_gate = match &s.plugin_gate {
            Some(g) => Some(plugin_gate_view(st, run, s, g, attempts.get(s.id.as_str()).copied()).await?),
            None => None,
        };
        if !origin.is_empty() || plugin_gate.is_some() {
            views.insert(s.id.clone(), StepOriginView { origin, plugin_gate });
        }
    }
    Ok((views, yanked.into_values().collect()))
}

async fn plugin_gate_view(
    st: &AppState,
    run: &RunId,
    s: &IrStep,
    g: &IrPluginGate,
    step: Option<&StepRun>,
) -> Result<PluginGateViewDto, ApiError> {
    let step_id = StepId(s.id.clone());
    let wait = step.and_then(|r| r.attempts.last());
    let waiting_since = wait.map(|a| a.started_at.0);
    let inputs = match wait {
        Some(a) => st.db.gate_wait_inputs(run, &step_id, &a.id).await?,
        None => None,
    };
    let released = st.db.step_results(run, &step_id).await?;
    let installation = match (st.plugins.as_deref(), st.db.run_tenant(run).await?) {
        (Some(p), Some((org, _))) => p.installation(&org, &g.installation).await.ok().flatten(),
        _ => None,
    };
    Ok(PluginGateViewDto {
        releaser: format!("plugin:{}", g.installation),
        installation: g.installation.clone(),
        gate: g.gate.clone(),
        plugin_state: installation.as_ref().map(|i| i.state.as_str().to_string()),
        killed_by: installation.as_ref().and_then(|i| i.killed_by.clone()),
        killed_at: installation.as_ref().and_then(|i| i.killed_at),
        expires_after: s.gate_expires_after,
        waiting_since,
        expires_at: match (waiting_since, s.gate_expires_after) {
            (Some(at), Some(secs)) => {
                Some(at.saturating_add((secs as i64).saturating_mul(1000)))
            }
            _ => None,
        },
        inputs,
        outputs: g
            .outputs
            .iter()
            .map(|o| GateOutputDto {
                name: o.name.clone(),
                r#type: o.r#type.clone(),
                value: released.get(&o.name).cloned(),
            })
            .collect(),
    })
}

/// The inputs bound in a Step's own invocation, from its stored scope.
fn bound_inputs(scope: Option<&serde_json::Value>) -> Vec<BoundInputDto> {
    let Some(bound) = scope
        .and_then(|s| s.pointer("/inputs/bound"))
        .and_then(|b| b.as_object())
    else {
        return Vec::new();
    };
    bound
        .iter()
        .filter_map(|(name, b)| {
            let v = b.get("value")?;
            let (kind, bound) = if let Some(l) = v.get("literal") {
                ("literal", l.clone())
            } else {
                ("template", v.get("template")?.clone())
            };
            Some(BoundInputDto {
                name: name.clone(),
                kind: kind.into(),
                bound,
            })
        })
        .collect()
}

/// Where a Step comes from: its invoke chain, the inputs bound in its
/// invocation, and its image, command and env rendered as its launch renders
/// them. Read on the Run's Project.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/steps/{step}/origin",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    summary = "Where a Step comes from (ADR-0076)",
    responses(
        (status = 200, body = StepOriginDto),
        (status = 404, description = "no such run or step")
    )
)]
pub(crate) async fn get_step_origin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<Json<StepOriginDto>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    let ir = read_ir(&st, &run).await?;
    let s = ir
        .steps
        .iter()
        .find(|s| s.id == step)
        .ok_or(ApiError::NotFound)?;
    let yanks = yanks_of(&st, &run, &ir).await?;
    let step_id = StepId(step.clone());
    let launched = st
        .db
        .steps_of_run(&run)
        .await?
        .into_iter()
        .find(|r| r.step == step_id)
        .is_some_and(|r| !r.attempts.is_empty());
    let (rendered, render_note) = match st.db.step_spec(&run, &step_id).await? {
        _ if s.plugin_gate.is_some() => (None, None),
        None => (None, None),
        Some(_) if !launched => (
            None,
            Some("the Step renders when it launches".to_string()),
        ),
        Some(spec) => {
            match scarab_engine::render::render_launch_spec(st.db.as_ref(), &run, &step_id, spec)
                .await?
            {
                Ok(r) => (
                    Some(RenderedSpecDto {
                        image: r.image,
                        command: r.command,
                        env: r.env,
                    }),
                    None,
                ),
                Err(why) => (None, Some(format!("it does not render: {why}"))),
            }
        }
    };
    Ok(Json(StepOriginDto {
        step: s.id.clone(),
        pipeline: st.db.run_pipeline(&run).await?,
        chain: s.origin.iter().map(|o| link(o, &yanks)).collect(),
        inputs: bound_inputs(s.scope.as_ref()),
        rendered,
        render_note,
    }))
}
