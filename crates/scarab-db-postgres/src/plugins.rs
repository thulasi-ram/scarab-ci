//! [`PluginStore`] on Postgres (ADR-0072, migration 0055): the
//! PluginInstallation registry with its kill switch, and report ingest into
//! the Deployment record.
//!
//! **Ingest is one transaction per batch**, in two phases, and its locks are
//! always taken in one order (migration 0055):
//!
//! 1. the **installation row**, `FOR UPDATE`, first — the kill switch is
//!    re-read under it, so a kill that commits first refuses the batch and
//!    one that commits after waits for it;
//! 2. per item, that target's **cursor row** — every item is decided here
//!    (duplicate, stale, unmapped, cap, or to be recorded) and its cursor
//!    written, but nothing is appended yet;
//! 3. **last**, the Org's **Deployment counter row**, taken explicitly, then
//!    the buffered appends and receipts.
//!
//! Cursor rows are only written under their installation's row lock, so they
//! are never contended across transactions; and the counter — the one lock a
//! batch shares with every Deployment request in the Org — is held only for
//! the appends, not across a hundred items' worth of deciding. No cycle can
//! form: request appends take only the counter, an Environment PUT takes its
//! installations `FOR SHARE` before any source row, and uninstall takes the
//! installation row before the sources naming it.
//!
//! Every lock wait in an ingest is bounded by `lock_timeout`
//! ([`crate::PLUGIN_INGEST_LOCK_TIMEOUT`]): a batch stuck behind the counter
//! gives up ([`PluginStoreError::Busy`]) rather than hold the installation
//! row — the row a kill needs — indefinitely.
//!
//! The org and the source written into the record come from the locked
//! installation row, never from a report.
//!
//! **PluginConnections** (ADR-0072 §5, migration 0056) follow the same order:
//! a bind takes the installation row `FOR UPDATE` before the connection row,
//! and uninstall takes the installation row before deleting the connections
//! (returning their credential keys, which live in the SecretProvider). The
//! broker's journal is append-only and keyed by nothing it locks.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use serde_json::Value;
use sqlx::Row;

use scarab_deployment::{
    DeploymentInput, DeploymentStatusReport, DeploymentTarget, DeploymentVersion,
};
use scarab_plugin::{
    BrokerJournalEntry, DeploymentSourceTarget, EnvironmentTargetsPage,
    InstallationDeploymentSource, InstallationState, InstallationUsage, PluginConnection,
    PluginConnectionAuthScheme, PluginInstallation, PluginManifest, PluginStore, PluginStoreError,
    PluginUninstall, ReportIngest, ReportItem, ReportItemOutcome, ReportOutcome, RuntimeStatus,
    SourceTargetsPage, TargetRouting, UnmappedGrouping, MAX_TARGETS_PER_INSTALLATION,
    MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION, PAGE_DEFAULT_LIMIT, PAGE_MAX_LIMIT,
};

use crate::{append_deployment_input, PostgresDb};

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

/// A target row as every read selects it. `routed` is the alias of the
/// `environment_deployment_sources` row its grouping joins to NOW (left-joined:
/// none = unmapped); `last_*` come through `last_*_expr`, so a read can hide a
/// status that was recorded somewhere else.
fn target_columns(last_status_visible: &str) -> String {
    format!(
        "t.installation_id, i.name AS installation, t.target_id, t.vendor_grouping, \
         routed.environment AS routed_environment, t.status_environment, \
         (routed.environment IS NOT NULL \
          AND routed.environment IS DISTINCT FROM t.status_environment) AS resend, \
         t.last_sequence, t.version_scheme, \
         CASE WHEN {last_status_visible} THEN t.last_version END AS last_version, \
         CASE WHEN {last_status_visible} THEN t.last_status END AS last_status, \
         CASE WHEN {last_status_visible} THEN t.last_label END AS last_label, \
         t.owner_projects, t.owners_reported_at, t.updated_at"
    )
}

