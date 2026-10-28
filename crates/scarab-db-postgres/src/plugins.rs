//! [`PluginStore`] on Postgres (ADR-0072, migrations 0055 and 0056): the
//! PluginInstallation registry with its kill switch, its PluginConnections
//! and the broker journal.
//!
//! **PluginConnections** (ADR-0072 §5, migration 0056): a bind takes the
//! installation row `FOR UPDATE` before the connection row, and uninstall
//! takes the installation row before deleting the connections (returning
//! their credential keys, which live in the SecretProvider). The broker's
//! journal is append-only and keyed by nothing it locks.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::Row;

use scarab_plugin::{
    BrokerJournalEntry, BrokerJournalPhase, InstallationState, PluginConnection,
    PluginConnectionAuthScheme, PluginInstallation, PluginManifest, PluginStore, PluginStoreError,
    PluginUninstall, RuntimeStatus, PAGE_DEFAULT_LIMIT, PAGE_MAX_LIMIT,
};

use crate::PostgresDb;

/// SQLSTATE `lock_not_available`: a lock wait passed `lock_timeout`.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// A sqlx error as the plugin store reports it. No connection free in time
/// and a lock wait past `lock_timeout` are [`PluginStoreError::Busy`] —
/// transient, nothing recorded — so the broker can answer 503 + Retry-After
/// instead of a 500 that reads like a bug.
fn plugin_store_err(e: sqlx::Error) -> PluginStoreError {
    match &e {
        sqlx::Error::PoolTimedOut => {
            PluginStoreError::Busy("no database connection was free in time".into())
        }
        sqlx::Error::Database(d) if d.code().as_deref() == Some(LOCK_NOT_AVAILABLE) => {
            PluginStoreError::Busy("a lock wait passed its timeout".into())
        }
        _ => PluginStoreError::Store(e.to_string()),
    }
}

/// A page size as the store applies it: 0 = the default, never more than
/// the maximum.
fn page_limit(limit: usize) -> i64 {
    let limit = if limit == 0 {
        PAGE_DEFAULT_LIMIT
    } else {
        limit.min(PAGE_MAX_LIMIT)
    };
    limit as i64
}

fn saturating_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

const INSTALLATION_COLUMNS: &str = "id, org, name, plugin, image, manifest, manifest_sha256, \
     config, state, runtime_status, runtime_message, probe_passed_at, approved_by, approved_at, \
     killed_by, killed_at";

/// One `plugin_installations` row back into the domain. A row whose manifest,
/// config or state this binary cannot read is reported, never defaulted: a
/// guessed manifest would be an approval nobody gave.
fn installation_from_row(
    r: &sqlx::postgres::PgRow,
) -> Result<PluginInstallation, PluginStoreError> {
    let id: String = r.get("id");
    let unreadable = |what: &str, e: String| {
        PluginStoreError::Store(format!("plugin installation {id}: unreadable {what}: {e}"))
    };
    let manifest: PluginManifest = serde_json::from_value(r.get::<Value, _>("manifest"))
        .map_err(|e| unreadable("manifest", e.to_string()))?;
    let config: BTreeMap<String, String> = serde_json::from_value(r.get::<Value, _>("config"))
        .map_err(|e| unreadable("config", e.to_string()))?;
    let state: String = r.get("state");
    let state = InstallationState::parse(&state)
        .ok_or_else(|| unreadable("state", format!("{state:?}")))?;
    let runtime_status: String = r.get("runtime_status");
    let runtime_status = RuntimeStatus::parse(&runtime_status)
        .ok_or_else(|| unreadable("runtime status", format!("{runtime_status:?}")))?;
    Ok(PluginInstallation {
        org: r.get("org"),
        name: r.get("name"),
        plugin: r.get("plugin"),
        image: r.get("image"),
        manifest,
        manifest_sha256: r.get("manifest_sha256"),
        config,
        state,
        runtime_status,
        runtime_message: r.get("runtime_message"),
        probe_passed_at: r.get("probe_passed_at"),
        approved_by: r.get("approved_by"),
        approved_at: r.get("approved_at"),
        killed_by: r.get("killed_by"),
        killed_at: r.get("killed_at"),
        id,
    })
}

