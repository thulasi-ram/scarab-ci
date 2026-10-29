//! [`LibraryStore`] on Postgres (ADR-0076 §4-§6; migration 0071).
//!
//! An install takes the installation row `FOR UPDATE` (or finds none),
//! compares it with what the caller expects to replace, and writes in the
//! same transaction. A first install racing another first install meets the
//! primary key; that is [`LibraryStoreError::Conflict`] as well. Lists order
//! by `COLLATE "C"`, bytewise, as the in-memory store does.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::Row;

use scarab_pipeline::InstalledLibrary;
use scarab_policy::library::{
    DefaultsState, LibraryInstallation, LibraryStore, LibraryStoreError, LibraryVersion,
    LibraryYank, MissingDefault, NewInstallation, OrgLibraryDefaults, Replaces,
};

use crate::PostgresDb;

/// SQLSTATE `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";

fn store_err(e: sqlx::Error) -> LibraryStoreError {
    LibraryStoreError::Store(e.to_string())
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(d) => d.code().map(|c| c.to_string()),
        _ => None,
    }
}

const VERSION_COLUMNS: &str =
    "org, content_hash, content, source, publisher, name, first_party, fetched_by, fetched_at";

fn version_from_row(r: &PgRow) -> LibraryVersion {
    LibraryVersion {
        org: r.get("org"),
        content_hash: r.get("content_hash"),
        content: r.get("content"),
        source: r.get("source"),
        publisher: r.get("publisher"),
        name: r.get("name"),
        first_party: r.get("first_party"),
        fetched_by: r.get("fetched_by"),
        fetched_at: r.get("fetched_at"),
    }
}

/// An installation joined with its version.
const INSTALLATION_SELECT: &str = "SELECT i.org, i.publisher, i.name, i.content_hash, \
     v.source, v.first_party, i.installed_by, i.installed_at, i.step_up_ref \
     FROM library_installations i \
     JOIN library_versions v ON v.org = i.org AND v.content_hash = i.content_hash";

fn installation_from_row(r: &PgRow) -> LibraryInstallation {
    LibraryInstallation {
        org: r.get("org"),
        publisher: r.get("publisher"),
        name: r.get("name"),
        content_hash: r.get("content_hash"),
        source: r.get("source"),
        first_party: r.get("first_party"),
        installed_by: r.get("installed_by"),
        installed_at: r.get("installed_at"),
        step_up_ref: r.get("step_up_ref"),
    }
}

const YANK_COLUMNS: &str = "org, content_hash, reason, yanked_by, yanked_at";

fn yank_from_row(r: &PgRow) -> LibraryYank {
    LibraryYank {
        org: r.get("org"),
        content_hash: r.get("content_hash"),
        reason: r.get("reason"),
        yanked_by: r.get("yanked_by"),
        yanked_at: r.get("yanked_at"),
    }
}

fn defaults_from_row(r: &PgRow) -> Result<OrgLibraryDefaults, LibraryStoreError> {
    let org: String = r.get("org");
    let state: String = r.get("state");
    let state = DefaultsState::parse(&state).ok_or_else(|| {
        LibraryStoreError::Store(format!("org {org}: unreadable defaults state `{state}`"))
    })?;
    let missing: Vec<MissingDefault> = serde_json::from_value(r.get::<Value, _>("missing"))
        .map_err(|e| LibraryStoreError::Store(format!("org {org}: unreadable missing: {e}")))?;
    Ok(OrgLibraryDefaults {
        org,
        state,
        missing,
        mirror: r.get("mirror"),
        updated_at: r.get("updated_at"),
    })
}

#[async_trait]
impl LibraryStore for PostgresDb {
    async fn put_version(&self, v: &LibraryVersion) -> Result<LibraryVersion, LibraryStoreError> {
        sqlx::query(&format!(
            "INSERT INTO library_versions ({VERSION_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (org, content_hash) DO NOTHING"
        ))
        .bind(&v.org)
        .bind(&v.content_hash)
        .bind(&v.content)
        .bind(&v.source)
        .bind(&v.publisher)
        .bind(&v.name)
        .bind(v.first_party)
        .bind(&v.fetched_by)
        .bind(v.fetched_at)
        .execute(self.pool())
        .await
        .map_err(|e| match sqlstate(&e).as_deref() {
            Some("23514") => LibraryStoreError::Invalid(e.to_string()),
            _ => store_err(e),
        })?;
        self.version(&v.org, &v.content_hash)
            .await?
            .ok_or_else(|| LibraryStoreError::Store("a stored version vanished".into()))
    }

