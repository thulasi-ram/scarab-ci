//! The Policy API (ADR-0075 §7, §9, §10, §11): org Policies, Project typed
//! settings, their version history, the decision logs, and the policy test
//! API.
//!
//! - **Org Policies** (`/v1/orgs/{org}/policies`) take CEL rules or a typed
//!   setting. Read on the Org to read them, Administer on the Org to write.
//! - **Project typed settings** (`/v1/orgs/{org}/projects/{project}/policies`)
//!   take a typed setting only (§7): a CEL body is a 400. Read on the
//!   Project to read them, Administer on the Project to write; an org admin
//!   passes both.
//! - Every body is compiled before it is saved, for its level: a Policy whose
//!   checkpoint the engine does not consult yet is refused there (400).
//!
//! **Step-up (§10).** Adding a Policy tightens (`policy.create` is Exempt). An
//! edit or delete is Conditional on the Policy as stored: when it is in
//! `audit` mode it enforces nothing, so changing it needs no step-up (this
//! covers moving it from `audit` to `enforce`); otherwise any change to an
//! enforced Policy counts as loosening, since whether an edited rule is
//! looser cannot be decided in general (fail closed). A save that changes
//! nothing is exempt. The confirmation that approved a save is recorded on
//! its PolicyVersion (`step_up_ref`).
//!
//! **Optimistic concurrency (ADR-0073 §3).** A write names the version it
//! was judged against: `expected_version` in the body of a PUT, and the
//! `expected_version` query parameter of a DELETE (a DELETE has no body).
//! A version other than the stored one is a 409 before anything is judged,
//! and the store writes only if it is still that version, so a concurrent
//! edit cannot slip a loosening past a write judged as exempt.
//!
//! **The policy test API** (`POST /v1/orgs/{org}/policy-test`) evaluates
//! without enforcing and without recording (§9). It takes a draft Policy, a
//! stored one, or none (the Project's effective Policies, what admission
//! would consult), against a supplied input, a recorded decision's input,
//! or a Pipeline compiled here from the YAML and library sources the caller
//! sends (no forge read). `scarab validate --server` calls the last form.
//! The effective Policies need Read on the input's Project, an org Policy by
//! id needs Read on the org, and a draft needs Administer on the org.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use scarab_engine::{DbError, RunId};
use scarab_identity::Action;
use scarab_policy::evaluate::{evaluate_stored, MatchContext, PolicyDecision};
use scarab_policy::ir::{compile, Level, Predicate};
use scarab_policy::policy::{Checkpoint, Mode, Policy};
use scarab_policy::run_create::{self, TriggerKind};
use scarab_policy::store::{
    DecisionCursor, DecisionRecord, NewPolicy, PolicyChange, PolicyScope, PolicyStore,
    PolicyStoreError, PolicyVersion, StoredPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use super::{authorize_scoped, rbac_scope, run_scope, ApiError, AppState};
use crate::step_up::GuardCtx;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// A stored Policy, at its current version.
#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyDto {
    pub id: String,
    /// `org` or `project`.
    pub level: String,
    pub org: String,
    /// The Project of a Project typed setting; absent at the org level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub name: String,
    /// 1 on create; each save, the delete included, adds one. A PUT or
    /// DELETE names it as `expected_version`.
    pub version: u64,
    /// The authored Policy (ADR-0075 §4): `name`, `checkpoint`, `match`,
    /// `mode`, and `rules` or `setting`.
    #[schema(value_type = Object)]
    pub policy: Policy,
    /// SHA-256 of the Policy's canonical JSON.
    pub content_hash: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Deleted Policies keep their history and are not consulted.
    pub deleted: bool,
}

impl From<StoredPolicy> for PolicyDto {
    fn from(p: StoredPolicy) -> Self {
        PolicyDto {
            id: p.id,
            level: match p.scope.level() {
                Level::Org => "org",
                Level::Project => "project",
            }
            .to_string(),
            org: p.scope.org().to_string(),
            project: p.scope.project().map(str::to_string),
            name: p.name,
            version: p.version,
            policy: p.body,
            content_hash: p.content_hash,
            created_at: p.created_at,
            updated_at: p.updated_at,
            deleted: p.deleted,
        }
    }
}

/// One saved version of a Policy (ADR-0075 §11).
#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyVersionDto {
    pub version: u64,
    #[schema(value_type = Object)]
    pub policy: Policy,
    pub content_hash: String,
    /// The Principal who saved it.
    pub author: String,
    pub reason: String,
    /// The StepUpConfirmation that approved this save, when one was needed.
    pub step_up_ref: Option<String>,
    /// This version deleted the Policy.
    pub deleted: bool,
    pub at: i64,
}

impl From<PolicyVersion> for PolicyVersionDto {
    fn from(v: PolicyVersion) -> Self {
        PolicyVersionDto {
            version: v.version,
            policy: v.body,
            content_hash: v.content_hash,
            author: v.author,
            reason: v.reason,
            step_up_ref: v.step_up_ref,
            deleted: v.deleted,
            at: v.at,
        }
    }
}

/// `POST …/policies`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePolicyRequest {
    /// The authored Policy (ADR-0075 §4). At the Project level it must be a
    /// typed `setting` (§7).
    #[schema(value_type = Object)]
    pub policy: Value,
    /// Why, recorded on the PolicyVersion. At most 2,000 bytes.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `PUT …/policies/{id}`: the whole Policy, replaced.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePolicyRequest {
    #[schema(value_type = Object)]
    pub policy: Value,
    /// The version this edit was made against; another stored version is a
    /// 409.
    pub expected_version: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `DELETE …/policies/{id}` query.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeletePolicyQuery {
    /// The version the delete was judged against; another stored version is
    /// a 409.
    pub expected_version: u64,
    pub reason: Option<String>,
}

/// A recorded PolicyDecision (ADR-0075 §9).
#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyDecisionRecordDto {
    pub id: String,
    pub org: String,
    pub project: Option<String>,
    /// The Run the decision admitted; absent for a refused `run.create`.
    pub run_id: Option<String>,
    pub step_id: Option<String>,
    pub attempt: Option<u32>,
    pub at: i64,
    /// The PolicyDecision: `checkpoint`, `schema_version`, `input`,
    /// `policy_set_digest`, `results` (one per Policy: `policy_id`,
    /// `version`, `name`, `level`, `mode`, `outcome`), and `outcome`
    /// (`{kind: allow}` or `{kind: deny, messages}`).
    #[schema(value_type = Object)]
    pub decision: PolicyDecision,
}

