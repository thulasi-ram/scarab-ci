//! Plugins (ADR-0072, MMP slice d1): the org-admin API for
//! PluginInstallations — the compiled-in first-party catalog, install,
//! uninstall, the kill switch.
//!
//! What a plugin itself calls lives on the CredentialBroker's own listener
//! ([`crate::broker`]), never here: this module is the public API.
//!
//! Slice d3 adds **PluginConnections** (ADR-0072 §5): per manifest connection
//! slot, an admin binds the one destination (`base_url`) and credential the
//! broker may use for it. The credential is write-only — written through to
//! the SecretProvider under a server-minted key, never readable back by any
//! endpoint — and every bind replaces destination and credential together,
//! so a destination is never re-aimed with a credential entered for another.
//!
//! **Third-party installs are off** (ADR-0072 consequences) until
//! StepUpConfirmation and publisher signatures exist: only a catalog entry
//! compiled into this binary can be installed, and only by **digest** of its
//! catalog repository. Every route needs Administer on the Org.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use scarab_engine::DbError;
use scarab_identity::Action;
use scarab_plugin::{
    admit_for_mmp, validate_installation_name, ConnectionSlot, InstallationState, PluginCapability,
    PluginConnection, PluginConnectionAuthScheme, PluginInstallation, PluginManifest,
    PluginStorage, PluginStoreError, RequestRule, RuntimeStatus, MAX_CONNECTION_BASE_URL_CHARS,
    MAX_CONNECTION_CA_PEM_BYTES, MAX_CONNECTION_CREDENTIAL_BYTES, PLUGIN_STORAGE_OWNER_ANNOTATION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use super::{authorize_scoped, rbac_scope, ApiError, AppState};
use crate::step_up::GuardCtx;

/// The first-party Argo CD plugin's manifest, compiled in: what an admin
/// approves for it is exactly this document plus a digest (ADR-0072 §1-2).
const ARGOCD_MANIFEST_JSON: &str = include_str!("../../scarab-plugin-argocd/manifest.json");

/// The reserved secret-scope org PluginConnection credentials live under
/// (ADR-0072 §5), beside forge credentials' `_forge`: a connection row holds
/// only its key there (`pc-<uuid>`), and the broker reads the bytes at use.
pub const PLUGIN_CONNECTION_CREDENTIALS_ORG: &str = "_plugin_connections";

/// [`PLUGIN_CONNECTION_CREDENTIALS_ORG`] as a secret scope.
pub(crate) fn plugin_connection_secret_scope() -> scarab_secrets::SecretScope {
    scarab_secrets::SecretScope::Org {
        org: PLUGIN_CONNECTION_CREDENTIALS_ORG.to_string(),
    }
}

/// The plugin runtime's policy as the API enforces it (from
/// [`crate::config::PluginsConfig`]).
#[derive(Debug, Clone)]
pub struct PluginPolicy {
    /// `SCARAB_PLUGINS=on`. Off, installs are refused; the registry, kill
    /// switch and read views still work.
    pub enabled: bool,
    /// The namespace a broker caller's ServiceAccount must be in.
    pub namespace: String,
    /// The repository the first-party Argo CD plugin installs from.
    pub argocd_repo: String,
    /// Dev only: a first-party install may name a tag instead of a digest.
    pub dev_allow_tag: bool,
}

impl PluginPolicy {
    pub fn from_config(c: &crate::config::PluginsConfig) -> Self {
        Self {
            enabled: c.enabled,
            namespace: c.namespace.clone(),
            argocd_repo: c.argocd_repo.clone(),
            dev_allow_tag: c.dev_allow_tag,
        }
    }

    /// Plugins off, every default — a hand-built [`AppState`]'s policy.
    pub fn off() -> Self {
        Self::from_config(&crate::config::PluginsConfig::off())
    }
}

/// One plugin this binary can install.
struct CatalogEntry {
    manifest: PluginManifest,
    repository: String,
    description: &'static str,
}

/// The compiled-in first-party catalog. Its manifests are this binary's own
/// files, so one that does not parse or is not admitted is a build defect —
/// `catalog_manifests_parse_and_are_admitted` keeps that from shipping.
fn first_party_catalog(policy: &PluginPolicy) -> Vec<CatalogEntry> {
    static ARGOCD: OnceLock<PluginManifest> = OnceLock::new();
    let argocd = ARGOCD.get_or_init(|| {
        serde_json::from_str(ARGOCD_MANIFEST_JSON).expect("the compiled-in Argo CD manifest parses")
    });
    vec![CatalogEntry {
        manifest: argocd.clone(),
        repository: policy.argocd_repo.clone(),
        description: "Reads Argo CD Applications through the CredentialBroker and \
                      resolves the `wait-live` gate (first-party).",
    }]
}

/// The refusal every non-first-party install gets, verbatim.
fn third_party_refusal(detail: String) -> ApiError {
    ApiError::Denied(format!("third-party installs are off (ADR-0072): {detail}"))
}

/// `image` must be `<repository>@sha256:<64 lowercase hex>` — a digest is
/// what an admin approves (ADR-0072 §10); a tag can change under the same
/// name. `<repository>:<tag>` only under the dev knob.
fn check_image(image: &str, repository: &str, allow_tag: bool) -> Result<(), ApiError> {
    if let Some(digest) = image
        .strip_prefix(repository)
        .and_then(|rest| rest.strip_prefix("@sha256:"))
    {
        if digest.len() == 64
            && digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Ok(());
        }
    }
    if allow_tag {
        if let Some(tag) = image
            .strip_prefix(repository)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            let ok = !tag.is_empty()
                && tag.len() <= 128
                && !tag.starts_with(['.', '-'])
                && tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
            if ok {
                return Ok(());
            }
        }
    }
    Err(third_party_refusal(format!(
        "image `{image}` is not `{repository}@sha256:<digest>`"
    )))
}

