//! Declarative (IaC) forge connections — ADR-0060 part D.
//!
//! Two things live here, and they are the whole of the decision:
//!
//! 1. **One owner per connection.** A connection is owned by the `connections:`
//!    config **or** by the database, never both. Config-owned connections are
//!    provisioned at boot, are authoritative over their row, and are read-only
//!    in the UI. A connection declared in config that already exists as a
//!    DB-owned row is a **collision that refuses the boot** — the operator must
//!    say which source owns it. This is what keeps IaC and the running system
//!    from drifting apart with no authority to break the tie.
//!
//! 2. **One credential-resolution path**: `env-override → SecretProvider`. A
//!    connection whose config supplies material directly (`credential.env` /
//!    `credential.file`) uses that; everything else resolves its
//!    `credential_ref` from `SecretProvider` under the `_forge` org. This
//!    generalizes `SCARAB_GITHUB_APP_PEM[_FILE]` from "just the PEM" to any
//!    connection + its credential: the PEM is now simply a kind-wide override in
//!    the same table.
//!
//! Config-owned connections are provisioned as **real registry rows** rather
//! than a parallel in-memory registry, so every existing consumer — the forge
//! router, the clone-step enricher, webhook resolution, `forge_repos`' foreign
//! key — works unchanged. The `owned_by_config` column is what makes ownership
//! durable, and therefore what makes the collision check exact on the *second*
//! boot as well as the first.

use std::collections::BTreeMap;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind};

use crate::config::ConnectionSpec;

/// Credential material supplied by the **deployment** rather than by
/// `SecretProvider` — the env-override half of the one resolution path
/// (ADR-0060 part D).
///
/// Two override shapes exist and they compose in one lookup:
///
/// - **per connection** — a config-declared `credential.env` / `credential.file`.
/// - **kind-wide GitHub App PEM** — `SCARAB_GITHUB_APP_PEM[_FILE]` (enh
///   245a99c), which predates the block and applies to *every* GitHub
///   connection in App mode, because at boot we do not know the installation
///   ids. Kept as the special case it is, resolved *after* an explicit
///   per-connection override so config always wins.
#[derive(Debug, Default, Clone)]
pub struct CredentialOverrides {
    by_connection: BTreeMap<String, String>,
    github_app_pem: Option<String>,
}

impl CredentialOverrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the credential for one connection id.
    pub fn with_connection(mut self, id: impl Into<String>, material: impl Into<String>) -> Self {
        self.by_connection.insert(id.into(), material.into());
        self
    }

    /// The kind-wide GitHub App PEM override, applied only in App mode — in
    /// token mode the credential is a plain token and the PEM means nothing, so
    /// silently using it there would produce a baffling auth failure.
    pub fn with_github_app_pem(mut self, pem: Option<String>, app_mode: bool) -> Self {
        self.github_app_pem = pem.filter(|_| app_mode);
        self
    }

    /// Every config-declared connection's material, keyed by id.
    pub fn from_specs(specs: &[ConnectionSpec]) -> Self {
        let mut out = Self::new();
        for spec in specs {
            if let Some(material) = &spec.credential_material {
                out.by_connection
                    .insert(spec.id.clone(), material.expose().to_string());
            }
        }
        out
    }

    /// `self` layered on top of `base`: `self`'s per-connection entries win, and
    /// `base`'s kind-wide App PEM is inherited when `self` declares none. Used to
    /// fold the config-declared overrides into the App-PEM one the forge router
    /// is constructed with, so both live in a single table.
    pub fn merged_over(&self, base: &CredentialOverrides) -> CredentialOverrides {
        let mut out = base.clone();
        out.by_connection.extend(
            self.by_connection
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        if self.github_app_pem.is_some() {
            out.github_app_pem = self.github_app_pem.clone();
        }
        out
    }

    /// The override serving `conn`, if any. `None` means "fall through to
    /// `SecretProvider`" — the second and last step of the path.
    pub fn material_for(&self, conn: &ForgeConnection) -> Option<&str> {
        // The kind-wide fallback: only GitHub App-mode connections have one.
        let kind_wide = match conn.kind {
            ForgeKind::GitHub => self.github_app_pem.as_deref(),
            ForgeKind::Forgejo => None,
        };
        self.by_connection
            .get(&conn.id)
            .map(String::as_str)
            .or(kind_wide)
    }

    /// Does the deployment supply this connection's credential itself? The
    /// Settings readout needs this to avoid reporting a working env-supplied
    /// credential as "MISSING" just because it is absent from `SecretProvider`.
    pub fn covers(&self, conn: &ForgeConnection) -> bool {
        self.material_for(conn).is_some()
    }
}

/// **The** credential-resolution path (ADR-0060 part D): deployment override
/// first, `SecretProvider` second. Every caller that needs a connection's
/// credential material goes through here, so there is exactly one order of
/// precedence in the process.
pub async fn resolve_connection_credential(
    overrides: &CredentialOverrides,
    secrets: &dyn scarab_secrets::SecretProvider,
    conn: &ForgeConnection,
) -> Result<Vec<u8>, scarab_secrets::SecretError> {
    match overrides.material_for(conn) {
        Some(material) => Ok(material.as_bytes().to_vec()),
        None => crate::connection_credential(secrets, conn).await,
    }
}

