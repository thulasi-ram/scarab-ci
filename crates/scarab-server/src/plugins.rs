//! Plugins (ADR-0072, MMP slice d1): the org-admin API for
//! PluginInstallations — the compiled-in first-party catalog, install,
//! uninstall, the kill switch — and the read views report ingest feeds (an
//! Environment's DeploymentTargets, an installation's unmapped groupings).
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
//! catalog repository. Every route needs Administer on the Org, except an
//! Environment's DeploymentTargets, which any Org reader may see.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use scarab_engine::DbError;
use scarab_identity::Action;
use scarab_plugin::{
    admit_for_mmp, validate_installation_name, ConnectionSlot, DeploymentSourceTarget,
    EnvironmentTargetsPage, InstallationDeploymentSource, InstallationState, InstallationUsage,
    PluginCapability, PluginConnection, PluginConnectionAuthScheme, PluginInstallation,
    PluginManifest, PluginStoreError, RequestRule, RuntimeStatus, UnmappedGrouping,
    MAX_CONNECTION_BASE_URL_CHARS, MAX_CONNECTION_CA_PEM_BYTES, MAX_CONNECTION_CREDENTIAL_BYTES,
    MAX_TARGETS_PER_INSTALLATION, MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION, PAGE_DEFAULT_LIMIT,
    PAGE_MAX_LIMIT,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use super::{authorize_scoped, rbac_scope, ApiError, AppState};

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
        description: "Reports Argo CD Application syncs into the Deployment record \
                      (first-party; ADR-0070 §11's reference reporter).",
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

/// A [`PluginStoreError`] as the API answers it.
pub(crate) fn plugin_store_api_error(e: PluginStoreError) -> ApiError {
    match e {
        PluginStoreError::Invalid(m) => ApiError::BadRequest(m),
        e @ (PluginStoreError::NameTaken(_) | PluginStoreError::NameRetired(_)) => {
            ApiError::Conflict(e.to_string())
        }
        PluginStoreError::NotFound => ApiError::NotFound,
        e @ PluginStoreError::Killed => ApiError::Denied(e.to_string()),
        e @ PluginStoreError::IdempotencyConflict { .. } => ApiError::Conflict(e.to_string()),
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
    ReportDeployments,
    SubscribeEvents,
    OfferActions,
    UseConnection,
    ContributeUi,
    ApproveGates,
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
    /// The PluginActions it offers under `offer_actions` (ADR-0072 §4): what
    /// a `plugin_action` step may name.
    pub actions: Vec<PluginActionDeclDto>,
}

/// A PluginAction a plugin offers (ADR-0074 §5).
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginActionDeclDto {
    pub name: String,
    /// Seconds between offers while the action answers `pending`.
    pub poll_interval_secs: u32,
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
            actions: m
                .actions
                .iter()
                .map(|a| PluginActionDeclDto {
                    name: a.name.clone(),
                    poll_interval_secs: a.poll_interval_secs(),
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

/// Whether an installation's reports and broker calls are accepted.
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

/// An Environment source that routes one of the installation's groupings.
#[derive(Debug, Serialize, ToSchema)]
pub struct InstallationDeploymentSourceDto {
    pub environment: String,
    pub vendor_grouping: String,
}

impl From<&InstallationDeploymentSource> for InstallationDeploymentSourceDto {
    fn from(s: &InstallationDeploymentSource) -> Self {
        Self {
            environment: s.environment.clone(),
            vendor_grouping: s.vendor_grouping.clone(),
        }
    }
}

/// How much of its per-installation caps an installation uses: past one, a
/// report about a NEW target (or a new unmapped grouping) is rejected; known
/// targets keep reporting.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginUsageDto {
    /// DeploymentTargets it has reported on.
    pub targets: u64,
    pub max_targets: u64,
    /// Groupings counted as reported on with no Environment listing them
    /// (one an Environment lists since stays counted until its next report).
    pub unmapped_groupings: u64,
    pub max_unmapped_groupings: u64,
}

impl From<InstallationUsage> for PluginUsageDto {
    fn from(u: InstallationUsage) -> Self {
        Self {
            targets: u.targets,
            max_targets: MAX_TARGETS_PER_INSTALLATION,
            unmapped_groupings: u.unmapped_groupings,
            max_unmapped_groupings: MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION,
        }
    }
}

/// A PluginInstallation (ADR-0072 §10).
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginInstallationDto {
    /// Server-minted, 10 hex; names the installation's ServiceAccount.
    pub id: String,
    pub org: String,
    /// Unique in the org and never reused; reports carry it as their
    /// DeploymentTarget `source`.
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
    /// The Environment sources that name this installation — at install,
    /// the ones written before it existed, which it now feeds.
    pub deployment_sources: Vec<InstallationDeploymentSourceDto>,
    pub usage: PluginUsageDto,
}

fn installation_dto(
    i: &PluginInstallation,
    sources: &[InstallationDeploymentSource],
    usage: InstallationUsage,
) -> PluginInstallationDto {
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
        deployment_sources: sources.iter().map(Into::into).collect(),
        usage: usage.into(),
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
}

/// What an uninstall removed.
#[derive(Debug, Serialize, ToSchema)]
pub struct UninstalledPluginDto {
    pub name: String,
    /// The Environment sources that named it — removed with it.
    pub removed_deployment_sources: Vec<InstallationDeploymentSourceDto>,
}

/// A grouping the installation reports on that no Environment lists.
#[derive(Debug, Serialize, ToSchema)]
pub struct UnmappedGroupingDto {
    pub vendor_grouping: String,
    /// Reports dropped for it so far.
    pub reports: u64,
    /// Unix-ms.
    pub first_at: i64,
    /// Unix-ms.
    pub last_at: i64,
    pub last_target_id: String,
}

impl From<&UnmappedGrouping> for UnmappedGroupingDto {
    fn from(u: &UnmappedGrouping) -> Self {
        Self {
            vendor_grouping: u.vendor_grouping.clone(),
            reports: u.reports,
            first_at: u.first_at,
            last_at: u.last_at,
            last_target_id: u.last_target_id.clone(),
        }
    }
}

/// A DeploymentTarget a source reports into an Environment — one whose
/// grouping the Environment lists NOW: who reports it, its owners and
/// declared version scheme, and its last status as reported HERE.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeploymentSourceTargetDto {
    /// The reporting installation's name — the target's `source`.
    pub source: String,
    /// `plugin:<name>`.
    pub principal: String,
    pub target_id: String,
    pub vendor_grouping: String,
    pub version_scheme: Option<String>,
    /// Projects (`org/repo`) that own the target, as the source last said.
    pub owner_projects: Vec<String>,
    /// Unix-ms the owners were last reported.
    pub owners_reported_at: Option<i64>,
    /// The last status reported into THIS Environment; absent when its last
    /// status went elsewhere (or nowhere) — a status recorded in another
    /// Environment is never shown as this one's.
    pub last_version: Option<String>,
    /// `in_progress` / `live` / `failed`, as last reported here.
    pub last_status: Option<String>,
    pub last_label: Option<String>,
    /// Its source owes a re-send here: the grouping was mapped or moved into
    /// this Environment after its last status. A running reporter re-sends
    /// within about a minute (ADR-0070 §11), and it appears on the board.
    pub resend: bool,
    pub last_sequence: u64,
    /// Unix-ms.
    pub updated_at: i64,
}

impl From<&DeploymentSourceTarget> for DeploymentSourceTargetDto {
    fn from(t: &DeploymentSourceTarget) -> Self {
        Self {
            source: t.installation.clone(),
            principal: scarab_plugin::PluginPrincipal::of(&t.installation)
                .subject()
                .to_string(),
            target_id: t.target_id.clone(),
            vendor_grouping: t.vendor_grouping.clone(),
            version_scheme: t.version_scheme.clone(),
            owner_projects: t.owner_projects.clone(),
            owners_reported_at: t.owners_reported_at,
            last_version: t.last_version.clone(),
            last_status: t.last_status.clone(),
            last_label: t.last_label.clone(),
            resend: t.resend,
            last_sequence: t.last_sequence,
            updated_at: t.updated_at,
        }
    }
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

async fn with_sources(
    st: &AppState,
    installation: &PluginInstallation,
) -> Result<PluginInstallationDto, ApiError> {
    let store = store(st)?;
    let sources = store
        .deployment_sources_naming(&installation.org, &installation.name)
        .await
        .map_err(plugin_store_api_error)?;
    let usage = store
        .installation_usage(&installation.id)
        .await
        .map_err(plugin_store_api_error)?;
    Ok(installation_dto(installation, &sources, usage))
}

/// A page size from a query: default [`PAGE_DEFAULT_LIMIT`], at most
/// [`PAGE_MAX_LIMIT`].
fn page_size(limit: Option<usize>) -> usize {
    limit.unwrap_or(PAGE_DEFAULT_LIMIT).clamp(1, PAGE_MAX_LIMIT)
}

/// `GET …/unmapped-groupings` paging: keyset on the grouping.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UnmappedGroupingsQuery {
    /// Start after this grouping — the last one of the previous page.
    pub after: Option<String>,
    /// Page size: default 500, at most 1000. A page shorter than this is
    /// the last.
    pub limit: Option<usize>,
}

/// `GET …/deployment-targets` paging: keyset on (source, target id).
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeploymentTargetsQuery {
    /// Start after this target — the last row of the previous page: its
    /// `source` …
    pub after_source: Option<String>,
    /// … and its `target_id`. Both or neither.
    pub after_target_id: Option<String>,
    /// Page size: default 500, at most 1000. A page shorter than this is
    /// the last.
    pub limit: Option<usize>,
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
        out.push(with_sources(&st, installation).await?);
    }
    Ok(Json(out))
}

/// Install a first-party plugin, pinned by digest. Administer on the Org.
///
/// Third-party installs are off (403), and so are installs while
/// `SCARAB_PLUGINS=off` (409). The response lists any Environment source
/// that already named this installation — written before it existed — which
/// it now feeds.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/plugins",
    params(("org" = String, Path, description = "org")),
    request_body = InstallPluginRequest,
    summary = "Install a first-party plugin (ADR-0072)",
    responses(
        (status = 201, body = PluginInstallationDto),
        (status = 400, description = "invalid name or config"),
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
    Ok((StatusCode::CREATED, Json(with_sources(&st, &stored).await?)))
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
    Ok(Json(with_sources(&st, &installation).await?))
}

/// Uninstall. Removes every Environment source naming it and its
/// connections (and their credentials), and retires the name in this org for
/// good; its Deployment history stays. Administer on the Org.
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
    // Its pending plugin_action steps can get no answer now (ADR-0074 §5).
    crate::plugin_actions::fail_for_installation(
        &*st.db,
        &*st.clock,
        &org,
        &name,
        "was uninstalled",
    )
    .await;
    Ok(Json(UninstalledPluginDto {
        name,
        removed_deployment_sources: removed.deployment_sources.iter().map(Into::into).collect(),
    }))
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
        // Its pending plugin_action steps can get no answer now (ADR-0074 §5).
        crate::plugin_actions::fail_for_installation(&*st.db, &*st.clock, org, name, "was killed")
            .await;
    }
    Ok(Json(with_sources(st, &installation).await?))
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

/// The groupings an installation reports on that no Environment lists —
/// what is waiting to be mapped (ADR-0071 §5) — a page at a time, by
/// grouping. Administer on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/plugins/{name}/unmapped-groupings",
    params(
        ("org" = String, Path, description = "org"),
        ("name" = String, Path, description = "installation name"),
        UnmappedGroupingsQuery
    ),
    summary = "A plugin's vendor groupings no Environment lists (ADR-0071 §5)",
    responses(
        (status = 200, body = [UnmappedGroupingDto]),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such installation, or plugins not configured")
    )
)]
pub(crate) async fn unmapped_groupings(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, name)): Path<(String, String)>,
    Query(q): Query<UnmappedGroupingsQuery>,
) -> Result<Json<Vec<UnmappedGroupingDto>>, ApiError> {
    org_admin(&st, &headers, &org).await?;
    let store = store(&st)?;
    let installation = store
        .installation(&org, &name)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::NotFound)?;
    let unmapped = store
        .unmapped_groupings(&installation.id, q.after.as_deref(), page_size(q.limit))
        .await
        .map_err(plugin_store_api_error)?;
    Ok(Json(unmapped.iter().map(Into::into).collect()))
}

/// The DeploymentTargets sources report into an Environment — those whose
/// grouping it lists now — with their owners, version scheme and last status
/// reported here, a page at a time by (source, target id). Read on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}/deployment-targets",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        DeploymentTargetsQuery
    ),
    summary = "An Environment's reported DeploymentTargets and their owners (ADR-0072)",
    responses(
        (status = 200, body = [DeploymentSourceTargetDto]),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "plugins not configured")
    )
)]
pub(crate) async fn environment_deployment_targets(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Query(q): Query<DeploymentTargetsQuery>,
) -> Result<Json<Vec<DeploymentSourceTargetDto>>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let after = match (q.after_source, q.after_target_id) {
        (Some(source), Some(target_id)) => Some((source, target_id)),
        (None, None) => None,
        _ => {
            return Err(ApiError::BadRequest(
                "`after_source` and `after_target_id` go together".into(),
            ))
        }
    };
    let page = EnvironmentTargetsPage {
        after,
        limit: page_size(q.limit),
    };
    let targets = store(&st)?
        .environment_targets(&org, &env, &page)
        .await
        .map_err(plugin_store_api_error)?;
    Ok(Json(targets.iter().map(Into::into).collect()))
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
