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
//!
//! The operator grant ([`grant_role`], `scarab-server admin grant`) lives
//! here too: the same shape (database credentials only, never migrates,
//! recorded with host, OS user and reason), for seeding a fresh install's
//! first members without a browser.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::{Extension, Json};
use serde::Serialize;
use utoipa::ToSchema;

use scarab_db_postgres::{BindingRow, PostgresDb};
use scarab_identity::{
    security_event_kind as kind, Action, Binding, PasskeyStore, Role, Scope, SecurityEventLog,
    StepUpStore,
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
        (status = 403, description = "`step_up_required` (mode `action`); not an Owner of the Org; or the member also belongs to an Org you do not own (not named)"),
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
    // The live Principal the path names — a merged (folded) id followed to
    // its target, exactly as the break-glass resolves it — used for every
    // check and every removal below: bindings, tokens and passkeys live
    // under it, not under an alias.
    let subject = live_subject(&st, &subject).await?;
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
    // An install-wide owner's passkeys are reset only by a bootstrap owner:
    // binding them into a small Org must not hand that Org's Owner the
    // power to reopen their first-passkey path.
    if !bootstrap && is_install_owner(&st, &subject).await? {
        return Err(ApiError::Denied(format!(
            "`{subject}` is an install-wide owner: only a bootstrap owner can reset their \
             passkeys"
        )));
    }
    if !bootstrap {
        for o in &orgs {
            if caller.role_in(&st, &Scope::Org(o.clone())).await? != Some(Role::Owner) {
                // Which other Orgs the member is in is those Orgs' business:
                // the refusal does not name them.
                return Err(ApiError::Denied(format!(
                    "`{subject}` also belongs to an Org you do not own: a passkey is not \
                     Org-scoped, so only an Owner of every Org they belong to (or a bootstrap \
                     owner) can reset theirs"
                )));
            }
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
    // The event lands in this Org's log: it names no other Org the member
    // belongs to.
    ev.detail = serde_json::json!({
        "reset_by": caller.principal.subject,
        "removed": removed,
        "windows_revoked": windows_revoked,
    });
    svc.record(ev).await;
    Ok(Json(PasskeyResetDto {
        subject,
        removed,
        windows_revoked,
    }))
}

/// `subject` followed through merges to the live Principal
/// ([`scarab_identity::resolve_principal`]). An id no Principal was ever
/// minted for (a binding made before anyone signed in) is kept as is; with
/// no identity store there is nothing to follow. Fails closed: a store
/// error refuses.
async fn live_subject(st: &AppState, subject: &str) -> Result<String, ApiError> {
    let Some(store) = st.identities.as_ref() else {
        return Ok(subject.to_string());
    };
    Ok(scarab_identity::resolve_principal(store.as_ref(), subject)
        .await
        .map_err(|e| {
            ApiError::Unavailable(format!(
                "could not resolve `{subject}` to its Principal, so nothing was reset: {e}"
            ))
        })?
        .unwrap_or_else(|| subject.to_string()))
}

/// Is `subject` an install-wide (bootstrap) owner — by id (aliases
/// included), or by a verified email of one of its linked identities under
/// that provider's owners rules? Fails closed: an identity store error
/// refuses rather than answering "no".
async fn is_install_owner(st: &AppState, subject: &str) -> Result<bool, ApiError> {
    if !crate::token_owner_roles(st, subject).await.is_empty() {
        return Ok(true);
    }
    let Some(store) = st.identities.as_ref() else {
        return Ok(false);
    };
    let identities = store.identities_of(subject).await.map_err(|e| {
        ApiError::Unavailable(format!(
            "could not read `{subject}`'s identities to tell whether they are an install-wide \
             owner, so nothing was reset: {e}"
        ))
    })?;
    Ok(identities.iter().any(|i| {
        let email = i.email.as_deref().filter(|_| i.email_verified);
        email.is_some()
            && st
                .oauth_providers
                .iter()
                .filter(|p| p.provider_id() == i.provider)
                .any(|p| p.matches_owner(subject, email))
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
    // Logged first, or not done: the reset is recorded with what it is
    // about to remove, and nothing is removed if it cannot be recorded.
    let held = PasskeyStore::list(db, &subject)
        .await
        .map_err(|e| format!("could not list the passkeys of `{subject}`: {e}"))?
        .len() as u64;
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
        "removed": held,
    });
    SecurityEventLog::append(db, &ev).await.map_err(|e| {
        format!(
            "the reset could not be recorded in the security event log, so nothing was \
             removed: {e}"
        )
    })?;
    let removed = PasskeyStore::remove_all(db, &subject)
        .await
        .map_err(|e| format!("could not remove the passkeys of `{subject}`: {e}"))?;
    let windows_revoked = StepUpStore::delete_windows_for(db, &subject)
        .await
        .map_err(|e| format!("could not revoke the step-ups of `{subject}`: {e}"))?;
    Ok(PasskeyResetDto {
        subject,
        removed,
        windows_revoked,
    })
}

/// The `caller` an operator grant is recorded as.
pub const OPERATOR_GRANT_CALLER: &str = "operator:grant";

/// How a [`grant_role`] error about a principal that does not exist starts:
/// `scarab-server admin grant` exits 3 on it, so a script can tell "has not
/// signed in yet" from a failure.
pub const NO_PRINCIPAL: &str = "no principal";

/// What [`grant_role`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorGrant {
    /// The principal already held exactly this role in the Org as a native
    /// binding: nothing written, nothing recorded.
    Unchanged { subject: String },
    /// The binding was written and recorded. `previous` is the Org-scope row
    /// it replaced, if any: a role, a revoked row (role `None`), or a role a
    /// forge import gave (origin `import`, now `native`).
    Granted {
        subject: String,
        previous: Option<BindingRow>,
    },
    /// `--if-absent`, and the principal has a binding row in the Org (at the
    /// Org or at a Project, revoked rows included): nothing written, nothing
    /// recorded. `rows` is what is there.
    Exists {
        subject: String,
        rows: Vec<BindingRow>,
    },
}

/// The operator grant (`scarab-server admin grant`): give `subject` `role`
/// in `org` as a native binding, for seeding a fresh install's first members
/// without a browser. Records `binding.operator_grant` in the Org's security
/// event log (caller `operator:grant`, with the host, OS user and reason) in
/// the same transaction as the binding: both land or neither does.
///
/// Without `if_absent` the role is set exactly: a higher role is lowered, a
/// revoked binding is restored, and a role a forge import gave becomes a
/// native binding (so a later forge sync cannot remove it). A principal that
/// already holds exactly `role` as a native binding is
/// [`Unchanged`](OperatorGrant::Unchanged), and nothing is recorded.
///
/// With `if_absent` it writes only when the principal has no binding row in
/// the Org at all: not at the Org scope, not at any Project, not a revoked
/// one. Otherwise it is [`Exists`](OperatorGrant::Exists) and nothing is
/// written: a seed script run again never undoes what an Owner changed.
///
/// Refuses an unknown role, an invalid org name, a database whose schema
/// predates the event log (it never migrates), and an id no Principal was
/// ever minted for — granting to one would hand the role to whoever next
/// signs in with that id (ADR-0069 §3).
#[allow(clippy::too_many_arguments)]
pub async fn grant_role(
    db: &PostgresDb,
    org: &str,
    subject: &str,
    role: &str,
    if_absent: bool,
    reason: Option<&str>,
    operator: &OperatorInfo,
    now_ms: i64,
) -> Result<OperatorGrant, String> {
    let role = match role {
        "viewer" => Role::Viewer,
        "member" => Role::Member,
        "admin" => Role::Admin,
        "owner" => Role::Owner,
        other => {
            return Err(format!(
                "unknown role `{other}` (want viewer, member, admin or owner)"
            ))
        }
    };
    if let Some(problem) = crate::org_name_problem(org) {
        return Err(problem);
    }
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
    let resolved = scarab_identity::resolve_principal(db, subject)
        .await
        .map_err(|e| format!("could not resolve `{subject}`: {e}"))?
        .ok_or_else(|| {
            format!(
                "{NO_PRINCIPAL} `{subject}` exists: they must sign in to Scarab once before \
                 a role can name them"
            )
        })?;
    let rows = || async {
        db.binding_rows(&resolved, org)
            .await
            .map_err(|e| format!("could not read the bindings of `{resolved}` in `{org}`: {e}"))
    };
    let existing = rows().await?;
    if if_absent && !existing.is_empty() {
        return Ok(OperatorGrant::Exists {
            subject: resolved,
            rows: existing,
        });
    }
    let name = |r: Role| match r {
        Role::Viewer => "viewer",
        Role::Member => "member",
        Role::Admin => "admin",
        Role::Owner => "owner",
    };
    let previous = existing.into_iter().find(|r| r.project.is_empty());
    if let Some(p) = &previous {
        if p.role.as_deref() == Some(name(role)) && p.origin == "native" {
            return Ok(OperatorGrant::Unchanged { subject: resolved });
        }
    }
    let mut ev = event(
        kind::BINDING_OPERATOR_GRANT,
        &resolved,
        OPERATOR_GRANT_CALLER.to_string(),
        Some(org.to_string()),
        now_ms,
    );
    ev.op = Some("binding.operator_grant".into());
    ev.target = Some(resolved.clone());
    ev.detail = serde_json::json!({
        "requested": subject,
        "role": name(role),
        "previous": previous.as_ref().and_then(|p| p.role.clone()),
        "previous_origin": previous.as_ref().map(|p| p.origin.clone()),
        "if_absent": if_absent,
        "reason": reason,
        "host": operator.host,
        "os_user": operator.os_user,
    });
    let binding = Binding {
        subject: resolved.clone(),
        scope: Scope::Org(org.to_string()),
        role,
    };
    let written = db
        .operator_grant(&ev, &binding, if_absent)
        .await
        .map_err(|e| {
            format!(
                "could not grant `{resolved}` {} in `{org}`; nothing was granted or \
                 recorded: {e}",
                name(role)
            )
        })?;
    if !written {
        // A row appeared between the read and the write (an Owner acted in
        // the UI meanwhile): leave it, as `--if-absent` promises.
        return Ok(OperatorGrant::Exists {
            rows: rows().await?,
            subject: resolved,
        });
    }
    Ok(OperatorGrant::Granted {
        subject: resolved,
        previous,
    })
}
