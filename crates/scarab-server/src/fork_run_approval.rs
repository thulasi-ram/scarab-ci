//! An org's **Fork Run approval** setting (ADR-0086 §9):
//! `GET` and `PUT /v1/orgs/{org}/security/fork-run-approval`.
//!
//! While it is on, Run creation puts one `manual` gate in front of every
//! root Step of a Run of fork code
//! ([`add_fork_run_approval_gate`](scarab_policy::fork_run_approval_gate::add_fork_run_approval_gate)),
//! so nothing in the Run starts, its shared services included, until
//! someone with write on the repository releases it. The gate unlocks
//! nothing: a released Run is still fork code.
//!
//! Anyone who may read the org reads the setting (`settings.read`). Only an
//! org admin changes it, only from a signed-in browser: no API token, no
//! Bearer session and no app acting for someone may change it, so it is
//! never part of a delegated grant. Turning it off asks for a passkey
//! confirmation of its own; turning it on does not. Each change is recorded
//! in the org's security events (`fork_run_approval.changed`).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_identity::{security_event_kind as kind, Action, ForkRunApproval};

use crate::step_up::GuardCtx;
use crate::{authenticate, refuse_reserved_org, ApiError, AppState};

/// An org's Fork Run approval setting: `GET` and `PUT`
/// `/v1/orgs/{org}/security/fork-run-approval`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ForkRunApprovalDto {
    /// A Run of fork code waits at a `manual` gate before any of its Steps
    /// starts, until someone with write on the repository releases it.
    pub enabled: bool,
    /// The caller may change the setting: an admin of the org, in a
    /// signed-in browser.
    pub can_change: bool,
}

/// `PUT /v1/orgs/{org}/security/fork-run-approval` body.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkRunApprovalUpdate {
    /// `true` turns Fork Run approval on. `false` turns it off, which asks
    /// for a passkey confirmation.
    pub enabled: bool,
}

/// Is `caller` a person in a signed-in browser: no API token, no session
/// presented as Bearer?
fn in_browser(caller: &crate::Caller) -> bool {
    caller.grant.is_none() && caller.via != Some(crate::AuthVia::Bearer)
}

/// May `caller` administer `org`?
async fn is_org_admin(st: &AppState, caller: &crate::Caller, org: &str) -> Result<bool, ApiError> {
    let scope = scarab_identity::Scope::Org(org.to_string());
    Ok(caller.may_globally(Action::Administer)
        || caller.may(st, Action::Administer, &scope).await?)
}

/// The setting stored for `org`, off when it has none or the server keeps
/// none.
async fn org_setting(st: &AppState, org: &str) -> Result<ForkRunApproval, ApiError> {
    match st.fork_run_approval.as_deref() {
        Some(store) => Ok(store
            .fork_run_approval(org)
            .await
            .map_err(crate::step_up::store_err)?
            .unwrap_or_default()),
        None => Ok(ForkRunApproval::default()),
    }
}

