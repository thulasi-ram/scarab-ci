//! Org-installed libraries (ADR-0076 §4, §6, §9): the API.
//!
//! **Install is two calls.** `POST …/libraries/fetch` reads the source,
//! checks it compiles as a closed library, stores it as a LibraryVersion and
//! returns it with the diff from the version installed under the same name.
//! `PUT …/libraries/{publisher}/{name}` installs a stored version by hash,
//! naming the hash it replaces, so what is installed is what the admin read.
//! The install needs a step-up (it loosens); the fetch installs nothing.
//!
//! **Private sources.** A fetch through a ForgeConnection is allowed only
//! when the source repository's owner is the org, or the repository is bound
//! to the org: a connection's credential may reach repositories of other
//! orgs, and connections are not yet per org.
//!
//! **Every library is installed the same way.** Scarab's own libraries (the
//! clone library among them) are fetched from their source and reviewed like
//! any other: a new org has none installed, and none is trusted with forge
//! credentials until an org admin trusts its source.

use std::collections::BTreeSet;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use scarab_engine::DbError;
use scarab_identity::Action;
use scarab_policy::library::{
    content_hash, valid_segment, LibraryInstallation, LibraryStore, LibraryStoreError,
    LibraryVersion, LibraryYank, NewInstallation, Replaces, TrustGrant, TrustedSource,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{authorize_scoped, connection_adapter, rbac_scope, ApiError, AppState};
use crate::library_fetch::{anonymous_url, KnownForges, LibrarySource};
use crate::step_up::GuardCtx;

/// The longest yank reason kept.
const MAX_REASON_BYTES: usize = 2_000;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryYankDto {
    pub content_hash: String,
    /// The library the yanked version was fetched for, `<publisher>/<name>`.
    pub publisher: String,
    pub name: String,
    /// The git source the version was fetched from, pinned to its commit.
    pub source: String,
    pub reason: String,
    pub yanked_by: String,
    pub yanked_at: i64,
    /// The Runs a yank with `cancel_runs` cancelled. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancelled_runs: Vec<String>,
}

/// A Run in progress that uses a library version.
#[derive(Debug, Serialize, ToSchema)]
pub struct YankImpactRunDto {
    pub id: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_number: Option<i64>,
    pub status: String,
    /// The Run's Steps inlined from the version.
    pub steps: Vec<String>,
    /// Of those, the ones running now.
    pub running_steps: Vec<String>,
}

/// What a yank of one version would stop in its org: the Runs in progress
/// that use it.
#[derive(Debug, Serialize, ToSchema)]
pub struct YankImpactDto {
    pub content_hash: String,
    pub runs: Vec<YankImpactRunDto>,
    /// How many of the Runs' Steps from the version are running now.
    pub running_steps: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryInstallationDto {
    /// 10 lowercase hex, minted at the first install and kept by upgrades.
    /// What the settings page's URL carries.
    pub id: String,
    pub publisher: String,
    pub name: String,
    pub content_hash: String,
    pub source: String,
    pub installed_by: String,
    pub installed_at: i64,
    /// The installed version is yanked in this org.
    pub yanked: Option<LibraryYankDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrgLibrariesDto {
    pub installations: Vec<LibraryInstallationDto>,
    pub yanks: Vec<LibraryYankDto>,
    /// The library sources this org trusts, one entry per source and grant
    /// (ADR-0076 §9, ADR-0018 amendment of 2026-11-12). No library is
    /// trusted without one.
    pub trusted_sources: Vec<TrustedSourceDto>,
}

/// What an org's trust in a library source grants. Each is trusted on its
/// own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustGrantDto {
    /// A Step from the source may receive a long-lived forge credential,
    /// and, with `registry_credential` too, the forge-derived registry
    /// credential.
    #[default]
    ForgeCredentials,
    /// A Step from the source may run in a `security.sandbox`.
    BuildSandbox,
    /// A Step from the source with `registry_auth` may receive the Run's
    /// registry credential.
    RegistryCredential,
}

impl From<TrustGrantDto> for TrustGrant {
    fn from(g: TrustGrantDto) -> Self {
        match g {
            TrustGrantDto::ForgeCredentials => TrustGrant::ForgeCredentials,
            TrustGrantDto::BuildSandbox => TrustGrant::BuildSandbox,
            TrustGrantDto::RegistryCredential => TrustGrant::RegistryCredential,
        }
    }
}

impl From<TrustGrant> for TrustGrantDto {
    fn from(g: TrustGrant) -> Self {
        match g {
            TrustGrant::ForgeCredentials => TrustGrantDto::ForgeCredentials,
            TrustGrant::BuildSandbox => TrustGrantDto::BuildSandbox,
            TrustGrant::RegistryCredential => TrustGrantDto::RegistryCredential,
        }
    }
}

/// A library source the org trusts with one grant (ADR-0076 §9, ADR-0018
/// amendment of 2026-11-12). Trust holds across upgrades from the same
/// source.
#[derive(Debug, Serialize, ToSchema)]
pub struct TrustedSourceDto {
    /// `<host>/<owner>/<repo>//<path>`, without a commit.
    pub source: String,
    /// What the trust grants.
    pub grant: TrustGrantDto,
    pub trusted_by: String,
    pub trusted_at: i64,
}

impl crate::people::ShowsPeople for LibraryYankDto {
    fn refs(&self) -> Vec<&str> {
        vec![&self.yanked_by]
    }
    fn show(&mut self, people: &crate::people::People) {
        people.show_one(&mut self.yanked_by);
    }
}

impl crate::people::ShowsPeople for LibraryInstallationDto {
    fn refs(&self) -> Vec<&str> {
        let mut refs = vec![self.installed_by.as_str()];
        refs.extend(self.yanked.refs());
        refs
    }
    fn show(&mut self, people: &crate::people::People) {
        people.show_one(&mut self.installed_by);
        self.yanked.show(people);
    }
}

impl crate::people::ShowsPeople for TrustedSourceDto {
    fn refs(&self) -> Vec<&str> {
        vec![&self.trusted_by]
    }
    fn show(&mut self, people: &crate::people::People) {
        people.show_one(&mut self.trusted_by);
    }
}

impl crate::people::ShowsPeople for OrgLibrariesDto {
    fn refs(&self) -> Vec<&str> {
        let mut refs = self.installations.refs();
        refs.extend(self.yanks.refs());
        refs.extend(self.trusted_sources.refs());
        refs
    }
    fn show(&mut self, people: &crate::people::People) {
        self.installations.show(people);
        self.yanks.show(people);
        self.trusted_sources.show(people);
    }
}

/// `DELETE …/library-trust?source=…&grant=…`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UntrustSourceQuery {
    /// The trusted source, `<host>/<owner>/<repo>//<path>`.
    #[serde(default)]
    pub source: Option<String>,
    /// The grant to remove; every grant of the source when omitted (a
    /// client from before grants existed means the source's whole trust).
    /// Named, the source's other grants stay.
    #[serde(default)]
    pub grant: Option<TrustGrantDto>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct TrustSourceRequest {
    /// `<host>/<owner>/<repo>//<path>`; an `@<commit>` is dropped, since
    /// trust is for every version from the source.
    pub source: String,
    /// What the trust grants; `forge_credentials` when omitted.
    #[serde(default)]
    pub grant: TrustGrantDto,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FetchLibraryRequest {
    /// `<host>/<owner>/<repo>//<path>@<commit>`.
    pub source: String,
    /// A ForgeConnection id, for a private repository. Omitted: fetched
    /// anonymously over https.
    #[serde(default)]
    pub connection: Option<String>,
    /// Defaults to the source's owner.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Defaults to the file name without its extension.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstalledVersionDto {
    pub content_hash: String,
    pub source: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FetchedLibraryDto {
    pub publisher: String,
    pub name: String,
    pub content_hash: String,
    pub source: String,
    pub content: String,
    /// What is installed under this name now, if anything: pass its hash as
    /// `replaces` to upgrade.
    pub current: Option<InstalledVersionDto>,
    /// The change from `current`, one line per entry, each starting with
    /// `+`, `-` or a space. Empty when nothing is installed.
    pub diff: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct InstallLibraryRequest {
    /// A version fetched into this org.
    pub content_hash: String,
    /// The hash installed now (an upgrade), or omitted for a first install.
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct YankLibraryRequest {
    pub content_hash: String,
    /// Shown in every refusal.
    pub reason: String,
    /// Also cancel the Runs in progress that use the version, as
    /// `POST /v1/runs/{id}/cancel` does: their running Steps stop. Default
    /// `false`: a Step that is running finishes.
    #[serde(default)]
    pub cancel_runs: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn store(st: &AppState) -> Result<&dyn LibraryStore, ApiError> {
    st.libraries.as_deref().ok_or(ApiError::NotFound)
}

pub(crate) fn store_api_error(e: LibraryStoreError) -> ApiError {
    match e {
        LibraryStoreError::UnknownVersion(h) => ApiError::BadRequest(format!(
            "version {h} has not been fetched into this org; fetch it first"
        )),
        LibraryStoreError::Conflict => ApiError::Conflict(
            "the installation changed since you read it; fetch again to see the current diff"
                .into(),
        ),
        LibraryStoreError::Invalid(m) => ApiError::BadRequest(m),
        LibraryStoreError::Store(m) => ApiError::Db(DbError::Other(m)),
    }
}

async fn org_caller(
    st: &AppState,
    headers: &HeaderMap,
    org: &str,
    action: Action,
) -> Result<String, ApiError> {
    let scope = rbac_scope(org, None);
    Ok(authorize_scoped(st, headers, action, Some(&scope))
        .await?
        .subject)
}

fn check_hash(h: &str) -> Result<(), ApiError> {
    if h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "a content hash is 64 lowercase hex characters".into(),
        ))
    }
}

fn check_names(publisher: &str, name: &str) -> Result<(), ApiError> {
    for s in [publisher, name] {
        if !valid_segment(s) {
            return Err(ApiError::BadRequest(format!(
                "`{s}` is not a library publisher or name: use letters, digits, `.`, `_` and \
                 `-`, not starting with `.` or `-`"
            )));
        }
    }
    Ok(())
}

/// `publisher`, `name`, `source`: of the yanked version.
fn yank_dto(y: &LibraryYank, publisher: &str, name: &str, source: &str) -> LibraryYankDto {
    LibraryYankDto {
        content_hash: y.content_hash.clone(),
        publisher: publisher.to_string(),
        name: name.to_string(),
        source: source.to_string(),
        reason: y.reason.clone(),
        yanked_by: y.yanked_by.clone(),
        yanked_at: y.yanked_at,
        cancelled_runs: Vec::new(),
    }
}

fn installation_dto(i: &LibraryInstallation, yanks: &[LibraryYank]) -> LibraryInstallationDto {
    LibraryInstallationDto {
        id: i.id.clone(),
        publisher: i.publisher.clone(),
        name: i.name.clone(),
        content_hash: i.content_hash.clone(),
        source: i.source.clone(),
        installed_by: i.installed_by.clone(),
        installed_at: i.installed_at,
        yanked: yanks
            .iter()
            .find(|y| y.content_hash == i.content_hash)
            .map(|y| yank_dto(y, &i.publisher, &i.name, &i.source)),
    }
}

/// A yank with the library and source of its version, read from the
/// version row. Every yank has one: `library_yanks` references
/// `library_versions`.
async fn described_yank(
    store: &dyn LibraryStore,
    y: &LibraryYank,
) -> Result<LibraryYankDto, ApiError> {
    let v = store
        .version(&y.org, &y.content_hash)
        .await
        .map_err(store_api_error)?
        .ok_or_else(|| {
            ApiError::Db(DbError::Other(format!(
                "yank of {} in org {} has no stored version",
                y.content_hash, y.org
            )))
        })?;
    Ok(yank_dto(y, &v.publisher, &v.name, &v.source))
}

/// A line diff of `old` to `new`: common lines start with a space, removed
/// ones with `-`, added ones with `+`. Exact for files up to a few thousand
/// changed lines; past that the changed middle is shown as removed, then
/// added.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut out = String::new();
    let mut line = |sign: char, text: &str| {
        out.push(sign);
        out.push_str(text);
        out.push('\n');
    };
    for l in &a[..prefix] {
        line(' ', l);
    }
    if am.len().saturating_mul(bm.len()) <= 4_000_000 {
        // Longest common subsequence over the changed middle.
        let (n, m) = (am.len(), bm.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] = if am[i] == bm[j] {
                    t[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if am[i] == bm[j] {
                line(' ', am[i]);
                i += 1;
                j += 1;
            } else if t[(i + 1) * (m + 1) + j] >= t[i * (m + 1) + j + 1] {
                line('-', am[i]);
                i += 1;
            } else {
                line('+', bm[j]);
                j += 1;
            }
        }
        for l in &am[i..] {
            line('-', l);
        }
        for l in &bm[j..] {
            line('+', l);
        }
    } else {
        for l in am {
            line('-', l);
        }
        for l in bm {
            line('+', l);
        }
    }
    for l in &a[a.len() - suffix..] {
        line(' ', l);
    }
    out
}

/// The forges an anonymous fetch may reach: github.com and every Forgejo
/// connection's https base URL.
async fn known_forges(st: &AppState) -> Result<KnownForges, ApiError> {
    let mut known = KnownForges::default();
    if let Some(connections) = st.connections.as_deref() {
        for c in connections
            .list_connections()
            .await
            .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?
        {
            if c.kind == scarab_forge::ForgeKind::Forgejo {
                known.forgejo_bases.push(c.base_url);
            }
        }
    }
    Ok(known)
}

/// The web host a connection serves: `api.github.com` serves `github.com`;
/// otherwise the base URL's host.
fn connection_web_host(conn: &scarab_forge::ForgeConnection) -> Option<String> {
    let host = reqwest::Url::parse(&conn.base_url)
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    Some(if host == "api.github.com" {
        "github.com".to_string()
    } else {
        host
    })
}

/// Reads `src`: anonymously, or through `connection` under the org rule.
pub(crate) async fn read_source(
    st: &AppState,
    org: &str,
    src: &LibrarySource,
    connection: Option<&str>,
) -> Result<String, ApiError> {
    let bytes = match connection {
        None => {
            let url = anonymous_url(src, &known_forges(st).await?).map_err(ApiError::Denied)?;
            let fetcher = st.library_fetcher.as_ref().ok_or_else(|| {
                ApiError::Conflict(
                    "anonymous library fetches are not configured on this Scarab; pick a \
                     connection"
                        .into(),
                )
            })?;
            fetcher.get(&url).await.map_err(|e| match e {
                crate::library_fetch::FetchError::Refused(m) => ApiError::Denied(m),
                crate::library_fetch::FetchError::Failed(m) => ApiError::BadRequest(m),
            })?
        }
        Some(id) => {
            // The org rule (git-bug 19b0a45): a private source is read only
            // through a connection of the installing org, and only from that
            // org's own repositories. A connection that does not exist and
            // one of another org answer the same, so the ids of other orgs'
            // connections cannot be probed from here.
            let not_ours = || {
                ApiError::Denied(format!(
                    "connection `{id}` does not belong to org `{org}`, so `{}/{}` cannot be \
                     read through it for `{org}`; install it from a public source, or through \
                     a connection of `{org}`",
                    src.owner, src.repo
                ))
            };
            let connections = st.connections.as_deref().ok_or_else(not_ours)?;
            let conn = connections
                .get_connection(id)
                .await
                .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?
                .filter(|c| c.org.as_deref() == Some(org))
                .ok_or_else(not_ours)?;
            if connection_web_host(&conn).as_deref() != Some(src.host.as_str()) {
                return Err(ApiError::BadRequest(format!(
                    "connection `{id}` serves {}, not {}",
                    connection_web_host(&conn).unwrap_or_default(),
                    src.host
                )));
            }
            // A connection of the org reaches what its credential reaches; it
            // reads for the org only the org's own repositories.
            if src.owner != org {
                return Err(ApiError::Denied(format!(
                    "`{}/{}` is not owned by org `{org}`; a connection of `{org}` reads only \
                     `{org}`'s repositories, so install it from a public source",
                    src.owner, src.repo
                )));
            }
            let forge = connection_adapter(st, &conn).await?;
            forge
                .read_file_at_ref(&src.repo_ref(), &src.commit, &src.path)
                .await
                .map_err(|e| ApiError::BadRequest(format!("reading {src}: {e}")))?
        }
    };
    if bytes.len() > scarab_pipeline::MAX_SOURCE_BYTES {
        return Err(ApiError::Denied(format!(
            "{src} is larger than {} bytes, the limit for a fetched file",
            scarab_pipeline::MAX_SOURCE_BYTES
        )));
    }
    String::from_utf8(bytes).map_err(|_| ApiError::BadRequest(format!("{src} is not UTF-8")))
}

/// Checks `content` compiles as `lib:<publisher>/<name>` in `org`, in place
/// of whatever is installed under that name.
async fn check_compiles(
    store: &dyn LibraryStore,
    org: &str,
    publisher: &str,
    name: &str,
    content: &str,
) -> Result<(), ApiError> {
    let qualified = format!("{publisher}/{name}");
    let mut installed = store
        .installed_for_compile(org)
        .await
        .map_err(store_api_error)?;
    installed.remove(&qualified);
    let content = content.to_string();
    let checked = tokio::task::spawn_blocking(move || {
        scarab_pipeline::check_installed_library(&qualified, &content, &installed)
    })
    .await
    .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
    checked.map_err(|d| {
        ApiError::BadRequest(format!(
            "the file cannot be installed as `lib:{publisher}/{name}`:\n  - {}",
            d.join("\n  - ")
        ))
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The org's libraries: installations with their yanks, the yanks, and the
/// sources the org trusts with forge credentials. Read on the org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/libraries",
    params(("org" = String, Path, description = "org")),
    summary = "An org's installed libraries (ADR-0076 §4)",
    responses(
        (status = 200, body = OrgLibrariesDto),
        (status = 403, description = "no read access to the org"),
        (status = 404, description = "libraries not configured")
    )
)]
pub(crate) async fn list_libraries(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<OrgLibrariesDto>, ApiError> {
    org_caller(&st, &headers, &org, Action::Read).await?;
    let store = store(&st)?;
    let installations = store.installations(&org).await.map_err(store_api_error)?;
    let yanks = store.yanks(&org).await.map_err(store_api_error)?;
    let trusted = store.trusted_sources(&org).await.map_err(store_api_error)?;
    let mut described = Vec::with_capacity(yanks.len());
    for y in &yanks {
        described.push(described_yank(store, y).await?);
    }
    let dto = OrgLibrariesDto {
        installations: installations
            .iter()
            .map(|i| installation_dto(i, &yanks))
            .collect(),
        yanks: described,
        trusted_sources: trusted.iter().map(trusted_dto).collect(),
    };
    Ok(Json(
        crate::people::shown(st.identities.as_deref(), dto).await,
    ))
}

/// One installation of the org, by its id, with its yank. Read on the org.
/// An id of another org's installation is not found.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/library-installations/{id}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "installation id")
    ),
    summary = "One installed library, by id (ADR-0076 §4)",
    responses(
        (status = 200, body = LibraryInstallationDto),
        (status = 403, description = "no read access to the org"),
        (status = 404, description = "no installation with this id in the org, or libraries not configured")
    )
)]
pub(crate) async fn get_library_installation(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, id)): Path<(String, String)>,
) -> Result<Json<LibraryInstallationDto>, ApiError> {
    org_caller(&st, &headers, &org, Action::Read).await?;
    let store = store(&st)?;
    let installation = store
        .installation_by_id(&org, &id)
        .await
        .map_err(store_api_error)?
        .ok_or(ApiError::NotFound)?;
    let yanks = store.yanks(&org).await.map_err(store_api_error)?;
    Ok(Json(installation_dto(&installation, &yanks)))
}

fn trusted_dto(t: &TrustedSource) -> TrustedSourceDto {
    TrustedSourceDto {
        source: t.source.clone(),
        grant: t.grant.into(),
        trusted_by: t.trusted_by.clone(),
        trusted_at: t.trusted_at,
    }
}

/// `source` in its trust form ([`trust_key`](scarab_policy::library::trust_key)),
/// checked by parsing it as a source pinned to a commit.
fn trust_source_key(source: &str) -> Result<String, ApiError> {
    let key = scarab_policy::library::trust_key(source.trim()).to_string();
    LibrarySource::parse(&format!("{key}@{}", "0".repeat(40))).map_err(|e| {
        ApiError::BadRequest(format!(
            "`{key}` is not a library source: write `<host>/<owner>/<repo>//<path>` ({e})"
        ))
    })?;
    Ok(key)
}

/// Trust a library source with one grant. With `forge_credentials` (the
/// default; ADR-0076 §9), a Step from a library installed from it may
/// receive a Forgejo or GitHub PAT connection's token. With `build_sandbox`
/// (ADR-0018, amendment of 2026-11-12), such a Step may run in a
/// `security.sandbox`; with `registry_credential`, a `registry_auth` Step
/// may receive the Run's registry credential (the forge-derived one only
/// with `forge_credentials` too). One grant does not give another. Trust
/// holds across upgrades from the same source.
/// Needs a step-up: it loosens. Administer on the org.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/library-trust",
    params(("org" = String, Path, description = "org")),
    request_body = TrustSourceRequest,
    summary = "Trust a library source with forge credentials, the build sandbox or the registry credential",
    responses(
        (status = 200, body = TrustedSourceDto),
        (status = 400, description = "not a library source"),
        (status = 403, description = "not an org admin, or no step-up"),
        (status = 404, description = "libraries not configured")
    )
)]
pub(crate) async fn trust_library_source(
    State(st): State<AppState>,
    guard: Option<Extension<GuardCtx>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<TrustSourceRequest>,
) -> Result<Json<TrustedSourceDto>, ApiError> {
    let by = org_caller(&st, &headers, &org, Action::Administer).await?;
    super::refuse_reserved_org(&org, "trust a library source")?;
    let source = trust_source_key(&body.source)?;
    super::note_org(&st, &org).await?;
    let now = st.clock.now().await.0;
    let t = store(&st)?
        .trust_source(&TrustedSource {
            org,
            source,
            grant: body.grant.into(),
            trusted_by: by.clone(),
            trusted_at: now,
            step_up_ref: guard.and_then(|Extension(g)| g.confirmation_id()),
        })
        .await
        .map_err(store_api_error)?;
    tracing::warn!(
        org = %t.org,
        source = %t.source,
        grant = t.grant.as_str(),
        by = %by,
        "library source trusted with {}",
        t.grant.describe()
    );
    Ok(Json(
        crate::people::shown(st.identities.as_deref(), trusted_dto(&t)).await,
    ))
}

