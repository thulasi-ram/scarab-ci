//! [`LibraryStore`] on Postgres (ADR-0076 §4, §6; migrations 0071, 0072,
//! 0090, 0096, 0097).
//!
//! An install takes the installation row `FOR UPDATE` (or finds none),
//! compares it with what the caller expects to replace, and writes in the
//! same transaction. A first install racing another first install meets the
//! primary key; that is [`LibraryStoreError::Conflict`] as well. Lists order
//! by `COLLATE "C"`, bytewise, as the in-memory store does.
//!
//! A first install takes its id from the column's default (migration 0092).
//! The insert names the id index as its conflict target, so an id that is
//! already taken inserts nothing and the insert runs again with a new one;
//! another installation under the same name is still a unique violation.
//!
//! An install that carries its source's trust grants reads them in its own
//! transaction under an advisory lock on (org, source) that every trust and
//! untrust of the source takes too, so a grant made while it runs is either
//! seen by it or made after it commits (git-bug 9c54b46).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use sqlx::postgres::PgRow;
use sqlx::Row;

use scarab_pipeline::InstalledLibrary;
use scarab_policy::library::{
    pinned_commit, trust_key, LibraryInstallation, LibraryStore, LibraryStoreError, LibraryVersion,
    LibraryYank, NewInstallation, Replaces, TrustGrant, TrustedSource,
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

const VERSION_COLUMNS: &str = "org, content_hash, content, source, name, fetched_by, fetched_at, \
     requested_ref, resolved_commit";

/// The commit in `source` when `resolved_commit` is NULL: a row written by a
/// binary from before migration 0097.
fn commit_or_source(resolved_commit: Option<String>, source: &str) -> String {
    resolved_commit.unwrap_or_else(|| pinned_commit(source).to_string())
}

fn version_from_row(r: &PgRow) -> LibraryVersion {
    let source: String = r.get("source");
    LibraryVersion {
        org: r.get("org"),
        content_hash: r.get("content_hash"),
        content: r.get("content"),
        name: r.get("name"),
        fetched_by: r.get("fetched_by"),
        fetched_at: r.get("fetched_at"),
        requested_ref: r.get("requested_ref"),
        resolved_commit: commit_or_source(r.get("resolved_commit"), &source),
        source,
    }
}

/// An installation joined with its version. Its ref and commit are its own,
/// copied at install; an install made by a binary from before migration
/// 0097 has neither, and reads the commit in its version's source.
const INSTALLATION_SELECT: &str = "SELECT i.id, i.org, i.name, i.content_hash, \
     v.source, i.installed_by, i.installed_at, i.step_up_ref, \
     i.requested_ref, i.resolved_commit \
     FROM library_installations i \
     JOIN library_versions v ON v.org = i.org AND v.content_hash = i.content_hash";

fn installation_from_row(r: &PgRow) -> LibraryInstallation {
    let source: String = r.get("source");
    LibraryInstallation {
        id: r.get("id"),
        org: r.get("org"),
        name: r.get("name"),
        content_hash: r.get("content_hash"),
        installed_by: r.get("installed_by"),
        installed_at: r.get("installed_at"),
        step_up_ref: r.get("step_up_ref"),
        requested_ref: r.get("requested_ref"),
        resolved_commit: commit_or_source(r.get("resolved_commit"), &source),
        source,
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

#[async_trait]
impl LibraryStore for PostgresDb {
    async fn put_version(&self, v: &LibraryVersion) -> Result<LibraryVersion, LibraryStoreError> {
        // A fetch of stored content keeps the first fetch's record; one from
        // the same file records its own ref and commit. `$10` is the trust
        // key of the fetched source: the stored source is that and `@` and
        // a commit.
        sqlx::query(&format!(
            "INSERT INTO library_versions ({VERSION_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (org, content_hash) DO UPDATE \
               SET requested_ref = EXCLUDED.requested_ref, \
                   resolved_commit = EXCLUDED.resolved_commit \
               WHERE left(library_versions.source, length($10::text) + 1) = $10::text || '@' \
                 AND strpos(substr(library_versions.source, length($10::text) + 2), '/') = 0"
        ))
        .bind(&v.org)
        .bind(&v.content_hash)
        .bind(&v.content)
        .bind(&v.source)
        .bind(&v.name)
        .bind(&v.fetched_by)
        .bind(v.fetched_at)
        .bind(&v.requested_ref)
        .bind(v.commit())
        .bind(trust_key(&v.source))
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
        self.install_tx(new, replaces, None).await
    }

    async fn install_carrying(
        &self,
        new: &NewInstallation,
        replaces: &Replaces,
        carry_grants: &BTreeSet<TrustGrant>,
    ) -> Result<LibraryInstallation, LibraryStoreError> {
        self.install_tx(new, replaces, Some(carry_grants)).await
    }

    async fn installation(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.name = $2"
        ))
        .bind(org)
        .bind(name)
        .fetch_optional(self.pool())
        .await
        .map_err(store_err)?;
        Ok(row.as_ref().map(installation_from_row))
    }

    async fn installation_by_id(
        &self,
        org: &str,
        id: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.id = $2"
        ))
        .bind(org)
        .bind(id)
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
             ORDER BY i.name COLLATE \"C\""
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
            "SELECT i.name, v.content, v.source \
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
                    r.get::<String, _>("name"),
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
        id: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.id = $2 FOR UPDATE OF i"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        sqlx::query("DELETE FROM library_installations WHERE org = $1 AND id = $2")
            .bind(org)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;
        tx.commit().await.map_err(store_err)?;
        Ok(Some(installation_from_row(&row)))
    }

    async fn prune_never_installed(
        &self,
        org: &str,
        keep: usize,
        except: &str,
    ) -> Result<u64, LibraryStoreError> {
        // An install racing this marks its version `installed_once` before
        // it commits; the DELETE waits on that row and re-checks it, so it
        // skips the version (READ COMMITTED re-evaluates a locked row). The
        // installations check is a second guard: a version an installation
        // points at is never deleted, whatever `installed_once` says.
        let deleted = sqlx::query(
            "DELETE FROM library_versions v \
             WHERE v.org = $1 AND NOT v.installed_once AND v.content_hash <> $3 \
               AND NOT EXISTS (SELECT 1 FROM library_yanks y \
                               WHERE y.org = v.org AND y.content_hash = v.content_hash) \
               AND NOT EXISTS (SELECT 1 FROM library_installations i \
                               WHERE i.org = v.org AND i.content_hash = v.content_hash) \
               AND v.content_hash NOT IN ( \
                 SELECT k.content_hash FROM library_versions k \
                 WHERE k.org = $1 AND NOT k.installed_once AND k.content_hash <> $3 \
                   AND NOT EXISTS (SELECT 1 FROM library_yanks y \
                                   WHERE y.org = k.org AND y.content_hash = k.content_hash) \
                   AND NOT EXISTS (SELECT 1 FROM library_installations i \
                                   WHERE i.org = k.org AND i.content_hash = k.content_hash) \
                 ORDER BY k.fetched_at DESC, k.content_hash DESC LIMIT $2)",
        )
        .bind(org)
        .bind(i64::try_from(keep).unwrap_or(i64::MAX))
        .bind(except)
        .execute(self.pool())
        .await
        .map_err(|e| match sqlstate(&e).as_deref() {
            // A yank or an install committed between the read and the
            // delete: nothing is lost by pruning on the next fetch instead.
            Some(FOREIGN_KEY_VIOLATION) => LibraryStoreError::Conflict,
            _ => store_err(e),
        })?;
        Ok(deleted.rows_affected())
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

    async fn trusted_sources(&self, org: &str) -> Result<Vec<TrustedSource>, LibraryStoreError> {
        trusted_rows(self.pool(), org, None).await
    }

    async fn trust_source(&self, t: &TrustedSource) -> Result<TrustedSource, LibraryStoreError> {
        // Under the source's trust lock: an install carrying its grants
        // reads them and installs in one transaction (git-bug 9c54b46).
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        lock_trust(&mut tx, &t.org, &t.source).await?;
        let row = match t.grant {
            TrustGrant::ForgeCredentials => {
                sqlx::query(
                    "INSERT INTO library_trusted_sources \
                       (org, source, trusted_by, trusted_at, step_up_ref) \
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT (org, source) DO NOTHING",
                )
                .bind(&t.org)
                .bind(&t.source)
                .bind(&t.trusted_by)
                .bind(t.trusted_at)
                .bind(&t.step_up_ref)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
                sqlx::query(
                    "SELECT org, source, 'forge_credentials' AS trust_grant, trusted_by, \
                            trusted_at, step_up_ref \
                     FROM library_trusted_sources WHERE org = $1 AND source = $2",
                )
                .bind(&t.org)
                .bind(&t.source)
                .fetch_one(&mut *tx)
                .await
                .map_err(store_err)?
            }
            grant => {
                sqlx::query(
                    "INSERT INTO library_source_grants \
                       (org, source, trust_grant, trusted_by, trusted_at, step_up_ref) \
                     VALUES ($1, $2, $3, $4, $5, $6) \
                     ON CONFLICT (org, source, trust_grant) DO NOTHING",
                )
                .bind(&t.org)
                .bind(&t.source)
                .bind(grant.as_str())
                .bind(&t.trusted_by)
                .bind(t.trusted_at)
                .bind(&t.step_up_ref)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
                sqlx::query(
                    "SELECT org, source, trust_grant, trusted_by, trusted_at, step_up_ref \
                     FROM library_source_grants \
                     WHERE org = $1 AND source = $2 AND trust_grant = $3",
                )
                .bind(&t.org)
                .bind(&t.source)
                .bind(grant.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_err)?
            }
        };
        tx.commit().await.map_err(store_err)?;
        trusted_from_row(&row)
    }

    async fn untrust_source(
        &self,
        org: &str,
        source: &str,
        grant: TrustGrant,
    ) -> Result<Option<TrustedSource>, LibraryStoreError> {
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        lock_trust(&mut tx, org, source).await?;
        let row = match grant {
            TrustGrant::ForgeCredentials => sqlx::query(
                "DELETE FROM library_trusted_sources WHERE org = $1 AND source = $2 \
                 RETURNING org, source, 'forge_credentials' AS trust_grant, trusted_by, \
                           trusted_at, step_up_ref",
            )
            .bind(org)
            .bind(source),
            grant => sqlx::query(
                "DELETE FROM library_source_grants \
                 WHERE org = $1 AND source = $2 AND trust_grant = $3 \
                 RETURNING org, source, trust_grant, trusted_by, trusted_at, step_up_ref",
            )
            .bind(org)
            .bind(source)
            .bind(grant.as_str()),
        }
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_err)?;
        tx.commit().await.map_err(store_err)?;
        row.as_ref().map(trusted_from_row).transpose()
    }
}

