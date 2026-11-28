//! An org's **Fork Run approval** setting (ADR-0086 §9):
//! `GET` and `PUT /v1/orgs/{org}/security/fork-run-approval`.
//!
//! Three values: `off`, `agent_started` (the default: a Run of fork code
//! started through an `oauth` PrincipalToken, which the OAuth authorization server mints when it is enabled) and
//! `all`. For each Run of fork code the setting gates, Run creation puts
//! one `manual` gate in front of every root Step
//! ([`add_fork_run_approval_gate`](scarab_policy::fork_run_approval_gate::add_fork_run_approval_gate)),
//! so nothing in the Run starts, its shared services included, until
//! someone with write on the repository releases it. The gate unlocks
//! nothing: a released Run is still fork code.
//!
//! Anyone who may read the org reads the setting (`settings.read`). Only an
//! org admin changes it, only from a signed-in browser: no API token, no
//! Bearer session and no app acting for someone may change it, so it is
//! never part of a delegated grant. A change that gates fewer Runs (`all` →
//! `agent_started`, `all` → `off`, `agent_started` → `off`) asks for a
//! passkey confirmation of its own; one that gates more does not. Each
//! change is recorded in the org's security events
//! (`fork_run_approval.changed`).

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

/// Which Runs of fork code wait at the Fork Run approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForkRunApprovalSettingDto {
    /// None.
    Off,
    /// Those started through an `oauth` token: an app acting for someone,
    /// such as an agent. The default.
    AgentStarted,
    /// Every Run of fork code.
    All,
}

impl From<ForkRunApproval> for ForkRunApprovalSettingDto {
    fn from(s: ForkRunApproval) -> Self {
        match s {
            ForkRunApproval::Off => Self::Off,
            ForkRunApproval::AgentStarted => Self::AgentStarted,
            ForkRunApproval::All => Self::All,
        }
    }
}

impl From<ForkRunApprovalSettingDto> for ForkRunApproval {
    fn from(s: ForkRunApprovalSettingDto) -> Self {
        match s {
            ForkRunApprovalSettingDto::Off => Self::Off,
            ForkRunApprovalSettingDto::AgentStarted => Self::AgentStarted,
            ForkRunApprovalSettingDto::All => Self::All,
        }
    }
}

/// An org's Fork Run approval setting: `GET` and `PUT`
/// `/v1/orgs/{org}/security/fork-run-approval`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ForkRunApprovalDto {
    /// Which Runs of fork code wait at a `manual` gate before any of their
    /// Steps starts, until someone with write on the repository releases it.
    pub setting: ForkRunApprovalSettingDto,
    /// The caller may change the setting: an admin of the org, in a
    /// signed-in browser.
    pub can_change: bool,
}

/// `PUT /v1/orgs/{org}/security/fork-run-approval` body.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ForkRunApprovalUpdate {
    /// The new setting. One that gates fewer Runs than the current one asks
    /// for a passkey confirmation.
    pub setting: ForkRunApprovalSettingDto,
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

/// The setting stored for `org`; the default, `agent_started`, when it has
/// none or the server keeps none.
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
/// it. `agent_started` when no admin has changed it.
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
        setting: setting.into(),
        can_change,
    }))
}

/// Change an org's Fork Run approval setting. Org admin only, from a
/// signed-in browser. A change that gates fewer Runs (`all` →
/// `agent_started`, `all` → `off`, `agent_started` → `off`) asks for a
/// passkey confirmation of its own; one that gates more asks for none. A
/// request that changes nothing asks for no confirmation and records
/// nothing. The write is a compare-and-set on the value the request read:
/// when another admin changed it in between, nothing is written (409). Recorded in the org's security events
/// (`fork_run_approval.changed`) with the old and the new value. Runs
/// created before the change keep or lack their gate.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/security/fork-run-approval",
    params(("org" = String, Path, description = "org")),
    request_body = ForkRunApprovalUpdate,
    summary = "Change an org's Fork Run approval setting (admin; always-ask step-up to loosen it)",
    responses(
        (status = 200, body = ForkRunApprovalDto),
        (status = 400, description = "a body that is not `{\"setting\": \"off\" | \"agent_started\" | \"all\"}`, or a reserved org (plain text)"),
        (status = 403, description = "`browser_session_required` (an API token or a Bearer session, even for a request that changes nothing), with `code` and `message`; not an admin of the org; or `step_up_required` (mode `action`) for a change that gates fewer Runs"),
        (status = 404, description = "the setting cannot be stored on this server"),
        (status = 409, description = "another admin changed the setting since this request read it (plain text): reload and choose again")
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
            "the body must be JSON of the form {\"setting\": \"off\"}, \
             {\"setting\": \"agent_started\"} or {\"setting\": \"all\"}"
                .into(),
        ));
    };
    let new = ForkRunApproval::from(update.setting);
    let old = store
        .fork_run_approval(&org)
        .await
        .map_err(crate::step_up::store_err)?
        .unwrap_or_default();
    if new == old {
        guard.exempt("the setting does not change");
        return Ok(Json(ForkRunApprovalDto {
            setting: old.into(),
            can_change: true,
        })
        .into_response());
    }
    if old.loosened_by(new) {
        guard
            .require(
                &st,
                vec![
                    format!("setting = {} → {}", old.as_str(), new.as_str()),
                    "fewer Runs of fork code wait for approval".to_string(),
                ],
            )
            .await?;
    } else {
        guard.exempt("the new setting gates more Runs of fork code");
    }

    let subject = caller.principal.subject.clone();
    let now = st.clock.now().await.0;
    // A compare-and-set on the value the step-up was decided (and
    // confirmed) against: when another admin changed it since, nothing is
    // written and the caller reloads.
    let written = store
        .put_fork_run_approval(&org, old, new, &subject, now)
        .await
        .map_err(crate::step_up::store_err)?;
    if !written {
        return Err(ApiError::Conflict(format!(
            "the Fork Run approval setting changed from `{}` since it was read; reload and \
             choose again",
            old.as_str()
        )));
    }
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
            "old": old.as_str(),
            "new": new.as_str(),
        });
        // The change is stored; `op.authorised`, written before it when a
        // confirmation was asked for, already records it. This record is
        // best-effort.
        if let Err(e) = log.append(&ev).await {
            tracing::error!(error = %e, org, "fork_run_approval.changed could not be recorded");
        }
    }
    Ok(Json(ForkRunApprovalDto {
        setting: new.into(),
        can_change: true,
    })
    .into_response())
}