/// A server-minted installation id: 10 random lowercase hex characters. It
/// names the ServiceAccount, so it is unguessable and never reused.
fn mint_installation_id() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    bytes[..5].iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest_sha256(manifest: &PluginManifest) -> String {
    let bytes = serde_json::to_vec(manifest).expect("a manifest serializes");
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The gates `plugin:<installation>/<gate>` resolves to in `org` (ADR-0076
/// §7): each gate declared by an installation whose approved manifest asks
/// for `resolve_gates`, keyed `<installation>/<gate>`. A killed installation's
/// gates still resolve: its gates wait, and expire, as any other's do.
pub(crate) async fn gates_for_compile(
    store: &dyn scarab_plugin::PluginStore,
    org: &str,
) -> Result<std::collections::BTreeMap<String, scarab_pipeline::PluginGateDef>, DbError> {
    let installations = store
        .installations(org)
        .await
        .map_err(|e| DbError::Other(format!("plugin store: {e}")))?;
    let mut out = std::collections::BTreeMap::new();
    for i in installations
        .iter()
        .filter(|i| i.org == org && i.manifest.has(PluginCapability::ResolveGates))
    {
        for (gate, def) in &i.manifest.gates {
            out.insert(
                format!("{}/{gate}", i.name),
                scarab_pipeline::PluginGateDef {
                    interface: def.interface.clone(),
                    manifest_sha256: i.manifest_sha256.clone(),
                },
            );
        }
    }
    Ok(out)
}

/// A [`PluginStoreError`] as the API answers it.
pub(crate) fn plugin_store_api_error(e: PluginStoreError) -> ApiError {
    match e {
        PluginStoreError::Invalid(m) => ApiError::BadRequest(m),
        e @ (PluginStoreError::NameTaken(_)
        | PluginStoreError::NameRetired(_)
        | PluginStoreError::Changed(_)) => ApiError::Conflict(e.to_string()),
        PluginStoreError::NotFound => ApiError::NotFound,
        e @ PluginStoreError::Killed => ApiError::Denied(e.to_string()),
        // Busy reaches here only from the public API's own pool; the broker
        // answers it 503 itself.
        e
        @ (PluginStoreError::IdTaken | PluginStoreError::Busy(_) | PluginStoreError::Store(_)) => {
            ApiError::Db(DbError::Other(e.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// One entry of the closed PluginCapability vocabulary (ADR-0072 §2).
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapabilityDto {
    /// Retired by ADR-0074; shown only on a manifest approved before then.
    ReportDeployments,
    SubscribeEvents,
    OfferActions,
    UseConnection,
    ContributeUi,
    /// Dropped by ADR-0076 §8; shown only so it can be refused by name.
    ApproveGates,
    /// Resolve this installation's own gates of kind `plugin` (ADR-0076 §7).
    ResolveGates,
}

impl From<PluginCapability> for PluginCapabilityDto {
    fn from(c: PluginCapability) -> Self {
        match c {
            PluginCapability::ReportDeployments => Self::ReportDeployments,
            PluginCapability::SubscribeEvents => Self::SubscribeEvents,
            PluginCapability::OfferActions => Self::OfferActions,
            PluginCapability::UseConnection => Self::UseConnection,
            PluginCapability::ContributeUi => Self::ContributeUi,
            PluginCapability::ApproveGates => Self::ApproveGates,
            PluginCapability::ResolveGates => Self::ResolveGates,
        }
    }
}

/// An allowed read on a connection: method + path pattern (`*` = one
/// segment) + the query keys it may carry.
#[derive(Debug, Serialize, ToSchema)]
pub struct RequestRuleDto {
    pub method: String,
    pub path: String,
    /// The query keys a request under this rule may carry, each at most
    /// once; empty = no query. Anything else is refused by the broker.
    pub query: Vec<String>,
}

impl From<&RequestRule> for RequestRuleDto {
    fn from(r: &RequestRule) -> Self {
        Self {
            method: r.method.clone(),
            path: r.path.clone(),
            query: r.query.clone(),
        }
    }
}

/// A named external system the plugin reaches through the broker.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectionSlotDto {
    pub name: String,
    pub auth_schemes: Vec<String>,
    pub read_rules: Vec<RequestRuleDto>,
}

/// What the plugin asks for — exactly what an admin approves with the
/// digest (ADR-0072 §1-2).
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginManifestDto {
    pub plugin: String,
    pub capabilities: Vec<PluginCapabilityDto>,
    pub connections: Vec<ConnectionSlotDto>,
    pub config_keys: Vec<String>,
    /// The gates the plugin resolves (ADR-0076 §7), by name; empty without
    /// `resolve_gates`.
    pub gates: Vec<GateDefinitionDto>,
}

/// A gate a plugin resolves: a Pipeline waits on it with `invoke:
/// plugin:<installation>/<name>`.
#[derive(Debug, Serialize, ToSchema)]
pub struct GateDefinitionDto {
    pub name: String,
    pub description: String,
    /// What a Pipeline passes under `with:`.
    pub inputs: Vec<GateFieldDto>,
    /// What the plugin returns when it releases the gate.
    pub outputs: Vec<GateFieldDto>,
}

/// One input or output of a gate definition, with its type.
#[derive(Debug, Serialize, ToSchema)]
pub struct GateFieldDto {
    pub name: String,
    /// `string`, `boolean`, `number` or `choice` (inputs only).
    #[serde(rename = "type")]
    pub r#type: String,
    /// Whether a Pipeline must pass it (always true for an output).
    pub required: bool,
}

fn param_type_wire(t: scarab_pipeline::ParamType) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl From<&PluginManifest> for PluginManifestDto {
    fn from(m: &PluginManifest) -> Self {
        Self {
            plugin: m.plugin.clone(),
            capabilities: m.capabilities.iter().map(|c| (*c).into()).collect(),
            connections: m
                .connections
                .iter()
                .map(|s| ConnectionSlotDto {
                    name: s.name.clone(),
                    auth_schemes: s.auth_schemes.clone(),
                    read_rules: s.read_rules.iter().map(RequestRuleDto::from).collect(),
                })
                .collect(),
            config_keys: m.config_keys.clone(),
            gates: m
                .gates
                .iter()
                .map(|(name, g)| GateDefinitionDto {
                    name: name.clone(),
                    description: g.description.clone(),
                    inputs: g
                        .interface
                        .inputs
                        .iter()
                        .map(|p| GateFieldDto {
                            name: p.name.clone(),
                            r#type: param_type_wire(p.r#type),
                            required: p.required,
                        })
                        .collect(),
                    outputs: g
                        .interface
                        .outputs
                        .iter()
                        .map(|o| GateFieldDto {
                            name: o.name.clone(),
                            r#type: param_type_wire(o.r#type),
                            required: true,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// A plugin this Scarab can install.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginCatalogEntryDto {
    /// The catalog key `POST …/plugins` names as `plugin`.
    pub plugin: String,
    pub description: String,
    /// Install as `<image_repository>@sha256:<digest>`.
    pub image_repository: String,
    pub manifest: PluginManifestDto,
    /// Always true today: third-party installs are off.
    pub first_party: bool,
}

/// Whether an installation's broker calls are accepted.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallationStateDto {
    Enabled,
    /// Stopped by the kill switch: nothing it sends is accepted.
    Killed,
}

impl From<InstallationState> for InstallationStateDto {
    fn from(s: InstallationState) -> Self {
        match s {
            InstallationState::Enabled => Self::Enabled,
            InstallationState::Killed => Self::Killed,
        }
    }
}

/// What the plugin runtime last observed of the workload.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatusDto {
    Pending,
    Probing,
    Running,
    ProbeFailed,
    Error,
    Stopped,
}

impl From<RuntimeStatus> for RuntimeStatusDto {
    fn from(s: RuntimeStatus) -> Self {
        match s {
            RuntimeStatus::Pending => Self::Pending,
            RuntimeStatus::Probing => Self::Probing,
            RuntimeStatus::Running => Self::Running,
            RuntimeStatus::ProbeFailed => Self::ProbeFailed,
            RuntimeStatus::Error => Self::Error,
            RuntimeStatus::Stopped => Self::Stopped,
        }
    }
}

/// A PluginInstallation (ADR-0072 §10).
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginInstallationDto {
    /// Server-minted, 10 hex; names the installation's ServiceAccount.
    pub id: String,
    pub org: String,
    /// Unique in the org and never reused.
    pub name: String,
    /// The catalog plugin.
    pub plugin: String,
    /// `plugin:<name>` — what it acts as in audit.
    pub principal: String,
    pub image: String,
    pub manifest: PluginManifestDto,
    pub manifest_sha256: String,
    pub config: BTreeMap<String, String>,
    pub state: InstallationStateDto,
    pub runtime_status: RuntimeStatusDto,
    pub runtime_message: String,
    /// Unix-ms the isolation probe last passed.
    pub probe_passed_at: Option<i64>,
    pub approved_by: String,
    /// Unix-ms.
    pub approved_at: i64,
    pub killed_by: Option<String>,
    /// Unix-ms.
    pub killed_at: Option<i64>,
    /// The PersistentVolumeClaim its Pod mounts (ADR-0074 §7); absent = none.
    pub storage: Option<PluginStorageDto>,
}

/// An installation's storage (ADR-0074 §7): a PersistentVolumeClaim an
/// operator created in the plugin namespace. Scarab mounts it read-write; it
/// never creates, backs up or reads it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginStorageDto {
    /// The claim's name in the plugin namespace.
    pub claim: String,
    /// Where the Plugin's container sees it.
    pub mount_path: String,
    /// The annotation the claim must carry before the runtime mounts it…
    pub owner_annotation: String,
    /// …and its value, `<org>/<installation name>`. The plugin namespace is
    /// shared by every org; this is how the operator says which
    /// installation the claim is for.
    pub owner_annotation_value: String,
}

fn storage_dto(i: &PluginInstallation) -> Option<PluginStorageDto> {
    i.storage.as_ref().map(|s| PluginStorageDto {
        claim: s.claim.clone(),
        mount_path: s.mount_path.clone(),
        owner_annotation: PLUGIN_STORAGE_OWNER_ANNOTATION.to_string(),
        owner_annotation_value: i.storage_owner(),
    })
}

/// `storage` on an install, and `PUT …/plugins/{name}/storage`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginStorageRequest {
    /// The PersistentVolumeClaim's name in the plugin namespace (a DNS-1123
    /// subdomain). Scarab does not create it.
    pub claim: String,
    /// An absolute path in the Plugin's container; omitted = `/data`. Not
    /// `/`, and clear of `/tmp`, `/var/run/secrets` and the other paths the
    /// runtime or the kubelet mounts.
    #[serde(default)]
    pub mount_path: Option<String>,
}

impl PluginStorageRequest {
    fn validated(&self) -> Result<PluginStorage, ApiError> {
        PluginStorage::new(&self.claim, self.mount_path.as_deref())
            .map_err(|e| ApiError::BadRequest(e.to_string()))
    }
}

fn installation_dto(i: &PluginInstallation) -> PluginInstallationDto {
    PluginInstallationDto {
        id: i.id.clone(),
        org: i.org.clone(),
        name: i.name.clone(),
        plugin: i.plugin.clone(),
        principal: i.principal().subject().to_string(),
        image: i.image.clone(),
        manifest: (&i.manifest).into(),
        manifest_sha256: i.manifest_sha256.clone(),
        config: i.config.clone(),
        state: i.state.into(),
        runtime_status: i.runtime_status.into(),
        runtime_message: i.runtime_message.clone(),
        probe_passed_at: i.probe_passed_at,
        approved_by: i.approved_by.clone(),
        approved_at: i.approved_at,
        killed_by: i.killed_by.clone(),
        killed_at: i.killed_at,
        storage: storage_dto(i),
    }
}

/// `POST /v1/orgs/{org}/plugins`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallPluginRequest {
    /// The installation's name: a DNS-1123 label, unique in the org and
    /// never reused once uninstalled.
    pub name: String,
    /// The catalog plugin (`argocd`).
    pub plugin: String,
    /// `<catalog image_repository>@sha256:<64 hex>`.
    pub image: String,
    /// Values for the manifest's `config_keys`; omitted = none.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// A PersistentVolumeClaim to mount (ADR-0074 §7); omitted = none.
    #[serde(default)]
    pub storage: Option<PluginStorageRequest>,
}

/// What an uninstall removed.
#[derive(Debug, Serialize, ToSchema)]
pub struct UninstalledPluginDto {
    pub name: String,
}

/// One connection slot of an installation and what an admin bound to it
/// (ADR-0072 §5). Never carries the credential, nor its secret key.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginConnectionDto {
    /// The manifest's slot name.
    pub slot: String,
    /// The schemes the manifest allows; `bearer` is the only one today.
    pub auth_schemes: Vec<String>,
    /// What the plugin may send through it: reads only.
    pub read_rules: Vec<RequestRuleDto>,
    /// Whether an admin has bound it. Unbound, the broker answers the
    /// plugin's requests on it with 409.
    pub configured: bool,
    /// Where every request on it goes — and nowhere else.
    pub base_url: Option<String>,
    pub auth_scheme: Option<String>,
    /// Whether the credential is present in the secret store. Write-only:
    /// no endpoint returns it.
    pub credential_set: bool,
    /// The CA certificates (PEM) that are the ONLY trust anchors for
    /// `base_url`; absent = the platform's roots. Certificates are public.
    pub ca_pem: Option<String>,
    /// `base_url` is plain `http://`: the credential crosses the network in
    /// the clear. Allowed (an in-cluster Service, say); the UI warns.
    pub insecure_http: bool,
    pub updated_by: Option<String>,
    /// Unix-ms.
    pub updated_at: Option<i64>,
}

fn connection_dto(
    slot: &ConnectionSlot,
    bound: Option<&PluginConnection>,
    credential_keys: &std::collections::BTreeSet<String>,
) -> PluginConnectionDto {
    PluginConnectionDto {
        slot: slot.name.clone(),
        auth_schemes: slot.auth_schemes.clone(),
        read_rules: slot.read_rules.iter().map(RequestRuleDto::from).collect(),
        configured: bound.is_some(),
        base_url: bound.map(|c| c.base_url.clone()),
        auth_scheme: bound.map(|c| c.auth_scheme.as_str().to_string()),
        credential_set: bound.is_some_and(|c| credential_keys.contains(&c.credential_ref)),
        ca_pem: bound.and_then(|c| c.ca_pem.clone()),
        insecure_http: bound.is_some_and(|c| c.base_url.starts_with("http://")),
        updated_by: bound.map(|c| c.updated_by.clone()),
        updated_at: bound.map(|c| c.updated_at),
    }
}

/// `PUT /v1/orgs/{org}/plugins/{name}/connections/{slot}`: bind the slot's
/// destination and credential, replacing any previous binding whole.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutPluginConnectionRequest {
    /// `https://host[:port][/prefix]` (or `http://`, warned about). Every
    /// request the plugin sends on this slot goes here; it cannot pick
    /// another host.
    pub base_url: String,
    /// The bearer token the broker adds upstream. Required on every bind —
    /// a new destination never inherits a credential entered for another —
    /// and never readable back.
    pub credential: String,
    /// PEM CA certificates to trust INSTEAD of the platform roots, for an
    /// internal CA (https only). There is no skip-verify.
    #[serde(default)]
    pub ca_pem: Option<String>,
}

/// The bound form of an admin's base URL: an absolute `http(s)` URL with a
/// host and no userinfo, query or fragment, without a trailing `/` — the
/// broker appends the matched path to it.
fn normalize_connection_base_url(raw: &str) -> Result<String, ApiError> {
    let raw = raw.trim();
    let bad = |why: &str| {
        Err(ApiError::BadRequest(format!(
            "base_url {why}; it is `https://host[:port][/prefix]`"
        )))
    };
    if raw.chars().count() > MAX_CONNECTION_BASE_URL_CHARS {
        return bad(&format!(
            "is longer than {MAX_CONNECTION_BASE_URL_CHARS} characters"
        ));
    }
    let Ok(url) = reqwest::Url::parse(raw) else {
        return bad("is not an absolute URL");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return bad("must be http or https");
    }
    if url.host_str().is_none_or(str::is_empty) {
        return bad("needs a host");
    }
    // Userinfo would be a second credential beside the bound one; a query or
    // fragment would be glued onto every request in ways nobody reads.
    if !url.username().is_empty() || url.password().is_some() {
        return bad("must not carry a user or password");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return bad("must not carry a query or fragment");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// The credential as the broker will send it: a bearer token, so visible
/// ASCII with no spaces — which also keeps it from ever splitting a header.
fn check_connection_credential(raw: &str) -> Result<String, ApiError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(ApiError::BadRequest("credential is required".into()));
    }
    if token.len() > MAX_CONNECTION_CREDENTIAL_BYTES {
        return Err(ApiError::BadRequest(format!(
            "credential is longer than {MAX_CONNECTION_CREDENTIAL_BYTES} bytes"
        )));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ApiError::BadRequest(
            "a bearer credential is visible ASCII with no spaces".into(),
        ));
    }
    Ok(token.to_string())
}

/// A CA bundle an upstream client can actually be built with, for an
/// `https` destination only (a CA means nothing over plain http).
fn check_connection_ca(raw: Option<&str>, base_url: &str) -> Result<Option<String>, ApiError> {
    let Some(pem) = raw.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if !base_url.starts_with("https://") {
        return Err(ApiError::BadRequest(
            "ca_pem applies to an https base_url only".into(),
        ));
    }
    if pem.len() > MAX_CONNECTION_CA_PEM_BYTES {
        return Err(ApiError::BadRequest(format!(
            "ca_pem is larger than {MAX_CONNECTION_CA_PEM_BYTES} bytes"
        )));
    }
    crate::broker_proxy::upstream_client(Some(pem))
        .map_err(|e| ApiError::BadRequest(format!("ca_pem: {e}")))?;
    Ok(Some(format!("{pem}\n")))
}

/// A fresh credential key: `pc-<uuid>`. Server-minted, so an admin can never
/// point a connection at another connection's secret.
fn mint_connection_credential_ref() -> String {
    format!("pc-{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn org_admin(st: &AppState, headers: &HeaderMap, org: &str) -> Result<String, ApiError> {
    let scope = rbac_scope(org, None);
    Ok(
        authorize_scoped(st, headers, Action::Administer, Some(&scope))
            .await?
            .subject,
    )
}

fn store(st: &AppState) -> Result<&dyn scarab_plugin::PluginStore, ApiError> {
    st.plugins.as_deref().ok_or(ApiError::NotFound)
}

/// The plugins this Scarab can install. Administer on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/plugin-catalog",
    params(("org" = String, Path, description = "org")),
    summary = "The installable (first-party) plugins (ADR-0072)",
    responses(
        (status = 200, body = [PluginCatalogEntryDto]),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "plugins not configured")
    )
)]
pub(crate) async fn plugin_catalog(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<PluginCatalogEntryDto>>, ApiError> {
    org_admin(&st, &headers, &org).await?;
    store(&st)?;
    Ok(Json(
        first_party_catalog(&st.plugin_policy)
            .iter()
            .map(|e| PluginCatalogEntryDto {
                plugin: e.manifest.plugin.clone(),
                description: e.description.to_string(),
                image_repository: e.repository.clone(),
                manifest: (&e.manifest).into(),
                first_party: true,
            })
            .collect(),
    ))
}

/// The org's PluginInstallations, by name. Administer on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/plugins",
    params(("org" = String, Path, description = "org")),
    summary = "List an org's plugin installations (ADR-0072)",
    responses(
        (status = 200, body = [PluginInstallationDto]),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "plugins not configured")
    )
)]
pub(crate) async fn list_plugins(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<PluginInstallationDto>>, ApiError> {
    org_admin(&st, &headers, &org).await?;
    let installations = store(&st)?
        .installations(&org)
        .await
        .map_err(plugin_store_api_error)?;
    let mut out = Vec::with_capacity(installations.len());
    for installation in &installations {
        out.push(installation_dto(installation));
    }
    Ok(Json(out))
}

/// Install a first-party plugin, pinned by digest. Administer on the Org.
///
/// Third-party installs are off (403), and so are installs while
/// `SCARAB_PLUGINS=off` (409).
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/plugins",
    params(("org" = String, Path, description = "org")),
    request_body = InstallPluginRequest,
    summary = "Install a first-party plugin (ADR-0072)",
    responses(
        (status = 201, body = PluginInstallationDto),
        (status = 400, description = "invalid name, config or storage"),
        (status = 403, description = "not an org admin, or not a first-party plugin pinned by digest"),
        (status = 404, description = "plugins not configured"),
        (status = 409, description = "the name is taken or retired, or plugins are off")
    )
)]
pub(crate) async fn install_plugin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<InstallPluginRequest>,
) -> Result<(StatusCode, Json<PluginInstallationDto>), ApiError> {
    let approved_by = org_admin(&st, &headers, &org).await?;
    super::refuse_reserved_org(&org, "have a plugin installed")?;
    let store = store(&st)?;
    if !st.plugin_policy.enabled {
        return Err(ApiError::Conflict(
            "plugins are off on this Scarab (SCARAB_PLUGINS=off); an installation could \
             never run"
                .into(),
        ));
    }
    validate_installation_name(&body.name).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let catalog = first_party_catalog(&st.plugin_policy);
    let entry = catalog
        .iter()
        .find(|e| e.manifest.plugin == body.plugin)
        .ok_or_else(|| {
            third_party_refusal(format!("`{}` is not a first-party plugin", body.plugin))
        })?;
    check_image(
        &body.image,
        &entry.repository,
        st.plugin_policy.dev_allow_tag,
    )?;
    admit_for_mmp(&entry.manifest).map_err(|e| ApiError::Denied(e.to_string()))?;
    entry
        .manifest
        .validate_config(&body.config)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let storage = body
        .storage
        .as_ref()
        .map(PluginStorageRequest::validated)
        .transpose()?;
    let now = st.clock.now().await.0;
    let mut installation = PluginInstallation {
        id: mint_installation_id(),
        org,
        name: body.name,
        plugin: entry.manifest.plugin.clone(),
        image: body.image,
        manifest_sha256: manifest_sha256(&entry.manifest),
        manifest: entry.manifest.clone(),
        config: body.config,
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by,
        approved_at: now,
        killed_by: None,
        killed_at: None,
        storage,
    };
    // A 40-bit id collides about never; when it does, mint again rather
    // than fail an admin's install.
    let mut attempts = 0;
    let stored = loop {
        match store.install(&installation).await {
            Err(PluginStoreError::IdTaken) if attempts < 3 => {
                attempts += 1;
                installation.id = mint_installation_id();
            }
            other => break other.map_err(plugin_store_api_error)?,
        }
    };
    Ok((StatusCode::CREATED, Json(installation_dto(&stored))))
}

