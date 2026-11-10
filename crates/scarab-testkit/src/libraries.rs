//! [`InMemoryLibraries`]: the `LibraryStore` port (ADR-0076) without
//! Postgres. The parity suite in
//! `scarab-db-postgres/tests/libraries_contract.rs` holds it to the Postgres
//! adapter's semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_pipeline::InstalledLibrary;
use scarab_policy::library::{
    LibraryInstallation, LibraryStore, LibraryStoreError, LibraryVersion, LibraryYank,
    NewInstallation, Replaces, TrustedSource,
};

#[derive(Default)]
struct State {
    /// (org, hash) → version.
    versions: BTreeMap<(String, String), LibraryVersion>,
    /// (org, publisher, name) → installation, without the version's fields.
    installations: BTreeMap<(String, String, String), NewInstallation>,
    /// (org, hash) → yank.
    yanks: BTreeMap<(String, String), LibraryYank>,
    /// (org, hash) of every version installed at least once.
    installed_once: BTreeSet<(String, String)>,
    /// (org, source) → trust.
    trusted: BTreeMap<(String, String), TrustedSource>,
}

impl State {
    fn full(&self, i: &NewInstallation) -> LibraryInstallation {
        let v = &self.versions[&(i.org.clone(), i.content_hash.clone())];
        LibraryInstallation {
            org: i.org.clone(),
            publisher: i.publisher.clone(),
            name: i.name.clone(),
            content_hash: i.content_hash.clone(),
            source: v.source.clone(),
            installed_by: i.installed_by.clone(),
            installed_at: i.installed_at,
            step_up_ref: i.step_up_ref.clone(),
        }
    }
}

#[derive(Default)]
pub struct InMemoryLibraries {
    state: Mutex<State>,
}

impl InMemoryLibraries {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LibraryStore for InMemoryLibraries {
    async fn put_version(&self, v: &LibraryVersion) -> Result<LibraryVersion, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        Ok(st
            .versions
            .entry((v.org.clone(), v.content_hash.clone()))
            .or_insert_with(|| v.clone())
            .clone())
    }

    async fn version(
        &self,
        org: &str,
        content_hash: &str,
    ) -> Result<Option<LibraryVersion>, LibraryStoreError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .versions
            .get(&(org.to_string(), content_hash.to_string()))
            .cloned())
    }

    async fn install(
        &self,
        new: &NewInstallation,
        replaces: &Replaces,
    ) -> Result<LibraryInstallation, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        if !st
            .versions
            .contains_key(&(new.org.clone(), new.content_hash.clone()))
        {
            return Err(LibraryStoreError::UnknownVersion(new.content_hash.clone()));
        }
        let key = (new.org.clone(), new.publisher.clone(), new.name.clone());
        let current = st.installations.get(&key).map(|i| i.content_hash.clone());
        let ok = match replaces {
            Replaces::Nothing => current.is_none(),
            Replaces::Version(h) => current.as_deref() == Some(h.as_str()),
        };
        if !ok {
            return Err(LibraryStoreError::Conflict);
        }
        st.installations.insert(key, new.clone());
        st.installed_once
            .insert((new.org.clone(), new.content_hash.clone()));
        Ok(st.full(new))
    }

    async fn installation(
        &self,
        org: &str,
        publisher: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .installations
            .get(&(org.to_string(), publisher.to_string(), name.to_string()))
            .map(|i| st.full(i)))
    }

    async fn installations(
        &self,
        org: &str,
    ) -> Result<Vec<LibraryInstallation>, LibraryStoreError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .installations
            .iter()
            .filter(|((o, _, _), _)| o == org)
            .map(|(_, i)| st.full(i))
            .collect())
    }

    async fn installed_for_compile(
        &self,
        org: &str,
    ) -> Result<BTreeMap<String, InstalledLibrary>, LibraryStoreError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .installations
            .iter()
            .filter(|((o, _, _), _)| o == org)
            .map(|((_, p, n), i)| {
                let v = &st.versions[&(i.org.clone(), i.content_hash.clone())];
                (
                    format!("{p}/{n}"),
                    InstalledLibrary {
                        content: v.content.clone(),
                        source: v.source.clone(),
                    },
                )
            })
            .collect())
    }

    async fn remove(
        &self,
        org: &str,
        publisher: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        let removed =
            st.installations
                .remove(&(org.to_string(), publisher.to_string(), name.to_string()));
        Ok(removed.map(|i| st.full(&i)))
    }

    async fn prune_never_installed(
        &self,
        org: &str,
        keep: usize,
        except: &str,
    ) -> Result<u64, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        let mut candidates: Vec<(i64, String)> = st
            .versions
            .iter()
            .filter(|((o, h), _)| {
                let key = (o.clone(), h.clone());
                o == org
                    && h != except
                    && !st.installed_once.contains(&key)
                    && !st.yanks.contains_key(&key)
            })
            .map(|((_, h), v)| (v.fetched_at, h.clone()))
            .collect();
        // Newest first; the same order as the Postgres adapter.
        candidates.sort_by(|a, b| b.cmp(a));
        let doomed: Vec<String> = candidates.into_iter().skip(keep).map(|(_, h)| h).collect();
        for h in &doomed {
            st.versions.remove(&(org.to_string(), h.clone()));
        }
        Ok(doomed.len() as u64)
    }

    async fn yank(&self, yank: &LibraryYank) -> Result<LibraryYank, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        let key = (yank.org.clone(), yank.content_hash.clone());
        if !st.versions.contains_key(&key) {
            return Err(LibraryStoreError::UnknownVersion(yank.content_hash.clone()));
        }
        Ok(st.yanks.entry(key).or_insert_with(|| yank.clone()).clone())
    }

    async fn unyank(
        &self,
        org: &str,
        content_hash: &str,
    ) -> Result<Option<LibraryYank>, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        Ok(st
            .yanks
            .remove(&(org.to_string(), content_hash.to_string())))
    }

    async fn yanks(&self, org: &str) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<LibraryYank> = st
            .yanks
            .values()
            .filter(|y| y.org == org)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.yanked_at
                .cmp(&a.yanked_at)
                .then_with(|| a.content_hash.cmp(&b.content_hash))
        });
        Ok(out)
    }

    async fn yanked(
        &self,
        org: &str,
        content_hashes: &[String],
    ) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .yanks
            .values()
            .filter(|y| y.org == org && content_hashes.contains(&y.content_hash))
            .cloned()
            .collect())
    }

    async fn trusted_sources(&self, org: &str) -> Result<Vec<TrustedSource>, LibraryStoreError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .trusted
            .iter()
            .filter(|((o, _), _)| o == org)
            .map(|(_, t)| t.clone())
            .collect())
    }

    async fn trust_source(&self, t: &TrustedSource) -> Result<TrustedSource, LibraryStoreError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .trusted
            .entry((t.org.clone(), t.source.clone()))
            .or_insert_with(|| t.clone())
            .clone())
    }

    async fn untrust_source(
        &self,
        org: &str,
        source: &str,
    ) -> Result<Option<TrustedSource>, LibraryStoreError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .trusted
            .remove(&(org.to_string(), source.to_string())))
    }
}
