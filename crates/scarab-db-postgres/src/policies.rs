//! [`PolicyStore`] on Postgres (ADR-0075 §9, §11; migration 0069).
//!
//! A write takes the Policy row `FOR UPDATE`, compares its version with the
//! one the caller read, and writes the row and a `policy_versions` row in one
//! transaction. Names are unique per scope among live Policies, held by a
//! partial unique index; a unique violation on it is
//! [`PolicyStoreError::NameTaken`]. A `step.admit` decision is held to one
//! per Attempt by another partial unique index; a violation there is
//! [`PolicyStoreError::Conflict`].

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::Row;

use scarab_policy::evaluate::{DecisionOutcome, PolicyDecision, PolicyResult};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::store::{
    decision_page_limit, DecisionCursor, DecisionRecord, NewPolicy, PolicyChange, PolicyScope,
    PolicyStore, PolicyStoreError, PolicyVersion, StoredPolicy,
};

use crate::PostgresDb;

/// SQLSTATE `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
const NAME_INDEX: &str = "policies_scope_name_live_idx";

fn store_err(e: sqlx::Error) -> PolicyStoreError {
    PolicyStoreError::Store(e.to_string())
}

/// The name of the unique index `e` violated, if it is a unique violation.
fn violated_index(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some(UNIQUE_VIOLATION) => {
            Some(d.constraint().unwrap_or_default().to_string())
        }
        _ => None,
    }
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

const POLICY_COLUMNS: &str =
    "id, org, project, name, version, body, content_hash, deleted, created_at, updated_at";

/// A `policies` row back into the domain. A body this binary cannot read is
/// reported, never defaulted.
fn policy_from_row(r: &PgRow) -> Result<StoredPolicy, PolicyStoreError> {
    let id: String = r.get("id");
    let body: Policy = serde_json::from_value(r.get::<Value, _>("body"))
        .map_err(|e| PolicyStoreError::Store(format!("policy {id}: unreadable body: {e}")))?;
    let org: String = r.get("org");
    let scope = match r.get::<Option<String>, _>("project") {
        None => PolicyScope::Org { org },
        Some(project) => PolicyScope::Project { org, project },
    };
    Ok(StoredPolicy {
        scope,
        name: r.get("name"),
        version: to_u64(r.get("version")),
        body,
        content_hash: r.get("content_hash"),
        deleted: r.get("deleted"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        id,
    })
}

fn version_from_row(r: &PgRow) -> Result<PolicyVersion, PolicyStoreError> {
    let policy_id: String = r.get("policy_id");
    let version = to_u64(r.get("version"));
    let body: Policy = serde_json::from_value(r.get::<Value, _>("body")).map_err(|e| {
        PolicyStoreError::Store(format!(
            "policy {policy_id} version {version}: unreadable body: {e}"
        ))
    })?;
    Ok(PolicyVersion {
        policy_id,
        version,
        body,
        content_hash: r.get("content_hash"),
        author: r.get("author"),
        reason: r.get("reason"),
        step_up_ref: r.get("step_up_ref"),
        deleted: r.get("deleted"),
        at: r.get("at"),
    })
}

const DECISION_COLUMNS: &str = "id, org, project, run_id, step_id, attempt, checkpoint, \
     schema_version, input, policy_set_digest, outcome, messages, results, at";

fn decision_from_row(r: &PgRow) -> Result<DecisionRecord, PolicyStoreError> {
    let id: String = r.get("id");
    let unreadable = |what: &str, e: String| {
        PolicyStoreError::Store(format!("decision {id}: unreadable {what}: {e}"))
    };
    let checkpoint: String = r.get("checkpoint");
    let checkpoint = Checkpoint::parse(&checkpoint)
        .ok_or_else(|| unreadable("checkpoint", checkpoint.clone()))?;
    let results: Vec<PolicyResult> = serde_json::from_value(r.get::<Value, _>("results"))
        .map_err(|e| unreadable("results", e.to_string()))?;
    let outcome = match r.get::<String, _>("outcome").as_str() {
        "allow" => DecisionOutcome::Allow,
        "deny" => DecisionOutcome::Deny {
            messages: serde_json::from_value(r.get::<Value, _>("messages"))
                .map_err(|e| unreadable("messages", e.to_string()))?,
        },
        other => return Err(unreadable("outcome", other.to_string())),
    };
    let attempt: Option<i64> = r.get("attempt");
    let schema_version: i32 = r.get("schema_version");
    Ok(DecisionRecord {
        org: r.get("org"),
        project: r.get("project"),
        run_id: r.get("run_id"),
        step_id: r.get("step_id"),
        attempt: attempt.map(|a| u32::try_from(a).unwrap_or(u32::MAX)),
        decision: PolicyDecision {
            checkpoint,
            schema_version: u32::try_from(schema_version).unwrap_or(0),
            input: r.get("input"),
            policy_set_digest: r.get("policy_set_digest"),
            results,
            outcome,
        },
        at: r.get("at"),
        id,
    })
}

fn body_json(body: &Policy) -> Result<Value, PolicyStoreError> {
    serde_json::to_value(body).map_err(|e| PolicyStoreError::Invalid(e.to_string()))
}

impl PostgresDb {
    /// Locks a live Policy, checks its version, and writes `body` as the next
    /// version (`deleted` for a delete).
    async fn write_version(
        &self,
        id: &str,
        expected_version: u64,
        body: Option<&Policy>,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        let row = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_err)?;
        let Some(row) = row else {
            return Err(PolicyStoreError::NotFound);
        };
        let current = policy_from_row(&row)?;
        if current.deleted {
            return Err(PolicyStoreError::NotFound);
        }
        if current.version != expected_version {
            return Err(PolicyStoreError::Conflict);
        }
        let deleted = body.is_none();
        let body = body.unwrap_or(&current.body);
        let json = body_json(body)?;
        let hash = body.content_hash();
        let version = current.version + 1;
        let updated = sqlx::query(&format!(
            "UPDATE policies SET name = $2, checkpoint = $3, version = $4, body = $5, \
             content_hash = $6, deleted = $7, updated_at = $8 WHERE id = $1 \
             RETURNING {POLICY_COLUMNS}"
        ))
        .bind(id)
        .bind(&body.name)
        .bind(body.checkpoint.as_str())
        .bind(to_i64(version))
        .bind(&json)
        .bind(&hash)
        .bind(deleted)
        .bind(change.at)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match violated_index(&e) {
            Some(ix) if ix == NAME_INDEX => PolicyStoreError::NameTaken(body.name.clone()),
            _ => store_err(e),
        })?;
        insert_version(&mut tx, id, version, &json, &hash, change, deleted).await?;
        tx.commit().await.map_err(store_err)?;
        policy_from_row(&updated)
    }
}

