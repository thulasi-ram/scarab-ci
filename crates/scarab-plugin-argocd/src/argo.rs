//! The slice of Argo CD's REST API (`argoproj.io/v1alpha1`) the reporter
//! reads, as Argo's grpc-gateway serves it.
//!
//! Every field is optional and every list tolerates `null` (Argo writes
//! `"items": null` for an empty list): the reporter must keep working across
//! Argo versions that add, drop or leave out fields, so a shape it does not
//! know degrades to "nothing there" rather than to a parse error that would
//! stop every report.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

/// `null` or absent → `T::default()`.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// An Argo CD `Application`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Application {
    #[serde(deserialize_with = "null_default")]
    pub metadata: ObjectMeta,
    #[serde(deserialize_with = "null_default")]
    pub spec: ApplicationSpec,
    #[serde(deserialize_with = "null_default")]
    pub status: ApplicationStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ObjectMeta {
    pub name: String,
    pub namespace: String,
    pub resource_version: String,
    #[serde(deserialize_with = "null_default")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApplicationSpec {
    /// The Argo CD project: the reporter's vendor grouping (ADR-0071 §5).
    pub project: String,
    pub sync_policy: Option<SyncPolicy>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncPolicy {
    /// Present (even `{}`) = automated sync is on.
    pub automated: Option<serde_json::Value>,
    pub retry: Option<RetryStrategy>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RetryStrategy {
    /// Argo's retry limit: `0` = no retries, negative = unlimited.
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApplicationStatus {
    #[serde(deserialize_with = "null_default")]
    pub sync: SyncStatus,
    #[serde(deserialize_with = "null_default")]
    pub health: HealthStatus,
    pub operation_state: Option<OperationState>,
    /// When Argo last compared live with target. Changes on every
    /// reconcile (about every three minutes), and on a refresh.
    pub reconciled_at: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub resources: Vec<ResourceStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncStatus {
    /// `Synced`, `OutOfSync` or `Unknown`.
    pub status: String,
    /// The target revision (single-source Applications).
    pub revision: Option<String>,
    /// The target revisions (multi-source Applications).
    #[serde(deserialize_with = "null_default")]
    pub revisions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HealthStatus {
    /// `Healthy`, `Progressing`, `Degraded`, `Suspended`, `Missing` or
    /// `Unknown`.
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OperationState {
    #[serde(deserialize_with = "null_default")]
    pub operation: Operation,
    /// `Running`, `Terminating`, `Failed`, `Error` or `Succeeded`.
    pub phase: String,
    pub message: Option<String>,
    /// How many times Argo has retried this operation. While retries
    /// remain, a failed attempt leaves the phase `Running`.
    pub retry_count: Option<i64>,
    pub sync_result: Option<SyncOperationResult>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Operation {
    pub sync: Option<SyncOperation>,
    pub retry: Option<RetryStrategy>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncOperation {
    pub revision: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub revisions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncOperationResult {
    #[serde(deserialize_with = "null_default")]
    pub resources: Vec<ResourceResult>,
    pub revision: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub revisions: Vec<String>,
}

/// One resource's outcome in a sync, hooks included.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceResult {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    /// `Synced`, `SyncFailed`, `Pruned`, `PruneSkipped`.
    pub status: Option<String>,
    pub message: Option<String>,
    /// `PreSync`, `Sync`, `PostSync`, `SyncFail`, `Skip` — set only on hooks.
    pub hook_type: Option<String>,
    /// `Running`, `Succeeded`, `Failed`, `Error`, `Terminating`.
    pub hook_phase: Option<String>,
    pub sync_phase: Option<String>,
}

/// One resource of the Application as its status lists it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceStatus {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub status: Option<String>,
    pub health: Option<HealthStatus>,
    pub hook: bool,
}

/// `GET /api/v1/applications`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApplicationList {
    #[serde(deserialize_with = "null_default")]
    pub metadata: ListMeta,
    #[serde(deserialize_with = "null_default")]
    pub items: Vec<Application>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListMeta {
    pub resource_version: String,
}

/// `GET /api/v1/applications/{name}/managed-resources`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ManagedResources {
    #[serde(deserialize_with = "null_default")]
    pub items: Vec<ResourceDiff>,
}

/// One managed resource: its target (rendered from git) and live states,
/// each a JSON-encoded object **string**, `"null"` or empty when absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceDiff {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub target_state: Option<String>,
    pub live_state: Option<String>,
    pub hook: bool,
}

/// One line of `GET /api/v1/stream/applications` (NDJSON, grpc-gateway's
/// stream envelope): a `result` or an `error`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StreamLine {
    pub result: Option<ApplicationWatchEvent>,
    pub error: Option<StreamError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ApplicationWatchEvent {
    /// `ADDED`, `MODIFIED` or `DELETED`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(deserialize_with = "null_default")]
    pub application: Application,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StreamError {
    pub message: String,
    pub http_code: Option<i64>,
}

impl Application {
    /// `<appNamespace>/<name>`: the DeploymentTarget id (unique per Argo CD
    /// instance even with apps-in-any-namespace).
    pub fn target_id(&self) -> String {
        format!("{}/{}", self.metadata.namespace, self.metadata.name)
    }

    /// What decides that the rendered manifests may have changed: the target
    /// revision(s) and the last reconcile. The reporter re-reads the managed
    /// resources only when this changes (ADR-0070 §4).
    pub fn manifests_fingerprint(&self) -> String {
        let sync = &self.status.sync;
        format!(
            "{}|{}|{}",
            sync.revision.as_deref().unwrap_or(""),
            sync.revisions.join(","),
            self.status.reconciled_at.as_deref().unwrap_or("")
        )
    }

    /// Automated sync is on.
    pub fn automated(&self) -> bool {
        self.spec
            .sync_policy
            .as_ref()
            .is_some_and(|p| p.automated.is_some())
    }
}
