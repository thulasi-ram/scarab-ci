//! The passkey and step-up ceremony endpoints (ADR-0073 §1, §2, §4).
//!
//! - `GET  /v1/me/security` — the caller's passkeys and step-up state.
//! - `POST /v1/me/passkeys/registration/options` — begin enrolling. The
//!   FIRST passkey needs this browser session's login to have completed
//!   within 5 minutes (else `403 fresh_login_required`); a later one is the
//!   always-ask crown jewel `passkey.add`.
//! - `POST /v1/me/passkeys/registration` — finish enrolling.
//! - `DELETE /v1/me/passkeys/{id}` — the always-ask crown jewel
//!   `passkey.remove` (enforced by the guard).
//! - `POST /v1/me/step-up/options` — a challenge that opens a window.
//! - `POST /v1/me/step-up` — answer a window or action challenge (one handed
//!   back by a 403, or from `options`): a StepUpConfirmation that expires 5
//!   minutes after it is made, never extended.
//!
//! Every ceremony runs in a **cookie** session: a challenge is bound to
//! `(principal, session hash, purpose)`, lives 2 minutes, and is consumed by
//! its first answer whatever the outcome.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_identity::{
    security_event_kind as kind, Action, ChallengePurpose, ConfirmationKind, Passkey, PasskeyUse,
    StepUpConfirmation, WebauthnChallenge, STEP_UP_WINDOW_MS,
};
use scarab_webauthn::{
    AuthenticationResponseJson, CoseAlgorithm, RegistrationResponseJson, StoredCredential,
    WebauthnError,
};

use super::{
    descriptors, event, random_bytes, random_id, sent_challenge, store_err, GuardCtx,
    StepUpService, WebauthnChallengeDto,
};
use crate::{authenticate, ApiError, AppState, Caller, SessionFacts};

/// The longest passkey label.
pub const MAX_LABEL_LEN: usize = 64;

/// One of the caller's passkeys.
#[derive(Debug, Serialize, ToSchema)]
pub struct PasskeyDto {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// The credential may be synced to other devices (BE).
    pub backup_eligible: bool,
    /// It is backed up / synced, as last seen (BS).
    pub backup_state: bool,
    /// Its signature counter went backwards: it is never accepted again.
    pub clone_suspected: bool,
    pub transports: Vec<String>,
}

impl From<&Passkey> for PasskeyDto {
    fn from(p: &Passkey) -> Self {
        Self {
            id: p.id.clone(),
            label: p.label.clone(),
            created_at: p.created_at,
            last_used_at: p.last_used_at,
            backup_eligible: p.backup_eligible,
            backup_state: p.backup_state,
            clone_suspected: p.clone_suspected_at.is_some(),
            transports: p.transports.clone(),
        }
    }
}

/// `GET /v1/me/security`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MeSecurityResponse {
    pub subject: String,
    /// `enforce` or `off`.
    pub step_up: String,
    /// Holds at least one usable (not clone-flagged) passkey.
    pub has_passkey: bool,
    /// How many usable (not clone-flagged) passkeys are held.
    pub usable_passkey_count: u64,
    /// The passkeys — empty for an API-token caller (see `passkeys_hidden`).
    pub passkeys: Vec<PasskeyDto>,
    /// The caller is an API token: a token, scoped to one Org or Project,
    /// never reads its owner's passkey list (only whether one is held).
    pub passkeys_hidden: bool,
    /// This browser session's open window, if any (unix ms).
    pub window_expires_at: Option<i64>,
    /// No usable passkey yet, and this browser session's login is fresh
    /// enough to enroll the first one.
    pub can_enroll_first: bool,
    /// Until when this session's login is fresh (a browser login only).
    pub fresh_login_until: Option<i64>,
}

/// `POST /v1/me/passkeys/registration/options`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct PasskeyRegistrationOptionsRequest {
    /// A name for the passkey ("MacBook Touch ID"). Default `Passkey`.
    #[serde(default)]
    pub label: Option<String>,
}

