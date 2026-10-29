//! Org-installed and first-party libraries (ADR-0076 §4-§6): the API, the
//! first-party release manifest, and the defaults a new org gets.
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
//! **First-party.** The server reads one release manifest, from the operator's
//! `SCARAB_LIBS_MANIFEST` (an https URL, a file, or `off`). A version is
//! first-party when its hash is in that manifest. An org admin can point the
//! defaults at a mirror or upload copies of the files, and every copy is
//! checked against the manifest's hash; an org admin cannot supply the
//! manifest, since that would let one org mark any content first-party.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use scarab_engine::DbError;
use scarab_identity::Action;
use scarab_policy::library::{
    content_hash, valid_segment, DefaultsState, LibraryInstallation, LibraryStore,
    LibraryStoreError, LibraryVersion, LibraryYank, MissingDefault, NewInstallation,
    OrgLibraryDefaults, Replaces,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{authorize_scoped, connection_adapter, rbac_scope, ApiError, AppState};
use crate::library_fetch::{anonymous_url, KnownForges, LibrarySource};
use crate::step_up::GuardCtx;

/// Who installs the defaults into a new org.
pub const DEFAULTS_INSTALLER: &str = "scarab:defaults";

/// The longest yank reason kept.
const MAX_REASON_BYTES: usize = 2_000;

// ---------------------------------------------------------------------------
// The release manifest (ADR-0076 §5)
// ---------------------------------------------------------------------------

/// `libs-manifest.json`, a release asset written by
/// `scripts/libs_manifest.py`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LibraryManifest {
    pub schema: u32,
    /// The release that wrote it, e.g. `v0.3.0`.
    pub release: String,
    pub libraries: Vec<ManifestLibrary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ManifestLibrary {
    pub publisher: String,
    pub name: String,
    /// Every released version, oldest first; the last is the newest.
    pub versions: Vec<ManifestVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ManifestVersion {
    pub content_hash: String,
    /// The git source, pinned to the release commit.
    pub source: String,
    pub release: String,
    /// This version fixes a security problem in an earlier one.
    pub security_fix: bool,
}

impl ManifestLibrary {
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.publisher, self.name)
    }

    pub fn latest(&self) -> Option<&ManifestVersion> {
        self.versions.last()
    }
}

impl LibraryManifest {
    /// Parses and checks a manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let m: LibraryManifest =
            serde_json::from_slice(bytes).map_err(|e| format!("not a library manifest: {e}"))?;
        if m.schema != 1 {
            return Err(format!("manifest schema {} is not 1", m.schema));
        }
        for l in &m.libraries {
            if !valid_segment(&l.publisher) || !valid_segment(&l.name) || l.versions.is_empty() {
                return Err(format!(
                    "manifest entry `{}` is not valid",
                    l.qualified_name()
                ));
            }
            for v in &l.versions {
                if v.content_hash.len() != 64
                    || !v.content_hash.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(format!(
                        "manifest entry `{}` has a bad hash",
                        l.qualified_name()
                    ));
                }
                LibrarySource::parse(&v.source)
                    .map_err(|e| format!("manifest entry `{}`: {e}", l.qualified_name()))?;
            }
        }
        Ok(m)
    }

    /// The library and version with this hash.
    pub fn find(&self, hash: &str) -> Option<(&ManifestLibrary, usize)> {
        self.libraries.iter().find_map(|l| {
            l.versions
                .iter()
                .position(|v| v.content_hash == hash)
                .map(|i| (l, i))
        })
    }
}

/// Where the server reads its release manifest from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestSource {
    Off,
    Url(String),
    File(std::path::PathBuf),
    /// Given in code (tests).
    Fixed(LibraryManifest),
}

impl ManifestSource {
    /// `SCARAB_LIBS_MANIFEST`: unset is this release's GitHub asset; `off`
    /// turns first-party libraries off; an `https://` value is a URL; any
    /// other value is a file path.
    pub fn from_env(value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            None => ManifestSource::Url(format!(
                "https://github.com/thulasi-ram/scarab-ci/releases/download/v{}/libs-manifest.json",
                env!("CARGO_PKG_VERSION")
            )),
            Some("off") => ManifestSource::Off,
            Some(v) if v.starts_with("https://") => ManifestSource::Url(v.to_string()),
            Some(v) => ManifestSource::File(v.into()),
        }
    }
}

