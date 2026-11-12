//! The `LibraryStore` port (ADR-0076 §4, §6): the libraries an org has
//! installed, the versions it has fetched, the versions it has yanked, and
//! the library sources it trusts with forge credentials.
//!
//! Every library is installed the same way, whoever wrote it: Scarab's own
//! libraries carry no mark and no trust of their own.
//!
//! Everything is per org. There is no orgs table: an org is the `org` of a
//! repo binding.
//!
//! - A **LibraryVersion** is one library file as fetched, keyed by
//!   `(org, content_hash)`; the hash is SHA-256 of the content, lowercase hex,
//!   the same hash the compiler records in a Step's `origin`.
//! - A **LibraryInstallation** names the version the org uses under one
//!   library name ([`library_name_error`]). Installing and upgrading take the hash the caller
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

pub use scarab_pipeline::{library_name_error, LIBRARY_NAME_RULE};

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
    /// The library name it was fetched for, as `invoke: lib:` names it.
    pub name: String,
    pub fetched_by: String,
    pub fetched_at: i64,
}

/// The version an org uses under one library name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryInstallation {
    /// Store-minted when the library is first installed: 10 random
    /// lowercase hex, unique across orgs. An upgrade keeps it; a removal and
    /// a new install get a new one. What the settings page's URL carries.
    #[serde(default)]
    pub id: String,
    pub org: String,
    /// The library name, as `invoke: lib:` names it.
    pub name: String,
    pub content_hash: String,
    /// The installed version's source.
    pub source: String,
    pub installed_by: String,
    pub installed_at: i64,
    /// The StepUpConfirmation that approved the install, when one was needed.
    pub step_up_ref: Option<String>,
}

/// An install or upgrade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewInstallation {
    pub org: String,
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

/// What an org's trust in a library source grants. Each is trusted on its
/// own: trusting a source with forge credentials does not trust it with the
/// build sandbox, and the reverse (ADR-0018, amendment of 2026-11-12). The
/// variants are in the order of their names, so a store lists a source's
/// grants in the same order whether it sorts the enum or the name.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TrustGrant {
    /// A Step from the source may run in a `security.sandbox`.
    BuildSandbox,
    /// A Step from the source may receive a long-lived forge credential
    /// (ADR-0076 §9), and, with [`RegistryCredential`](Self::RegistryCredential)
    /// too, the forge-derived registry credential.
    #[default]
    ForgeCredentials,
    /// A Step from the source with `registry_auth` may receive the Run's
    /// registry credential: the `REGISTRY_AUTH` secret, or the forge-derived
    /// one when the source is trusted with forge credentials too.
    RegistryCredential,
}

impl TrustGrant {
    /// The name the store and the API use.
    pub fn as_str(self) -> &'static str {
        match self {
            TrustGrant::ForgeCredentials => "forge_credentials",
            TrustGrant::BuildSandbox => "build_sandbox",
            TrustGrant::RegistryCredential => "registry_credential",
        }
    }

    /// The grant named `s` ([`as_str`](Self::as_str)), if any.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "forge_credentials" => Some(TrustGrant::ForgeCredentials),
            "build_sandbox" => Some(TrustGrant::BuildSandbox),
            "registry_credential" => Some(TrustGrant::RegistryCredential),
            _ => None,
        }
    }

    /// How a message names what the grant trusts the source with.
    pub fn describe(self) -> &'static str {
        match self {
            TrustGrant::ForgeCredentials => "forge credentials",
            TrustGrant::BuildSandbox => "the build sandbox",
            TrustGrant::RegistryCredential => "the registry credential",
        }
    }
}

