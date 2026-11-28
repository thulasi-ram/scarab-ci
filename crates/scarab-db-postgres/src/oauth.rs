//! [`OAuthStore`] on Postgres (ADR-0086 §7, migration 0113): client metadata
//! hosts, authorization codes and grants.

use async_trait::async_trait;
use sqlx::Row;

use scarab_identity::{
    IdentityError, OAuthAuthorizationCode, OAuthClientMetadataHost, OAuthGrant, OAuthStore,
};

use crate::{identity_err, role_from_str, role_str, PostgresDb};

const CODE_COLUMNS: &str = "org, installation_id, resource, client_id, client_host, \
     redirect_uri, code_challenge, principal_subject, operations, role, created_at, expires_at";

const GRANT_COLUMNS: &str = "id, org, installation_id, resource, client_id, client_host, \
     principal_subject, operations, role, created_at, last_used_at, expires_at, generation, \
     rotated_at, access_token_id, revoked_at, revoked_reason";

/// A role this binary does not know fails closed: the row reads as an error,
/// never as a lower or higher role.
fn stored_role(r: &sqlx::postgres::PgRow) -> Result<scarab_identity::Role, IdentityError> {
    let raw: String = r.get("role");
    role_from_str(&raw).ok_or_else(|| IdentityError::Issuance(format!("unreadable role {raw:?}")))
}

fn code_from_row(r: &sqlx::postgres::PgRow) -> Result<OAuthAuthorizationCode, IdentityError> {
    Ok(OAuthAuthorizationCode {
        org: r.get("org"),
        installation_id: r.get("installation_id"),
        resource: r.get("resource"),
        client_id: r.get("client_id"),
        client_host: r.get("client_host"),
        redirect_uri: r.get("redirect_uri"),
        code_challenge: r.get("code_challenge"),
        principal_subject: r.get("principal_subject"),
        operations: r.get::<Vec<String>, _>("operations"),
        role: stored_role(r)?,
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
    })
}

fn grant_from_row(r: &sqlx::postgres::PgRow) -> Result<OAuthGrant, IdentityError> {
    Ok(OAuthGrant {
        id: r.get("id"),
        org: r.get("org"),
        installation_id: r.get("installation_id"),
        resource: r.get("resource"),
        client_id: r.get("client_id"),
        client_host: r.get("client_host"),
        principal_subject: r.get("principal_subject"),
        operations: r.get::<Vec<String>, _>("operations"),
        role: stored_role(r)?,
        created_at: r.get("created_at"),
        last_used_at: r.get("last_used_at"),
        expires_at: r.get("expires_at"),
        generation: r.get("generation"),
        rotated_at: r.get("rotated_at"),
        access_token_id: r.get("access_token_id"),
        revoked_at: r.get("revoked_at"),
        revoked_reason: r.get("revoked_reason"),
    })
}

#[async_trait]
impl OAuthStore for PostgresDb {
    async fn client_metadata_hosts(
        &self,
        org: &str,
    ) -> Result<Vec<OAuthClientMetadataHost>, IdentityError> {
        let rows = sqlx::query(
            "SELECT org, host, added_by, added_at FROM oauth_client_metadata_hosts
              WHERE org = $1 ORDER BY host",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows
            .iter()
            .map(|r| OAuthClientMetadataHost {
                org: r.get("org"),
                host: r.get("host"),
                added_by: r.get("added_by"),
                added_at: r.get("added_at"),
            })
            .collect())
    }