impl From<DecisionRecord> for PolicyDecisionRecordDto {
    fn from(d: DecisionRecord) -> Self {
        PolicyDecisionRecordDto {
            id: d.id,
            org: d.org,
            project: d.project,
            run_id: d.run_id,
            step_id: d.step_id,
            attempt: d.attempt,
            at: d.at,
            decision: d.decision,
        }
    }
}

/// One page of the org decision log, newest first.
#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyDecisionPageDto {
    pub decisions: Vec<PolicyDecisionRecordDto>,
    /// Pass as `before` for the next page; absent on the last one.
    pub next: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PolicyDecisionsQuery {
    /// At most this many (default and cap 200).
    pub limit: Option<usize>,
    /// The `next` of the previous page.
    pub before: Option<String>,
}

/// `POST /v1/orgs/{org}/policy-test`. Name at most one of `policy` and
/// `policy_id` (neither: the Project's effective Policies), and exactly one
/// of `input`, `decision_id` and `pipeline`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyTestRequest {
    /// A draft Policy, compiled at the org level. Needs Administer on the
    /// org; stored Policies need Read on the input's Project (an org Policy
    /// by id, Read on the org).
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub policy: Option<Value>,
    /// A stored Policy of this org (either level).
    #[serde(default)]
    pub policy_id: Option<String>,
    /// A `run.create` input, as a PolicyDecision records it.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub input: Option<Value>,
    /// A recorded decision of this org, whose input is evaluated again.
    #[serde(default)]
    pub decision_id: Option<String>,
    /// A Pipeline to compile here and build the input from.
    #[serde(default)]
    pub pipeline: Option<PolicyTestPipeline>,
}

