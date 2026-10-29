//! The `LibraryStore` port (ADR-0076 §4, §5, §6): the libraries an org has
//! installed, the versions it has fetched, the versions it has yanked, and
//! where its first-party defaults stand.
//!
//! Everything is per org. There is no orgs table: an org is the `org` of a
//! repo binding.
//!
//! - A **LibraryVersion** is one library file as fetched, keyed by
//!   `(org, content_hash)`; the hash is SHA-256 of the content, lowercase hex,
//!   the same hash the compiler records in a Step's `origin`.
//! - A **LibraryInstallation** names the version the org uses for one
//!   `<publisher>/<name>`. Installing and upgrading take the hash the caller
//!   expects to replace, so an upgrade confirmed against one diff cannot
//!   replace a version someone else installed in between.
//! - A **yank** refuses a version in one org (§6). [`yank_refusals`] is the
//!   check, a PolicyInvariant: no Policy is consulted and none can let a
//!   yanked version run.
//!
//! The caller supplies times, as every store port here does.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use scarab_pipeline::{InstalledLibrary, OriginEntry, OriginKind};

/// One library file as fetched into an org.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryVersion {
    pub org: String,
    /// SHA-256 of `content`, lowercase hex.
    pub content_hash: String,
    pub content: String,
    /// The git source it was fetched from, pinned to a commit:
    /// `github.com/acme/scarab-libs//deploy-helm.yaml@<commit>`.
    pub source: String,
    /// The `<publisher>/<name>` it was fetched for.
    pub publisher: String,
    pub name: String,
    /// The hash is listed in a Scarab release manifest the server trusts
    /// (ADR-0076 §5).
    pub first_party: bool,
    pub fetched_by: String,
    pub fetched_at: i64,
}

/// The version an org uses for one `<publisher>/<name>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryInstallation {
    pub org: String,
    pub publisher: String,
    pub name: String,
    pub content_hash: String,
    /// The installed version's source.
    pub source: String,
    /// The installed version is first-party.
    pub first_party: bool,
    pub installed_by: String,
    pub installed_at: i64,
    /// The StepUpConfirmation that approved the install, when one was needed.
    pub step_up_ref: Option<String>,
}

impl LibraryInstallation {
    /// `<publisher>/<name>`, as `invoke: lib:` names it.
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.publisher, self.name)
    }
}

/// An install or upgrade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewInstallation {
    pub org: String,
    pub publisher: String,
    pub name: String,
    /// A version already stored in the org.
    pub content_hash: String,
    pub installed_by: String,
    pub installed_at: i64,
    pub step_up_ref: Option<String>,
}

/// What the caller expects to replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replaces {
    /// Nothing: a first install.
    Nothing,
    /// The installation at this hash: an upgrade (or a downgrade).
    Version(String),
}

/// A yanked version (ADR-0076 §6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryYank {
    pub org: String,
    pub content_hash: String,
    /// What the admin wrote; every refusal quotes it.
    pub reason: String,
    pub yanked_by: String,
    pub yanked_at: i64,
}

/// Where an org's first-party defaults stand (ADR-0076 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultsState {
    /// The org had repos bound before libraries existed: it is offered the
    /// first-party libraries, and none is installed without an admin.
    Offered,
    /// A first bind claimed the org; installing has not finished.
    Pending,
    /// Every default is installed.
    Installed,
    /// Some defaults could not be installed; `missing` says which and why.
    Missing,
}

impl DefaultsState {
    pub fn as_str(self) -> &'static str {
        match self {
            DefaultsState::Offered => "offered",
            DefaultsState::Pending => "pending",
            DefaultsState::Installed => "installed",
            DefaultsState::Missing => "missing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "offered" => DefaultsState::Offered,
            "pending" => DefaultsState::Pending,
            "installed" => DefaultsState::Installed,
            "missing" => DefaultsState::Missing,
            _ => return None,
        })
    }
}

/// A default library that could not be installed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingDefault {
    pub publisher: String,
    pub name: String,
    /// Why, e.g. "fetching github.com/…: connection refused".
    pub error: String,
}

/// An org's first-party defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgLibraryDefaults {
    pub org: String,
    pub state: DefaultsState,
    pub missing: Vec<MissingDefault>,
    /// The base URL an admin pointed the defaults at instead of the release
    /// source, if any.
    pub mirror: Option<String>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LibraryStoreError {
    /// No such version in the org (for an install or a yank).
    #[error("no version {0} has been fetched in this org")]
    UnknownVersion(String),
    /// The installation is not the one the caller expected to replace.
    #[error("the installation changed since it was read")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("library store: {0}")]
    Store(String),
}

#[async_trait]
pub trait LibraryStore: Send + Sync {
    /// Stores a version, or returns the one already stored at
    /// `(org, content_hash)` unchanged: the content is the same, and the
    /// first fetch's record is kept.
    async fn put_version(&self, v: &LibraryVersion) -> Result<LibraryVersion, LibraryStoreError>;

    async fn version(
        &self,
        org: &str,
        content_hash: &str,
    ) -> Result<Option<LibraryVersion>, LibraryStoreError>;

    /// Installs a stored version under `<publisher>/<name>`, if the current
    /// installation is what `replaces` says.
    async fn install(
        &self,
        new: &NewInstallation,
        replaces: &Replaces,
    ) -> Result<LibraryInstallation, LibraryStoreError>;