/// `POST /v1/me/passkeys/registration`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PasskeyRegistrationRequest {
    pub challenge_id: String,
    /// `RegistrationResponseJSON` — `PublicKeyCredential.toJSON()`.
    #[schema(value_type = Object)]
    pub credential: serde_json::Value,
}

/// `POST /v1/me/step-up`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StepUpConfirmRequest {
    pub challenge_id: String,
    /// `AuthenticationResponseJSON` — `PublicKeyCredential.toJSON()`.
    #[schema(value_type = Object)]
    pub credential: serde_json::Value,
}

/// A StepUpConfirmation just made.
#[derive(Debug, Serialize, ToSchema)]
pub struct StepUpConfirmResponse {
    pub confirmation_id: String,
    /// `window` or `action`.
    pub kind: String,
    pub confirmed_at: i64,
    /// Fixed at `confirmed_at` + 5 minutes; never extended.
    pub expires_at: i64,
}

pub(crate) fn ceremony(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> ApiError {
    ApiError::Ceremony {
        status,
        code,
        message: message.into(),
    }
}

pub(crate) fn webauthn_refused(e: WebauthnError) -> ApiError {
    ceremony(StatusCode::BAD_REQUEST, e.code(), e.to_string())
}

pub(crate) fn challenge_invalid() -> ApiError {
    ceremony(
        StatusCode::BAD_REQUEST,
        "challenge_invalid",
        "the challenge is unknown, already used, expired, or from another session",
    )
}

pub(crate) fn service(st: &AppState) -> Result<std::sync::Arc<StepUpService>, ApiError> {
    st.step_up.service().cloned().ok_or(ApiError::NotFound)
}

/// The caller, who must be a human in a browser: a cookie session, never a
/// token or a session presented as Bearer. Passkeys are browser-only
/// (ADR-0073 §8).
pub(crate) async fn browser_session(
    st: &AppState,
    headers: &HeaderMap,
    action: Action,
) -> Result<(Caller, SessionFacts), ApiError> {
    let caller = authenticate(st, headers, action).await?;
    if !caller.is_browser_session() {
        return Err(ceremony(
            StatusCode::FORBIDDEN,
            "browser_session_required",
            "passkeys and step-up confirmations are made in a signed-in browser",
        ));
    }
    let facts = caller.session.clone().expect("a browser session has facts");
    Ok((caller, facts))
}

/// Consume `id` in this session, for this principal and one of `purposes`.
pub(crate) async fn consume(
    svc: &StepUpService,
    id: &str,
    caller: &Caller,
    facts: &SessionFacts,
    purposes: &[ChallengePurpose],
    now: i64,
) -> Result<(WebauthnChallenge, [u8; 32]), ApiError> {
    let row = svc
        .challenges
        .consume(id, &facts.id_hash, now)
        .await
        .map_err(store_err)?
        .ok_or_else(challenge_invalid)?;
    if row.principal != caller.principal.subject || !purposes.contains(&row.purpose) {
        return Err(challenge_invalid());
    }
    let sent = sent_challenge(&row).ok_or_else(challenge_invalid)?;
    Ok((row, sent))
}

/// The caller's passkeys and step-up state.
#[utoipa::path(
    get,
    path = "/v1/me/security",
    summary = "My passkeys and step-up state (ADR-0073)",
    responses((status = 200, body = MeSecurityResponse), (status = 401, description = "not signed in"))
)]
pub(crate) async fn me_security(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MeSecurityResponse>, ApiError> {
    let caller = authenticate(&st, &headers, Action::Read).await?;
    let now = st.clock.now().await.0;
    let subject = caller.principal.subject.clone();
    let fresh_login_until = caller
        .session
        .as_ref()
        .and_then(|s| s.fresh_login_until)
        .filter(|_| caller.is_browser_session());
    let Some(svc) = st.step_up.service() else {
        return Ok(Json(MeSecurityResponse {
            subject,
            step_up: "off".into(),
            has_passkey: false,
            usable_passkey_count: 0,
            passkeys: vec![],
            passkeys_hidden: false,
            window_expires_at: None,
            can_enroll_first: false,
            fresh_login_until,
        }));
    };
    let passkeys = svc.passkeys.list(&subject).await.map_err(store_err)?;
    let usable_passkey_count = passkeys
        .iter()
        .filter(|p| p.clone_suspected_at.is_none())
        .count() as u64;
    let has_passkey = usable_passkey_count > 0;
    // An API token is a narrowing of its owner (one scope, a ceiling): it
    // learns whether its human can confirm (the CLI's `security setup`
    // polls that), never the passkeys themselves (git-bug fe777ed). A
    // session presented as Bearer is the human's own and sees all.
    let passkeys_hidden = caller.grant.is_some();
    let window_expires_at = match caller.session.as_ref() {
        Some(s) if caller.is_browser_session() => svc
            .confirmations
            .window_expiry(&s.id_hash, &subject, now)
            .await
            .map_err(store_err)?,
        _ => None,
    };
    Ok(Json(MeSecurityResponse {
        subject,
        step_up: "enforce".into(),
        has_passkey,
        usable_passkey_count,
        passkeys: if passkeys_hidden {
            vec![]
        } else {
            passkeys.iter().map(PasskeyDto::from).collect()
        },
        passkeys_hidden,
        window_expires_at,
        can_enroll_first: !has_passkey && fresh_login_until.is_some_and(|t| t > now),
        fresh_login_until,
    }))
}

/// Begin enrolling a passkey.
#[utoipa::path(
    post,
    path = "/v1/me/passkeys/registration/options",
    request_body = PasskeyRegistrationOptionsRequest,
    summary = "Begin enrolling a passkey (ADR-0073 §2)",
    responses(
        (status = 200, body = WebauthnChallengeDto, description = "PublicKeyCredentialCreationOptionsJSON under `public_key`"),
        (status = 403, description = "`fresh_login_required` (a first passkey needs a browser login from the last 5 minutes), `step_up_required` (a later passkey is always-ask), or not a browser session"),
        (status = 404, description = "step-up is off")
    )
)]
pub(crate) async fn passkey_registration_options(
    State(st): State<AppState>,
    Extension(ctx): Extension<GuardCtx>,
    headers: HeaderMap,
    body: Option<Json<PasskeyRegistrationOptionsRequest>>,
) -> Result<Json<WebauthnChallengeDto>, ApiError> {
    let svc = service(&st)?;
    let (caller, facts) = browser_session(&st, &headers, Action::Write).await?;
    let now = st.clock.now().await.0;
    let subject = caller.principal.subject.as_str();
    let label = body
        .and_then(|Json(b)| b.label)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "Passkey".to_string());
    if label.chars().count() > MAX_LABEL_LEN || label.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(format!(
            "a passkey label is at most {MAX_LABEL_LEN} printable characters"
        )));
    }
    let all = svc.passkeys.list(subject).await.map_err(store_err)?;
    if all.iter().all(|p| p.clone_suspected_at.is_some()) {
        // The FIRST usable passkey: a valid session is not enough (a stolen
        // one could enroll the thief's passkey), only a fresh browser login.
        if !facts.fresh_login_until.is_some_and(|t| t > now) {
            // A refusal like any other (ADR-0073 §7): who, and what for.
            let mut ev = event(
                kind::STEP_UP_REFUSED,
                subject,
                caller.event_label(),
                None,
                now,
            );
            ev.op = Some(ctx.op.id.to_string());
            ev.target = Some(label.clone());
            ev.session_hash = Some(facts.id_hash.clone());
            ev.detail = serde_json::json!({ "mode": "enroll", "reason": "fresh login required" });
            svc.record_refusal(ev).await;
            return Err(ApiError::FreshLoginRequired);
        }
        ctx.exempt("first passkey: gated by a fresh browser login instead");
    } else {
        // A later passkey is the always-ask crown jewel `passkey.add`.
        ctx.require(&st, vec![format!("add passkey `{label}`")])
            .await?;
    }
    let (row, sent) = svc
        .mint_challenge(
            subject,
            &facts.id_hash,
            ChallengePurpose::Register,
            None,
            None,
            Some(label),
            None,
            now,
        )
        .await?;
    let handle = svc
        .passkeys
        .user_handle(subject, random_bytes())
        .await
        .map_err(store_err)?;
    let display = caller
        .principal
        .display_name
        .clone()
        .unwrap_or_else(|| subject.to_string());
    Ok(Json(WebauthnChallengeDto {
        id: row.id,
        public_key: scarab_webauthn::creation_options(
            svc.rp(),
            &handle,
            subject,
            &display,
            &sent,
            &descriptors(&all),
        ),
        expires_at: row.expires_at,
    }))
}

