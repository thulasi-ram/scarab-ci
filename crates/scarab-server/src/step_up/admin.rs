//! Passkey resets (ADR-0073 §10): the two ways a human who lost every
//! passkey gets back in. Either way every passkey of theirs is removed, every
//! standing step-up (window, unconsumed action confirmation) revoked, and the
//! reset recorded; their next enrollment is a "first passkey" again, so it
//! needs a fresh browser login.
//!
//! - **An Owner's reset** — `POST /v1/orgs/{org}/members/{subject}/passkeys/reset`,
//!   the always-ask crown jewel `passkey.reset`. The caller must be Owner of
//!   the Org, the target a member of it — and, because a passkey is not
//!   Org-scoped, the caller must be Owner of EVERY Org the target belongs to
//!   (or a bootstrap owner): an Owner of a small Org must not be able to
//!   strip the passkeys of an admin of a bigger one.
//! - **The operator's break-glass** — `scarab-server admin reset-passkeys
//!   --principal …`, against the database directly ([`reset_passkeys`]),
//!   for when no Owner can step up. No route reaches it, and the `scarab`
//!   CLI has no such command: it needs the database credentials.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::{Extension, Json};
use serde::Serialize;
use utoipa::ToSchema;

use scarab_db_postgres::PostgresDb;
use scarab_identity::{
    security_event_kind as kind, Action, PasskeyStore, Role, Scope, SecurityEventLog, StepUpStore,
};

use super::ceremony::service;
use super::{event, store_err, GuardCtx};
use crate::{authenticate, ApiError, AppState};

/// The first migration the break-glass needs (the OrgSecurityEventLog).
pub const BREAK_GLASS_MIN_SCHEMA: i64 = 62;

/// The `caller` an operator break-glass is recorded as.
pub const BREAK_GLASS_CALLER: &str = "operator:break-glass";

/// What a passkey reset removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PasskeyResetDto {
    /// The principal whose passkeys were reset.
    pub subject: String,
    /// How many passkeys were removed.
    pub removed: u64,
    /// How many standing step-up confirmations (windows, unconsumed action
    /// confirmations) were revoked.
    pub windows_revoked: u64,
}

/// Reset a member's passkeys (an Owner; always-ask).
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/members/{subject}/passkeys/reset",
    params(
        ("org" = String, Path, description = "org"),
        ("subject" = String, Path, description = "the member's principal id")
    ),
    summary = "Reset a member's passkeys (ADR-0073 §10; Owner, always-ask step-up)",
    responses(
        (status = 200, body = PasskeyResetDto),
        (status = 403, description = "`step_up_required` (mode `action`); not an Owner of the Org; or the member also belongs to an Org you do not own (the text names it)"),
        (status = 404, description = "not a member of the Org, or step-up is off")
    )
)]
pub(crate) async fn reset_member_passkeys(
    State(st): State<AppState>,
    ctx: Option<Extension<GuardCtx>>,
    headers: HeaderMap,
    Path((org, subject)): Path<(String, String)>,
) -> Result<Json<PasskeyResetDto>, ApiError> {
    let svc = service(&st)?;
    let caller = authenticate(&st, &headers, Action::Administer).await?;
    let org_scope = Scope::Org(org.clone());
    if caller.role_in(&st, &org_scope).await? != Some(Role::Owner) {
        return Err(ApiError::Forbidden);
    }
    let rbac = st.rbac.as_ref().ok_or(ApiError::NotFound)?;
    let orgs = rbac.orgs_of(&subject).await.map_err(store_err)?;
    if !orgs.iter().any(|o| o == &org) {
        return Err(ApiError::NotFound);
    }
    // Owner of every Org the target is in, or a bootstrap owner (a human —
    // never a token, whose authority is one scope).
    let bootstrap = caller.grant.is_none()
        && crate::token_owner_roles(&st, &caller.principal.subject)
            .await
            .contains(&Role::Owner);
    if !bootstrap {
        let mut not_owned = Vec::new();
        for o in &orgs {
            if caller.role_in(&st, &Scope::Org(o.clone())).await? != Some(Role::Owner) {
                not_owned.push(o.clone());
            }
        }
        if !not_owned.is_empty() {
            return Err(ApiError::Denied(format!(
                "`{subject}` also belongs to {}, which you do not own: a passkey is not \
                 Org-scoped, so only an Owner of every Org they belong to (or a bootstrap \
                 owner) can reset theirs",
                not_owned.join(", ")
            )));
        }
    }
    let removed = svc.passkeys.remove_all(&subject).await.map_err(store_err)?;
    let windows_revoked = svc
        .confirmations
        .delete_windows_for(&subject)
        .await
        .map_err(store_err)?;
    let now = st.clock.now().await.0;
    let mut ev = event(
        kind::PASSKEY_RESET,
        &subject,
        caller.event_label(),
        Some(org.clone()),
        now,
    );
    ev.op = Some("passkey.reset".into());
    ev.target = Some(format!("{org}/{subject}"));
    ev.confirmation_id = ctx.and_then(|Extension(c)| c.confirmation_id());
    ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({
        "reset_by": caller.principal.subject,
        "removed": removed,
        "windows_revoked": windows_revoked,
        "orgs": orgs,
    });
    svc.record(ev).await;
    Ok(Json(PasskeyResetDto {
        subject,
        removed,
        windows_revoked,
    }))
}