/// The Pipeline form of the policy test: what `scarab validate --server`
/// sends.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyTestPipeline {
    /// The Pipeline file's YAML.
    pub yaml: String,
    /// The Pipeline file's repository path; the Pipeline's name when it
    /// declares none. Default `.scarab/ci.yaml`.
    #[serde(default)]
    pub path: Option<String>,
    /// Every `file:` library the Pipeline invokes, transitively: repository
    /// path (`.scarab/lib/deploy.yaml`) to source.
    #[serde(default)]
    pub libs: BTreeMap<String, String>,
    /// The Project the Run would be created in.
    pub project: String,
    pub trigger: PolicyTestTrigger,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyTestTrigger {
    /// `push`, `pull_request`, `tag`, `release`, `comment`, `cron`,
    /// `manual`, `api` or `upstream`.
    pub kind: String,
    /// The symbolic ref, as admission sees it: `refs/heads/main` for a push
    /// or a dispatch, `refs/pull/<n>/head` for a pull request,
    /// `refs/tags/<tag>` for a tag or release. Required for those kinds;
    /// `comment`, `cron` and `upstream` have none.
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
    /// The forge account that caused the event. Default: the caller for
    /// `manual` and `api` (as a dispatch is), none otherwise.
    #[serde(default)]
    pub actor: Option<String>,
    /// The forge's immutable id of that account (a webhook's `sender.id`),
    /// which admission resolves to the linked Principal (ADR-0069); webhook
    /// kinds only. Default none: the Run has no `principal`.
    #[serde(default)]
    pub actor_id: Option<String>,
    /// A pull request from a fork; `pull_request` only. Default false.
    #[serde(default)]
    pub fork: bool,
    /// The dispatch reason; `manual` and `api` only. Default none.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What `policy-test` answers. Nothing is recorded.
#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyTestResponse {
    /// The PolicyDecision, as admission would record it (see
    /// [`PolicyDecisionRecordDto::decision`]). A draft Policy has
    /// `policy_id` "" and `version` 0.
    #[schema(value_type = Object)]
    pub decision: PolicyDecision,
    /// Per evaluated Policy, what each rule reads.
    pub policies: Vec<PolicyReadsDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyReadsDto {
    pub policy_id: String,
    pub version: u64,
    pub name: String,
    /// Empty when the stored Policy no longer compiles.
    pub rules: Vec<RuleReadsDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RuleReadsDto {
    /// The rule's index in `rules`; 0 for a typed setting.
    pub index: usize,
    /// `cel` or `typed`.
    pub predicate: String,
    /// The input fields a CEL rule reads (`trigger.ref`, `steps`); empty for
    /// a typed setting, which reads Step and gate origins.
    pub reads: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The longest `reason` a Policy change records, in bytes.
pub const MAX_REASON_BYTES: usize = 2_000;
/// The most live Policies one scope (an org, or one Project) may hold. Each
/// is compiled again at every Run admission it applies to.
pub const MAX_POLICIES_PER_SCOPE: usize = 256;

/// A change's `reason`, refused over [`MAX_REASON_BYTES`].
fn reason_of(reason: Option<String>) -> Result<String, ApiError> {
    let reason = reason.unwrap_or_default();
    if reason.len() > MAX_REASON_BYTES {
        return Err(ApiError::BadRequest(format!(
            "`reason` is at most {MAX_REASON_BYTES} bytes; this one is {}",
            reason.len()
        )));
    }
    Ok(reason)
}

fn store(st: &AppState) -> Result<&dyn PolicyStore, ApiError> {
    st.policies.as_deref().ok_or(ApiError::NotFound)
}

fn store_api_error(e: PolicyStoreError) -> ApiError {
    match e {
        PolicyStoreError::NotFound => ApiError::NotFound,
        PolicyStoreError::Conflict => ApiError::Conflict(
            "the policy changed since it was read; read it again and retry".into(),
        ),
        e @ PolicyStoreError::NameTaken(_) => ApiError::Conflict(e.to_string()),
        PolicyStoreError::Invalid(m) => ApiError::BadRequest(m),
        e @ PolicyStoreError::Store(_) => ApiError::Db(DbError::Other(e.to_string())),
    }
}

fn org_scope(org: &str) -> PolicyScope {
    PolicyScope::Org {
        org: org.to_string(),
    }
}

fn project_scope(org: &str, project: &str) -> PolicyScope {
    PolicyScope::Project {
        org: org.to_string(),
        project: project.to_string(),
    }
}

/// The RBAC scope a Policy at `scope` is read and written at.
fn rbac_scope_of(scope: &PolicyScope) -> scarab_identity::Scope {
    rbac_scope(scope.org(), scope.project())
}

/// Runs CEL checking or evaluation off the async workers: both run on the
/// engine's own large-stack thread, and this keeps that join from blocking
/// a worker.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::Db(DbError::Other(format!("policy evaluation: {e}"))))
}

fn diagnostics_text(ds: &[scarab_policy::check::Diagnostic]) -> String {
    let lines: Vec<String> = ds.iter().map(|d| format!("- {d}")).collect();
    format!("the Policy does not compile:\n{}", lines.join("\n"))
}

/// Parses an authored Policy and compiles it for `level`, every finding in
/// the 400.
async fn compiled_body(raw: Value, level: Level) -> Result<Policy, ApiError> {
    let policy =
        Policy::from_json(raw).map_err(|e| ApiError::BadRequest(format!("invalid Policy: {e}")))?;
    let checked = policy.clone();
    blocking(move || compile(&checked, level))
        .await?
        .map_err(|ds| ApiError::BadRequest(diagnostics_text(&ds)))?;
    Ok(policy)
}

/// The Policy `id` at exactly `scope`; 404 for another scope's.
async fn stored_at(
    store: &dyn PolicyStore,
    scope: &PolicyScope,
    id: &str,
) -> Result<StoredPolicy, ApiError> {
    let p = store
        .get(id)
        .await
        .map_err(store_api_error)?
        .ok_or(ApiError::NotFound)?;
    if &p.scope != scope {
        return Err(ApiError::NotFound);
    }
    Ok(p)
}

/// The live Policy a write changes, at the version the caller judged it
/// against.
async fn live_at_version(
    store: &dyn PolicyStore,
    scope: &PolicyScope,
    id: &str,
    expected_version: u64,
) -> Result<StoredPolicy, ApiError> {
    let p = stored_at(store, scope, id).await?;
    if p.deleted {
        return Err(ApiError::NotFound);
    }
    if p.version != expected_version {
        return Err(ApiError::Conflict(format!(
            "the policy is at version {}, not {expected_version}; read it again and retry",
            p.version
        )));
    }
    Ok(p)
}

/// What an edit of `stored` into `new` loosens (ADR-0075 §10). A Policy in
/// `audit` mode enforces nothing, so no edit of it loosens, moving it to
/// `enforce` included. An enforced Policy: any change counts (fail closed).
fn update_loosenings(stored: &StoredPolicy, new: &Policy) -> Result<&'static str, Vec<String>> {
    if stored.body.mode == Mode::Audit {
        return Ok("the stored Policy is in audit mode and enforces nothing");
    }
    if &stored.body == new {
        return Ok("the save changes nothing");
    }
    let mut out = Vec::new();
    if new.mode == Mode::Audit {
        out.push(format!(
            "moves the enforced policy `{}` to audit mode",
            stored.name
        ));
    }
    let same_but_mode = Policy {
        mode: stored.body.mode,
        ..new.clone()
    };
    if same_but_mode != stored.body {
        out.push(format!(
            "edits the enforced policy `{}`; an edit is not known to tighten, so it counts \
             as loosening",
            stored.name
        ));
    }
    Err(out)
}

async fn judge(
    st: &AppState,
    guard: &GuardCtx,
    judged: Result<&'static str, Vec<String>>,
) -> Result<(), ApiError> {
    match judged {
        Ok(why) => {
            guard.exempt(why);
            Ok(())
        }
        Err(loosenings) => guard.require(st, loosenings).await,
    }
}

// ---------------------------------------------------------------------------
// Shared handler bodies (org and Project)
// ---------------------------------------------------------------------------

async fn list_at(
    st: &AppState,
    headers: &HeaderMap,
    scope: PolicyScope,
) -> Result<Json<Vec<PolicyDto>>, ApiError> {
    authorize_scoped(st, headers, Action::Read, Some(&rbac_scope_of(&scope))).await?;
    let list = store(st)?.list(&scope).await.map_err(store_api_error)?;
    Ok(Json(list.into_iter().map(Into::into).collect()))
}

async fn create_at(
    st: &AppState,
    headers: &HeaderMap,
    scope: PolicyScope,
    body: CreatePolicyRequest,
) -> Result<(StatusCode, Json<PolicyDto>), ApiError> {
    let principal = authorize_scoped(
        st,
        headers,
        Action::Administer,
        Some(&rbac_scope_of(&scope)),
    )
    .await?;
    let store = store(st)?;
    let reason = reason_of(body.reason)?;
    let policy = compiled_body(body.policy, scope.level()).await?;
    // A soft cap: two creates racing at the limit can both pass.
    let live = store.list(&scope).await.map_err(store_api_error)?.len();
    if live >= MAX_POLICIES_PER_SCOPE {
        return Err(ApiError::Conflict(format!(
            "this scope already has {live} Policies, the most it may hold \
             ({MAX_POLICIES_PER_SCOPE}); delete one first"
        )));
    }
    let now = st.clock.now().await.0;
    let stored = store
        .create(&NewPolicy {
            id: uuid::Uuid::now_v7().to_string(),
            scope,
            body: policy,
            author: principal.subject,
            reason,
            at: now,
        })
        .await
        .map_err(store_api_error)?;
    Ok((StatusCode::CREATED, Json(stored.into())))
}

async fn get_at(
    st: &AppState,
    headers: &HeaderMap,
    scope: PolicyScope,
    id: &str,
) -> Result<Json<PolicyDto>, ApiError> {
    authorize_scoped(st, headers, Action::Read, Some(&rbac_scope_of(&scope))).await?;
    Ok(Json(stored_at(store(st)?, &scope, id).await?.into()))
}

async fn versions_at(
    st: &AppState,
    headers: &HeaderMap,
    scope: PolicyScope,
    id: &str,
) -> Result<Json<Vec<PolicyVersionDto>>, ApiError> {
    authorize_scoped(st, headers, Action::Read, Some(&rbac_scope_of(&scope))).await?;
    let store = store(st)?;
    stored_at(store, &scope, id).await?;
    let versions = store.versions(id).await.map_err(store_api_error)?;
    Ok(Json(versions.into_iter().map(Into::into).collect()))
}

async fn update_at(
    st: &AppState,
    guard: &GuardCtx,
    headers: &HeaderMap,
    scope: PolicyScope,
    id: &str,
    body: UpdatePolicyRequest,
) -> Result<Json<PolicyDto>, ApiError> {
    let principal = authorize_scoped(
        st,
        headers,
        Action::Administer,
        Some(&rbac_scope_of(&scope)),
    )
    .await?;
    let store = store(st)?;
    let reason = reason_of(body.reason)?;
    let stored = live_at_version(store, &scope, id, body.expected_version).await?;
    let policy = compiled_body(body.policy, scope.level()).await?;
    judge(st, guard, update_loosenings(&stored, &policy)).await?;
    let now = st.clock.now().await.0;
    let saved = store
        .update(
            id,
            stored.version,
            &policy,
            &PolicyChange {
                author: principal.subject,
                reason,
                step_up_ref: guard.confirmation_id(),
                at: now,
            },
        )
        .await
        .map_err(store_api_error)?;
    Ok(Json(saved.into()))
}

async fn delete_at(
    st: &AppState,
    guard: &GuardCtx,
    headers: &HeaderMap,
    scope: PolicyScope,
    id: &str,
    q: DeletePolicyQuery,
) -> Result<Json<PolicyDto>, ApiError> {
    let principal = authorize_scoped(
        st,
        headers,
        Action::Administer,
        Some(&rbac_scope_of(&scope)),
    )
    .await?;
    let store = store(st)?;
    let reason = reason_of(q.reason)?;
    let stored = live_at_version(store, &scope, id, q.expected_version).await?;
    // ADR-0075 §10: deleting an enforced Policy loosens; an audit-mode one
    // enforces nothing.
    let judged = if stored.body.mode == Mode::Audit {
        Ok("the stored Policy is in audit mode and enforces nothing")
    } else {
        Err(vec![format!(
            "deletes the enforced policy `{}`",
            stored.name
        )])
    };
    judge(st, guard, judged).await?;
    let now = st.clock.now().await.0;
    let deleted = store
        .delete(
            id,
            stored.version,
            &PolicyChange {
                author: principal.subject,
                reason,
                step_up_ref: guard.confirmation_id(),
                at: now,
            },
        )
        .await
        .map_err(store_api_error)?;
    Ok(Json(deleted.into()))
}

// ---------------------------------------------------------------------------
// Org Policies
// ---------------------------------------------------------------------------

/// The org's own Policies (not its Projects' typed settings), by name.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/policies",
    params(("org" = String, Path, description = "org")),
    summary = "List the org's Policies (ADR-0075 §7)",
    responses(
        (status = 200, body = [PolicyDto]),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "policies not configured")
    )
)]
pub(crate) async fn list_org_policies(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<PolicyDto>>, ApiError> {
    list_at(&st, &headers, org_scope(&org)).await
}

/// Add an org Policy: CEL rules or a typed setting, compiled before it is
/// saved. Adding a Policy tightens, so it never needs a step-up (§10).
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/policies",
    params(("org" = String, Path, description = "org")),
    request_body = CreatePolicyRequest,
    summary = "Add an org Policy (ADR-0075 §4, §10)",
    responses(
        (status = 201, body = PolicyDto),
        (status = 400, description = "not a Policy, or it does not compile (its checkpoint is not yet consulted, a CEL error, an effect the checkpoint does not allow), or `reason` is over 2,000 bytes"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "policies not configured"),
        (status = 409, description = "a live Policy here has that name, or the scope holds its 256 Policies")
    )
)]
pub(crate) async fn create_org_policy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<PolicyDto>), ApiError> {
    create_at(&st, &headers, org_scope(&org), body).await
}

