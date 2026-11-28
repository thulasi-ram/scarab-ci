//! [`OAuthStore`] on Postgres (ADR-0086 §7, migration 0113): client metadata
//! hosts, authorization codes and grants.

use async_trait::async_trait;
use sqlx::Row;

use scarab_identity::{
    IdentityError, OAuthAuthorizationCode, OAuthClientMetadataHost, OAuthGrant, OAuthStore,
    PrincipalToken,
};

use crate::{identity_err, role_from_str, role_str, scope_cols, PostgresDb};

const CODE_COLUMNS: &str = "org, installation_id, resource, client_id, client_host, \
     redirect_uri, code_challenge, principal_subject, operations, role, created_at, expires_at, \
     grant_id";

const GRANT_COLUMNS: &str = "id, org, installation_id, resource, client_id, client_host, \
     principal_subject, operations, role, created_at, last_used_at, expires_at, generation, \
     rotated_at, access_token_id, retried, revoked_at, revoked_reason";

/// Revoke, inside `tx`, every live access token minted for one of
/// `grant_ids`.
async fn revoke_grant_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    grant_ids: &[String],
    now_ms: i64,
) -> Result<(), IdentityError> {
    if grant_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE api_tokens SET revoked_at = $2
          WHERE oauth_grant_id = ANY($1) AND revoked_at IS NULL",
    )
    .bind(grant_ids)
    .bind(now_ms)
    .execute(&mut **tx)
    .await
    .map_err(identity_err)?;
    Ok(())
}

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
        grant_id: r.get("grant_id"),
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
        retried: r.get("retried"),
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
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        let done =
            sqlx::query("DELETE FROM oauth_client_metadata_hosts WHERE org = $1 AND host = $2")
                .bind(org)
                .bind(host)
                .execute(&mut *tx)
                .await
                .map_err(identity_err)?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        let revoked: Vec<String> = sqlx::query_scalar(
            "UPDATE oauth_grants SET revoked_at = $3, revoked_reason = 'client metadata host removed'
              WHERE org = $1 AND client_host = $2 AND revoked_at IS NULL
              RETURNING id",
        )
        .bind(org)
        .bind(host)
        .bind(now_ms)
        .fetch_all(&mut *tx)
        .await
        .map_err(identity_err)?;
        revoke_grant_tokens(&mut tx, &revoked, now_ms).await?;
        tx.commit().await.map_err(identity_err)?;
        Ok(true)
    }

    async fn put_authorization_code(
        &self,
        code: &OAuthAuthorizationCode,
        code_hash: &str,
    ) -> Result<(), IdentityError> {
        sqlx::query(&format!(
            "INSERT INTO oauth_authorization_codes (code_hash, {CODE_COLUMNS})
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"
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
        .bind(&code.grant_id)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn consume_authorization_code(
        &self,
        code_hash: &str,
        client_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
        now_ms: i64,
    ) -> Result<Option<OAuthAuthorizationCode>, IdentityError> {
        // One statement: of two redemptions of one code, exactly one sees
        // `consumed_at IS NULL`; a wrong client, redirect or verifier
        // matches no row and spends nothing.
        let row = sqlx::query(&format!(
            "UPDATE oauth_authorization_codes SET consumed_at = $2
              WHERE code_hash = $1 AND client_id = $3 AND redirect_uri = $4
                AND code_challenge = $5 AND consumed_at IS NULL AND expires_at > $2
              RETURNING {CODE_COLUMNS}"
        ))
        .bind(code_hash)
        .bind(now_ms)
        .bind(client_id)
        .bind(redirect_uri)
        .bind(code_challenge)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        row.as_ref().map(code_from_row).transpose()
    }

    async fn pending_authorization_code(
        &self,
        code_hash: &str,
        client_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
        now_ms: i64,
    ) -> Result<Option<OAuthAuthorizationCode>, IdentityError> {
        let row = sqlx::query(&format!(
            "SELECT {CODE_COLUMNS} FROM oauth_authorization_codes
              WHERE code_hash = $1 AND client_id = $3 AND redirect_uri = $4
                AND code_challenge = $5 AND consumed_at IS NULL AND expires_at > $2"
        ))
        .bind(code_hash)
        .bind(now_ms)
        .bind(client_id)
        .bind(redirect_uri)
        .bind(code_challenge)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        row.as_ref().map(code_from_row).transpose()
    }

    async fn replayed_authorization_code(
        &self,
        code_hash: &str,
        client_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
    ) -> Result<Option<String>, IdentityError> {
        sqlx::query_scalar(
            "SELECT grant_id FROM oauth_authorization_codes
              WHERE code_hash = $1 AND client_id = $2 AND redirect_uri = $3
                AND code_challenge = $4 AND consumed_at IS NOT NULL",
        )
        .bind(code_hash)
        .bind(client_id)
        .bind(redirect_uri)
        .bind(code_challenge)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18)"
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
        .bind(grant.retried)
        .bind(grant.revoked_at)
        .bind(&grant.revoked_reason)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn put_oauth_access_token(
        &self,
        token: &PrincipalToken,
        stored_hash: &str,
        grant_id: &str,
    ) -> Result<(), IdentityError> {
        // The columns `ApiTokenStore::put` writes, and the grant's id.
        let (org, project) = scope_cols(&token.scope);
        sqlx::query(
            "INSERT INTO api_tokens
                 (id, token_hash, name, owner_subject, scope_org, scope_project,
                  role, expires_at, created_by, created_at,
                  kind, operations, audience, oauth_client, oauth_grant_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(&token.id)
        .bind(stored_hash)
        .bind(&token.name)
        .bind(&token.owner_subject)
        .bind(org)
        .bind(project)
        .bind(role_str(token.role))
        .bind(token.expires_at)
        .bind(&token.created_by)
        .bind(token.created_at)
        .bind(token.kind.as_str())
        .bind(&token.operations)
        .bind(&token.audience)
        .bind(&token.oauth_client)
        .bind(grant_id)
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
        previous_access_token_id: Option<&str>,
        access_token_id: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        let done = sqlx::query(
            "UPDATE oauth_grants
                SET generation = generation + 1, rotated_at = $3, last_used_at = $3,
                    access_token_id = $4, retried = false
              WHERE id = $1 AND generation = $2 AND revoked_at IS NULL
                AND access_token_id IS NOT DISTINCT FROM $5",
        )
        .bind(id)
        .bind(from_generation)
        .bind(now_ms)
        .bind(access_token_id)
        .bind(previous_access_token_id)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn replace_oauth_grant_access_token(
        &self,
        id: &str,
        generation: i64,
        previous_access_token_id: Option<&str>,
        access_token_id: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        let done = sqlx::query(
            "UPDATE oauth_grants SET access_token_id = $3, last_used_at = $4, retried = true
              WHERE id = $1 AND generation = $2 AND revoked_at IS NULL AND NOT retried
                AND access_token_id IS NOT DISTINCT FROM $5",
        )
        .bind(id)
        .bind(generation)
        .bind(access_token_id)
        .bind(now_ms)
        .bind(previous_access_token_id)
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
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        let row = sqlx::query(&format!(
            "UPDATE oauth_grants SET revoked_at = $2, revoked_reason = $3
              WHERE id = $1 AND revoked_at IS NULL
              RETURNING {GRANT_COLUMNS}"
        ))
        .bind(id)
        .bind(now_ms)
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await
        .map_err(identity_err)?;
        let grant = row.as_ref().map(grant_from_row).transpose()?;
        if grant.is_some() {
            revoke_grant_tokens(&mut tx, &[id.to_string()], now_ms).await?;
        }
        tx.commit().await.map_err(identity_err)?;
        Ok(grant)
    }

    async fn revoke_installation_oauth_grants(
        &self,
        installation_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Vec<OAuthGrant>, IdentityError> {
        let mut tx = self.pool().begin().await.map_err(identity_err)?;
        let rows = sqlx::query(&format!(
            "UPDATE oauth_grants SET revoked_at = $2, revoked_reason = $3
              WHERE installation_id = $1 AND revoked_at IS NULL
              RETURNING {GRANT_COLUMNS}"
        ))
        .bind(installation_id)
        .bind(now_ms)
        .bind(reason)
        .fetch_all(&mut *tx)
        .await
        .map_err(identity_err)?;
        let grants: Vec<OAuthGrant> = rows.iter().map(grant_from_row).collect::<Result<_, _>>()?;
        let ids: Vec<String> = grants.iter().map(|g| g.id.clone()).collect();
        revoke_grant_tokens(&mut tx, &ids, now_ms).await?;
        tx.commit().await.map_err(identity_err)?;
        Ok(grants)
    }
}