async fn insert_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    version: u64,
    body: &Value,
    hash: &str,
    change: &PolicyChange,
    deleted: bool,
) -> Result<(), PolicyStoreError> {
    sqlx::query(
        "INSERT INTO policy_versions \
         (policy_id, version, body, content_hash, author, reason, step_up_ref, deleted, at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(to_i64(version))
    .bind(body)
    .bind(hash)
    .bind(&change.author)
    .bind(&change.reason)
    .bind(&change.step_up_ref)
    .bind(deleted)
    .bind(change.at)
    .execute(&mut **tx)
    .await
    .map_err(store_err)?;
    Ok(())
}

#[async_trait]
impl PolicyStore for PostgresDb {
    async fn create(&self, new: &NewPolicy) -> Result<StoredPolicy, PolicyStoreError> {
        let body = &new.body;
        let json = body_json(body)?;
        let hash = body.content_hash();
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        let row = sqlx::query(&format!(
            "INSERT INTO policies \
             (id, org, project, name, checkpoint, version, body, content_hash, deleted, \
              created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, 1, $6, $7, false, $8, $8) \
             RETURNING {POLICY_COLUMNS}"
        ))
        .bind(&new.id)
        .bind(new.scope.org())
        .bind(new.scope.project())
        .bind(&body.name)
        .bind(body.checkpoint.as_str())
        .bind(&json)
        .bind(&hash)
        .bind(new.at)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match violated_index(&e) {
            Some(ix) if ix == NAME_INDEX => PolicyStoreError::NameTaken(body.name.clone()),
            Some(_) => PolicyStoreError::Conflict,
            None => store_err(e),
        })?;
        let change = PolicyChange {
            author: new.author.clone(),
            reason: new.reason.clone(),
            step_up_ref: None,
            at: new.at,
        };
        insert_version(&mut tx, &new.id, 1, &json, &hash, &change, false).await?;
        tx.commit().await.map_err(store_err)?;
        policy_from_row(&row)
    }

    async fn update(
        &self,
        id: &str,
        expected_version: u64,
        body: &Policy,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        self.write_version(id, expected_version, Some(body), change)
            .await
    }

    async fn delete(
        &self,
        id: &str,
        expected_version: u64,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        self.write_version(id, expected_version, None, change).await
    }

    async fn get(&self, id: &str) -> Result<Option<StoredPolicy>, PolicyStoreError> {
        let row = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        row.as_ref().map(policy_from_row).transpose()
    }

    async fn list(&self, scope: &PolicyScope) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies \
             WHERE org = $1 AND project IS NOT DISTINCT FROM $2 AND NOT deleted \
             ORDER BY name"
        ))
        .bind(scope.org())
        .bind(scope.project())
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        rows.iter().map(policy_from_row).collect()
    }

    async fn versions(&self, id: &str) -> Result<Vec<PolicyVersion>, PolicyStoreError> {
        let rows = sqlx::query(
            "SELECT policy_id, version, body, content_hash, author, reason, step_up_ref, \
             deleted, at FROM policy_versions WHERE policy_id = $1 ORDER BY version",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        rows.iter().map(version_from_row).collect()
    }

    async fn version(
        &self,
        id: &str,
        version: u64,
    ) -> Result<Option<PolicyVersion>, PolicyStoreError> {
        let row = sqlx::query(
            "SELECT policy_id, version, body, content_hash, author, reason, step_up_ref, \
             deleted, at FROM policy_versions WHERE policy_id = $1 AND version = $2",
        )
        .bind(id)
        .bind(to_i64(version))
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        row.as_ref().map(version_from_row).transpose()
    }

    async fn effective(
        &self,
        org: &str,
        project: Option<&str>,
        checkpoint: Checkpoint,
    ) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {POLICY_COLUMNS} FROM policies \
             WHERE org = $1 AND checkpoint = $2 AND NOT deleted \
               AND (project IS NULL OR project = $3) \
             ORDER BY project IS NOT NULL, name"
        ))
        .bind(org)
        .bind(checkpoint.as_str())
        .bind(project)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        rows.iter().map(policy_from_row).collect()
    }

    async fn record_decision(&self, record: &DecisionRecord) -> Result<String, PolicyStoreError> {
        let d = &record.decision;
        let results = serde_json::to_value(&d.results)
            .map_err(|e| PolicyStoreError::Invalid(e.to_string()))?;
        let outcome = match d.outcome {
            DecisionOutcome::Allow => "allow",
            DecisionOutcome::Deny { .. } => "deny",
        };
        sqlx::query(&format!(
            "INSERT INTO policy_decisions ({DECISION_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"
        ))
        .bind(&record.id)
        .bind(&record.org)
        .bind(&record.project)
        .bind(&record.run_id)
        .bind(&record.step_id)
        .bind(record.attempt.map(i64::from))
        .bind(d.checkpoint.as_str())
        .bind(i32::try_from(d.schema_version).unwrap_or(i32::MAX))
        .bind(&d.input)
        .bind(&d.policy_set_digest)
        .bind(outcome)
        .bind(serde_json::json!(d.messages()))
        .bind(&results)
        .bind(record.at)
        .execute(self.pool())
        .await
        .map_err(|e| match violated_index(&e) {
            // The step.admit index, or the primary key: recorded already.
            Some(_) => PolicyStoreError::Conflict,
            None => store_err(e),
        })?;
        Ok(record.id.clone())
    }

    async fn decision(&self, id: &str) -> Result<Option<DecisionRecord>, PolicyStoreError> {
        let row = sqlx::query(&format!(
            "SELECT {DECISION_COLUMNS} FROM policy_decisions WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        row.as_ref().map(decision_from_row).transpose()
    }

    async fn decisions_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {DECISION_COLUMNS} FROM policy_decisions WHERE run_id = $1 ORDER BY at, id"
        ))
        .bind(run_id)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        rows.iter().map(decision_from_row).collect()
    }

    async fn org_decisions(
        &self,
        org: &str,
        limit: usize,
        before: Option<&DecisionCursor>,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        let limit = decision_page_limit(limit) as i64;
        let rows = sqlx::query(&format!(
            "SELECT {DECISION_COLUMNS} FROM policy_decisions \
             WHERE org = $1 AND ($3::BIGINT IS NULL OR (at, id) < ($3, $4)) \
             ORDER BY at DESC, id DESC LIMIT $2"
        ))
        .bind(org)
        .bind(limit)
        .bind(before.map(|c| c.at))
        .bind(before.map(|c| c.id.as_str()))
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        rows.iter().map(decision_from_row).collect()
    }
}