/// Finish enrolling a passkey.
#[utoipa::path(
    post,
    path = "/v1/me/passkeys/registration",
    request_body = PasskeyRegistrationRequest,
    summary = "Finish enrolling a passkey (ADR-0073 §2)",
    responses(
        (status = 201, body = PasskeyDto),
        (status = 400, description = "challenge unknown/used/expired, or the credential failed verification (`code` names the check)"),
        (status = 409, description = "this credential is already registered"),
        (status = 404, description = "step-up is off")
    )
)]
pub(crate) async fn passkey_registration(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PasskeyRegistrationRequest>,
) -> Result<(StatusCode, Json<PasskeyDto>), ApiError> {
    let svc = service(&st)?;
    let (caller, facts) = browser_session(&st, &headers, Action::Write).await?;
    let now = st.clock.now().await.0;
    let (row, sent) = consume(
        &svc,
        &req.challenge_id,
        &caller,
        &facts,
        &[ChallengePurpose::Register],
        now,
    )
    .await?;
    let resp: RegistrationResponseJson = serde_json::from_value(req.credential)
        .map_err(|_| webauthn_refused(WebauthnError::MalformedResponse))?;
    let cred =
        scarab_webauthn::verify_registration(svc.rp(), &sent, &resp).map_err(webauthn_refused)?;
    if svc
        .passkeys
        .get_by_credential(&cred.credential_id)
        .await
        .map_err(store_err)?
        .is_some()
    {
        return Err(ApiError::Conflict(
            "this passkey is already registered".into(),
        ));
    }
    let subject = caller.principal.subject.clone();
    let first = svc.usable_passkeys(&subject).await?.is_empty();
    let passkey = Passkey {
        id: random_id(),
        principal: subject.clone(),
        credential_id: cred.credential_id,
        public_key_cose: cred.public_key_cose,
        alg: cred.alg.id() as i32,
        sign_count: cred.sign_count,
        label: row.label.clone().unwrap_or_else(|| "Passkey".into()),
        transports: cred.transports,
        aaguid: Some(cred.aaguid.to_vec()),
        backup_eligible: cred.backup_eligible,
        backup_state: cred.backup_state,
        created_at: now,
        created_session_hash: facts.id_hash.clone(),
        last_used_at: None,
        clone_suspected_at: None,
    };
    svc.passkeys.insert(&passkey).await.map_err(|e| match e {
        scarab_identity::IdentityError::Conflict(_) => {
            ApiError::Conflict("this passkey is already registered".into())
        }
        other => store_err(other),
    })?;
    let mut ev = event(
        kind::PASSKEY_ENROLLED,
        &subject,
        caller.event_label(),
        None,
        now,
    );
    ev.passkey_id = Some(passkey.id.clone());
    ev.session_hash = Some(facts.id_hash.clone());
    ev.target = Some(passkey.label.clone());
    ev.detail = serde_json::json!({
        "label": passkey.label,
        "first": first,
        "alg": passkey.alg,
        "backup_eligible": passkey.backup_eligible,
    });
    // Logged, or not enrolled.
    if let Err(e) = svc.events.append(&ev).await {
        tracing::error!(error = %e, "passkey.enrolled could not be recorded; undoing");
        if let Err(e) = svc.passkeys.remove(&subject, &passkey.id).await {
            tracing::error!(error = %e, passkey = %passkey.id, "could not undo an unrecorded enrollment");
        }
        return Err(ApiError::Unavailable(
            "the security event log is unavailable, so the passkey was not enrolled".into(),
        ));
    }
    Ok((StatusCode::CREATED, Json(PasskeyDto::from(&passkey))))
}