/// One installation. Administer on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/plugins/{name}",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    summary = "One plugin installation (ADR-0072)",
    responses(
        (status = 200, body = PluginInstallationDto),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured")
    )
)]
pub(crate) async fn get_plugin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<PluginInstallationDto>, ApiError> {
    org_admin(&st, &headers, &org).await?;
    let installation = store(&st)?
        .installation(&org, &name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(installation_dto(&installation)))
}

/// Uninstall. Removes its connections (and their credentials), and retires
/// the name in this org for good. Administer on the Org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/plugins/{name}",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    summary = "Uninstall a plugin; its name is retired (ADR-0072)",
    responses(
        (status = 200, body = UninstalledPluginDto),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured")
    )
)]
pub(crate) async fn uninstall_plugin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<UninstalledPluginDto>, ApiError> {
    let by = org_admin(&st, &headers, &org).await?;
    let now = st.clock.now().await.0;
    let removed = store(&st)?
        .uninstall(&org, &name, &by, now)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    // Its connections went with it; their credentials live in another store.
    for credential_ref in &removed.connection_credential_refs {
        delete_connection_credential(&st, credential_ref).await;
    }
    Ok(Json(UninstalledPluginDto { name }))
}

async fn set_state(
    st: &AppState,
    headers: &HeaderMap,
    org: &str,
    name: &str,
    state: InstallationState,
) -> Result<Json<PluginInstallationDto>, ApiError> {
    let by = org_admin(st, headers, org).await?;
    let now = st.clock.now().await.0;
    let installation = store(st)?
        .set_installation_state(org, name, state, &by, now)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    if state == InstallationState::Killed {
        tracing::warn!(
            org,
            installation = name,
            by,
            "plugin installation killed (ADR-0072 §10)"
        );
    }
    Ok(Json(installation_dto(&installation)))
}