/// One org Policy, deleted or not.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/policies/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "policy id")
    ),
    summary = "Read an org Policy (ADR-0075 §11)",
    responses(
        (status = 200, body = PolicyDto),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Policy in this org, or not configured")
    )
)]
pub(crate) async fn get_org_policy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, String)>,
) -> Result<Json<PolicyDto>, ApiError> {
    get_at(&st, &headers, org_scope(&org), &id).await
}

/// Replace an org Policy. A step-up is needed unless the stored version is
/// in `audit` mode (or the save changes nothing): any edit of an enforced
/// Policy counts as loosening (§10).
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/policies/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "policy id")
    ),
    request_body = UpdatePolicyRequest,
    summary = "Replace an org Policy (ADR-0075 §10, §11)",
    responses(
        (status = 200, body = PolicyDto),
        (status = 400, description = "not a Policy, or it does not compile, or `reason` is over 2,000 bytes"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such live Policy in this org, or not configured"),
        (status = 409, description = "`expected_version` is not the stored version, or the name is taken")
    )
)]
pub(crate) async fn update_org_policy(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, String)>,
    Json(body): Json<UpdatePolicyRequest>,
) -> Result<Json<PolicyDto>, ApiError> {
    update_at(&st, &guard, &headers, org_scope(&org), &id, body).await
}

/// Delete an org Policy (its history stays). A step-up is needed unless the
/// stored version is in `audit` mode (§10).
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/policies/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "policy id"),
        DeletePolicyQuery
    ),
    summary = "Delete an org Policy (ADR-0075 §10)",
    responses(
        (status = 200, body = PolicyDto),
        (status = 400, description = "`reason` is over 2,000 bytes"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such live Policy in this org, or not configured"),
        (status = 409, description = "`expected_version` is not the stored version")
    )
)]
pub(crate) async fn delete_org_policy(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, String)>,
    Query(q): Query<DeletePolicyQuery>,
) -> Result<Json<PolicyDto>, ApiError> {
    delete_at(&st, &guard, &headers, org_scope(&org), &id, q).await
}

/// Every version of an org Policy, oldest first.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/policies/{id}/versions",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "policy id")
    ),
    summary = "An org Policy's version history (ADR-0075 §11)",
    responses(
        (status = 200, body = [PolicyVersionDto]),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Policy in this org, or not configured")
    )
)]
pub(crate) async fn list_org_policy_versions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, String)>,
) -> Result<Json<Vec<PolicyVersionDto>>, ApiError> {
    versions_at(&st, &headers, org_scope(&org), &id).await
}