    async fn put_client_metadata_host(
        &self,
        host: &OAuthClientMetadataHost,
    ) -> Result<(), IdentityError> {
        sqlx::query(
            "INSERT INTO oauth_client_metadata_hosts (org, host, added_by, added_at)
             VALUES ($1, $2, $3, $4) ON CONFLICT (org, host) DO NOTHING",
        )
        .bind(&host.org)
        .bind(&host.host)
        .bind(&host.added_by)
        .bind(host.added_at)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn delete_client_metadata_host(
        &self,
        org: &str,
        host: &str,
    ) -> Result<bool, IdentityError> {
        let done =
            sqlx::query("DELETE FROM oauth_client_metadata_hosts WHERE org = $1 AND host = $2")
                .bind(org)
                .bind(host)
                .execute(self.pool())
                .await
                .map_err(identity_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn put_authorization_code(
        &self,
        code: &OAuthAuthorizationCode,
        code_hash: &str,
    ) -> Result<(), IdentityError> {
        sqlx::query(&format!(
            "INSERT INTO oauth_authorization_codes (code_hash, {CODE_COLUMNS})
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"
        ))
        .bind(code_hash)
        .bind(&code.org)
        .bind(&code.installation_id)
        .bind(&code.resource)
        .bind(&code.client_id)
        .bind(&code.client_host)
        .bind(&code.redirect_uri)
        .bind(&code.code_challenge)
        .bind(&code.principal_subject)
        .bind(&code.operations)
        .bind(role_str(code.role))
        .bind(code.created_at)
        .bind(code.expires_at)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn consume_authorization_code(
        &self,
        code_hash: &str,
        now_ms: i64,
    ) -> Result<Option<OAuthAuthorizationCode>, IdentityError> {
        // One statement: of two redemptions of one code, exactly one sees
        // `consumed_at IS NULL`.
        let row = sqlx::query(&format!(
            "UPDATE oauth_authorization_codes SET consumed_at = $2
              WHERE code_hash = $1 AND consumed_at IS NULL AND expires_at > $2
              RETURNING {CODE_COLUMNS}"
        ))
        .bind(code_hash)
        .bind(now_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        row.as_ref().map(code_from_row).transpose()
    }

    async fn delete_expired_authorization_codes(&self, now_ms: i64) -> Result<u64, IdentityError> {
        let done = sqlx::query("DELETE FROM oauth_authorization_codes WHERE expires_at <= $1")
            .bind(now_ms)
            .execute(self.pool())
            .await
            .map_err(identity_err)?;
        Ok(done.rows_affected())
    }

    async fn put_oauth_grant(&self, grant: &OAuthGrant) -> Result<(), IdentityError> {
        sqlx::query(&format!(
            "INSERT INTO oauth_grants ({GRANT_COLUMNS})
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)"
        ))
        .bind(&grant.id)
        .bind(&grant.org)
        .bind(&grant.installation_id)
        .bind(&grant.resource)
        .bind(&grant.client_id)
        .bind(&grant.client_host)
        .bind(&grant.principal_subject)
        .bind(&grant.operations)
        .bind(role_str(grant.role))
        .bind(grant.created_at)
        .bind(grant.last_used_at)
        .bind(grant.expires_at)
        .bind(grant.generation)
        .bind(grant.rotated_at)
        .bind(&grant.access_token_id)
        .bind(grant.revoked_at)
        .bind(&grant.revoked_reason)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn oauth_grant(&self, id: &str) -> Result<Option<OAuthGrant>, IdentityError> {
        let row = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM oauth_grants WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        row.as_ref().map(grant_from_row).transpose()
    }

    async fn oauth_grants_of(&self, principal: &str) -> Result<Vec<OAuthGrant>, IdentityError> {
        let rows = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM oauth_grants WHERE principal_subject = $1
              ORDER BY created_at DESC, id"
        ))
        .bind(principal)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        rows.iter().map(grant_from_row).collect()
    }

    async fn rotate_oauth_grant(
        &self,
        id: &str,
        from_generation: i64,
        access_token_id: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        let done = sqlx::query(
            "UPDATE oauth_grants
                SET generation = generation + 1, rotated_at = $3, last_used_at = $3,
                    access_token_id = $4
              WHERE id = $1 AND generation = $2 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(from_generation)
        .bind(now_ms)
        .bind(access_token_id)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn replace_oauth_grant_access_token(
        &self,
        id: &str,
        generation: i64,
        access_token_id: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        let done = sqlx::query(
            "UPDATE oauth_grants SET access_token_id = $3, last_used_at = $4
              WHERE id = $1 AND generation = $2 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(generation)
        .bind(access_token_id)
        .bind(now_ms)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn revoke_oauth_grant(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Option<OAuthGrant>, IdentityError> {
        let row = sqlx::query(&format!(
            "UPDATE oauth_grants SET revoked_at = $2, revoked_reason = $3
              WHERE id = $1 AND revoked_at IS NULL
              RETURNING {GRANT_COLUMNS}"
        ))
        .bind(id)
        .bind(now_ms)
        .bind(reason)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        row.as_ref().map(grant_from_row).transpose()
    }

    async fn revoke_installation_oauth_grants(
        &self,
        installation_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Vec<OAuthGrant>, IdentityError> {
        let rows = sqlx::query(&format!(
            "UPDATE oauth_grants SET revoked_at = $2, revoked_reason = $3
              WHERE installation_id = $1 AND revoked_at IS NULL
              RETURNING {GRANT_COLUMNS}"
        ))
        .bind(installation_id)
        .bind(now_ms)
        .bind(reason)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        rows.iter().map(grant_from_row).collect()
    }
}