/// Remove one of the caller's passkeys (always-ask, enforced by the guard).
#[utoipa::path(
    delete,
    path = "/v1/me/passkeys/{id}",
    params(("id" = String, Path, description = "passkey id")),
    summary = "Remove a passkey (ADR-0073 §2; always-ask step-up)",
    responses(
        (status = 204, description = "removed"),
        (status = 403, description = "`step_up_required` (mode `action`)"),
        (status = 404, description = "no such passkey of yours, or step-up is off")
    )
)]
pub(crate) async fn remove_passkey(
    State(st): State<AppState>,
    ctx: Option<Extension<GuardCtx>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let svc = service(&st)?;
    let caller = authenticate(&st, &headers, Action::Write).await?;
    let subject = caller.principal.subject.clone();
    if !svc
        .passkeys
        .remove(&subject, &id)
        .await
        .map_err(store_err)?
    {
        return Err(ApiError::NotFound);
    }
    // Every step-up it signed goes with it: a removed passkey leaves no
    // window or unconsumed action confirmation standing.
    let revoked = svc
        .confirmations
        .delete_for_passkey(&id)
        .await
        .map_err(store_err)?;
    let now = st.clock.now().await.0;
    let mut ev = event(
        kind::PASSKEY_REMOVED,
        &subject,
        caller.event_label(),
        None,
        now,
    );
    ev.passkey_id = Some(id);
    ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({ "confirmations_revoked": revoked });
    // Linked to the always-ask confirmation (or, replayed, the request)
    // that authorised it.
    ev.confirmation_id = ctx.and_then(|Extension(c)| c.confirmation_id());
    svc.record(ev).await;
    Ok(StatusCode::NO_CONTENT)
}

