//! [`InMemoryLibraries`]: the `LibraryStore` port (ADR-0076) without
//! Postgres. The parity suite in
//! `scarab-db-postgres/tests/libraries_contract.rs` holds it to the Postgres
//! adapter's semantics.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_pipeline::InstalledLibrary;
use scarab_policy::library::{
    DefaultsState, LibraryInstallation, LibraryStore, LibraryStoreError, LibraryVersion,
    LibraryYank, NewInstallation, OrgLibraryDefaults, Replaces,
};

#[derive(Default)]
struct State {
    /// (org, hash) → version.
    versions: BTreeMap<(String, String), LibraryVersion>,
    /// (org, publisher, name) → installation, without the version's fields.
    installations: BTreeMap<(String, String, String), NewInstallation>,
    /// (org, hash) → yank.
    yanks: BTreeMap<(String, String), LibraryYank>,
    defaults: BTreeMap<String, OrgLibraryDefaults>,
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
            first_party: v.first_party,
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

    /// Marks `org` as having had repos bound before libraries existed, as
    /// migration 0071 does for every org in `forge_repos`.
    pub fn with_offered_org(self, org: &str) -> Self {
        self.state.lock().unwrap().defaults.insert(
            org.to_string(),
            OrgLibraryDefaults {
                org: org.to_string(),
                state: DefaultsState::Offered,
                missing: Vec::new(),
                mirror: None,
                updated_at: 0,
            },
        );
        self
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

    async fn claim_defaults(&self, org: &str, at: i64) -> Result<bool, LibraryStoreError> {
        let mut st = self.state.lock().unwrap();
        if st.defaults.contains_key(org) {
            return Ok(false);
        }
        st.defaults.insert(
            org.to_string(),
            OrgLibraryDefaults {
                org: org.to_string(),
                state: DefaultsState::Pending,
                missing: Vec::new(),
                mirror: None,
                updated_at: at,
            },
        );
        Ok(true)
    }

    async fn defaults(&self, org: &str) -> Result<Option<OrgLibraryDefaults>, LibraryStoreError> {
        Ok(self.state.lock().unwrap().defaults.get(org).cloned())
    }

    async fn set_defaults(&self, d: &OrgLibraryDefaults) -> Result<(), LibraryStoreError> {
        self.state
            .lock()
            .unwrap()
            .defaults
            .insert(d.org.clone(), d.clone());
        Ok(())
    }
}