// ---------------------------------------------------------------------------
// Project typed settings
// ---------------------------------------------------------------------------

/// A Project's typed settings, by name.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/projects/{project}/policies",
    params(
        ("org" = String, Path, description = "org"),
        ("project" = String, Path, description = "project")
    ),
    summary = "List a Project's typed settings (ADR-0075 §7)",
    responses(
        (status = 200, body = [PolicyDto]),
        (status = 403, description = "no Read on the Project"),
        (status = 404, description = "policies not configured")
    )
)]
pub(crate) async fn list_project_policies(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, project)): Path<(String, String)>,
) -> Result<Json<Vec<PolicyDto>>, ApiError> {
    list_at(&st, &headers, project_scope(&org, &project)).await
}

/// Add a Project typed setting. Project admins write typed settings only:
/// a `rules` body is a 400 (ADR-0075 §7). Adding one tightens (§10).
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/projects/{project}/policies",
    params(
        ("org" = String, Path, description = "org"),
        ("project" = String, Path, description = "project")
    ),
    request_body = CreatePolicyRequest,
    summary = "Add a Project typed setting (ADR-0075 §7, §10)",
    responses(
        (status = 201, body = PolicyDto),
        (status = 400, description = "not a typed setting (ADR-0075 §7), or it does not compile, or `reason` is over 2,000 bytes"),
        (status = 403, description = "not a Project admin"),
        (status = 404, description = "policies not configured"),
        (status = 409, description = "a live Policy here has that name, or the scope holds its 256 Policies")
    )
)]
pub(crate) async fn create_project_policy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, project)): Path<(String, String)>,
    Json(body): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<PolicyDto>), ApiError> {
    create_at(&st, &headers, project_scope(&org, &project), body).await
}

/// One Project typed setting, deleted or not.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/projects/{project}/policies/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("project" = String, Path, description = "project"),
        ("id" = String, Path, description = "policy id")
    ),
    summary = "Read a Project typed setting (ADR-0075 §11)",
    responses(
        (status = 200, body = PolicyDto),
        (status = 403, description = "no Read on the Project"),
        (status = 404, description = "no such Policy in this Project, or not configured")
    )
)]
pub(crate) async fn get_project_policy(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, project, id)): Path<(String, String, String)>,
) -> Result<Json<PolicyDto>, ApiError> {
    get_at(&st, &headers, project_scope(&org, &project), &id).await
}

/// Replace a Project typed setting; step-up as for an org Policy (§10).
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/projects/{project}/policies/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("project" = String, Path, description = "project"),
        ("id" = String, Path, description = "policy id")
    ),
    request_body = UpdatePolicyRequest,
    summary = "Replace a Project typed setting (ADR-0075 §10, §11)",
    responses(
        (status = 200, body = PolicyDto),
        (status = 400, description = "not a typed setting (ADR-0075 §7), or it does not compile, or `reason` is over 2,000 bytes"),
        (status = 403, description = "not a Project admin"),
        (status = 404, description = "no such live Policy in this Project, or not configured"),
        (status = 409, description = "`expected_version` is not the stored version, or the name is taken")
    )
)]
pub(crate) async fn update_project_policy(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, project, id)): Path<(String, String, String)>,
    Json(body): Json<UpdatePolicyRequest>,
) -> Result<Json<PolicyDto>, ApiError> {
    update_at(
        &st,
        &guard,
        &headers,
        project_scope(&org, &project),
        &id,
        body,
    )
    .await
}

/// Delete a Project typed setting; step-up as for an org Policy (§10).
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/projects/{project}/policies/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("project" = String, Path, description = "project"),
        ("id" = String, Path, description = "policy id"),
        DeletePolicyQuery
    ),
    summary = "Delete a Project typed setting (ADR-0075 §10)",
    responses(
        (status = 200, body = PolicyDto),
        (status = 400, description = "`reason` is over 2,000 bytes"),
        (status = 403, description = "not a Project admin"),
        (status = 404, description = "no such live Policy in this Project, or not configured"),
        (status = 409, description = "`expected_version` is not the stored version")
    )
)]
pub(crate) async fn delete_project_policy(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, project, id)): Path<(String, String, String)>,
    Query(q): Query<DeletePolicyQuery>,
) -> Result<Json<PolicyDto>, ApiError> {
    delete_at(&st, &guard, &headers, project_scope(&org, &project), &id, q).await
}

/// Every version of a Project typed setting, oldest first.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/projects/{project}/policies/{id}/versions",
    params(
        ("org" = String, Path, description = "org"),
        ("project" = String, Path, description = "project"),
        ("id" = String, Path, description = "policy id")
    ),
    summary = "A Project typed setting's version history (ADR-0075 §11)",
    responses(
        (status = 200, body = [PolicyVersionDto]),
        (status = 403, description = "no Read on the Project"),
        (status = 404, description = "no such Policy in this Project, or not configured")
    )
)]
pub(crate) async fn list_project_policy_versions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, project, id)): Path<(String, String, String)>,
) -> Result<Json<Vec<PolicyVersionDto>>, ApiError> {
    versions_at(&st, &headers, project_scope(&org, &project), &id).await
}

// ---------------------------------------------------------------------------
// Decision logs
// ---------------------------------------------------------------------------

fn cursor_text(d: &DecisionRecord) -> String {
    format!("{}:{}", d.at, d.id)
}

fn parse_cursor(s: &str) -> Result<DecisionCursor, ApiError> {
    let bad = || ApiError::BadRequest(format!("`before` is not a page cursor: {s:?}"));
    let (at, id) = s.split_once(':').ok_or_else(bad)?;
    Ok(DecisionCursor {
        at: at.parse().map_err(|_| bad())?,
        id: id.to_string(),
    })
}