/// Holds the transaction's lock on `org`'s trust in `source` (its trust
/// key) until it ends: an install carrying the source's grants and a trust
/// or untrust of it take turns. An advisory lock, because a new grant is a
/// new row that no row lock covers.
async fn lock_trust(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: &str,
    source: &str,
) -> Result<(), LibraryStoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("scarab library trust\n{org}\n{source}"))
        .execute(&mut **tx)
        .await
        .map_err(store_err)?;
    Ok(())
}

/// The sources `org` trusts, each grant a row, by source then grant; with
/// `source`, only that source's.
async fn trusted_rows<'e, E>(
    exec: E,
    org: &str,
    source: Option<&str>,
) -> Result<Vec<TrustedSource>, LibraryStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    // Trust with forge credentials is a row of `library_trusted_sources`;
    // every other grant is a row of `library_source_grants` (migration
    // 0095).
    let rows = sqlx::query(
        // Postgres refuses an expression (COLLATE) in a UNION's own
        // ORDER BY, so the union is ordered from outside.
        "SELECT * FROM ( \
            SELECT org, source, 'forge_credentials' AS trust_grant, trusted_by, trusted_at, \
                   step_up_ref \
            FROM library_trusted_sources WHERE org = $1 \
            UNION ALL \
            SELECT org, source, trust_grant, trusted_by, trusted_at, step_up_ref \
            FROM library_source_grants WHERE org = $1 \
         ) t \
         WHERE $2::text IS NULL OR source = $2 \
         ORDER BY source COLLATE \"C\", trust_grant COLLATE \"C\"",
    )
    .bind(org)
    .bind(source)
    .fetch_all(exec)
    .await
    .map_err(store_err)?;
    rows.iter().map(trusted_from_row).collect()
}