const CONNECTION_COLUMNS: &str = "installation_id, slot, base_url, auth_scheme, \
     credential_ref, ca_pem, updated_by, updated_at";

/// One `plugin_connections` row back into the domain. An auth scheme this
/// binary does not know is reported, never defaulted: injecting a credential
/// the wrong way is not a safe guess.
fn connection_from_row(r: &sqlx::postgres::PgRow) -> Result<PluginConnection, PluginStoreError> {
    let scheme: String = r.get("auth_scheme");
    let auth_scheme = PluginConnectionAuthScheme::parse(&scheme).ok_or_else(|| {
        PluginStoreError::Store(format!(
            "plugin connection: unreadable auth scheme {scheme:?}"
        ))
    })?;
    Ok(PluginConnection {
        installation_id: r.get("installation_id"),
        slot: r.get("slot"),
        base_url: r.get("base_url"),
        auth_scheme,
        credential_ref: r.get("credential_ref"),
        ca_pem: r.get("ca_pem"),
        updated_by: r.get("updated_by"),
        updated_at: r.get("updated_at"),
    })
}

#[async_trait]
impl PluginStore for PostgresDb {
    async fn install(
        &self,
        installation: &PluginInstallation,
    ) -> Result<PluginInstallation, PluginStoreError> {
        installation
            .validate()
            .map_err(|e| PluginStoreError::Invalid(e.to_string()))?;
        let manifest = serde_json::to_value(&installation.manifest)
            .map_err(|e| PluginStoreError::Store(e.to_string()))?;
        let config = serde_json::to_value(&installation.config)
            .map_err(|e| PluginStoreError::Store(e.to_string()))?;
        let mut tx = self.pool().begin().await.map_err(plugin_store_err)?;
        // Claim the name in the ledger first: its PRIMARY KEY is what makes a
        // name single-use per Org for good, live or retired. `DO NOTHING`
        // covers both the name and the id constraint; which one hit is read
        // back below.
        let claimed = sqlx::query(
            "INSERT INTO plugin_installation_names (org, name, installation_id, installed_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(&installation.org)
        .bind(&installation.name)
        .bind(&installation.id)
        .bind(installation.approved_at)
        .execute(&mut *tx)
        .await
        .map_err(plugin_store_err)?
        .rows_affected();
        if claimed == 0 {
            let retired: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT retired_at FROM plugin_installation_names WHERE org = $1 AND name = $2",
            )
            .bind(&installation.org)
            .bind(&installation.name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(plugin_store_err)?;
            return Err(match retired {
                Some(Some(_)) => PluginStoreError::NameRetired(installation.name.clone()),
                Some(None) => PluginStoreError::NameTaken(installation.name.clone()),
                None => PluginStoreError::IdTaken,
            });
        }
        sqlx::query(
            "INSERT INTO plugin_installations
                 (org, name, id, plugin, image, manifest, manifest_sha256, config, state,
                  runtime_status, runtime_message, probe_passed_at, approved_by, approved_at,
                  killed_by, killed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(&installation.org)
        .bind(&installation.name)
        .bind(&installation.id)
        .bind(&installation.plugin)
        .bind(&installation.image)
        .bind(manifest)
        .bind(&installation.manifest_sha256)
        .bind(config)
        .bind(installation.state.as_str())
        .bind(installation.runtime_status.as_str())
        .bind(&installation.runtime_message)
        .bind(installation.probe_passed_at)
        .bind(&installation.approved_by)
        .bind(installation.approved_at)
        .bind(&installation.killed_by)
        .bind(installation.killed_at)
        .execute(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        tx.commit().await.map_err(plugin_store_err)?;
        Ok(installation.clone())
    }

    async fn installation(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<PluginInstallation>, PluginStoreError> {
        let sql = format!(
            "SELECT {INSTALLATION_COLUMNS} FROM plugin_installations WHERE org = $1 AND name = $2"
        );
        sqlx::query(&sql)
            .bind(org)
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(plugin_store_err)?
            .as_ref()
            .map(installation_from_row)
            .transpose()
    }

    async fn installation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<PluginInstallation>, PluginStoreError> {
        let sql = format!("SELECT {INSTALLATION_COLUMNS} FROM plugin_installations WHERE id = $1");
        sqlx::query(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(plugin_store_err)?
            .as_ref()
            .map(installation_from_row)
            .transpose()
    }

    async fn installations(&self, org: &str) -> Result<Vec<PluginInstallation>, PluginStoreError> {
        let sql = format!(
            "SELECT {INSTALLATION_COLUMNS} FROM plugin_installations WHERE org = $1 ORDER BY name"
        );
        sqlx::query(&sql)
            .bind(org)
            .fetch_all(self.pool())
            .await
            .map_err(plugin_store_err)?
            .iter()
            .map(installation_from_row)
            .collect()
    }

    async fn all_installations(&self) -> Result<Vec<PluginInstallation>, PluginStoreError> {
        let sql = format!("SELECT {INSTALLATION_COLUMNS} FROM plugin_installations ORDER BY id");
        sqlx::query(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(plugin_store_err)?
            .iter()
            .map(installation_from_row)
            .collect()
    }

    async fn set_runtime_status(
        &self,
        id: &str,
        decided_in: InstallationState,
        status: RuntimeStatus,
        message: &str,
        probe_passed_at: Option<i64>,
    ) -> Result<bool, PluginStoreError> {
        // Guarded by the state the runtime decided in: a kill or enable that
        // committed meanwhile wins, and the runtime decides again next pass.
        let written = sqlx::query(
            "UPDATE plugin_installations
                SET runtime_status = $3, runtime_message = $4, probe_passed_at = $5
              WHERE id = $1 AND state = $2",
        )
        .bind(id)
        .bind(decided_in.as_str())
        .bind(status.as_str())
        .bind(message)
        .bind(probe_passed_at)
        .execute(self.pool())
        .await
        .map_err(plugin_store_err)?
        .rows_affected();
        Ok(written == 1)
    }

    async fn set_installation_state(
        &self,
        org: &str,
        name: &str,
        state: InstallationState,
        by: &str,
        at: i64,
    ) -> Result<Option<PluginInstallation>, PluginStoreError> {
        // A repeat kill keeps the first `killed_by`/`killed_at`: who stopped
        // it is the fact worth keeping. Enabling clears both.
        //
        // Enabling a KILLED installation also forgets its last probe: the
        // runtime must prove isolation again before the workload starts
        // (ADR-0072 §9, "at every start"). An enable of one already enabled
        // changes nothing. (Every SET expression reads the row as it was.)
        let sql = format!(
            "UPDATE plugin_installations SET
                 state = $3,
                 killed_by = CASE WHEN $3 = 'killed' THEN COALESCE(killed_by, $4) END,
                 killed_at = CASE WHEN $3 = 'killed' THEN COALESCE(killed_at, $5) END,
                 runtime_status = CASE WHEN $3 = 'enabled' AND state = 'killed'
                                       THEN 'pending' ELSE runtime_status END,
                 runtime_message = CASE WHEN $3 = 'enabled' AND state = 'killed'
                                        THEN '' ELSE runtime_message END,
                 probe_passed_at = CASE WHEN $3 = 'enabled' AND state = 'killed'
                                        THEN NULL ELSE probe_passed_at END
             WHERE org = $1 AND name = $2
             RETURNING {INSTALLATION_COLUMNS}"
        );
        sqlx::query(&sql)
            .bind(org)
            .bind(name)
            .bind(state.as_str())
            .bind(by)
            .bind(at)
            .fetch_optional(self.pool())
            .await
            .map_err(plugin_store_err)?
            .as_ref()
            .map(installation_from_row)
            .transpose()
    }

    async fn uninstall(
        &self,
        org: &str,
        name: &str,
        by: &str,
        at: i64,
    ) -> Result<Option<PluginUninstall>, PluginStoreError> {
        let mut tx = self.pool().begin().await.map_err(plugin_store_err)?;
        // The installation row's lock FIRST, so a connection bind (which
        // takes it before any connection row) cannot slip a credential in
        // between.
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM plugin_installations WHERE org = $1 AND name = $2 FOR UPDATE",
        )
        .bind(org)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        let Some(id) = id else {
            return Ok(None);
        };
        // The connections' secret keys, read as the rows go: the secrets live
        // in another store, so the caller deletes them (ADR-0072 §5).
        let mut connection_credential_refs: Vec<String> = sqlx::query_scalar(
            "DELETE FROM plugin_connections WHERE installation_id = $1 RETURNING credential_ref",
        )
        .bind(&id)
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        connection_credential_refs.sort();
        sqlx::query("DELETE FROM plugin_installations WHERE id = $1")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_store_err)?;
        sqlx::query(
            "UPDATE plugin_installation_names SET retired_at = $3, retired_by = $4
             WHERE org = $1 AND name = $2",
        )
        .bind(org)
        .bind(name)
        .bind(at)
        .bind(by)
        .execute(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        tx.commit().await.map_err(plugin_store_err)?;
        Ok(Some(PluginUninstall {
            connection_credential_refs,
        }))
    }

    async fn put_connection(
        &self,
        connection: &PluginConnection,
    ) -> Result<Option<String>, PluginStoreError> {
        let c = connection;
        if c.slot.is_empty()
            || c.credential_ref.is_empty()
            || !(c.base_url.starts_with("http://") || c.base_url.starts_with("https://"))
        {
            return Err(PluginStoreError::Invalid(
                "a plugin connection needs a slot, an http(s) base URL and a credential".into(),
            ));
        }
        let mut tx = self.pool().begin().await.map_err(plugin_store_err)?;
        // The installation row first (the module's lock order): an uninstall
        // that holds it wins and this finds nothing, and one that comes later
        // sees — and deletes — the binding written here.
        let installed: Option<String> =
            sqlx::query_scalar("SELECT id FROM plugin_installations WHERE id = $1 FOR UPDATE")
                .bind(&c.installation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(plugin_store_err)?;
        if installed.is_none() {
            return Err(PluginStoreError::NotFound);
        }
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT credential_ref FROM plugin_connections
              WHERE installation_id = $1 AND slot = $2 FOR UPDATE",
        )
        .bind(&c.installation_id)
        .bind(&c.slot)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        // Replaced whole: destination, credential handle and CA move together
        // (ADR-0072 §5 — the destination is part of the authority).
        sqlx::query(
            "INSERT INTO plugin_connections
                 (installation_id, slot, base_url, auth_scheme, credential_ref, ca_pem,
                  updated_by, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (installation_id, slot) DO UPDATE SET
                 base_url = EXCLUDED.base_url,
                 auth_scheme = EXCLUDED.auth_scheme,
                 credential_ref = EXCLUDED.credential_ref,
                 ca_pem = EXCLUDED.ca_pem,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&c.installation_id)
        .bind(&c.slot)
        .bind(&c.base_url)
        .bind(c.auth_scheme.as_str())
        .bind(&c.credential_ref)
        .bind(&c.ca_pem)
        .bind(&c.updated_by)
        .bind(c.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        tx.commit().await.map_err(plugin_store_err)?;
        Ok(previous)
    }

    async fn connections(
        &self,
        installation_id: &str,
    ) -> Result<Vec<PluginConnection>, PluginStoreError> {
        let sql = format!(
            "SELECT {CONNECTION_COLUMNS} FROM plugin_connections
              WHERE installation_id = $1 ORDER BY slot"
        );
        sqlx::query(&sql)
            .bind(installation_id)
            .fetch_all(self.pool())
            .await
            .map_err(plugin_store_err)?
            .iter()
            .map(connection_from_row)
            .collect()
    }

    async fn connection(
        &self,
        installation_id: &str,
        slot: &str,
    ) -> Result<Option<PluginConnection>, PluginStoreError> {
        let sql = format!(
            "SELECT {CONNECTION_COLUMNS} FROM plugin_connections
              WHERE installation_id = $1 AND slot = $2"
        );
        sqlx::query(&sql)
            .bind(installation_id)
            .bind(slot)
            .fetch_optional(self.pool())
            .await
            .map_err(plugin_store_err)?
            .as_ref()
            .map(connection_from_row)
            .transpose()
    }

    async fn delete_connection(
        &self,
        installation_id: &str,
        slot: &str,
    ) -> Result<Option<PluginConnection>, PluginStoreError> {
        let sql = format!(
            "DELETE FROM plugin_connections WHERE installation_id = $1 AND slot = $2
             RETURNING {CONNECTION_COLUMNS}"
        );
        sqlx::query(&sql)
            .bind(installation_id)
            .bind(slot)
            .fetch_optional(self.pool())
            .await
            .map_err(plugin_store_err)?
            .as_ref()
            .map(connection_from_row)
            .transpose()
    }

    async fn append_broker_journal(
        &self,
        entries: &[BrokerJournalEntry],
    ) -> Result<(), PluginStoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        // One round trip per batch: the journal writer drains its queue in
        // batches so a busy broker costs the pool a statement, not a row.
        let col = |f: fn(&BrokerJournalEntry) -> String| entries.iter().map(f).collect::<Vec<_>>();
        let opt = |f: fn(&BrokerJournalEntry) -> Option<String>| {
            entries.iter().map(f).collect::<Vec<_>>()
        };
        let num =
            |f: fn(&BrokerJournalEntry) -> Option<i64>| entries.iter().map(f).collect::<Vec<_>>();
        sqlx::query(
            "INSERT INTO plugin_broker_journal
                 (installation_id, slot, method, path, rule, status, bytes, started_at,
                  duration_ms, denied_reason, request_id, phase, upstream_path, query)
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
                                  $6::int[], $7::bigint[], $8::bigint[], $9::bigint[],
                                  $10::text[], $11::text[], $12::text[], $13::text[],
                                  $14::text[])",
        )
        .bind(col(|e| e.installation_id.clone()))
        .bind(col(|e| e.slot.clone()))
        .bind(col(|e| e.method.clone()))
        .bind(col(|e| e.path.clone()))
        .bind(opt(|e| e.rule.clone()))
        .bind(
            entries
                .iter()
                .map(|e| e.status.map(i32::from))
                .collect::<Vec<_>>(),
        )
        .bind(num(|e| e.bytes.map(saturating_i64)))
        .bind(entries.iter().map(|e| e.started_at).collect::<Vec<_>>())
        .bind(num(|e| e.duration_ms.map(saturating_i64)))
        .bind(opt(|e| e.denied_reason.clone()))
        .bind(col(|e| e.request_id.clone()))
        .bind(col(|e| e.phase.as_str().to_string()))
        .bind(opt(|e| e.upstream_path.clone()))
        .bind(opt(|e| e.query.clone()))
        .execute(self.pool())
        .await
        .map_err(plugin_store_err)?;
        Ok(())
    }

    async fn broker_journal(
        &self,
        installation_id: &str,
        limit: usize,
    ) -> Result<Vec<BrokerJournalEntry>, PluginStoreError> {
        sqlx::query(
            "SELECT installation_id, slot, method, path, rule, status, bytes, started_at,
                    duration_ms, denied_reason, request_id, phase, upstream_path, query
               FROM plugin_broker_journal
              WHERE installation_id = $1
              ORDER BY id DESC
              LIMIT $2",
        )
        .bind(installation_id)
        .bind(page_limit(limit))
        .fetch_all(self.pool())
        .await
        .map_err(plugin_store_err)?
        .iter()
        .map(|r| {
            let phase: String = r.get("phase");
            Ok(BrokerJournalEntry {
                request_id: r.get("request_id"),
                phase: BrokerJournalPhase::parse(&phase).ok_or_else(|| {
                    PluginStoreError::Store(format!("broker journal: unreadable phase {phase:?}"))
                })?,
                installation_id: r.get("installation_id"),
                slot: r.get("slot"),
                method: r.get("method"),
                path: r.get("path"),
                upstream_path: r.get("upstream_path"),
                query: r.get("query"),
                rule: r.get("rule"),
                status: r
                    .get::<Option<i32>, _>("status")
                    .and_then(|s| u16::try_from(s).ok()),
                bytes: r.get::<Option<i64>, _>("bytes").map(|b| b.max(0) as u64),
                started_at: r.get("started_at"),
                duration_ms: r
                    .get::<Option<i64>, _>("duration_ms")
                    .map(|d| d.max(0) as u64),
                denied_reason: r.get("denied_reason"),
            })
        })
        .collect()
    }

    async fn prune_broker_journal(&self, started_before: i64) -> Result<u64, PluginStoreError> {
        Ok(
            sqlx::query("DELETE FROM plugin_broker_journal WHERE started_at < $1")
                .bind(started_before)
                .execute(self.pool())
                .await
                .map_err(plugin_store_err)?
                .rows_affected(),
        )
    }
}