    async fn version(
        &self,
        org: &str,
        content_hash: &str,
    ) -> Result<Option<LibraryVersion>, LibraryStoreError> {
        let row = sqlx::query(&format!(
            "SELECT {VERSION_COLUMNS} FROM library_versions WHERE org = $1 AND content_hash = $2"
        ))
        .bind(org)
        .bind(content_hash)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        Ok(row.as_ref().map(version_from_row))
    }

    async fn install(
        &self,
        new: &NewInstallation,
        replaces: &Replaces,
    ) -> Result<LibraryInstallation, LibraryStoreError> {
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM library_versions WHERE org = $1 AND content_hash = $2)",
        )
        .bind(&new.org)
        .bind(&new.content_hash)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_err)?;
        if !known {
            return Err(LibraryStoreError::UnknownVersion(new.content_hash.clone()));
        }
        // The write itself checks `replaces`, so two admins racing cannot
        // both pass a read and then overwrite each other: a first install
        // only inserts (a row another install committed is a unique
        // violation), and an upgrade only updates the row that still holds
        // the version it names.
        let map_write = |e: sqlx::Error| match sqlstate(&e).as_deref() {
            Some(UNIQUE_VIOLATION) => LibraryStoreError::Conflict,
            Some(FOREIGN_KEY_VIOLATION) => {
                LibraryStoreError::UnknownVersion(new.content_hash.clone())
            }
            _ => store_err(e),
        };
        let write = match replaces {
            Replaces::Nothing => sqlx::query(
                "INSERT INTO library_installations \
                 (org, publisher, name, content_hash, installed_by, installed_at, step_up_ref) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            ),
            Replaces::Version(_) => sqlx::query(
                "UPDATE library_installations SET \
                   content_hash = $4, installed_by = $5, installed_at = $6, step_up_ref = $7 \
                 WHERE org = $1 AND publisher = $2 AND name = $3 AND content_hash = $8",
            ),
        }
        .bind(&new.org)
        .bind(&new.publisher)
        .bind(&new.name)
        .bind(&new.content_hash)
        .bind(&new.installed_by)
        .bind(new.installed_at)
        .bind(&new.step_up_ref);
        let write = match replaces {
            Replaces::Nothing => write,
            Replaces::Version(h) => write.bind(h),
        };
        let written = write
            .execute(&mut *tx)
            .await
            .map_err(map_write)?
            .rows_affected();
        if written == 0 {
            return Err(LibraryStoreError::Conflict);
        }
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.publisher = $2 AND i.name = $3"
        ))
        .bind(&new.org)
        .bind(&new.publisher)
        .bind(&new.name)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_err)?;
        tx.commit()
            .await
            .map_err(|e| match sqlstate(&e).as_deref() {
                Some(UNIQUE_VIOLATION) => LibraryStoreError::Conflict,
                _ => store_err(e),
            })?;
        Ok(installation_from_row(&row))
    }

    async fn installation(
        &self,
        org: &str,
        publisher: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.publisher = $2 AND i.name = $3"
        ))
        .bind(org)
        .bind(publisher)
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        Ok(row.as_ref().map(installation_from_row))
    }

    async fn installations(
        &self,
        org: &str,
    ) -> Result<Vec<LibraryInstallation>, LibraryStoreError> {
        let rows = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 \
             ORDER BY i.publisher COLLATE \"C\", i.name COLLATE \"C\""
        ))
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        Ok(rows.iter().map(installation_from_row).collect())
    }

    async fn installed_for_compile(
        &self,
        org: &str,
    ) -> Result<BTreeMap<String, InstalledLibrary>, LibraryStoreError> {
        let rows = sqlx::query(
            "SELECT i.publisher, i.name, v.content, v.source \
             FROM library_installations i \
             JOIN library_versions v ON v.org = i.org AND v.content_hash = i.content_hash \
             WHERE i.org = $1",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    format!(
                        "{}/{}",
                        r.get::<String, _>("publisher"),
                        r.get::<String, _>("name")
                    ),
                    InstalledLibrary {
                        content: r.get("content"),
                        source: r.get("source"),
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
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.publisher = $2 AND i.name = $3 \
             FOR UPDATE OF i"
        ))
        .bind(org)
        .bind(publisher)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        sqlx::query(
            "DELETE FROM library_installations WHERE org = $1 AND publisher = $2 AND name = $3",
        )
        .bind(org)
        .bind(publisher)
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;
        tx.commit().await.map_err(store_err)?;
        Ok(Some(installation_from_row(&row)))
    }

    async fn yank(&self, yank: &LibraryYank) -> Result<LibraryYank, LibraryStoreError> {
        sqlx::query(&format!(
            "INSERT INTO library_yanks ({YANK_COLUMNS}) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (org, content_hash) DO NOTHING"
        ))
        .bind(&yank.org)
        .bind(&yank.content_hash)
        .bind(&yank.reason)
        .bind(&yank.yanked_by)
        .bind(yank.yanked_at)
        .execute(self.pool())
        .await
        .map_err(|e| match sqlstate(&e).as_deref() {
            Some(FOREIGN_KEY_VIOLATION) => {
                LibraryStoreError::UnknownVersion(yank.content_hash.clone())
            }
            _ => store_err(e),
        })?;
        let row = sqlx::query(&format!(
            "SELECT {YANK_COLUMNS} FROM library_yanks WHERE org = $1 AND content_hash = $2"
        ))
        .bind(&yank.org)
        .bind(&yank.content_hash)
        .fetch_one(self.pool())
        .await
        .map_err(store_err)?;
        Ok(yank_from_row(&row))
    }

    async fn unyank(
        &self,
        org: &str,
        content_hash: &str,
    ) -> Result<Option<LibraryYank>, LibraryStoreError> {
        let row = sqlx::query(&format!(
            "DELETE FROM library_yanks WHERE org = $1 AND content_hash = $2 \
             RETURNING {YANK_COLUMNS}"
        ))
        .bind(org)
        .bind(content_hash)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        Ok(row.as_ref().map(yank_from_row))
    }

    async fn yanks(&self, org: &str) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {YANK_COLUMNS} FROM library_yanks WHERE org = $1 \
             ORDER BY yanked_at DESC, content_hash COLLATE \"C\""
        ))
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        Ok(rows.iter().map(yank_from_row).collect())
    }

    async fn yanked(
        &self,
        org: &str,
        content_hashes: &[String],
    ) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        if content_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "SELECT {YANK_COLUMNS} FROM library_yanks \
             WHERE org = $1 AND content_hash = ANY($2) ORDER BY content_hash COLLATE \"C\""
        ))
        .bind(org)
        .bind(content_hashes)
        .fetch_all(self.pool())
        .await
        .map_err(store_err)?;
        Ok(rows.iter().map(yank_from_row).collect())
    }

    async fn claim_defaults(&self, org: &str, at: i64) -> Result<bool, LibraryStoreError> {
        let made = sqlx::query(
            "INSERT INTO library_org_defaults (org, state, updated_at) VALUES ($1, 'pending', $2) \
             ON CONFLICT (org) DO NOTHING",
        )
        .bind(org)
        .bind(at)
        .execute(self.pool())
        .await
        .map_err(store_err)?
        .rows_affected();
        Ok(made == 1)
    }

    async fn defaults(&self, org: &str) -> Result<Option<OrgLibraryDefaults>, LibraryStoreError> {
        let row = sqlx::query(
            "SELECT org, state, missing, mirror, updated_at FROM library_org_defaults \
             WHERE org = $1",
        )
        .bind(org)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        row.as_ref().map(defaults_from_row).transpose()
    }

    async fn set_defaults(&self, d: &OrgLibraryDefaults) -> Result<(), LibraryStoreError> {
        let missing = serde_json::to_value(&d.missing)
            .map_err(|e| LibraryStoreError::Invalid(e.to_string()))?;
        sqlx::query(
            "INSERT INTO library_org_defaults (org, state, missing, mirror, updated_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (org) DO UPDATE SET state = EXCLUDED.state, \
               missing = EXCLUDED.missing, mirror = EXCLUDED.mirror, \
               updated_at = EXCLUDED.updated_at",
        )
        .bind(&d.org)
        .bind(d.state.as_str())
        .bind(&missing)
        .bind(&d.mirror)
        .bind(d.updated_at)
        .execute(self.pool())
        .await
        .map_err(store_err)?;
        Ok(())
    }
}