/// Stop trusting a library source with one grant, or with every grant when
/// `grant` is omitted; a named grant leaves the others. It tightens: no
/// step-up.
/// Runs created after it refuse a Step from that source that needs the
/// grant, and each launch of an existing Run checks it again. Administer
/// on the org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/library-trust",
    params(("org" = String, Path, description = "org"), UntrustSourceQuery),
    summary = "Stop trusting a library source with one grant or all of them",
    responses(
        (status = 204, description = "no longer trusted"),
        (status = 400, description = "no `source`, or not a library source"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "not trusted with that grant (with any, when none is named), or libraries not configured")
    )
)]
pub(crate) async fn untrust_library_source(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    axum::extract::Query(q): axum::extract::Query<UntrustSourceQuery>,
) -> Result<StatusCode, ApiError> {
    org_caller(&st, &headers, &org, Action::Administer).await?;
    let source = q
        .source
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("name the trusted `source`".into()))?;
    let source = trust_source_key(source)?;
    let store = store(&st)?;
    let grants: Vec<TrustGrant> = match q.grant {
        Some(grant) => vec![grant.into()],
        None => vec![
            TrustGrant::ForgeCredentials,
            TrustGrant::BuildSandbox,
            TrustGrant::RegistryCredential,
        ],
    };
    let mut removed = false;
    for grant in grants {
        removed |= store
            .untrust_source(&org, &source, grant)
            .await
            .map_err(store_api_error)?
            .is_some();
    }
    if !removed {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// How many fetched versions that were never installed an org keeps, besides
/// the one just fetched. A fetch past this deletes the oldest of them.
pub const MAX_NEVER_INSTALLED_VERSIONS: usize = 50;

/// Fetch a library from a git source pinned to a commit, check it compiles
/// as a closed library, store it, and return it with the diff from the
/// version installed under the same name. Installs nothing. The org keeps
/// the [`MAX_NEVER_INSTALLED_VERSIONS`] most recent fetches that were never
/// installed; an older one has to be fetched again. Administer on the org.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/libraries/fetch",
    params(("org" = String, Path, description = "org")),
    request_body = FetchLibraryRequest,
    summary = "Fetch a library version to review before installing (ADR-0076 §4)",
    responses(
        (status = 200, body = FetchedLibraryDto),
        (status = 400, description = "a bad source or name, a fetch that failed, or a file that does not compile as a closed library"),
        (status = 403, description = "not an org admin, a host Scarab does not fetch from, or a private repository outside the org"),
        (status = 404, description = "libraries not configured")
    )
)]
pub(crate) async fn fetch_library(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<FetchLibraryRequest>,
) -> Result<Json<FetchedLibraryDto>, ApiError> {
    let by = org_caller(&st, &headers, &org, Action::Administer).await?;
    super::refuse_reserved_org(&org, "install a library")?;
    let store = store(&st)?;
    let src = LibrarySource::parse(&body.source).map_err(ApiError::BadRequest)?;
    let publisher = body.publisher.clone().unwrap_or_else(|| src.owner.clone());
    let name = body.name.clone().unwrap_or_else(|| src.default_name());
    check_names(&publisher, &name)?;
    let content = read_source(&st, &org, &src, body.connection.as_deref()).await?;
    super::note_org(&st, &org).await?;
    let hash = content_hash(&content);
    check_compiles(store, &org, &publisher, &name, &content).await?;
    let now = st.clock.now().await.0;
    let stored = store
        .put_version(&LibraryVersion {
            org: org.clone(),
            content_hash: hash.clone(),
            content: content.clone(),
            source: src.to_string(),
            publisher: publisher.clone(),
            name: name.clone(),
            fetched_by: by,
            fetched_at: now,
        })
        .await
        .map_err(store_api_error)?;
    // Bound what fetches store: an org keeps its most recent fetched
    // versions that were never installed, and the older ones go. A failed
    // prune does not fail the fetch; the next fetch prunes again.
    if let Err(e) = store
        .prune_never_installed(&org, MAX_NEVER_INSTALLED_VERSIONS, &stored.content_hash)
        .await
    {
        tracing::warn!(org = %org, error = %e, "pruning fetched library versions failed");
    }
    let current = store
        .installation(&org, &publisher, &name)
        .await
        .map_err(store_api_error)?;
    let diff = match &current {
        Some(c) => {
            let old = store
                .version(&org, &c.content_hash)
                .await
                .map_err(store_api_error)?
                .map(|v| v.content)
                .unwrap_or_default();
            line_diff(&old, &content)
        }
        None => String::new(),
    };
    Ok(Json(FetchedLibraryDto {
        publisher,
        name,
        content_hash: stored.content_hash,
        source: stored.source,
        content,
        current: current.map(|c| InstalledVersionDto {
            content_hash: c.content_hash,
            source: c.source,
        }),
        diff,
    }))
}