/// The kill switch (ADR-0072 §10): from the next request on, nothing the
/// installation sends the broker is accepted. Administer on the Org.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/plugins/{name}/kill",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    summary = "Kill a plugin installation at once (ADR-0072)",
    responses(
        (status = 200, body = PluginInstallationDto),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured")
    )
)]
pub(crate) async fn kill_plugin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<PluginInstallationDto>, ApiError> {
    set_state(&st, &headers, &org, &name, InstallationState::Killed).await
}

/// Undo a kill. Administer on the Org.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/plugins/{name}/enable",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    summary = "Re-enable a killed plugin installation (ADR-0072)",
    responses(
        (status = 200, body = PluginInstallationDto),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured")
    )
)]
pub(crate) async fn enable_plugin(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<PluginInstallationDto>, ApiError> {
    set_state(&st, &headers, &org, &name, InstallationState::Enabled).await
}

fn describe_storage(s: &PluginStorage) -> String {
    format!("claim `{}` at `{}`", s.claim, s.mount_path)
}

/// Set the installation's storage (ADR-0074 §7): the PersistentVolumeClaim,
/// made by an operator in the plugin namespace and annotated for this
/// installation, that the runtime mounts read-write into its Pod. Scarab does
/// not create, back up or read it. Administer on the Org.
///
/// Adding storage to an installation that has none needs no step-up; moving
/// it to another claim or path does (ADR-0073 §3, window), since that
/// re-aims what the plugin reads and writes.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/plugins/{name}/storage",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    request_body = PluginStorageRequest,
    summary = "Set a plugin installation's storage claim (ADR-0074)",
    responses(
        (status = 200, body = PluginInstallationDto),
        (status = 400, description = "invalid claim or mount path"),
        (status = 403, description = "not an org admin, or a step-up is required"),
        (status = 404, description = "no such installation, or plugins not configured"),
        (status = 409, description = "the storage changed meanwhile")
    )
)]
pub(crate) async fn put_plugin_storage(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
    Json(body): Json<PluginStorageRequest>,
) -> Result<Json<PluginInstallationDto>, ApiError> {
    let by = org_admin(&st, &headers, &org).await?;
    let storage = body.validated()?;
    let store = store(&st)?;
    let current = store
        .installation(&org, &name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    match &current.storage {
        None => guard.exempt("adds storage to an installation that has none"),
        Some(was) if *was == storage => guard.exempt("unchanged"),
        Some(was) => {
            guard
                .require(
                    &st,
                    vec![format!(
                        "storage moves from {} to {}",
                        describe_storage(was),
                        describe_storage(&storage)
                    )],
                )
                .await?
        }
    }
    let updated = store
        .set_installation_storage(&org, &name, Some(&storage), current.storage.as_ref())
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    tracing::info!(
        org,
        installation = name,
        by,
        claim = storage.claim,
        mount_path = storage.mount_path,
        "plugin installation storage set (ADR-0074 §7)"
    );
    Ok(Json(installation_dto(&updated)))
}

/// Remove the installation's storage: the runtime stops mounting the claim.
/// The claim and its data stay in the cluster; Scarab never deletes them.
/// Administer on the Org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/plugins/{name}/storage",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    summary = "Stop mounting a plugin installation's storage claim (ADR-0074)",
    responses(
        (status = 200, body = PluginInstallationDto),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured"),
        (status = 409, description = "the storage changed meanwhile")
    )
)]
pub(crate) async fn delete_plugin_storage(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<PluginInstallationDto>, ApiError> {
    let by = org_admin(&st, &headers, &org).await?;
    let store = store(&st)?;
    let current = store
        .installation(&org, &name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    let Some(was) = &current.storage else {
        return Ok(Json(installation_dto(&current)));
    };
    let updated = store
        .set_installation_storage(&org, &name, None, Some(was))
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    tracing::info!(
        org,
        installation = name,
        by,
        claim = was.claim,
        "plugin installation storage removed (ADR-0074 §7); the claim is left as it is"
    );
    Ok(Json(installation_dto(&updated)))
}

/// An installation in `org` and the manifest slot `slot` of it, or 404.
async fn installation_and_slot(
    st: &AppState,
    org: &str,
    name: &str,
    slot: &str,
) -> Result<(PluginInstallation, ConnectionSlot), ApiError> {
    let installation = store(st)?
        .installation(org, name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    let slot = installation
        .manifest
        .connections
        .iter()
        .find(|s| s.name == slot)
        .cloned()
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "plugin installation `{name}` declares no connection slot `{slot}`"
            ))
        })?;
    Ok((installation, slot))
}

