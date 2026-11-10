//! Stand-ins the integration tests share: a forge that serves files from a
//! map, and the Argo CD plugin's manifest at a fixed source, so a test
//! installs Scarab's own plugin and libraries through the same fetch and
//! install calls an org admin makes, with no network.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::library_fetch::{AnonymousFetcher, FetchError, LibrarySource};

/// An [`AnonymousFetcher`] that answers from a map of URL to file and
/// records every URL asked. An unknown URL fails as a refused connection.
#[derive(Default)]
pub struct StaticFetcher {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    asked: Mutex<Vec<String>>,
}

impl StaticFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves `body` at `url`.
    pub fn serve_url(&self, url: &str, body: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(url.to_string(), body.as_bytes().to_vec());
    }

    /// Serves `body` at the URL a github.com `source`
    /// (`github.com/<owner>/<repo>//<path>@<commit>`) is read from.
    pub fn serve(&self, source: &str, body: &str) {
        let src = LibrarySource::parse_manifest(source).expect("a github.com source");
        assert_eq!(src.host, "github.com", "serve() reads github.com sources");
        self.serve_url(&raw_github_url(&src), body);
    }

    /// Every URL asked, in order.
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// Where an anonymous fetch reads a github.com source.
pub fn raw_github_url(src: &LibrarySource) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        src.owner, src.repo, src.commit, src.path
    )
}

#[async_trait]
impl AnonymousFetcher for StaticFetcher {
    async fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.asked.lock().unwrap().push(url.to_string());
        self.files
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Failed(format!("fetching {url}: connection refused")))
    }
}

/// The Argo CD plugin's manifest as this repository holds it.
pub const ARGOCD_MANIFEST: &str = include_str!("../../scarab-plugin-argocd/manifest.json");

/// A source for [`ARGOCD_MANIFEST`]: the file's path in this repository at
/// a made-up commit.
pub const ARGOCD_MANIFEST_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//crates/scarab-plugin-argocd/manifest.json@\
     0000000000000000000000000000000000000a90";

/// Scarab's clone library as this repository holds it.
pub const CLONE_LIBRARY: &str = include_str!("../../../libs/clone.yaml");

/// A source for [`CLONE_LIBRARY`]: its path in this repository at a
/// made-up commit.
pub const CLONE_LIBRARY_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@00000000000000000000000000000000000c1013";

/// The SHA-256 of a manifest as an install records it and
/// `POST …/plugin-fetch` returns it.
pub fn manifest_sha256(manifest: &str) -> String {
    let m: scarab_plugin::PluginManifest =
        serde_json::from_str(manifest).expect("a plugin manifest");
    crate::plugins::manifest_sha256(&m)
}

/// A [`StaticFetcher`] that serves [`ARGOCD_MANIFEST`] at
/// [`ARGOCD_MANIFEST_SOURCE`].
pub fn argocd_fetcher() -> StaticFetcher {
    let fetcher = StaticFetcher::new();
    fetcher.serve(ARGOCD_MANIFEST_SOURCE, ARGOCD_MANIFEST);
    fetcher
}

/// The body of `POST /v1/orgs/{org}/plugins` that installs the Argo CD
/// plugin as `name` from `image`: the manifest at [`ARGOCD_MANIFEST_SOURCE`]
/// with the hash an admin reviewed.
pub fn argocd_install_body(name: &str, image: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "image": image,
        "source": ARGOCD_MANIFEST_SOURCE,
        "manifest_sha256": manifest_sha256(ARGOCD_MANIFEST),
    })
}