/// Install or upgrade `lib:<publisher>/<name>` to a fetched version. Needs a
/// step-up: installing and upgrading loosen (ADR-0076 §4). `replaces` must
/// be the hash installed now, or omitted when nothing is. Administer on the
/// org.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/libraries/{publisher}/{name}",
    params(
        ("org" = String, Path, description = "org"),
        ("publisher" = String, Path, description = "publisher"),
        ("name" = String, Path, description = "library name")
    ),
    request_body = InstallLibraryRequest,
    summary = "Install or upgrade a library (ADR-0076 §4)",
    responses(
        (status = 200, body = LibraryInstallationDto),
        (status = 400, description = "a bad name or hash, a version not fetched into this org, or one that no longer compiles here"),
        (status = 403, description = "not an org admin, or no step-up"),
        (status = 404, description = "libraries not configured"),
        (status = 409, description = "`replaces` is not what is installed now")
    )
)]
pub(crate) async fn install_library(
    State(st): State<AppState>,
    guard: Option<Extension<GuardCtx>>,
    headers: HeaderMap,
    Path((org, publisher, name)): Path<(String, String, String)>,
    Json(body): Json<InstallLibraryRequest>,
) -> Result<Json<LibraryInstallationDto>, ApiError> {
    let by = org_caller(&st, &headers, &org, Action::Administer).await?;
    super::refuse_reserved_org(&org, "install a library")?;
    check_names(&publisher, &name)?;
    check_hash(&body.content_hash)?;
    let store = store(&st)?;
    super::note_org(&st, &org).await?;
    let version = store
        .version(&org, &body.content_hash)
        .await
        .map_err(store_api_error)?
        .ok_or_else(|| {
            store_api_error(LibraryStoreError::UnknownVersion(body.content_hash.clone()))
        })?;
    // Checked again: the org's other libraries may have changed since the
    // fetch, and a version fetched under another name is installed here.
    check_compiles(store, &org, &publisher, &name, &version.content).await?;
    let replaces = match body.replaces {
        Some(h) => Replaces::Version(h),
        None => Replaces::Nothing,
    };
    let now = st.clock.now().await.0;
    let installed = store
        .install(
            &NewInstallation {
                org: org.clone(),
                publisher,
                name,
                content_hash: version.content_hash,
                installed_by: by,
                installed_at: now,
                step_up_ref: guard.and_then(|Extension(g)| g.confirmation_id()),
            },
            &replaces,
        )
        .await
        .map_err(store_api_error)?;
    let yanks = store.yanks(&org).await.map_err(store_api_error)?;
    Ok(Json(
        crate::people::shown(
            st.identities.as_deref(),
            installation_dto(&installed, &yanks),
        )
        .await,
    ))
}