/// Why boot provisioning refused. Both variants are boot failures (ADR-0048):
/// a declarative block that cannot be applied must not degrade into "some of
/// your connections exist".
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProvisionError {
    #[error(
        "forge connection `{id}` is declared in the `connections:` config but already exists \
         as a database-owned connection (created through the API/UI, or by the GitHub \
         installation webhook). A connection has exactly ONE owner — config or the \
         database, never both (ADR-0060 part D) — because two owners means config and the \
         running system can disagree with no authority to break the tie. Either remove \
         `{id}` from the config, or delete the database-owned connection first."
    )]
    OwnershipCollision { id: String },

    #[error("forge connection registry unavailable while provisioning `{id}`: {message}")]
    Store { id: String, message: String },
}

/// What a boot's provisioning did — logged, and asserted on by tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Provisioned {
    /// Connection ids config now owns (provisioned or refreshed this boot).
    pub owned: Vec<String>,
    /// Ids that were config-owned but are no longer declared. Ownership is
    /// **released** back to the database rather than the connection being
    /// deleted: a Project (and its Environments, secrets and RBAC) hangs off a
    /// repo binding, so undeclaring a connection must not destroy governance.
    /// The operator can then delete it explicitly in the UI.
    pub released: Vec<String>,
    /// Repo bindings written this boot (`owner/name`), each of which *is* a
    /// Project (ADR-0046).
    pub bound: Vec<String>,
}

/// Apply the declarative `connections:` block to the registry (ADR-0060 part D).
///
/// Idempotent: re-running with the same specs re-upserts the same rows and
/// re-claims ownership, so a restart is a no-op and a changed `base_url` in
/// config wins over whatever the row said — config is authoritative for the
/// connections it declares.
pub async fn provision(
    store: &dyn ForgeConnectionStore,
    specs: &[ConnectionSpec],
) -> Result<Provisioned, ProvisionError> {
    let store_err = |id: &str| {
        let id = id.to_string();
        move |e: scarab_forge::RegistryError| ProvisionError::Store {
            id,
            message: e.to_string(),
        }
    };

    let already_config_owned: std::collections::BTreeSet<String> = store
        .config_owned_connection_ids()
        .await
        .map_err(store_err(""))?
        .into_iter()
        .collect();

    let mut out = Provisioned::default();
    for spec in specs {
        // The collision check: a row we did not provision, with an id config
        // now claims. Checked BEFORE any write, so a refused boot leaves the
        // registry exactly as it was.
        let existing = store
            .get_connection(&spec.id)
            .await
            .map_err(store_err(&spec.id))?;
        if existing.is_some() && !already_config_owned.contains(&spec.id) {
            return Err(ProvisionError::OwnershipCollision {
                id: spec.id.clone(),
            });
        }

        store
            .put_connection(&ForgeConnection {
                id: spec.id.clone(),
                kind: spec.kind,
                base_url: spec.base_url.clone(),
                credential_ref: spec.credential_ref.clone(),
            })
            .await
            .map_err(store_err(&spec.id))?;
        // Claim ownership only after the row exists — the marker is an UPDATE.
        store
            .set_connection_owned_by_config(&spec.id, true)
            .await
            .map_err(store_err(&spec.id))?;
        out.owned.push(spec.id.clone());

        for repo in &spec.repos {
            // Org = owner, Project = name: the same 1:1 mapping the
            // installation webhook and re-sync use, so a config-bound Project is
            // indistinguishable from a webhook-registered one.
            store
                .bind_repo(&spec.id, repo, &repo.owner, &repo.name)
                .await
                .map_err(store_err(&spec.id))?;
            out.bound.push(format!("{}/{}", repo.owner, repo.name));
        }
    }

    // Undeclared: hand ownership back so the connection becomes editable and
    // deletable again. Nothing is deleted, and nothing ends up with two owners —
    // config has stopped claiming it, so the database is the sole owner.
    let declared: std::collections::BTreeSet<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    for id in &already_config_owned {
        if !declared.contains(id.as_str()) {
            store
                .set_connection_owned_by_config(id, false)
                .await
                .map_err(store_err(id))?;
            out.released.push(id.clone());
        }
    }
    Ok(out)
}

/// Guard for any endpoint that would **mutate** a connection (ADR-0060 part D):
/// config-owned connections are read-only through the API, because the config is
/// authoritative and a write here would be silently reverted on the next boot.
///
/// Returns `true` when `id` is config-owned and the caller must refuse.
pub async fn is_config_owned(
    store: &dyn ForgeConnectionStore,
    id: &str,
) -> Result<bool, scarab_forge::RegistryError> {
    Ok(store
        .config_owned_connection_ids()
        .await?
        .iter()
        .any(|owned| owned == id))
}