/// A challenge that, answered, opens a 5-minute window in this session.
#[utoipa::path(
    post,
    path = "/v1/me/step-up/options",
    summary = "Begin a step-up: a challenge that opens a 5-minute window (ADR-0073 §4)",
    responses(
        (status = 200, body = WebauthnChallengeDto, description = "PublicKeyCredentialRequestOptionsJSON under `public_key`"),
        (status = 400, description = "`no_passkey`: enroll one first"),
        (status = 403, description = "not a browser session"),
        (status = 404, description = "step-up is off")
    )
)]
pub(crate) async fn step_up_options(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<WebauthnChallengeDto>, ApiError> {
    let svc = service(&st)?;
    let (caller, facts) = browser_session(&st, &headers, Action::Write).await?;
    let now = st.clock.now().await.0;
    let subject = caller.principal.subject.as_str();
    let passkeys = svc.usable_passkeys(subject).await?;
    if passkeys.is_empty() {
        return Err(ceremony(
            StatusCode::BAD_REQUEST,
            "no_passkey",
            "you have no passkey; enroll one first",
        ));
    }
    let (row, sent) = svc
        .mint_challenge(
            subject,
            &facts.id_hash,
            ChallengePurpose::Window,
            None,
            None,
            None,
            None,
            now,
        )
        .await?;
    Ok(Json(WebauthnChallengeDto {
        id: row.id,
        public_key: scarab_webauthn::request_options(svc.rp(), &sent, &descriptors(&passkeys)),
        expires_at: row.expires_at,
    }))
}

/// Answer a window or action challenge with a passkey assertion.
#[utoipa::path(
    post,
    path = "/v1/me/step-up",
    request_body = StepUpConfirmRequest,
    summary = "Confirm a step-up with a passkey (ADR-0073 §4)",
    responses(
        (status = 200, body = StepUpConfirmResponse),
        (status = 400, description = "challenge unknown/used/expired/another session's, unknown credential, or the assertion failed verification (`code` names the check)"),
        (status = 403, description = "`clone_suspected`: the passkey's counter went backwards and it is now refused for good; or not a browser session"),
        (status = 404, description = "step-up is off")
    )
)]
pub(crate) async fn step_up_confirm(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StepUpConfirmRequest>,
) -> Result<Json<StepUpConfirmResponse>, ApiError> {
    let svc = service(&st)?;
    let (caller, facts) = browser_session(&st, &headers, Action::Write).await?;
    let now = st.clock.now().await.0;
    let subject = caller.principal.subject.clone();
    let (row, sent) = consume(
        &svc,
        &req.challenge_id,
        &caller,
        &facts,
        &[ChallengePurpose::Window, ChallengePurpose::Action],
        now,
    )
    .await?;
    let passkey = verify_assertion(&svc, &caller, &facts, &sent, req.credential, now).await?;
    let kind_ = match row.purpose {
        ChallengePurpose::Action => ConfirmationKind::Action,
        _ => ConfirmationKind::Window,
    };
    let confirmation = StepUpConfirmation {
        id: random_id(),
        principal: subject.clone(),
        session_hash: facts.id_hash.clone(),
        passkey_id: passkey.id.clone(),
        kind: kind_,
        action_digest: row.action_digest.clone(),
        request_id: None,
        confirmed_at: now,
        // Fixed here and never moved: activity does not extend a window.
        expires_at: now + STEP_UP_WINDOW_MS,
        consumed_at: None,
    };
    let op = row
        .summary
        .as_ref()
        .and_then(|s| s.get("op"))
        .and_then(|o| o.as_str())
        .map(str::to_string);
    let target = row
        .summary
        .as_ref()
        .and_then(|s| s.pointer("/summary/target"))
        .and_then(|o| o.as_str())
        .map(str::to_string);
    // The Org the guard resolved for the operation it was confirmed for, so
    // the confirmation lands in that Org's log and no other.
    let org = row
        .summary
        .as_ref()
        .and_then(|s| s.get("org"))
        .and_then(|o| o.as_str())
        .map(str::to_string);
    let mut ev = event(
        kind::STEP_UP_CONFIRMED,
        &subject,
        caller.event_label(),
        org,
        now,
    );
    ev.op = op;
    ev.target = target;
    ev.confirmation_id = Some(confirmation.id.clone());
    ev.passkey_id = Some(passkey.id.clone());
    ev.session_hash = Some(facts.id_hash.clone());
    ev.detail = serde_json::json!({
        "kind": kind_.as_str(),
        "expires_at": confirmation.expires_at,
        "summary": row.summary.as_ref().and_then(|s| s.get("summary")),
    });
    // Logged, or not made: a confirmation that cannot be recorded is never
    // stored, so no window or action confirmation exists unlogged.
    if let Err(e) = svc.events.append(&ev).await {
        tracing::error!(error = %e, "step_up.confirmed could not be recorded; refusing");
        return Err(ApiError::Unavailable(
            "the security event log is unavailable, so the step-up was not recorded".into(),
        ));
    }
    svc.confirmations
        .put_confirmation(&confirmation)
        .await
        .map_err(store_err)?;
    Ok(Json(StepUpConfirmResponse {
        confirmation_id: confirmation.id,
        kind: kind_.as_str().to_string(),
        confirmed_at: now,
        expires_at: confirmation.expires_at,
    }))
}