/// Remove an installation. Pipelines that invoke it then fail to compile,
/// never in the middle of a Run. Removing tightens: no step-up. Administer
/// on the org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/libraries/{publisher}/{name}",
    params(
        ("org" = String, Path, description = "org"),
        ("publisher" = String, Path, description = "publisher"),
        ("name" = String, Path, description = "library name")
    ),
    summary = "Remove an installed library (ADR-0076 §4)",
    responses(
        (status = 204, description = "removed"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "not installed, or libraries not configured")
    )
)]
pub(crate) async fn remove_library(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, publisher, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    org_caller(&st, &headers, &org, Action::Administer).await?;
    store(&st)?
        .remove(&org, &publisher, &name)
        .await
        .map_err(store_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Yank a version in this org: new Runs that use it are refused, and so are
/// launches of its Steps and releases of its gates in Runs that already
/// use it, Reruns and Retries included (ADR-0076 §6). Yanking tightens: no
/// step-up. Administer on the org.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/library-yanks",
    params(("org" = String, Path, description = "org")),
    request_body = YankLibraryRequest,
    summary = "Yank a library version (ADR-0076 §6)",
    responses(
        (status = 200, body = LibraryYankDto),
        (status = 400, description = "a bad hash, an empty or long reason, or a version this org never fetched"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "libraries not configured")
    )
)]
pub(crate) async fn yank_library(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<YankLibraryRequest>,
) -> Result<Json<LibraryYankDto>, ApiError> {
    let by = org_caller(&st, &headers, &org, Action::Administer).await?;
    check_hash(&body.content_hash)?;
    let reason = body.reason.trim();
    if reason.is_empty() || reason.len() > MAX_REASON_BYTES {
        return Err(ApiError::BadRequest(format!(
            "a yank needs a reason of 1 to {MAX_REASON_BYTES} bytes; every refusal shows it"
        )));
    }
    let now = st.clock.now().await.0;
    let store = store(&st)?;
    let y = store
        .yank(&LibraryYank {
            org,
            content_hash: body.content_hash,
            reason: reason.to_string(),
            yanked_by: by.clone(),
            yanked_at: now,
        })
        .await
        .map_err(store_api_error)?;
    tracing::warn!(org = %y.org, version = %y.content_hash, by = %by, "library version yanked");
    let mut dto = described_yank(store, &y).await?;
    // After the yank is stored, so no Step from the version launches while
    // the Runs are being cancelled. Each cancel is the operator cancel a
    // Run's Cancel button makes, journalled with the caller as `by`.
    if body.cancel_runs {
        for r in yank_impact(&st, &y.org, &y.content_hash).await?.runs {
            let run = scarab_engine::RunId(r.id.clone());
            match scarab_engine::cancel_run_request(&*st.db, &*st.clock, &run, Some(by.clone()))
                .await
            {
                Ok(true) => dto.cancelled_runs.push(r.id),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(run = %r.id, error = %e, "cancelling a Run on yank failed")
                }
            }
        }
    }
    Ok(Json(
        crate::people::shown(st.identities.as_deref(), dto).await,
    ))
}