/// The keys present in the connection-credential scope — names only, never
/// a value — so a binding whose secret went missing reads "not set".
async fn credential_keys(st: &AppState) -> Result<std::collections::BTreeSet<String>, ApiError> {
    match st.secrets.as_deref() {
        Some(secrets) => Ok(secrets
            .list_scoped(&plugin_connection_secret_scope())
            .await
            .map_err(super::secret_err)?
            .into_iter()
            .collect()),
        None => Ok(Default::default()),
    }
}

/// Delete a credential no binding references any more. Best effort: a
/// failure leaves an unreachable secret (logged), never a broken binding.
async fn delete_connection_credential(st: &AppState, credential_ref: &str) {
    if let Some(secrets) = st.secrets.as_deref() {
        if let Err(e) = secrets
            .delete(&plugin_connection_secret_scope(), credential_ref)
            .await
        {
            tracing::warn!(credential_ref, error = %e,
                "plugin connection: an unreferenced credential could not be deleted");
        }
    }
}

/// An installation's connection slots and what is bound to each, in
/// manifest order — never a credential. Administer on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/plugins/{name}/connections",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name")
    ),
    summary = "A plugin's connection slots and their bindings (ADR-0072 §5)",
    responses(
        (status = 200, body = [PluginConnectionDto]),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured")
    )
)]
pub(crate) async fn list_plugin_connections(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
) -> Result<Json<Vec<PluginConnectionDto>>, ApiError> {
    org_admin(&st, &headers, &org).await?;
    let store = store(&st)?;
    let installation = store
        .installation(&org, &name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    let bound = store
        .connections(&installation.id)
        .await
        .map_err(plugin_store_api_error)?;
    let keys = credential_keys(&st).await?;
    Ok(Json(
        installation
            .manifest
            .connections
            .iter()
            .map(|slot| connection_dto(slot, bound.iter().find(|c| c.slot == slot.name), &keys))
            .collect(),
    ))
}

/// Bind a connection slot to its destination and credential (ADR-0072 §5),
/// replacing any previous binding whole. Administer on the Org.
///
/// The credential is written through to the secret store under a fresh
/// server-minted key and is never readable back; the previous binding's
/// credential is deleted once the new binding is stored. A response stream
/// the plugin has open on the old binding is cut within seconds.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/plugins/{name}/connections/{slot}",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name"),
        ("slot" = String, Path, description = "the manifest's connection slot")
    ),
    request_body = PutPluginConnectionRequest,
    summary = "Bind a plugin connection's destination and credential (ADR-0072 §5)",
    responses(
        (status = 200, body = PluginConnectionDto),
        (status = 400, description = "unknown slot, or a malformed base URL, credential or CA"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins or the secret store not configured")
    )
)]
pub(crate) async fn put_plugin_connection(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name, slot)): Path<(String, String, String)>,
    Json(body): Json<PutPluginConnectionRequest>,
) -> Result<Json<PluginConnectionDto>, ApiError> {
    let by = org_admin(&st, &headers, &org).await?;
    let store = store(&st)?;
    // No secret store, no write-through target: refusing is the only honest
    // answer, as for forge connections (ADR-0060 part D).
    let secrets = st.secrets.as_deref().ok_or(ApiError::NotFound)?;
    let (installation, slot) = installation_and_slot(&st, &org, &name, &slot).await?;
    if !slot
        .auth_schemes
        .iter()
        .any(|s| s == PluginConnectionAuthScheme::Bearer.as_str())
    {
        return Err(ApiError::BadRequest(format!(
            "connection slot `{}` allows no scheme the broker can inject",
            slot.name
        )));
    }
    let base_url = normalize_connection_base_url(&body.base_url)?;
    let credential = check_connection_credential(&body.credential)?;
    let ca_pem = check_connection_ca(body.ca_pem.as_deref(), &base_url)?;

    // Secret first, row second: a failure in between leaves an orphan
    // secret (deleted below), never a binding whose credential never landed.
    let credential_ref = mint_connection_credential_ref();
    secrets
        .put(
            &plugin_connection_secret_scope(),
            scarab_secrets::Secret {
                key: credential_ref.clone(),
                value: credential.into_bytes(),
            },
        )
        .await
        .map_err(super::secret_err)?;
    let connection = PluginConnection {
        installation_id: installation.id.clone(),
        slot: slot.name.clone(),
        base_url,
        auth_scheme: PluginConnectionAuthScheme::Bearer,
        credential_ref: credential_ref.clone(),
        ca_pem,
        updated_by: by.clone(),
        updated_at: st.clock.now().await.0,
    };
    let previous = match store.put_connection(&connection).await {
        Ok(previous) => previous,
        Err(e) => {
            delete_connection_credential(&st, &credential_ref).await;
            return Err(plugin_store_api_error(e));
        }
    };
    if let Some(previous) = previous {
        delete_connection_credential(&st, &previous).await;
    }
    // The destination is part of the authority: who aimed it where is the
    // audit line worth keeping.
    tracing::warn!(
        org,
        installation = %installation.name,
        slot = %slot.name,
        base_url = %connection.base_url,
        by,
        "plugin connection bound (ADR-0072 §5)"
    );
    let keys = credential_keys(&st).await?;
    Ok(Json(connection_dto(&slot, Some(&connection), &keys)))
}