/// Verify `credential` as an assertion over `sent` by one of the caller's
/// OWN passkeys, and accept its signature counter with a compare-and-swap.
/// A counter that does not move forward flags the passkey (and records
/// `passkey.clone_suspected`); a flagged passkey is refused for good.
/// Returns the passkey that signed.
pub(crate) async fn verify_assertion(
    svc: &StepUpService,
    caller: &Caller,
    facts: &SessionFacts,
    sent: &[u8; 32],
    credential: serde_json::Value,
    now: i64,
) -> Result<Passkey, ApiError> {
    let subject = caller.principal.subject.clone();
    let resp: AuthenticationResponseJson = serde_json::from_value(credential)
        .map_err(|_| webauthn_refused(WebauthnError::MalformedResponse))?;
    let raw_id = URL_SAFE_NO_PAD
        .decode(resp.raw_id.as_bytes())
        .map_err(|_| webauthn_refused(WebauthnError::MalformedResponse))?;
    let unknown = || {
        ceremony(
            StatusCode::BAD_REQUEST,
            "unknown_credential",
            "that passkey is not one of yours",
        )
    };
    let passkey = svc
        .passkeys
        .get_by_credential(&raw_id)
        .await
        .map_err(store_err)?
        .filter(|p| p.principal == subject)
        .ok_or_else(unknown)?;
    let clone_refused = || {
        ceremony(
            StatusCode::FORBIDDEN,
            "clone_suspected",
            "this passkey's signature counter went backwards, so it may have been cloned; it \
             is refused from now on — use another passkey or ask an Owner to reset yours",
        )
    };
    if passkey.clone_suspected_at.is_some() {
        return Err(clone_refused());
    }
    let alg = CoseAlgorithm::from_id(i64::from(passkey.alg))
        .ok_or_else(|| webauthn_refused(WebauthnError::StoredCredentialInconsistent))?;
    let handle = svc
        .passkeys
        .user_handle(&subject, random_bytes())
        .await
        .map_err(store_err)?;
    let stored = StoredCredential {
        credential_id: passkey.credential_id.clone(),
        public_key_cose: passkey.public_key_cose.clone(),
        alg,
        sign_count: passkey.sign_count,
        backup_eligible: passkey.backup_eligible,
    };
    let clone_event = |detail: serde_json::Value| {
        let mut ev = event(
            kind::PASSKEY_CLONE_SUSPECTED,
            &subject,
            caller.event_label(),
            None,
            now,
        );
        ev.passkey_id = Some(passkey.id.clone());
        ev.session_hash = Some(facts.id_hash.clone());
        ev.detail = detail;
        ev
    };
    let ok = match scarab_webauthn::verify_assertion(svc.rp(), sent, &stored, &handle, &resp) {
        Ok(ok) => ok,
        Err(WebauthnError::CloneSuspected) => {
            svc.passkeys
                .mark_clone(&passkey.id, now)
                .await
                .map_err(store_err)?;
            let revoked = revoke_for_clone(svc, &passkey.id).await?;
            svc.record(clone_event(serde_json::json!({
                "stored_count": passkey.sign_count,
                "confirmations_revoked": revoked,
            })))
            .await;
            return Err(clone_refused());
        }
        Err(e) => return Err(webauthn_refused(e)),
    };
    // The counter is accepted with a compare-and-swap: two racing
    // assertions cannot both pass, and the loser is flagged.
    match svc
        .passkeys
        .record_use(&passkey.id, ok.new_count, ok.backup_state, now)
        .await
        .map_err(store_err)?
    {
        PasskeyUse::Recorded => {}
        PasskeyUse::CloneSuspected => {
            let revoked = revoke_for_clone(svc, &passkey.id).await?;
            svc.record(clone_event(serde_json::json!({
                "stored_count": passkey.sign_count,
                "presented_count": ok.new_count,
                "confirmations_revoked": revoked,
            })))
            .await;
            return Err(clone_refused());
        }
        PasskeyUse::NotFound => return Err(unknown()),
    }
    Ok(passkey)
}

/// A passkey flagged as a suspected clone: every step-up it signed is
/// revoked — whoever holds the copy must not keep a window it opened.
async fn revoke_for_clone(svc: &StepUpService, passkey_id: &str) -> Result<u64, ApiError> {
    svc.confirmations
        .delete_for_passkey(passkey_id)
        .await
        .map_err(store_err)
}

impl IntoResponse for super::StepUpRefusal {
    fn into_response(self) -> Response {
        (StatusCode::FORBIDDEN, Json(self)).into_response()
    }
}