/// The Runs in progress in `org` whose Steps use version `hash`: read from
/// each active Run's stored `origin`.
pub(crate) async fn yank_impact(
    st: &AppState,
    org: &str,
    hash: &str,
) -> Result<YankImpactDto, ApiError> {
    let active = st.db.active_runs().await?;
    let tenants = if active.is_empty() {
        Default::default()
    } else {
        st.db.run_tenants(&active).await?
    };
    let mut runs = Vec::new();
    let mut running = 0;
    for run in active {
        let Some((run_org, project)) = tenants.get(&run) else {
            continue;
        };
        if run_org != org {
            continue;
        }
        let Some(ir) = st.db.run_ir(&run).await? else {
            continue;
        };
        let Ok(ir) = serde_json::from_value::<StoredIr>(ir) else {
            continue;
        };
        let using: Vec<String> = ir
            .steps
            .into_iter()
            .filter(|s| {
                s.origin.iter().any(|o| {
                    o.kind == scarab_pipeline::OriginKind::Library && o.content_hash == hash
                })
            })
            .map(|s| s.id)
            .collect();
        if using.is_empty() {
            continue;
        }
        let Some(status) = st.db.run_status(&run).await? else {
            continue;
        };
        let running_steps: Vec<String> = st
            .db
            .steps_of_run(&run)
            .await?
            .into_iter()
            .filter(|s| s.status == scarab_engine::StepStatus::Running && using.contains(&s.step.0))
            .map(|s| s.step.0)
            .collect();
        running += running_steps.len();
        runs.push(YankImpactRunDto {
            run_number: st.db.run_number(&run).await?,
            id: run.0,
            project: project.clone(),
            status: crate::run_status_name(status).to_string(),
            steps: using,
            running_steps,
        });
    }
    runs.sort_by(|a, b| (&a.project, a.run_number).cmp(&(&b.project, b.run_number)));
    Ok(YankImpactDto {
        content_hash: hash.to_string(),
        runs,
        running_steps: running,
    })
}