/// Unbind a connection slot and delete its credential. The broker answers
/// the plugin's requests on it with 409 from then on, and cuts a response
/// stream already open within seconds. Administer on the Org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/plugins/{name}/connections/{slot}",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name"),
        ("slot" = String, Path, description = "the manifest's connection slot")
    ),
    summary = "Unbind a plugin connection and delete its credential (ADR-0072 §5)",
    responses(
        (status = 204, description = "unbound"),
        (status = 400, description = "unknown slot"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation or binding, or plugins not configured")
    )
)]
pub(crate) async fn delete_plugin_connection(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name, slot)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let by = org_admin(&st, &headers, &org).await?;
    let (installation, slot) = installation_and_slot(&st, &org, &name, &slot).await?;
    let removed = store(&st)?
        .delete_connection(&installation.id, &slot.name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    delete_connection_credential(&st, &removed.credential_ref).await;
    tracing::warn!(
        org,
        installation = %installation.name,
        slot = %slot.name,
        by,
        "plugin connection unbound (ADR-0072 §5)"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";

    #[test]
    fn catalog_manifests_parse_and_are_admitted() {
        for entry in first_party_catalog(&PluginPolicy::off()) {
            assert_eq!(
                admit_for_mmp(&entry.manifest),
                Ok(()),
                "{}",
                entry.manifest.plugin
            );
        }
    }

    #[test]
    fn only_a_digest_of_the_catalog_repository_is_accepted() {
        let digest = "a".repeat(64);
        assert!(check_image(&format!("{REPO}@sha256:{digest}"), REPO, false).is_ok());
        for image in [
            format!("{REPO}:v1"),
            format!("{REPO}@sha256:{}", "A".repeat(64)),
            format!("{REPO}@sha256:{}", "a".repeat(63)),
            format!("{REPO}-evil@sha256:{digest}"),
            format!("ghcr.io/evil/argocd@sha256:{digest}"),
            format!("{REPO}@sha512:{digest}"),
            REPO.to_string(),
        ] {
            assert!(check_image(&image, REPO, false).is_err(), "{image}");
        }
    }

    #[test]
    fn the_dev_knob_admits_a_tag_of_the_catalog_repository_only() {
        assert!(check_image(&format!("{REPO}:dev"), REPO, true).is_ok());
        for image in [
            format!("{REPO}:"),
            format!("{REPO}:-x"),
            format!("{REPO}:a/b"),
            "ghcr.io/evil/argocd:dev".to_string(),
        ] {
            assert!(check_image(&image, REPO, true).is_err(), "{image}");
        }
    }

    #[test]
    fn minted_ids_are_installation_ids() {
        for _ in 0..32 {
            assert!(scarab_plugin::is_installation_id(&mint_installation_id()));
        }
    }
}