/// The server's release manifest, read on first use and kept for an hour; a
/// failed read is kept for a minute, so a forge outage is not asked again on
/// every request.
pub struct FirstParty {
    source: ManifestSource,
    cached: tokio::sync::Mutex<Option<(Instant, Result<Arc<LibraryManifest>, String>)>>,
}

impl FirstParty {
    pub fn new(source: ManifestSource) -> Self {
        FirstParty {
            source,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// The manifest; `Ok(None)` when first-party libraries are off.
    pub async fn manifest(&self) -> Result<Option<Arc<LibraryManifest>>, String> {
        let url = match &self.source {
            ManifestSource::Off => return Ok(None),
            ManifestSource::Fixed(m) => return Ok(Some(Arc::new(m.clone()))),
            ManifestSource::Url(u) => Some(u.clone()),
            ManifestSource::File(_) => None,
        };
        let mut cached = self.cached.lock().await;
        if let Some((at, r)) = cached.as_ref() {
            let keep = if r.is_ok() {
                Duration::from_secs(3600)
            } else {
                Duration::from_secs(60)
            };
            if at.elapsed() < keep {
                return r.clone().map(Some);
            }
        }
        let read = match (&self.source, url) {
            (_, Some(u)) => read_manifest_url(&u).await,
            (ManifestSource::File(p), None) => std::fs::read(p)
                .map_err(|e| format!("reading {}: {e}", p.display()))
                .and_then(|b| LibraryManifest::parse(&b)),
            _ => unreachable!("handled above"),
        }
        .map(Arc::new);
        *cached = Some((Instant::now(), read.clone()));
        read.map(Some)
    }

    /// Is `hash` in the manifest? `false` when it cannot be read.
    pub async fn is_first_party(&self, hash: &str) -> bool {
        matches!(self.manifest().await, Ok(Some(m)) if m.find(hash).is_some())
    }
}

/// The operator chose this URL, so redirects (GitHub release downloads
/// redirect to its object store) are followed; https only.
async fn read_manifest_url(url: &str) -> Result<LibraryManifest, String> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("reading the release manifest {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "reading the release manifest {url}: {}",
            resp.status()
        ));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("reading the release manifest {url}: {e}"))?;
    if bytes.len() > scarab_pipeline::MAX_SOURCE_BYTES {
        return Err(format!("the release manifest {url} is over 1 MiB"));
    }
    LibraryManifest::parse(&bytes)
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// A newer first-party version than the one installed.
#[derive(Debug, Serialize, ToSchema)]
pub struct UpdateNoticeDto {
    pub content_hash: String,
    pub source: String,
    pub release: String,
    /// A version between the installed one and this one is a security fix.
    pub security_fix: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryYankDto {
    pub content_hash: String,
    pub reason: String,
    pub yanked_by: String,
    pub yanked_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryInstallationDto {
    pub publisher: String,
    pub name: String,
    pub content_hash: String,
    pub source: String,
    pub first_party: bool,
    pub installed_by: String,
    pub installed_at: i64,
    /// The installed version is yanked in this org.
    pub yanked: Option<LibraryYankDto>,
    /// A newer first-party version exists.
    pub update: Option<UpdateNoticeDto>,
}

/// A first-party library in the server's release manifest.
#[derive(Debug, Serialize, ToSchema)]
pub struct FirstPartyLibraryDto {
    pub publisher: String,
    pub name: String,
    pub content_hash: String,
    pub source: String,
    pub release: String,
    pub security_fix: bool,
    /// Installed in this org under its name, at any version.
    pub installed: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MissingDefaultDto {
    /// Empty when the release manifest itself could not be read.
    pub publisher: String,
    pub name: String,
    pub error: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LibraryDefaultsDto {
    /// `offered` (an org from before libraries: nothing installed without an
    /// admin), `pending`, `installed` or `missing`.
    pub state: String,
    pub missing: Vec<MissingDefaultDto>,
    pub mirror: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrgLibrariesDto {
    pub installations: Vec<LibraryInstallationDto>,
    pub yanks: Vec<LibraryYankDto>,
    pub first_party: Vec<FirstPartyLibraryDto>,
    /// Why the release manifest could not be read, if it could not.
    pub manifest_error: Option<String>,
    pub defaults: Option<LibraryDefaultsDto>,
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
    pub first_party: bool,
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
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct RetryDefaultsRequest {
    /// A mirror of Scarab's repository, `<host>/<owner>/<repo>`; each
    /// library is read from the same path and commit there.
    #[serde(default)]
    pub mirror: Option<String>,
    /// A ForgeConnection to read the mirror through.
    #[serde(default)]
    pub connection: Option<String>,
    /// Uploaded copies: `<publisher>/<name>` → the file's content.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
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

fn yank_dto(y: &LibraryYank) -> LibraryYankDto {
    LibraryYankDto {
        content_hash: y.content_hash.clone(),
        reason: y.reason.clone(),
        yanked_by: y.yanked_by.clone(),
        yanked_at: y.yanked_at,
    }
}

/// The newer first-party version for an installed hash, if there is one.
pub fn update_notice(manifest: &LibraryManifest, installed: &str) -> Option<UpdateNoticeDto> {
    let (lib, at) = manifest.find(installed)?;
    let latest = lib.latest()?;
    if at + 1 >= lib.versions.len() {
        return None;
    }
    Some(UpdateNoticeDto {
        content_hash: latest.content_hash.clone(),
        source: latest.source.clone(),
        release: latest.release.clone(),
        security_fix: lib.versions[at + 1..].iter().any(|v| v.security_fix),
    })
}

fn installation_dto(
    i: &LibraryInstallation,
    yanks: &[LibraryYank],
    manifest: Option<&LibraryManifest>,
) -> LibraryInstallationDto {
    LibraryInstallationDto {
        publisher: i.publisher.clone(),
        name: i.name.clone(),
        content_hash: i.content_hash.clone(),
        source: i.source.clone(),
        first_party: i.first_party,
        installed_by: i.installed_by.clone(),
        installed_at: i.installed_at,
        yanked: yanks
            .iter()
            .find(|y| y.content_hash == i.content_hash)
            .map(yank_dto),
        update: manifest.and_then(|m| update_notice(m, &i.content_hash)),
    }
}

fn defaults_dto(d: &OrgLibraryDefaults) -> LibraryDefaultsDto {
    LibraryDefaultsDto {
        state: d.state.as_str().to_string(),
        missing: d
            .missing
            .iter()
            .map(|m| MissingDefaultDto {
                publisher: m.publisher.clone(),
                name: m.name.clone(),
                error: m.error.clone(),
            })
            .collect(),
        mirror: d.mirror.clone(),
        updated_at: d.updated_at,
    }
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
            let connections = st.connections.as_deref().ok_or(ApiError::NotFound)?;
            let conn = connections
                .get_connection(id)
                .await
                .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?
                .ok_or_else(|| ApiError::BadRequest(format!("no connection `{id}`")))?;
            if connection_web_host(&conn).as_deref() != Some(src.host.as_str()) {
                return Err(ApiError::BadRequest(format!(
                    "connection `{id}` serves {}, not {}",
                    connection_web_host(&conn).unwrap_or_default(),
                    src.host
                )));
            }
            // The org rule: the org's own repositories, or ones bound to it.
            let bound_here = match connections
                .resolve(&src.repo_ref())
                .await
                .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?
            {
                Some(r) => r.org == org,
                None => false,
            };
            if src.owner != org && !bound_here {
                return Err(ApiError::Denied(format!(
                    "`{}/{}` is not owned by org `{org}` and is not bound to it, so it cannot be \
                     read through a connection for this org; install it from a public source, \
                     or bind the repository to `{org}` first",
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
            "{src} is larger than {} bytes, the limit for a library",
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

/// The org's libraries: installations with yanks and update notices, the
/// first-party libraries this Scarab offers, and where the org's defaults
/// stand. Read on the org.
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
    let defaults = store.defaults(&org).await.map_err(store_api_error)?;
    let (manifest, manifest_error) = match st.first_party.manifest().await {
        Ok(m) => (m, None),
        Err(e) => (None, Some(e)),
    };
    let installed_names: BTreeSet<String> =
        installations.iter().map(|i| i.qualified_name()).collect();
    let first_party = manifest
        .as_deref()
        .map(|m| {
            m.libraries
                .iter()
                .filter_map(|l| {
                    let latest = l.latest()?;
                    Some(FirstPartyLibraryDto {
                        publisher: l.publisher.clone(),
                        name: l.name.clone(),
                        content_hash: latest.content_hash.clone(),
                        source: latest.source.clone(),
                        release: latest.release.clone(),
                        security_fix: latest.security_fix,
                        installed: installed_names.contains(&l.qualified_name()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(OrgLibrariesDto {
        installations: installations
            .iter()
            .map(|i| installation_dto(i, &yanks, manifest.as_deref()))
            .collect(),
        yanks: yanks.iter().map(yank_dto).collect(),
        first_party,
        manifest_error,
        defaults: defaults.as_ref().map(defaults_dto),
    }))
}

/// Fetch a library from a git source pinned to a commit, check it compiles
/// as a closed library, store it, and return it with the diff from the
/// version installed under the same name. Installs nothing. Administer on
/// the org.
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
    let publisher = body.publisher.unwrap_or_else(|| src.owner.clone());
    let name = body.name.unwrap_or_else(|| src.default_name());
    check_names(&publisher, &name)?;
    let content = read_source(&st, &org, &src, body.connection.as_deref()).await?;
    check_compiles(store, &org, &publisher, &name, &content).await?;
    let hash = content_hash(&content);
    let now = st.clock.now().await.0;
    let stored = store
        .put_version(&LibraryVersion {
            org: org.clone(),
            content_hash: hash.clone(),
            content: content.clone(),
            source: src.to_string(),
            publisher: publisher.clone(),
            name: name.clone(),
            first_party: st.first_party.is_first_party(&hash).await,
            fetched_by: by,
            fetched_at: now,
        })
        .await
        .map_err(store_api_error)?;
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
        first_party: stored.first_party,
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
    let manifest = st.first_party.manifest().await.ok().flatten();
    Ok(Json(installation_dto(
        &installed,
        &yanks,
        manifest.as_deref(),
    )))
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
    let y = store(&st)?
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
    Ok(Json(yank_dto(&y)))
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
    let y = store(&st)?
        .unyank(&org, &hash)
        .await
        .map_err(store_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(yank_dto(&y)))
}

/// Install the first-party defaults this org is missing, from the release
/// source, a mirror, or uploaded copies; every copy must have the hash the
/// release manifest lists. Needs a step-up, as any install does. Administer
/// on the org.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/library-defaults/retry",
    params(("org" = String, Path, description = "org")),
    request_body = RetryDefaultsRequest,
    summary = "Retry installing the first-party libraries (ADR-0076 §5)",
    responses(
        (status = 200, body = LibraryDefaultsDto),
        (status = 400, description = "a bad mirror, or an uploaded file for no first-party library"),
        (status = 403, description = "not an org admin, or no step-up"),
        (status = 404, description = "libraries not configured")
    )
)]
pub(crate) async fn retry_library_defaults(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Json(body): Json<RetryDefaultsRequest>,
) -> Result<Json<LibraryDefaultsDto>, ApiError> {
    let by = org_caller(&st, &headers, &org, Action::Administer).await?;
    super::refuse_reserved_org(&org, "install a library")?;
    store(&st)?;
    let mirror = match body
        .mirror
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(m) => Some(parse_mirror(m)?),
        None => None,
    };
    let d = install_org_defaults(
        &st,
        &org,
        &by,
        DefaultsFrom {
            mirror,
            connection: body.connection,
            files: body.files,
        },
    )
    .await?;
    Ok(Json(defaults_dto(&d)))
}

/// `<host>/<owner>/<repo>`, checked by parsing it as a source.
fn parse_mirror(m: &str) -> Result<String, ApiError> {
    let m = m
        .strip_prefix("https://")
        .unwrap_or(m)
        .trim_end_matches('/');
    LibrarySource::parse(&format!("{m}//x.yaml@{}", "0".repeat(40))).map_err(|_| {
        ApiError::BadRequest(format!(
            "`{m}` is not a mirror: write `<host>/<owner>/<repo>`, a copy of Scarab's \
                 repository on a forge"
        ))
    })?;
    Ok(m.to_string())
}

/// Where [`install_org_defaults`] reads the files.
#[derive(Default)]
pub struct DefaultsFrom {
    /// `<host>/<owner>/<repo>`.
    pub mirror: Option<String>,
    pub connection: Option<String>,
    /// Uploaded copies by `<publisher>/<name>`.
    pub files: BTreeMap<String, String>,
}

/// Installs every first-party library of the release manifest that `org`
/// has not installed under its name, and records what is missing. A failure
/// to reach a source is recorded, never returned: a new org is created
/// whether or not its defaults can be read.
pub async fn install_org_defaults(
    st: &AppState,
    org: &str,
    by: &str,
    from: DefaultsFrom,
) -> Result<OrgLibraryDefaults, ApiError> {
    let store = store(st)?;
    let now = st.clock.now().await.0;
    let mut missing = Vec::new();
    match st.first_party.manifest().await {
        Ok(None) => {}
        Err(e) => missing.push(MissingDefault {
            publisher: String::new(),
            name: String::new(),
            error: format!("the release manifest could not be read: {e}"),
        }),
        Ok(Some(manifest)) => {
            for name in from.files.keys() {
                if !manifest
                    .libraries
                    .iter()
                    .any(|l| &l.qualified_name() == name)
                {
                    return Err(ApiError::BadRequest(format!(
                        "`{name}` is not a first-party library of this release"
                    )));
                }
            }
            for lib in &manifest.libraries {
                if let Err(error) = install_one_default(st, store, org, by, lib, &from, now).await {
                    missing.push(MissingDefault {
                        publisher: lib.publisher.clone(),
                        name: lib.name.clone(),
                        error,
                    });
                }
            }
        }
    }
    let d = OrgLibraryDefaults {
        org: org.to_string(),
        state: if missing.is_empty() {
            DefaultsState::Installed
        } else {
            DefaultsState::Missing
        },
        missing,
        mirror: from.mirror.clone(),
        updated_at: now,
    };
    store.set_defaults(&d).await.map_err(store_api_error)?;
    if d.state == DefaultsState::Missing {
        tracing::warn!(
            org,
            missing = d.missing.len(),
            "first-party libraries not installed"
        );
    }
    Ok(d)
}

async fn install_one_default(
    st: &AppState,
    store: &dyn LibraryStore,
    org: &str,
    by: &str,
    lib: &ManifestLibrary,
    from: &DefaultsFrom,
    now: i64,
) -> Result<(), String> {
    if store
        .installation(org, &lib.publisher, &lib.name)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Ok(());
    }
    let want = lib.latest().ok_or("the manifest lists no version")?;
    let release_src = LibrarySource::parse(&want.source)?;
    let (content, source) = match from.files.get(&lib.qualified_name()) {
        Some(c) => (c.clone(), release_src.to_string()),
        None => {
            let src = match &from.mirror {
                Some(m) => LibrarySource::parse(&format!(
                    "{m}//{}@{}",
                    release_src.path, release_src.commit
                ))?,
                None => release_src,
            };
            let content = read_source(st, org, &src, from.connection.as_deref())
                .await
                .map_err(|e| api_error_text(&e))?;
            (content, src.to_string())
        }
    };
    let hash = content_hash(&content);
    if hash != want.content_hash {
        return Err(format!(
            "the copy read has hash {hash}, and the release manifest lists {}",
            want.content_hash
        ));
    }
    store
        .put_version(&LibraryVersion {
            org: org.to_string(),
            content_hash: hash.clone(),
            content,
            source,
            publisher: lib.publisher.clone(),
            name: lib.name.clone(),
            first_party: true,
            fetched_by: by.to_string(),
            fetched_at: now,
        })
        .await
        .map_err(|e| e.to_string())?;
    match store
        .install(
            &NewInstallation {
                org: org.to_string(),
                publisher: lib.publisher.clone(),
                name: lib.name.clone(),
                content_hash: hash,
                installed_by: by.to_string(),
                installed_at: now,
                step_up_ref: None,
            },
            &Replaces::Nothing,
        )
        .await
    {
        // An admin installed one under this name meanwhile; theirs stays.
        Ok(_) | Err(LibraryStoreError::Conflict) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn api_error_text(e: &ApiError) -> String {
    match e {
        ApiError::BadRequest(m) | ApiError::Denied(m) | ApiError::Conflict(m) => m.clone(),
        ApiError::NotFound => "not found".into(),
        ApiError::Db(e) => e.to_string(),
        _ => "the request was refused".into(),
    }
}

/// After a repo is bound into `org`: if the org has no defaults record, it
/// is new, so claim it and install the first-party libraries in the
/// background. The bind has already succeeded and never waits on a forge.
pub async fn offer_defaults_on_bind(st: &AppState, org: &str) {
    let Some(store) = st.libraries.as_deref() else {
        return;
    };
    let now = st.clock.now().await.0;
    match store.claim_defaults(org, now).await {
        Ok(true) => {
            let st = st.clone();
            let org = org.to_string();
            tokio::spawn(async move {
                if let Err(e) =
                    install_org_defaults(&st, &org, DEFAULTS_INSTALLER, DefaultsFrom::default())
                        .await
                {
                    tracing::warn!(
                        org,
                        error = %api_error_text(&e),
                        "installing first-party libraries failed"
                    );
                }
            });
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(org, error = %e, "claiming first-party library defaults failed"),
    }
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
    origin: Vec<scarab_pipeline::OriginEntry>,
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
    let Some(s) = ir.steps.iter().find(|s| s.id == step) else {
        return Ok(None);
    };
    let refusals = yank_refusals_for(store, &org, &[(format!("`{step}`"), &s.origin)])
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

    fn manifest(versions: &[(&str, bool)]) -> LibraryManifest {
        LibraryManifest {
            schema: 1,
            release: "v1".into(),
            libraries: vec![ManifestLibrary {
                publisher: "thulasi-ram".into(),
                name: "clone".into(),
                versions: versions
                    .iter()
                    .map(|(h, s)| ManifestVersion {
                        content_hash: h.repeat(64),
                        source: format!(
                            "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@{}",
                            "0".repeat(40)
                        ),
                        release: "v1".into(),
                        security_fix: *s,
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn a_notice_is_marked_when_any_newer_version_is_a_security_fix() {
        let m = manifest(&[("a", false), ("b", true), ("c", false)]);
        let n = update_notice(&m, &"a".repeat(64)).unwrap();
        assert_eq!(n.content_hash, "c".repeat(64));
        assert!(n.security_fix);
        assert!(!update_notice(&m, &"b".repeat(64)).unwrap().security_fix);
        assert!(update_notice(&m, &"c".repeat(64)).is_none());
        assert!(update_notice(&m, &"d".repeat(64)).is_none());
    }

    /// The manifest `scripts/libs_manifest.py` writes for its fixture (its
    /// own test checks it writes exactly this), read as the server reads a
    /// release asset.
    #[test]
    fn the_generators_manifest_parses() {
        let bytes = include_bytes!("../../../scripts/testdata/libs_manifest/expected.json");
        let m = LibraryManifest::parse(bytes).unwrap();
        assert_eq!(m.libraries[0].qualified_name(), "thulasi-ram/clone");
        let v = m.libraries[0].latest().unwrap();
        assert!(!v.security_fix);
        assert!(LibrarySource::parse(&v.source).is_ok());
    }

    #[test]
    fn a_manifest_with_a_bad_source_is_refused() {
        let mut m = manifest(&[("a", false)]);
        m.libraries[0].versions[0].source = "github.com/x/y//z.yaml@main".into();
        assert!(LibraryManifest::parse(&serde_json::to_vec(&m).unwrap()).is_err());
        let ok = manifest(&[("a", false)]);
        assert_eq!(
            LibraryManifest::parse(&serde_json::to_vec(&ok).unwrap()).unwrap(),
            ok
        );
    }
}