/// The Runs in progress in this org that use a library version: what a yank
/// of it would stop. Administer on the org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/library-yanks/{hash}/impact",
    params(
        ("org" = String, Path, description = "org"),
        ("hash" = String, Path, description = "content hash")
    ),
    summary = "The Runs in progress that use a library version (ADR-0076 §6)",
    responses(
        (status = 200, body = YankImpactDto),
        (status = 400, description = "not a content hash"),
        (status = 403, description = "not an org admin")
    )
)]
pub(crate) async fn get_yank_impact(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, hash)): Path<(String, String)>,
) -> Result<Json<YankImpactDto>, ApiError> {
    org_caller(&st, &headers, &org, Action::Administer).await?;
    check_hash(&hash)?;
    Ok(Json(yank_impact(&st, &org, &hash).await?))
}

/// Lift a yank. Needs a step-up: it loosens (ADR-0076 §6). Administer on
/// the org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/library-yanks/{hash}",
    params(
        ("org" = String, Path, description = "org"),
        ("hash" = String, Path, description = "content hash")
    ),
    summary = "Lift a library yank (ADR-0076 §6)",
    responses(
        (status = 200, body = LibraryYankDto),
        (status = 403, description = "not an org admin, or no step-up"),
        (status = 404, description = "not yanked, or libraries not configured")
    )
)]
pub(crate) async fn unyank_library(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, hash)): Path<(String, String)>,
) -> Result<Json<LibraryYankDto>, ApiError> {
    org_caller(&st, &headers, &org, Action::Administer).await?;
    let store = store(&st)?;
    let y = store
        .unyank(&org, &hash)
        .await
        .map_err(store_api_error)?
        .ok_or(ApiError::NotFound)?;
    let dto = described_yank(store, &y).await?;
    Ok(Json(
        crate::people::shown(st.identities.as_deref(), dto).await,
    ))
}