/// The org's decision log, newest first: every `run.create` consultation,
/// those that refused a Run included (ADR-0075 §9).
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/policy-decisions",
    params(("org" = String, Path, description = "org"), PolicyDecisionsQuery),
    summary = "The org's PolicyDecision log (ADR-0075 §9)",
    responses(
        (status = 200, body = PolicyDecisionPageDto),
        (status = 400, description = "`before` is not a cursor"),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "policies not configured")
    )
)]
pub(crate) async fn list_org_policy_decisions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Query(q): Query<PolicyDecisionsQuery>,
) -> Result<Json<PolicyDecisionPageDto>, ApiError> {
    authorize_scoped(&st, &headers, Action::Read, Some(&rbac_scope(&org, None))).await?;
    let store = store(&st)?;
    let before = q.before.as_deref().map(parse_cursor).transpose()?;
    let limit = scarab_policy::store::decision_page_limit(q.limit.unwrap_or(0));
    let page = store
        .org_decisions(&org, limit, before.as_ref())
        .await
        .map_err(store_api_error)?;
    let next = (page.len() == limit)
        .then(|| page.last().map(cursor_text))
        .flatten();
    Ok(Json(PolicyDecisionPageDto {
        decisions: page.into_iter().map(Into::into).collect(),
        next,
    }))
}

/// A Run's PolicyDecisions, oldest first. Read on the Run's Project.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/policy-decisions",
    params(("id" = String, Path, description = "run id")),
    summary = "A Run's PolicyDecisions (ADR-0075 §9)",
    responses(
        (status = 200, body = [PolicyDecisionRecordDto]),
        (status = 403, description = "no Read on the Run's Project"),
        (status = 404, description = "no such run")
    )
)]
pub(crate) async fn list_run_policy_decisions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<PolicyDecisionRecordDto>>, ApiError> {
    let run = RunId(id);
    let scope = run_scope(&st, &run).await;
    authorize_scoped(&st, &headers, Action::Read, scope.as_ref()).await?;
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    // No store wired: no Policy was consulted, so the Run has none.
    let Some(store) = st.policies.as_deref() else {
        return Ok(Json(Vec::new()));
    };
    let decisions = store
        .decisions_for_run(&run.0)
        .await
        .map_err(store_api_error)?;
    Ok(Json(decisions.into_iter().map(Into::into).collect()))
}

// ---------------------------------------------------------------------------
// The policy test API
// ---------------------------------------------------------------------------

/// What a `run.create` input's `match` is evaluated against, read from the
/// input itself, so a supplied or recorded input is matched as admission
/// matched it.
fn match_context_of(input: &Value) -> Result<MatchContext, ApiError> {
    let bad = |what: &str| ApiError::BadRequest(format!("the input has no {what}"));
    let project = input
        .pointer("/project/name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`project.name`"))?
        .to_string();
    let environment = match input.pointer("/pipeline/environment") {
        None | Some(Value::Null) => None,
        Some(Value::String(e)) => Some(e.clone()),
        Some(_) => return Err(bad("string `pipeline.environment`")),
    };
    let trigger_kind: TriggerKind = input
        .pointer("/trigger/kind")
        .cloned()
        .and_then(|k| serde_json::from_value(k).ok())
        .ok_or_else(|| bad("valid `trigger.kind`"))?;
    Ok(MatchContext {
        project,
        environment,
        trigger_kind,
    })
}

/// The forge event admission would see for the Pipeline form's trigger.
fn test_event(
    t: PolicyTestTrigger,
    repo: scarab_forge::RepoRef,
    caller: &str,
) -> Result<scarab_forge::Event, ApiError> {
    use scarab_forge::Event;
    let kind: TriggerKind = serde_json::from_value(Value::String(t.kind.clone()))
        .map_err(|_| ApiError::BadRequest(format!("`{}` is not a trigger kind", t.kind)))?;
    let bad = |m: String| ApiError::BadRequest(m);
    let dispatched = matches!(kind, TriggerKind::Manual | TriggerKind::Api);
    if t.fork && kind != TriggerKind::PullRequest {
        return Err(bad(
            "only a pull_request trigger can come from a fork".into()
        ));
    }
    if t.reason.is_some() && !dispatched {
        return Err(bad("only a manual or api trigger carries a reason".into()));
    }
    if t.actor_id.is_some() && dispatched {
        return Err(bad(
            "a manual or api trigger's actor is a Principal; `actor_id` is for webhook kinds"
                .into(),
        ));
    }
    let actor_id = t.actor_id.clone().unwrap_or_default();
    let needs_ref = || {
        t.r#ref
            .clone()
            .ok_or_else(|| bad(format!("a {} trigger needs `ref`", t.kind)))
    };
    let tag_of = || {
        let r = needs_ref()?;
        r.strip_prefix("refs/tags/")
            .filter(|tag| !tag.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                bad(format!(
                    "a {} trigger's ref is refs/tags/<tag>, not {r}",
                    t.kind
                ))
            })
    };
    let sha = t.sha.clone().unwrap_or_default();
    let actor = match (&t.actor, dispatched) {
        (Some(a), _) => a.clone(),
        (None, true) => caller.to_string(),
        (None, false) => String::new(),
    };
    Ok(match kind {
        TriggerKind::Push => Event::Push {
            actor,
            actor_id: actor_id.clone(),
            repo,
            r#ref: needs_ref()?,
            after: sha,
            message: String::new(),
        },
        TriggerKind::PullRequest => {
            let r = needs_ref()?;
            let number = r
                .strip_prefix("refs/pull/")
                .and_then(|rest| rest.strip_suffix("/head"))
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(|| {
                    bad(format!(
                        "a pull_request trigger's ref is refs/pull/<n>/head, as admission sees \
                         it, not {r}"
                    ))
                })?;
            Event::PullRequest {
                actor,
                actor_id: actor_id.clone(),
                repo,
                number,
                head: sha,
                title: String::new(),
                base: String::new(),
                fork: t.fork,
            }
        }
        TriggerKind::Tag => Event::Tag {
            actor,
            actor_id: actor_id.clone(),
            repo,
            tag: tag_of()?,
        },
        TriggerKind::Release => Event::Release {
            actor,
            actor_id: actor_id.clone(),
            repo,
            tag: tag_of()?,
        },
        TriggerKind::Manual => Event::Manual {
            actor,
            repo,
            r#ref: needs_ref()?,
            sha,
            reason: t.reason,
        },
        TriggerKind::Api => Event::Api {
            actor,
            repo,
            r#ref: needs_ref()?,
            sha,
            reason: t.reason,
        },
        TriggerKind::Comment | TriggerKind::Cron | TriggerKind::Upstream if t.r#ref.is_some() => {
            return Err(bad(format!("a {} trigger has no ref", t.kind)))
        }
        TriggerKind::Comment => Event::Comment {
            actor,
            actor_id: actor_id.clone(),
            repo,
            issue: 0,
            body: String::new(),
        },
        TriggerKind::Cron => Event::Cron {
            schedule: String::new(),
        },
        TriggerKind::Upstream => Event::Upstream {
            repo,
            run: String::new(),
        },
    })
}