    async fn installation(
        &self,
        org: &str,
        publisher: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError>;

    /// The org's installations by publisher, then name (bytewise).
    async fn installations(&self, org: &str)
        -> Result<Vec<LibraryInstallation>, LibraryStoreError>;

    /// What the compiler resolves `lib:` against in `org`: each
    /// `<publisher>/<name>` with its installed content and source.
    async fn installed_for_compile(
        &self,
        org: &str,
    ) -> Result<BTreeMap<String, InstalledLibrary>, LibraryStoreError>;

    /// Removes an installation; the removed one, or `None`. Its version
    /// stays stored, so Runs that used it still show what they ran.
    async fn remove(
        &self,
        org: &str,
        publisher: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError>;

    /// Yanks a stored version. Yanking a yanked version keeps the first
    /// yank and returns it.
    async fn yank(&self, yank: &LibraryYank) -> Result<LibraryYank, LibraryStoreError>;

    /// Lifts a yank; the lifted one, or `None`.
    async fn unyank(
        &self,
        org: &str,
        content_hash: &str,
    ) -> Result<Option<LibraryYank>, LibraryStoreError>;

    /// The org's yanks, newest first.
    async fn yanks(&self, org: &str) -> Result<Vec<LibraryYank>, LibraryStoreError>;

    /// The yanks in `org` among `content_hashes`.
    async fn yanked(
        &self,
        org: &str,
        content_hashes: &[String],
    ) -> Result<Vec<LibraryYank>, LibraryStoreError>;

    /// Records the org's defaults as [`DefaultsState::Pending`] if the org
    /// has no record yet. `true` when this call made the record: the caller
    /// installs the defaults. Exactly one caller gets `true` per org.
    async fn claim_defaults(&self, org: &str, at: i64) -> Result<bool, LibraryStoreError>;

    async fn defaults(&self, org: &str) -> Result<Option<OrgLibraryDefaults>, LibraryStoreError>;

    /// Replaces the org's defaults record.
    async fn set_defaults(&self, d: &OrgLibraryDefaults) -> Result<(), LibraryStoreError>;
}

/// The `lib:` content hashes in an `origin` chain.
pub fn library_hashes(origin: &[OriginEntry]) -> impl Iterator<Item = &str> {
    origin
        .iter()
        .filter(|o| o.kind == OriginKind::Library)
        .map(|o| o.content_hash.as_str())
}

/// The refusal for each yanked version an `origin` chain uses (ADR-0076 §6),
/// naming the library, the version and the admin's reason. Empty when none
/// is yanked. `what` names the refused thing ("step `deploy/helm`").
pub fn yank_refusals(what: &str, origin: &[OriginEntry], yanks: &[LibraryYank]) -> Vec<String> {
    let mut out = Vec::new();
    for link in origin.iter().filter(|o| o.kind == OriginKind::Library) {
        if let Some(y) = yanks.iter().find(|y| y.content_hash == link.content_hash) {
            let msg = yank_message(what, &link.name, &y.content_hash, &y.reason);
            if !out.contains(&msg) {
                out.push(msg);
            }
        }
    }
    out
}

/// One yank refusal.
pub fn yank_message(what: &str, library: &str, content_hash: &str, reason: &str) -> String {
    let short = content_hash.get(..12).unwrap_or(content_hash);
    format!("{what} uses `lib:{library}` version {short}, which an org admin yanked: {reason}")
}

/// SHA-256 of `content`, lowercase hex: a LibraryVersion's key, and what the
/// compiler records in `origin`.
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(content.as_bytes()) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Is `s` a library publisher or name: `[A-Za-z0-9._-]`, not starting with
/// `.` or `-`, at most 100 bytes? The compiler's rule for each segment of
/// `lib:<publisher>/<name>`.
pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: OriginKind, name: &str, hash: &str) -> OriginEntry {
        OriginEntry {
            kind,
            name: name.into(),
            source: None,
            content_hash: hash.into(),
        }
    }

    fn yank(hash: &str, reason: &str) -> LibraryYank {
        LibraryYank {
            org: "acme".into(),
            content_hash: hash.into(),
            reason: reason.into(),
            yanked_by: "user:amy".into(),
            yanked_at: 1,
        }
    }

    #[test]
    fn a_yanked_library_anywhere_in_the_chain_is_refused_with_the_reason() {
        let origin = [
            link(OriginKind::Library, "acme/outer", &"a".repeat(64)),
            link(OriginKind::Library, "acme/inner", &"b".repeat(64)),
        ];
        let refusals = yank_refusals(
            "step `ship/helm`",
            &origin,
            &[yank(&"b".repeat(64), "pins a vulnerable image")],
        );
        assert_eq!(
            refusals,
            vec![
                "step `ship/helm` uses `lib:acme/inner` version bbbbbbbbbbbb, which an org admin \
                 yanked: pins a vulnerable image"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_file_link_with_a_yanked_hash_is_not_a_library() {
        // A yank names an installed version; a `file:` link with the same
        // bytes is the repository's own copy (ADR-0076 §3).
        let origin = [link(
            OriginKind::File,
            ".scarab/lib/x.yaml",
            &"a".repeat(64),
        )];
        assert!(yank_refusals("step `x`", &origin, &[yank(&"a".repeat(64), "r")]).is_empty());
    }

    #[test]
    fn segments_follow_the_compiler() {
        assert!(valid_segment("thulasi-ram"));
        assert!(valid_segment("clone"));
        for bad in ["", ".x", "-x", "a/b", "a b", &"x".repeat(101)] {
            assert!(!valid_segment(bad), "{bad}");
        }
    }
}