// ---------------------------------------------------------------------------
// Yank: a PolicyInvariant (ADR-0076 §6, ADR-0075 §7)
// ---------------------------------------------------------------------------

/// The refusals for every yanked version that `items` (each a thing's
/// description and its `origin`) use in `org`. Asks the store only when an
/// origin has a `lib:` link. Empty when nothing is yanked.
pub async fn yank_refusals_for(
    store: &dyn LibraryStore,
    org: &str,
    items: &[(String, &[scarab_pipeline::OriginEntry])],
) -> Result<Vec<String>, LibraryStoreError> {
    let hashes: BTreeSet<String> = items
        .iter()
        .flat_map(|(_, o)| scarab_policy::library::library_hashes(o).map(str::to_string))
        .collect();
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    let hashes: Vec<String> = hashes.into_iter().collect();
    let yanks = store.yanked(org, &hashes).await?;
    if yanks.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (what, origin) in items {
        for m in scarab_policy::library::yank_refusals(what, origin, &yanks) {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    Ok(out)
}

/// Just the part of a stored IR the yank check reads.
#[derive(Deserialize)]
struct StoredIr {
    #[serde(default)]
    steps: Vec<StoredStep>,
}

#[derive(Deserialize)]
struct StoredStep {
    id: String,
    #[serde(default)]
    origin: Vec<StoredOrigin>,
}

/// One link of a stored `origin`, read so that a field this release does not
/// know is ignored: a Run created by a newer server still answers the yank
/// check here. The compiler's [`OriginEntry`](scarab_pipeline::OriginEntry)
/// stays strict, and an authored `origin` is refused at compile either way.
#[derive(Deserialize)]
struct StoredOrigin {
    kind: scarab_pipeline::OriginKind,
    name: String,
    #[serde(default)]
    source: Option<String>,
    content_hash: String,
}

impl From<StoredOrigin> for scarab_pipeline::OriginEntry {
    fn from(o: StoredOrigin) -> Self {
        Self {
            kind: o.kind,
            name: o.name,
            source: o.source,
            content_hash: o.content_hash,
        }
    }
}

/// The refusal for launching or releasing `step` of `run`, when its stored
/// `origin` uses a version yanked in the Run's org (ADR-0076 §6). Checked at
/// every launch and gate release, so a Rerun or Retry of a Run created
/// before the yank is refused too: a Rerun does not recompile, and the
/// stored IR keeps the version it ran. `Ok(None)`: not yanked, or no store
/// is wired, or the Run has no org.
///
/// This is where `step.admit` and `gate.resolve` (ADR-0075 §3) will be
/// consulted; the yank check moves inside them as a PolicyInvariant.
pub async fn stored_step_yank_refusal(
    db: &dyn scarab_engine::Db,
    store: Option<&dyn LibraryStore>,
    run: &scarab_engine::RunId,
    step: &str,
) -> Result<Option<String>, String> {
    let Some(store) = store else {
        return Ok(None);
    };
    let Some((org, _)) = db.run_tenant(run).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    // Most orgs have no yank: answer without reading the Run's IR.
    if store
        .yanks(&org)
        .await
        .map_err(|e| format!("library store: {e}"))?
        .is_empty()
    {
        return Ok(None);
    }
    let Some(ir) = db.run_ir(run).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let ir: StoredIr = serde_json::from_value(ir)
        .map_err(|e| format!("run {}: the stored IR has no readable origin: {e}", run.0))?;
    let Some(s) = ir.steps.into_iter().find(|s| s.id == step) else {
        return Ok(None);
    };
    let origin: Vec<scarab_pipeline::OriginEntry> = s.origin.into_iter().map(Into::into).collect();
    let refusals = yank_refusals_for(store, &org, &[(format!("`{step}`"), &origin)])
        .await
        .map_err(|e| format!("library store: {e}"))?;
    Ok((!refusals.is_empty()).then(|| refusals.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_diff_marks_changed_lines_only() {
        let old = "kind: library\nsteps:\n  - id: a\n    image: x@sha256:1\n";
        let new = "kind: library\nsteps:\n  - id: a\n    image: x@sha256:2\n";
        assert_eq!(
            line_diff(old, new),
            " kind: library\n steps:\n   - id: a\n-    image: x@sha256:1\n+    image: x@sha256:2\n"
        );
        assert_eq!(line_diff("", "a\n"), "+a\n");
        assert_eq!(line_diff("a\nb\nc\n", "a\nc\n"), " a\n-b\n c\n");
    }
}