/// The Pipeline form's input (an `Err` when it could not be built), what
/// `match` sees, and the Pipeline's plugin gates.
type PipelineInput = (
    Result<Value, String>,
    MatchContext,
    Vec<run_create::PluginGate>,
);

/// The input of the Pipeline form: compiled here from the sent sources,
/// then built through the code admission runs (`admission_before_policies`,
/// `run_create_facts`, `run_create_input`), so the input evaluated is the
/// one admission would record for the same event. A refusal admission makes
/// before consulting Policies is a 400. An input that cannot be built (a
/// Run over a list or string cap) is an `Err` evaluated as admission
/// evaluates it, with the Pipeline's plugin gates.
async fn pipeline_input(
    st: &AppState,
    org: &str,
    caller: &str,
    p: PolicyTestPipeline,
) -> Result<PipelineInput, ApiError> {
    let repo = scarab_forge::RepoRef {
        owner: org.to_string(),
        name: p.project.clone(),
    };
    let event = test_event(p.trigger, repo.clone(), caller)?;
    // `lib:` resolves against the org's installations, as at admission.
    let installed = crate::installed_libraries(st.libraries.as_deref(), org)
        .await
        .map_err(ApiError::Db)?;
    let libraries = scarab_pipeline::Libraries {
        files: p.libs,
        installed,
    };
    let (ir, _) = crate::compile_bounded(p.yaml, libraries)
        .await
        .map_err(|e| ApiError::BadRequest(format!("the Pipeline does not compile: {e}")))?;

    let refused = |why: String| {
        ApiError::BadRequest(format!(
            "admission refuses this Run before its Policies are consulted: {why}"
        ))
    };
    let gov = match crate::admission_before_policies(
        st.environment_governance.as_deref(),
        &event,
        &ir,
        &repo,
    )
    .await
    {
        Ok(Ok(gov)) => gov,
        Ok(Err(refusal)) => {
            return Err(refused(crate::admission_refusal_message(
                &refusal, org, &repo.name,
            )))
        }
        Err(crate::TriggerError::ReasonRequired(m)) => return Err(refused(m)),
        Err(crate::TriggerError::Db(e)) => return Err(ApiError::Db(e)),
        Err(e) => return Err(ApiError::Db(DbError::Other(e.to_string()))),
    };

    let path = p.path.unwrap_or_else(|| ".scarab/ci.yaml".to_string());
    let now = st.clock.now().await.0;
    // The actor's linked Principal, as admission resolves it.
    let linked = st
        .actor_links()
        .principal_of(&event, &repo)
        .await
        .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
    let (facts, ctx) = crate::run_create_facts(&event, linked, &path, &repo, &gov, now, None);
    let plugin_gates = run_create::plugin_gates(&ir);
    let input = match facts {
        Ok(facts) => blocking(move || crate::run_create_input(&ir, facts)).await?,
        Err(e) => Err(e),
    };
    Ok((input, ctx, plugin_gates))
}

/// A supplied or recorded input with a string or a list over its cap is a
/// 400: the evaluator would refuse to bind it, no admission records one, and
/// the plugin default, which reads the gates without binding them, would
/// walk an uncapped list whole.
fn check_input_caps(input: &Value, schema_version: u32) -> Result<(), ApiError> {
    use scarab_policy::schema::OverCap;
    let Some(schema) = scarab_policy::ir::schema_for(Checkpoint::RunCreate, schema_version) else {
        return Ok(());
    };
    scarab_policy::schema::check_caps(&schema, input).map_err(|e| {
        ApiError::BadRequest(match e {
            OverCap::String(e) => format!(
                "the input's `{}` is {} bytes; a run.create input string is at most {} bytes",
                e.path, e.len, e.max
            ),
            OverCap::List(e) => format!(
                "the input's `{}` has {} items; a run.create input holds at most {} there",
                e.path, e.len, e.cap
            ),
        })
    })
}