impl PostgresDb {
    /// [`LibraryStore::install`], and with `carry`
    /// [`LibraryStore::install_carrying`]: the source's grants are read in
    /// this transaction under [`lock_trust`], which a trust or untrust of
    /// the source takes too.
    async fn install_tx(
        &self,
        new: &NewInstallation,
        replaces: &Replaces,
        carry: Option<&BTreeSet<TrustGrant>>,
    ) -> Result<LibraryInstallation, LibraryStoreError> {
        let mut tx = self.pool().begin().await.map_err(store_err)?;
        let source: Option<String> = sqlx::query_scalar(
            "SELECT source FROM library_versions WHERE org = $1 AND content_hash = $2",
        )
        .bind(&new.org)
        .bind(&new.content_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_err)?;
        let Some(source) = source else {
            return Err(LibraryStoreError::UnknownVersion(new.content_hash.clone()));
        };
        if let Some(carry) = carry {
            let key = trust_key(&source);
            lock_trust(&mut tx, &new.org, key).await?;
            let held: BTreeSet<TrustGrant> = trusted_rows(&mut *tx, &new.org, Some(key))
                .await?
                .into_iter()
                .map(|t| t.grant)
                .collect();
            if &held != carry {
                return Err(LibraryStoreError::CarriedGrants(held));
            }
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
        let written = match replaces {
            Replaces::Nothing => {
                // 40 random bits collide about never; when they do, the
                // default mints another on the next try.
                let mut written = 0;
                for _ in 0..3 {
                    written = sqlx::query(
                        "INSERT INTO library_installations \
                         (org, name, content_hash, installed_by, installed_at, step_up_ref, \
                          requested_ref, resolved_commit) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                         ON CONFLICT (id) DO NOTHING",
                    )
                    .bind(&new.org)
                    .bind(&new.name)
                    .bind(&new.content_hash)
                    .bind(&new.installed_by)
                    .bind(new.installed_at)
                    .bind(&new.step_up_ref)
                    .bind(&new.requested_ref)
                    .bind(&new.resolved_commit)
                    .execute(&mut *tx)
                    .await
                    .map_err(map_write)?
                    .rows_affected();
                    if written == 1 {
                        break;
                    }
                }
                if written == 0 {
                    return Err(LibraryStoreError::Store(
                        "no free installation id after 3 tries".into(),
                    ));
                }
                written
            }
            Replaces::Version(h) => sqlx::query(
                "UPDATE library_installations SET \
                   content_hash = $3, installed_by = $4, installed_at = $5, step_up_ref = $6, \
                   requested_ref = $8, resolved_commit = $9 \
                 WHERE org = $1 AND name = $2 AND content_hash = $7",
            )
            .bind(&new.org)
            .bind(&new.name)
            .bind(&new.content_hash)
            .bind(&new.installed_by)
            .bind(new.installed_at)
            .bind(&new.step_up_ref)
            .bind(h)
            .bind(&new.requested_ref)
            .bind(&new.resolved_commit)
            .execute(&mut *tx)
            .await
            .map_err(map_write)?
            .rows_affected(),
        };
        if written == 0 {
            return Err(LibraryStoreError::Conflict);
        }
        sqlx::query(
            "UPDATE library_versions SET installed_once = true \
             WHERE org = $1 AND content_hash = $2 AND NOT installed_once",
        )
        .bind(&new.org)
        .bind(&new.content_hash)
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;
        let row = sqlx::query(&format!(
            "{INSTALLATION_SELECT} WHERE i.org = $1 AND i.name = $2"
        ))
        .bind(&new.org)
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
}

fn trusted_from_row(r: &PgRow) -> Result<TrustedSource, LibraryStoreError> {
    let grant: String = r.get("trust_grant");
    Ok(TrustedSource {
        org: r.get("org"),
        source: r.get("source"),
        grant: TrustGrant::parse(&grant).ok_or_else(|| {
            LibraryStoreError::Store(format!("unknown library trust grant `{grant}`"))
        })?,
        trusted_by: r.get("trusted_by"),
        trusted_at: r.get("trusted_at"),
        step_up_ref: r.get("step_up_ref"),
    })
}