/// An org's Fork Run approval setting, and whether the caller may change
/// it. Off when no admin has turned it on.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/security/fork-run-approval",
    params(("org" = String, Path, description = "org")),
    summary = "An org's Fork Run approval setting",
    responses(
        (status = 200, body = ForkRunApprovalDto),
        (status = 400, description = "a reserved org (plain text)"),
        (status = 401, description = "not signed in"),
        (status = 403, description = "not a member of the org")
    )
)]
pub(crate) async fn get_fork_run_approval(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<ForkRunApprovalDto>, ApiError> {
    let scope = scarab_identity::Scope::Org(org.clone());
    let caller = crate::authorize_scoped_caller(&st, &headers, Action::Read, Some(&scope)).await?;
    refuse_reserved_org(&org, "have a Fork Run approval setting")?;
    let setting = org_setting(&st, &org).await?;
    let can_change = in_browser(&caller)
        && st.fork_run_approval.is_some()
        && is_org_admin(&st, &caller, &org).await?;
    Ok(Json(ForkRunApprovalDto {
        enabled: setting.enabled,
        can_change,
    }))
}

/// Turn an org's Fork Run approval on or off. Org admin only, from a
/// signed-in browser. Turning it off asks for a passkey confirmation of its
/// own: a Run of fork code would start with no approval. Turning it on
/// asks for none. A request that changes nothing asks for no confirmation
/// and records nothing. Recorded in the org's security events
/// (`fork_run_approval.changed`) with the old and the new value. Runs
/// created before the change keep or lack their gate.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/security/fork-run-approval",
    params(("org" = String, Path, description = "org")),
    request_body = ForkRunApprovalUpdate,
    summary = "Turn an org's Fork Run approval on or off (admin; always-ask step-up to turn it off)",
    responses(
        (status = 200, body = ForkRunApprovalDto),
        (status = 400, description = "a body that is not `{\"enabled\": true}` or `{\"enabled\": false}`, or a reserved org (plain text)"),
        (status = 403, description = "`browser_session_required` (an API token or a Bearer session, even for a request that changes nothing), with `code` and `message`; not an admin of the org; or `step_up_required` (mode `action`) to turn it off"),
        (status = 404, description = "the setting cannot be stored on this server")
    )
)]
pub(crate) async fn put_fork_run_approval(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path(org): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let caller = authenticate(&st, &headers, Action::Administer).await?;
    // A person in a browser only, before anything else: no token, and no
    // app acting for someone, may change it, so it is never delegated.
    if let Some(refused) = crate::secret_match::refuse_unless_browser(
        &caller,
        "The Fork Run approval setting can be changed",
    ) {
        return Ok(refused);
    }
    if !is_org_admin(&st, &caller, &org).await? {
        return Err(ApiError::Forbidden);
    }
    refuse_reserved_org(&org, "have a Fork Run approval setting")?;
    let store = st.fork_run_approval.clone().ok_or(ApiError::NotFound)?;
    let Ok(update) = serde_json::from_slice::<ForkRunApprovalUpdate>(&body) else {
        return Err(ApiError::BadRequest(
            "the body must be JSON of the form {\"enabled\": true} or {\"enabled\": false}".into(),
        ));
    };
    let new = ForkRunApproval {
        enabled: update.enabled,
    };
    let old = store
        .fork_run_approval(&org)
        .await
        .map_err(crate::step_up::store_err)?
        .unwrap_or_default();
    if new == old {
        guard.exempt("the setting does not change");
        return Ok(Json(ForkRunApprovalDto {
            enabled: old.enabled,
            can_change: true,
        })
        .into_response());
    }
    if old.loosened_by(new) {
        guard
            .require(
                &st,
                vec![
                    "enabled = true → false".to_string(),
                    "a Run of fork code starts with no approval".to_string(),
                ],
            )
            .await?;
    } else {
        guard.exempt("turning Fork Run approval on only adds a gate");
    }

    let subject = caller.principal.subject.clone();
    let now = st.clock.now().await.0;
    // The setting this write replaced, read with it: another admin may have
    // changed it since the read above.
    let old = store
        .put_fork_run_approval(&org, new, &subject, now)
        .await
        .map_err(crate::step_up::store_err)?
        .unwrap_or_default();
    if let Some(log) = st.security_events.as_deref() {
        let mut ev = crate::step_up::event(
            kind::FORK_RUN_APPROVAL_CHANGED,
            &subject,
            caller.event_label(),
            Some(org.clone()),
            now,
        );
        ev.op = Some(crate::step_up::ops::FORK_RUN_APPROVAL_PUT.id.to_string());
        ev.target = Some(org.clone());
        ev.confirmation_id = guard.confirmation_id();
        ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
        ev.detail = serde_json::json!({
            "old": { "enabled": old.enabled },
            "new": { "enabled": new.enabled },
        });
        // The change is stored; `op.authorised`, written before it when a
        // confirmation was asked for, already records it. This record is
        // best-effort.
        if let Err(e) = log.append(&ev).await {
            tracing::error!(error = %e, org, "fork_run_approval.changed could not be recorded");
        }
    }
    Ok(Json(ForkRunApprovalDto {
        enabled: new.enabled,
        can_change: true,
    })
    .into_response())
}