/// Who ran the break-glass, from the machine it ran on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorInfo {
    pub host: String,
    pub os_user: String,
}

impl OperatorInfo {
    /// The host name and OS user of this process (best-effort: `unknown`).
    pub fn current() -> Self {
        let host = std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|h| h.trim().to_string())
                    .filter(|h| !h.is_empty())
            })
            .or_else(|| {
                std::process::Command::new("uname")
                    .arg("-n")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|h| h.trim().to_string())
                    .filter(|h| !h.is_empty())
            })
            .unwrap_or_else(|| "unknown".into());
        let os_user = ["USER", "LOGNAME", "USERNAME"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
            .unwrap_or_else(|| "unknown".into());
        Self { host, os_user }
    }
}

/// The operator break-glass (ADR-0073 §10): remove every passkey of
/// `principal` (merged ids followed to the live Principal), revoke its
/// standing step-ups, and record `passkey.break_glass_reset` (Org-less,
/// caller `operator:break-glass`, with the host, OS user and reason).
///
/// Refuses a database whose schema predates the event log — it never
/// migrates one — and an id no Principal was ever minted for.
pub async fn reset_passkeys(
    db: &PostgresDb,
    principal: &str,
    reason: Option<&str>,
    operator: &OperatorInfo,
    now_ms: i64,
) -> Result<PasskeyResetDto, String> {
    let version = db
        .schema_version()
        .await
        .map_err(|e| format!("could not read the schema version: {e}"))?;
    match version {
        Some(v) if v >= BREAK_GLASS_MIN_SCHEMA => {}
        Some(v) => {
            return Err(format!(
                "the database schema is at migration {v}, older than {BREAK_GLASS_MIN_SCHEMA} \
                 (the security event log); upgrade the server first — this command never \
                 migrates"
            ))
        }
        None => {
            return Err("the database has never been migrated; this command never migrates".into())
        }
    }
    let subject = scarab_identity::resolve_principal(db, principal)
        .await
        .map_err(|e| format!("could not resolve `{principal}`: {e}"))?
        .ok_or_else(|| format!("no principal `{principal}` exists"))?;
    let removed = PasskeyStore::remove_all(db, &subject)
        .await
        .map_err(|e| format!("could not remove the passkeys of `{subject}`: {e}"))?;
    let windows_revoked = StepUpStore::delete_windows_for(db, &subject)
        .await
        .map_err(|e| format!("could not revoke the step-ups of `{subject}`: {e}"))?;
    let mut ev = event(
        kind::PASSKEY_BREAK_GLASS_RESET,
        &subject,
        BREAK_GLASS_CALLER.to_string(),
        None,
        now_ms,
    );
    ev.op = Some("passkey.break_glass_reset".into());
    ev.target = Some(subject.clone());
    ev.detail = serde_json::json!({
        "requested": principal,
        "reason": reason,
        "host": operator.host,
        "os_user": operator.os_user,
        "removed": removed,
        "windows_revoked": windows_revoked,
    });
    SecurityEventLog::append(db, &ev).await.map_err(|e| {
        format!(
            "removed {removed} passkey(s) of `{subject}`, but the reset could NOT be recorded \
             in the security event log: {e}"
        )
    })?;
    Ok(PasskeyResetDto {
        subject,
        removed,
        windows_revoked,
    })
}