/// A library source an org trusts with one [`TrustGrant`] (ADR-0076 §9,
/// ADR-0018 amendment of 2026-11-12). Keyed by the org, the source without
/// its commit ([`trust_key`]) and the grant, so trust carries across
/// upgrades from the same source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedSource {
    pub org: String,
    /// `<host>/<owner>/<repo>//<path>`, no `@<commit>`.
    pub source: String,
    /// What the trust grants. A record stored before grants existed is
    /// trust with forge credentials.
    #[serde(default)]
    pub grant: TrustGrant,
    pub trusted_by: String,
    pub trusted_at: i64,
    /// The StepUpConfirmation that approved it, when one was needed.
    pub step_up_ref: Option<String>,
}

/// What an org's trust in a library source is keyed by: the source without
/// its `@<commit>` (ADR-0076 §9), so an upgrade from the same source keeps
/// it.
pub fn trust_key(source: &str) -> &str {
    match source.rsplit_once('@') {
        Some((path, commit)) if !commit.contains('/') => path,
        _ => source,
    }
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

    /// Installs a stored version under `new.name`, if the current
    /// installation is what `replaces` says.
    async fn install(
        &self,
        new: &NewInstallation,
        replaces: &Replaces,
    ) -> Result<LibraryInstallation, LibraryStoreError>;

    async fn installation(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError>;

    /// The installation in `org` with this id, or `None`: an id of another
    /// org's installation is not found here.
    async fn installation_by_id(
        &self,
        org: &str,
        id: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        Ok(self
            .installations(org)
            .await?
            .into_iter()
            .find(|i| i.id == id))
    }

    /// The org's installations by name (bytewise).
    async fn installations(&self, org: &str)
        -> Result<Vec<LibraryInstallation>, LibraryStoreError>;

    /// What the compiler resolves `lib:` against in `org`: each library
    /// name with its installed content and source.
    async fn installed_for_compile(
        &self,
        org: &str,
    ) -> Result<BTreeMap<String, InstalledLibrary>, LibraryStoreError>;

    /// Removes the installation with this id; the removed one, or `None`.
    /// By id, so a remove never takes an installation made under the same
    /// name after the caller looked. Its version stays stored, so Runs that
    /// used it still show what they ran.
    async fn remove(
        &self,
        org: &str,
        id: &str,
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

    /// Deletes the org's fetched versions that were never installed, except
    /// the `keep` most recently fetched and `except` (the one a fetch just
    /// stored). A version that was installed once, or is yanked, stays: a
    /// Run may have used it. Returns how many were deleted.
    async fn prune_never_installed(
        &self,
        org: &str,
        keep: usize,
        except: &str,
    ) -> Result<u64, LibraryStoreError>;

    /// The sources the org trusts, each with what it grants (ADR-0076 §9,
    /// ADR-0018 amendment of 2026-11-12), by source then grant. A store that
    /// keeps none trusts none, which only ever refuses more.
    async fn trusted_sources(&self, _org: &str) -> Result<Vec<TrustedSource>, LibraryStoreError> {
        Ok(Vec::new())
    }

    /// Trusts a source ([`trust_key`] form) with `t.grant`. Trusting a
    /// source already trusted with that grant keeps the first record and
    /// returns it.
    async fn trust_source(&self, _t: &TrustedSource) -> Result<TrustedSource, LibraryStoreError> {
        Err(LibraryStoreError::Store(
            "this library store does not keep trusted sources".into(),
        ))
    }

    /// Stops trusting a source with `grant`; the record removed, or `None`.
    /// Its other grants stay.
    async fn untrust_source(
        &self,
        _org: &str,
        _source: &str,
        _grant: TrustGrant,
    ) -> Result<Option<TrustedSource>, LibraryStoreError> {
        Ok(None)
    }
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
    fn trust_is_keyed_by_the_source_without_its_commit() {
        assert_eq!(
            trust_key("github.com/acme/libs//ci/git.yaml@0123abcd"),
            "github.com/acme/libs//ci/git.yaml"
        );
        assert_eq!(
            trust_key("github.com/acme/libs//git.yaml"),
            "github.com/acme/libs//git.yaml"
        );
    }
}