/// What each rule of `policies` reads.
fn reads_of(policies: &[StoredPolicy]) -> Vec<PolicyReadsDto> {
    policies
        .iter()
        .map(|p| PolicyReadsDto {
            policy_id: p.id.clone(),
            version: p.version,
            name: p.name.clone(),
            rules: p
                .compile()
                .map(|ir| {
                    ir.rules
                        .iter()
                        .enumerate()
                        .map(|(index, r)| match &r.predicate {
                            Predicate::Cel(c) => RuleReadsDto {
                                index,
                                predicate: "cel".into(),
                                reads: c.reads.clone(),
                            },
                            Predicate::Typed(_) => RuleReadsDto {
                                index,
                                predicate: "typed".into(),
                                reads: Vec::new(),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// Evaluate `run.create` Policies without enforcing and without recording
/// (ADR-0075 §9). Read on the org evaluates stored Policies: the answer
/// names org Policies and can replay any recorded decision of the org, both
/// of which org Read already shows. Read on a Project evaluates that
/// Project's effective Policies and replays its decisions, which is what a
/// refused Run of the Project shows. A draft `policy` needs Administer on the
/// org, as saving it would: it runs rules the caller wrote, and only an org
/// admin's rules run at admission.
///
/// Policies: `policy` (a draft, compiled at the org level; a 400 when it
/// does not compile), `policy_id` (a stored Policy of this org), or neither
/// (the org's and the input's Project's effective Policies, as admission
/// consults them). Input: `input`, `decision_id` (its recorded input and
/// schema version), or `pipeline` (compiled here with `libs`; no forge
/// read). A supplied or recorded input with a string or a list over its cap
/// is a 400.
/// A Pipeline whose Run is over an input cap is answered as admission
/// decides it: each matched Policy is an error, which denies when the Policy
/// is enforced.
///
/// Permissions: Read on the input's Project for its effective Policies
/// (neither `policy` nor `policy_id`, which is what `scarab validate
/// --server` sends) or its own typed setting by id; Read on the org for an
/// org Policy by id; Administer on the org for a draft `policy`, as saving it
/// would need. Org Read covers every Project.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/policy-test",
    params(("org" = String, Path, description = "org")),
    request_body = PolicyTestRequest,
    summary = "Evaluate run.create Policies without enforcing or recording (ADR-0075 §9)",
    responses(
        (status = 200, body = PolicyTestResponse),
        (status = 400, description = "not one input form, a draft that does not compile, a Pipeline that does not compile, a Run admission refuses before its Policies, an input that cannot be matched, or a supplied input with a string or a list over its cap"),
        (status = 403, description = "no Read on the input's Project or the org, an org Policy by id without Read on the org, or a draft `policy` without Administer on the org"),
        (status = 404, description = "no such Policy or decision in this org, or policies not configured")
    )
)]
pub(crate) async fn policy_test(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<PolicyTestRequest>,
) -> Result<Json<PolicyTestResponse>, ApiError> {
    // A draft runs rules the caller wrote: org Administer, as saving one.
    let action = if body.policy.is_some() {
        Action::Administer
    } else {
        Action::Read
    };
    let caller = super::authenticate(&st, &headers, action).await?;
    let org_allowed =
        caller.may_globally(action) || caller.may(&st, action, &rbac_scope(&org, None)).await?;
    if body.policy.is_some() && !org_allowed {
        return Err(ApiError::Forbidden);
    }
    // Without org Read, Read on the input's Project evaluates that
    // Project's Policies: the decision a refused Run of the Project shows.
    // Checked before the input is compiled or evaluated.
    let project_allowed = |project: String| {
        let (st, caller, org) = (&st, &caller, &org);
        async move {
            let scope = rbac_scope(org, Some(&project));
            if org_allowed || caller.may(st, Action::Read, &scope).await? {
                Ok(())
            } else {
                Err(ApiError::Forbidden)
            }
        }
    };
    let store = store(&st)?;
    if body.policy.is_some() && body.policy_id.is_some() {
        return Err(ApiError::BadRequest(
            "name `policy` or `policy_id`, not both".into(),
        ));
    }
    let forms = [
        body.input.is_some(),
        body.decision_id.is_some(),
        body.pipeline.is_some(),
    ];
    if forms.iter().filter(|f| **f).count() != 1 {
        return Err(ApiError::BadRequest(
            "name exactly one of `input`, `decision_id` and `pipeline`".into(),
        ));
    }

    // The input (an `Err` when a Pipeline's could not be built), its schema
    // version, what `match` sees, and the Pipeline's plugin gates.
    let (input, schema_version, ctx, plugin_gates) = if let Some(input) = body.input {
        let ctx = match_context_of(&input)?;
        project_allowed(ctx.project.clone()).await?;
        check_input_caps(&input, run_create::SCHEMA_VERSION)?;
        (Ok(input), run_create::SCHEMA_VERSION, ctx, None)
    } else if let Some(id) = body.decision_id {
        let record = store
            .decision(&id)
            .await
            .map_err(store_api_error)?
            .filter(|d| d.org == org)
            .ok_or(ApiError::NotFound)?;
        match record.project.clone() {
            Some(project) => project_allowed(project).await?,
            None if org_allowed => {}
            None => return Err(ApiError::Forbidden),
        }
        if record.decision.checkpoint != Checkpoint::RunCreate {
            return Err(ApiError::BadRequest(format!(
                "decision `{id}` is at `{}`; only `run.create` is evaluated",
                record.decision.checkpoint
            )));
        }
        if record.decision.input.is_null() {
            return Err(ApiError::BadRequest(format!(
                "decision `{id}` recorded no input (its Run did not fit an input list cap)"
            )));
        }
        check_input_caps(&record.decision.input, record.decision.schema_version)?;
        let ctx = match_context_of(&record.decision.input)?;
        let version = record.decision.schema_version;
        (Ok(record.decision.input), version, ctx, None)
    } else {
        let pipeline = body.pipeline.expect("one form is present");
        project_allowed(pipeline.project.clone()).await?;
        let (input, ctx, gates) =
            pipeline_input(&st, &org, &caller.principal.subject, pipeline).await?;
        (input, run_create::SCHEMA_VERSION, ctx, Some(gates))
    };

    // The Policies, as stored rows, so one evaluator serves all three.
    let policies: Vec<StoredPolicy> = if let Some(raw) = body.policy {
        let policy = compiled_body(raw, Level::Org).await?;
        vec![StoredPolicy {
            id: String::new(),
            scope: org_scope(&org),
            name: policy.name.clone(),
            version: 0,
            content_hash: policy.content_hash(),
            body: policy,
            created_at: 0,
            updated_at: 0,
            deleted: false,
        }]
    } else if let Some(id) = body.policy_id {
        let p = store
            .get(&id)
            .await
            .map_err(store_api_error)?
            .filter(|p| p.scope.org() == org && !p.deleted)
            .ok_or(ApiError::NotFound)?;
        // An org Policy is read with org Read, as listing it is.
        if p.scope.project().is_none() && !org_allowed {
            return Err(ApiError::Forbidden);
        }
        // A Project typed setting applies to its own Project's Runs only.
        if let Some(project) = p.scope.project().filter(|p| *p != ctx.project) {
            return Err(ApiError::BadRequest(format!(
                "policy `{id}` is a typed setting of project `{project}`; the input is for \
                 project `{}`",
                ctx.project
            )));
        }
        vec![p]
    } else {
        store
            .effective(&org, Some(&ctx.project), Checkpoint::RunCreate)
            .await
            .map_err(store_api_error)?
    };

    let reads_input = policies.clone();
    let decision = blocking(move || {
        evaluate_stored(
            Checkpoint::RunCreate,
            schema_version,
            input.as_ref().map_err(String::as_str),
            plugin_gates.as_deref(),
            &ctx,
            &policies,
        )
    })
    .await?;
    let reads = blocking(move || reads_of(&reads_input)).await?;
    Ok(Json(PolicyTestResponse {
        decision,
        policies: reads,
    }))
}