fn target_from_row(r: &sqlx::postgres::PgRow) -> DeploymentSourceTarget {
    let mut owner_projects: Vec<String> = r.get("owner_projects");
    owner_projects.sort();
    DeploymentSourceTarget {
        installation_id: r.get("installation_id"),
        installation: r.get("installation"),
        target_id: r.get("target_id"),
        vendor_grouping: r.get("vendor_grouping"),
        environment: r.get("routed_environment"),
        status_environment: r.get("status_environment"),
        resend: r.get("resend"),
        // The CHECK keeps it positive.
        last_sequence: r.get::<i64, _>("last_sequence") as u64,
        version_scheme: r.get("version_scheme"),
        last_version: r.get("last_version"),
        last_status: r.get("last_status"),
        last_label: r.get("last_label"),
        owner_projects,
        owners_reported_at: r.get("owners_reported_at"),
        updated_at: r.get("updated_at"),
    }
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

fn saturating_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Where an ingested item's `input_seq` comes from: known already (a
/// duplicate of a stored receipt), or the seq of a buffered append.
#[derive(Debug, Clone, Copy)]
enum InputSeq {
    Known(Option<u64>),
    Append(usize),
}

/// One item's decision in phase 1, finished in phase 2.
struct Decided {
    outcome: ReportOutcome,
    reason: Option<String>,
    input_seq: InputSeq,
    /// Write a receipt for it (every outcome but `duplicate` and `rejected`).
    receipt: bool,
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
        // The installation row's lock FIRST, so an ingest in flight finishes
        // first and none starts after, a connection bind (which takes it
        // before any connection row) cannot slip a credential in between, and
        // an Environment PUT naming it (which holds it FOR SHARE before
        // touching any source row) either commits before this or waits for it
        // — no two transactions ever wait on each other's dependent rows.
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
        // Cursors, receipts and unmapped counts go with it (ON DELETE
        // CASCADE).
        sqlx::query("DELETE FROM plugin_installations WHERE id = $1")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_store_err)?;
        // Every source naming it goes too: a source naming a retired name
        // could never route again, and leaving it would read as if it might.
        let mut removed: Vec<InstallationDeploymentSource> = sqlx::query(
            "DELETE FROM environment_deployment_sources WHERE org = $1 AND installation = $2
             RETURNING environment, vendor_grouping",
        )
        .bind(org)
        .bind(name)
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_store_err)?
        .iter()
        .map(|r| InstallationDeploymentSource {
            environment: r.get("environment"),
            vendor_grouping: r.get("vendor_grouping"),
        })
        .collect();
        removed.sort();
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
            deployment_sources: removed,
            connection_credential_refs,
        }))
    }

    async fn deployment_sources_naming(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Vec<InstallationDeploymentSource>, PluginStoreError> {
        Ok(sqlx::query(
            "SELECT environment, vendor_grouping FROM environment_deployment_sources
             WHERE org = $1 AND installation = $2
             ORDER BY environment, vendor_grouping",
        )
        .bind(org)
        .bind(name)
        .fetch_all(self.pool())
        .await
        .map_err(plugin_store_err)?
        .iter()
        .map(|r| InstallationDeploymentSource {
            environment: r.get("environment"),
            vendor_grouping: r.get("vendor_grouping"),
        })
        .collect())
    }

    async fn ingest_report(
        &self,
        installation_id: &str,
        items: &[ReportIngest],
        received_at: i64,
    ) -> Result<Vec<ReportItemOutcome>, PluginStoreError> {
        let mut tx = self.pool().begin().await.map_err(plugin_store_err)?;
        // Every lock wait below is bounded: a batch stuck behind the Org's
        // counter must not hold the installation row — the lock a kill
        // takes — for longer than this.
        sqlx::query(&format!(
            "SET LOCAL lock_timeout = '{}ms'",
            self.plugin_ingest_lock_timeout.as_millis()
        ))
        .execute(&mut *tx)
        .await
        .map_err(plugin_store_err)?;
        // 1. The installation, locked: the kill switch re-read under the lock
        //    a kill takes, and the org + name every report is written under.
        let Some(installation) = sqlx::query(
            "SELECT org, name, state FROM plugin_installations WHERE id = $1 FOR UPDATE",
        )
        .bind(installation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_store_err)?
        else {
            return Err(PluginStoreError::NotFound);
        };
        if installation.get::<String, _>("state") != InstallationState::Enabled.as_str() {
            return Err(PluginStoreError::Killed);
        }
        let org: String = installation.get("org");
        let name: String = installation.get("name");

        // The caps, counted once per batch: under the installation lock no
        // other batch of this installation adds rows meanwhile, and this
        // batch's own new rows are counted as they are made.
        let (mut target_rows, mut unmapped_rows): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM deployment_source_targets WHERE installation_id = $1),
                    (SELECT count(*) FROM deployment_source_unmapped WHERE installation_id = $1)",
        )
        .bind(installation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plugin_store_err)?;

        // 2. Decide every item; write cursors and unmapped counts; buffer the
        //    appends and receipts for phase 3.
        let mut decided: Vec<Decided> = Vec::with_capacity(items.len());
        let mut appends: Vec<DeploymentInput> = Vec::new();
        // Keys this batch has used: a repeat within the batch must see the
        // earlier copy even though its receipt is not written yet.
        let mut seen: HashMap<&str, (&str, ReportOutcome, InputSeq)> = HashMap::new();
        for ingest in items {
            let item = &ingest.item;
            let key = item.idempotency_key();
            let first = match seen.get(key) {
                Some(&(content, outcome, input_seq)) => {
                    Some((content.to_string(), outcome, input_seq))
                }
                None => sqlx::query(
                    "SELECT content_sha256, outcome, input_seq FROM plugin_report_receipts
                     WHERE installation_id = $1 AND idempotency_key = $2",
                )
                .bind(installation_id)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(plugin_store_err)?
                .map(|r| {
                    let outcome: String = r.get("outcome");
                    (
                        r.get::<String, _>("content_sha256"),
                        // The CHECK admits only outcomes `parse` knows.
                        ReportOutcome::parse(&outcome).unwrap_or(ReportOutcome::Accepted),
                        InputSeq::Known(r.get::<Option<i64>, _>("input_seq").map(|s| s as u64)),
                    )
                }),
            };
            if let Some((content, outcome, input_seq)) = first {
                if content != ingest.content_sha256 {
                    // Dropping `tx` rolls the whole batch back.
                    return Err(PluginStoreError::IdempotencyConflict {
                        idempotency_key: key.to_string(),
                    });
                }
                decided.push(Decided {
                    outcome: ReportOutcome::Duplicate,
                    reason: Some(format!("first received as {}", outcome.as_str())),
                    input_seq,
                    receipt: false,
                });
                continue;
            }

            // The cursor. Not newer than the last accepted → stale.
            let sequence = item.sequence() as i64;
            let cursor = sqlx::query(
                "SELECT last_sequence FROM deployment_source_targets
                 WHERE installation_id = $1 AND target_id = $2
                 FOR UPDATE",
            )
            .bind(installation_id)
            .bind(item.target_id())
            .fetch_optional(&mut *tx)
            .await
            .map_err(plugin_store_err)?;
            let last: Option<i64> = cursor.as_ref().map(|r| r.get("last_sequence"));
            if let Some(last) = last.filter(|last| sequence <= *last) {
                seen.insert(
                    key,
                    (
                        &ingest.content_sha256,
                        ReportOutcome::Stale,
                        InputSeq::Known(None),
                    ),
                );
                decided.push(Decided {
                    outcome: ReportOutcome::Stale,
                    reason: Some(format!("sequence {sequence} is not newer than {last}")),
                    input_seq: InputSeq::Known(None),
                    receipt: true,
                });
                continue;
            }

            // Route by the sources an Environment lists *now* (ADR-0071 §5)
            // — the same single-statement lookup as
            // `environment_for_deployment_source`, in this transaction.
            let environment: Option<String> = sqlx::query_scalar(
                "SELECT environment FROM environment_deployment_sources
                 WHERE org = $1 AND installation = $2 AND vendor_grouping = $3",
            )
            .bind(&org)
            .bind(&name)
            .bind(item.vendor_grouping())
            .fetch_optional(&mut *tx)
            .await
            .map_err(plugin_store_err)?;

            // The caps block only NEW rows: a known target keeps reporting,
            // and a grouping already counted keeps counting.
            let new_target = cursor.is_none();
            let new_unmapped = environment.is_none()
                && sqlx::query_scalar::<_, i64>(
                    // `1::BIGINT`: a bare `1` is INT4 and will not decode as i64.
                    "SELECT 1::BIGINT FROM deployment_source_unmapped
                     WHERE installation_id = $1 AND vendor_grouping = $2",
                )
                .bind(installation_id)
                .bind(item.vendor_grouping())
                .fetch_optional(&mut *tx)
                .await
                .map_err(plugin_store_err)?
                .is_none();
            let over_cap = if new_target && target_rows as u64 >= MAX_TARGETS_PER_INSTALLATION {
                Some(format!(
                    "cap: installation `{name}` already reports {MAX_TARGETS_PER_INSTALLATION} \
                     targets; a new one is refused (a known one keeps reporting)"
                ))
            } else if new_unmapped
                && unmapped_rows as u64 >= MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION
            {
                Some(format!(
                    "cap: installation `{name}` already has \
                     {MAX_UNMAPPED_GROUPINGS_PER_INSTALLATION} groupings no environment lists; \
                     a new one is refused until an environment lists some"
                ))
            } else {
                None
            };
            if let Some(reason) = over_cap {
                // No receipt: nothing of it was stored, so the reporter may
                // send the same key again once there is room.
                decided.push(Decided {
                    outcome: ReportOutcome::Rejected,
                    reason: Some(reason),
                    input_seq: InputSeq::Known(None),
                    receipt: false,
                });
                continue;
            }
            if new_target {
                target_rows += 1;
            }

            // Record it (buffered), or count the drop.
            let (outcome, input_seq, appended_to) = match (&environment, item) {
                (None, _) => {
                    sqlx::query(
                        "INSERT INTO deployment_source_unmapped AS u
                             (installation_id, vendor_grouping, org, reports, first_at, last_at,
                              last_target_id)
                         VALUES ($1, $2, $3, 1, $4, $4, $5)
                         ON CONFLICT (installation_id, vendor_grouping) DO UPDATE SET
                             reports = u.reports + 1,
                             last_at = GREATEST(u.last_at, EXCLUDED.last_at),
                             last_target_id = EXCLUDED.last_target_id",
                    )
                    .bind(installation_id)
                    .bind(item.vendor_grouping())
                    .bind(&org)
                    .bind(received_at)
                    .bind(item.target_id())
                    .execute(&mut *tx)
                    .await
                    .map_err(plugin_store_err)?;
                    if new_unmapped {
                        unmapped_rows += 1;
                    }
                    (ReportOutcome::Unmapped, InputSeq::Known(None), None)
                }
                (Some(environment), ReportItem::Status(status)) => {
                    appends.push(DeploymentInput::Report {
                        target: DeploymentTarget {
                            environment: environment.clone(),
                            source: name.clone(),
                            id: status.target_id.clone(),
                        },
                        version: DeploymentVersion(status.version.clone()),
                        report: DeploymentStatusReport {
                            status: status.status.into(),
                            label: status.label.clone(),
                            link: status.link.clone(),
                            // Kept as `report_source_at`, display-only; the
                            // append stamps `received_at` (ADR-0070 §8).
                            at: status.source_at,
                        },
                    });
                    (
                        ReportOutcome::Accepted,
                        InputSeq::Append(appends.len() - 1),
                        Some(environment.as_str()),
                    )
                }
                (Some(_), ReportItem::Ownership(_)) => {
                    (ReportOutcome::Accepted, InputSeq::Known(None), None)
                }
            };
            if environment.is_some() {
                // Mapped now: whatever was counted against it is history.
                let cleared = sqlx::query(
                    "DELETE FROM deployment_source_unmapped
                     WHERE installation_id = $1 AND vendor_grouping = $2",
                )
                .bind(installation_id)
                .bind(item.vendor_grouping())
                .execute(&mut *tx)
                .await
                .map_err(plugin_store_err)?
                .rows_affected();
                unmapped_rows -= cleared as i64;
            }

            // Advance the cursor, and keep its grouping, where this report
            // routed, where its status was recorded, its scheme, its last
            // status and its owners. When the routing changed since the last
            // report, the stored status is another Environment's fact: reset
            // it first, then apply what this item carries (red-team F-3).
            let status = match item {
                ReportItem::Status(s) => Some(s),
                ReportItem::Ownership(_) => None,
            };
            let owners: Option<Vec<String>> = item.owners().map(|o| {
                let mut o = o.to_vec();
                o.sort();
                o
            });
            sqlx::query(
                "INSERT INTO deployment_source_targets AS t
                     (installation_id, target_id, org, vendor_grouping, environment,
                      status_environment, last_sequence, version_scheme, last_version,
                      last_status, last_label, owner_projects, owners_reported_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                         COALESCE($12::text[], '{}'),
                         CASE WHEN $12::text[] IS NULL THEN NULL ELSE $13::bigint END, $13)
                 ON CONFLICT (installation_id, target_id) DO UPDATE SET
                     vendor_grouping = EXCLUDED.vendor_grouping,
                     environment = EXCLUDED.environment,
                     -- A status says where it was recorded (NULL = dropped);
                     -- an ownership report says nothing about status.
                     status_environment = CASE
                         WHEN $14 THEN EXCLUDED.status_environment
                         WHEN t.environment IS DISTINCT FROM EXCLUDED.environment THEN NULL
                         ELSE t.status_environment END,
                     last_sequence = EXCLUDED.last_sequence,
                     version_scheme = COALESCE(EXCLUDED.version_scheme, t.version_scheme),
                     last_version = CASE
                         WHEN t.environment IS DISTINCT FROM EXCLUDED.environment
                             THEN EXCLUDED.last_version
                         ELSE COALESCE(EXCLUDED.last_version, t.last_version) END,
                     last_status = CASE
                         WHEN t.environment IS DISTINCT FROM EXCLUDED.environment
                             THEN EXCLUDED.last_status
                         ELSE COALESCE(EXCLUDED.last_status, t.last_status) END,
                     last_label = CASE
                         WHEN t.environment IS DISTINCT FROM EXCLUDED.environment
                             THEN EXCLUDED.last_label
                         ELSE COALESCE(EXCLUDED.last_label, t.last_label) END,
                     owner_projects = CASE WHEN $12::text[] IS NULL THEN t.owner_projects
                                           ELSE EXCLUDED.owner_projects END,
                     owners_reported_at = COALESCE(EXCLUDED.owners_reported_at,
                                                   t.owners_reported_at),
                     updated_at = EXCLUDED.updated_at",
            )
            .bind(installation_id)
            .bind(item.target_id())
            .bind(&org)
            .bind(item.vendor_grouping())
            .bind(&environment)
            .bind(appended_to)
            .bind(sequence)
            .bind(status.map(|s| s.version_scheme.as_str()))
            .bind(status.map(|s| s.version.as_str()))
            .bind(status.map(|s| s.status.as_str()))
            .bind(status.map(|s| s.label.as_str()))
            .bind(owners)
            .bind(received_at)
            .bind(status.is_some())
            .execute(&mut *tx)
            .await
            .map_err(plugin_store_err)?;

            seen.insert(key, (&ingest.content_sha256, outcome, input_seq));
            decided.push(Decided {
                outcome,
                reason: None,
                input_seq,
                receipt: true,
            });
        }

        // 3. Last: the Org's counter, then the buffered appends. Locked
        //    explicitly so a wait on it is this module's to classify (a
        //    lock timeout is `Busy`, not a store failure) and so the lock
        //    order is visible here rather than buried in the append.
        let mut seqs: Vec<u64> = Vec::with_capacity(appends.len());
        if !appends.is_empty() {
            sqlx::query("SELECT 1 FROM deployment_input_counters WHERE org = $1 FOR UPDATE")
                .bind(&org)
                .fetch_optional(&mut *tx)
                .await
                .map_err(plugin_store_err)?;
            for input in &appends {
                let logged = append_deployment_input(&mut tx, &org, input, received_at)
                    .await
                    .map_err(|e| PluginStoreError::Store(e.to_string()))?;
                seqs.push(logged.seq);
            }
        }
        let resolve = |input_seq: InputSeq| match input_seq {
            InputSeq::Known(seq) => seq,
            InputSeq::Append(i) => Some(seqs[i]),
        };

        // The receipts, in one statement.
        let mut keys: Vec<&str> = Vec::new();
        let mut contents: Vec<&str> = Vec::new();
        let mut receipt_outcomes: Vec<&str> = Vec::new();
        let mut input_seqs: Vec<Option<i64>> = Vec::new();
        for (ingest, d) in items.iter().zip(&decided) {
            if d.receipt {
                keys.push(ingest.item.idempotency_key());
                contents.push(&ingest.content_sha256);
                receipt_outcomes.push(d.outcome.as_str());
                input_seqs.push(resolve(d.input_seq).map(|s| s as i64));
            }
        }
        if !keys.is_empty() {
            sqlx::query(
                "INSERT INTO plugin_report_receipts
                     (installation_id, idempotency_key, content_sha256, outcome, input_seq,
                      received_at)
                 SELECT $1, k, c, o, s, $6
                 FROM UNNEST($2::text[], $3::text[], $4::text[], $5::bigint[]) AS r(k, c, o, s)",
            )
            .bind(installation_id)
            .bind(&keys)
            .bind(&contents)
            .bind(&receipt_outcomes)
            .bind(&input_seqs)
            .bind(received_at)
            .execute(&mut *tx)
            .await
            .map_err(plugin_store_err)?;
        }
        tx.commit().await.map_err(plugin_store_err)?;
        Ok(decided
            .into_iter()
            .map(|d| ReportItemOutcome {
                outcome: d.outcome,
                input_seq: resolve(d.input_seq),
                reason: d.reason,
            })
            .collect())
    }

    async fn source_targets(
        &self,
        installation_id: &str,
        page: &SourceTargetsPage,
    ) -> Result<Vec<DeploymentSourceTarget>, PluginStoreError> {
        // The plugin's own view: its cursors' last status is its seed state
        // wherever it was recorded.
        let sql = format!(
            "SELECT {} FROM deployment_source_targets t
             JOIN plugin_installations i ON i.id = t.installation_id
             LEFT JOIN environment_deployment_sources routed
               ON routed.org = i.org AND routed.installation = i.name
              AND routed.vendor_grouping = t.vendor_grouping
             WHERE t.installation_id = $1
               AND ($2::text IS NULL OR t.target_id > $2)
               AND (NOT $3 OR (routed.environment IS NOT NULL
                               AND routed.environment IS DISTINCT FROM t.status_environment))
             ORDER BY t.target_id
             LIMIT $4",
            target_columns("TRUE")
        );
        Ok(sqlx::query(&sql)
            .bind(installation_id)
            .bind(page.after.as_deref())
            .bind(page.resend_only)
            .bind(page_limit(page.limit))
            .fetch_all(self.pool())
            .await
            .map_err(plugin_store_err)?
            .iter()
            .map(target_from_row)
            .collect())
    }

    async fn environment_targets(
        &self,
        org: &str,
        environment: &str,
        page: &EnvironmentTargetsPage,
    ) -> Result<Vec<DeploymentSourceTarget>, PluginStoreError> {
        // Routed by the sources as they are NOW (not by where each target's
        // last report went), so an admin's move shows at once — and a status
        // recorded in another Environment is not shown as this one's.
        let sql = format!(
            "SELECT {} FROM environment_deployment_sources routed
             JOIN plugin_installations i
               ON i.org = routed.org AND i.name = routed.installation
             JOIN deployment_source_targets t
               ON t.installation_id = i.id AND t.vendor_grouping = routed.vendor_grouping
             WHERE routed.org = $1 AND routed.environment = $2
               AND ($3::text IS NULL OR (i.name, t.target_id) > ($3::text, $4::text))
             ORDER BY i.name, t.target_id
             LIMIT $5",
            target_columns("t.status_environment = routed.environment")
        );
        let (after_installation, after_target) = match &page.after {
            Some((installation, target)) => (Some(installation.as_str()), Some(target.as_str())),
            None => (None, None),
        };
        Ok(sqlx::query(&sql)
            .bind(org)
            .bind(environment)
            .bind(after_installation)
            .bind(after_target)
            .bind(page_limit(page.limit))
            .fetch_all(self.pool())
            .await
            .map_err(plugin_store_err)?
            .iter()
            .map(target_from_row)
            .collect())
    }

    async fn target_routings(
        &self,
        org: &str,
        targets: &[(String, String)],
    ) -> Result<Vec<TargetRouting>, PluginStoreError> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let (installations, ids): (Vec<&str>, Vec<&str>) = targets
            .iter()
            .map(|(installation, id)| (installation.as_str(), id.as_str()))
            .unzip();
        Ok(sqlx::query(
            "SELECT i.name AS installation, t.target_id, routed.environment
             FROM UNNEST($2::text[], $3::text[]) AS q(installation, target_id)
             JOIN plugin_installations i ON i.org = $1 AND i.name = q.installation
             JOIN deployment_source_targets t
               ON t.installation_id = i.id AND t.target_id = q.target_id
             LEFT JOIN environment_deployment_sources routed
               ON routed.org = i.org AND routed.installation = i.name
              AND routed.vendor_grouping = t.vendor_grouping",
        )
        .bind(org)
        .bind(&installations)
        .bind(&ids)
        .fetch_all(self.pool())
        .await
        .map_err(plugin_store_err)?
        .iter()
        .map(|r| TargetRouting {
            installation: r.get("installation"),
            target_id: r.get("target_id"),
            environment: r.get("environment"),
        })
        .collect())
    }

    async fn unmapped_groupings(
        &self,
        installation_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<UnmappedGrouping>, PluginStoreError> {
        // Filtered by the sources as they are NOW, so a grouping an admin has
        // just mapped drops off without waiting for its next report.
        Ok(sqlx::query(
            "SELECT u.vendor_grouping, u.reports, u.first_at, u.last_at, u.last_target_id
             FROM deployment_source_unmapped u
             JOIN plugin_installations i ON i.id = u.installation_id
             WHERE u.installation_id = $1
               AND ($2::text IS NULL OR u.vendor_grouping > $2)
               AND NOT EXISTS (
                   SELECT 1 FROM environment_deployment_sources s
                   WHERE s.org = i.org AND s.installation = i.name
                     AND s.vendor_grouping = u.vendor_grouping)
             ORDER BY u.vendor_grouping
             LIMIT $3",
        )
        .bind(installation_id)
        .bind(after)
        .bind(page_limit(limit))
        .fetch_all(self.pool())
        .await
        .map_err(plugin_store_err)?
        .iter()
        .map(|r| UnmappedGrouping {
            vendor_grouping: r.get("vendor_grouping"),
            reports: r.get::<i64, _>("reports") as u64,
            first_at: r.get("first_at"),
            last_at: r.get("last_at"),
            last_target_id: r.get("last_target_id"),
        })
        .collect())
    }

    async fn installation_usage(
        &self,
        installation_id: &str,
    ) -> Result<InstallationUsage, PluginStoreError> {
        let (targets, unmapped_groupings): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM deployment_source_targets WHERE installation_id = $1),
                    (SELECT count(*) FROM deployment_source_unmapped WHERE installation_id = $1)",
        )
        .bind(installation_id)
        .fetch_one(self.pool())
        .await
        .map_err(plugin_store_err)?;
        Ok(InstallationUsage {
            targets: targets as u64,
            unmapped_groupings: unmapped_groupings as u64,
        })
    }

    async fn prune_report_receipts(&self, received_before: i64) -> Result<u64, PluginStoreError> {
        Ok(
            sqlx::query("DELETE FROM plugin_report_receipts WHERE received_at < $1")
                .bind(received_before)
                .execute(self.pool())
                .await
                .map_err(plugin_store_err)?
                .rows_affected(),
        )
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
                  duration_ms, denied_reason)
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
                                  $6::int[], $7::bigint[], $8::bigint[], $9::bigint[],
                                  $10::text[])",
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
        Ok(sqlx::query(
            "SELECT installation_id, slot, method, path, rule, status, bytes, started_at,
                    duration_ms, denied_reason
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
        .map(|r| BrokerJournalEntry {
            installation_id: r.get("installation_id"),
            slot: r.get("slot"),
            method: r.get("method"),
            path: r.get("path"),
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
        .collect())
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
