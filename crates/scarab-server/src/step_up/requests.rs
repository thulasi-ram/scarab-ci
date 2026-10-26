//! StepUpConfirmationRequest (ADR-0073 §6): what a machine — an API token,
//! or a session presented as `Bearer` (a CLI) — gets instead of a step-up it
//! can never give.
//!
//! **Raise.** A crown jewel called by such a caller is parked as a request
//! for the human it acts for (the token's owner, the session's principal):
//! the body sealed under the KEK with the AAD bound to the request id, the
//! action digest, a redacted summary, a 5-minute expiry. The 403 carries a
//! one-time `confirmation_url`. At most
//! [`MAX_PENDING_STEP_UP_REQUESTS`](scarab_identity::MAX_PENDING_STEP_UP_REQUESTS) may wait
//! per requester and
//! [`MAX_PENDING_STEP_UP_REQUESTS_PER_HUMAN`](scarab_identity::MAX_PENDING_STEP_UP_REQUESTS_PER_HUMAN) per
//! human, counted atomically with the insert (then 429); a body bigger than
//! the operation's [`Op::max_request_body`] raises none (413). A
//! PluginPrincipal never raises one, nor does a requester that could never
//! execute the operation ([`requestable`]).
//!
//! **Confirm.** Only that human, in their own browser, with their own
//! passkey: `GET /v1/step-up/requests/{id}` shows what would run (anyone
//! else gets 404 — another admin learns nothing), `…/options` mints a
//! challenge committed to the request's digest, and `…/confirm` verifies the
//! assertion, moves the request pending → executing exactly once
//! (compare-and-swap; 409 if it already ran, 410 once expired), and
//! **replays** it.
//!
//! **Replay.** The stored method, path, query and body run through the real
//! router — so the real handler's validation and authorization decide —
//! inside the [`REPLAY`] task-local, which nothing on the network can set.
//! Under it, `authenticate` ignores headers and rebuilds the requester as it
//! is NOW (the token by id, still live; the Bearer session, still valid),
//! narrowed to the human's live authority, and `require` is satisfied only by
//! the confirmed operation with the confirmed digest — a request runs once,
//! exactly as raised, never wider. The response is handed to the human and
//! never stored (a minted token's plaintext shows once); the sealed body is
//! cleared when the request settles.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_identity::{
    security_event_kind as kind, Action, BeginExecution, ChallengePurpose, ConfirmationKind,
    Principal, RequestState, Role, StepUpConfirmation, StepUpRequest, StepUpRequestPendingCaps,
    StepUpRequestRaiseOutcome, StepUpRequester, STEP_UP_REQUEST_TTL_MS, STEP_UP_WINDOW_MS,
};

use super::ceremony::{
    browser_session, challenge_invalid, consume, service, verify_assertion, StepUpConfirmRequest,
};
use super::{
    descriptors, event, random_id, store_err, Op, OpSummary, OrgOf, Prepared, StepUpMachineUse,
    StepUpRefusal, StepUpService, WebauthnChallengeDto,
};
use crate::{authenticate, ApiError, AppState, AuthVia, Caller, SessionFacts};

/// The most of a replayed response handed back to the human.
const MAX_RESULT_BODY: usize = 1024 * 1024;

/// The longest failure text kept on a settled request (`result_summary`). A
/// success keeps only its status: its body (a minted token, say) is never
/// stored.
const MAX_RESULT_SUMMARY: usize = 200;

const MSG_REQUEST: &str = "This needs a step-up confirmation by a human with a passkey. A \
     confirmation request was raised: open `confirmation_url` in a browser signed in as the \
     human this credential acts for, and confirm it with a passkey within 5 minutes. It then \
     runs once, exactly as sent.";

/// A session id — a bearer credential — held only to seal it into a Bearer
/// session's request. Its `Debug` never prints it.
#[derive(Clone)]
pub(crate) struct SessionSecret(pub(crate) String);

impl std::fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionSecret(<redacted>)")
    }
}

/// Who raised the request being replayed, as the replay re-checks it.
#[derive(Debug, Clone)]
pub(crate) enum ReplayRequester {
    /// An issued API token, by row id.
    ApiToken(String),
    /// A session presented as Bearer, by its id (opened from the seal).
    BearerSession(SessionSecret),
}

/// The authority a replay runs under: one confirmed request.
#[derive(Debug, Clone)]
pub(crate) struct ReplayAuthority {
    pub(crate) request_id: String,
    /// The `request`-kind StepUpConfirmation the human made.
    pub(crate) confirmation_id: String,
    /// The one operation it may satisfy …
    pub(crate) op_id: String,
    /// … with exactly this digest …
    pub(crate) digest: Vec<u8>,
    /// … changing no more than the human was shown (a conditional handler
    /// re-judges its loosenings at replay; any it did not confirm refuse).
    pub(crate) changes: Vec<String>,
    pub(crate) requester: ReplayRequester,
    pub(crate) for_human: String,
    /// A Bearer session's global roles are capped at what the session held
    /// when it raised the request.
    pub(crate) role_cap: Option<Role>,
}

tokio::task_local! {
    /// Set by [`confirm_request`] around its replay — and nowhere else. No
    /// header, body or route can set it.
    static REPLAY: Arc<ReplayAuthority>;
}

/// The request being replayed on this task, if any.
pub(crate) fn replay_authority() -> Option<Arc<ReplayAuthority>> {
    REPLAY.try_with(Arc::clone).ok()
}

/// The requester a machine caller raises a request as: an API token by id,
/// a Bearer session by its hash. `None` for anyone else (a browser, or the
/// dev-insecure synthetic caller), who never raises one.
pub(crate) fn requester_of(caller: &Caller) -> Option<StepUpRequester> {
    if let Some(token) = &caller.grant {
        return Some(StepUpRequester::ApiToken {
            id: token.id.clone(),
        });
    }
    match (&caller.via, &caller.session) {
        (Some(AuthVia::Bearer), Some(s)) => Some(StepUpRequester::BearerSession {
            session_hash: s.id_hash.clone(),
        }),
        _ => None,
    }
}

/// Why a machine caller may not raise a StepUpConfirmationRequest for an
/// operation: it could never execute it, so the request would only ever end
/// `failed` — and would hand a compromised machine a plausible prompt to
/// send its human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unrequestable {
    /// Only a human in a browser can perform it ([`StepUpMachineUse::BrowserOnly`]).
    BrowserOnly,
    /// An API token can never perform it ([`StepUpMachineUse::NotByToken`]).
    NotByToken,
    /// It acts on a scope (an Org, or an Org-level write for a Project
    /// token) the API token's scope does not reach.
    OutsideTokenScope,
}

impl Unrequestable {
    /// The `code` of the 403 and the `reason` of the `step_up.refused` event.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::BrowserOnly => "browser_session_required",
            Self::NotByToken => "not_by_api_token",
            Self::OutsideTokenScope => "outside_token_scope",
        }
    }

    pub(crate) fn message(self, op: &Op) -> String {
        let what = op.label;
        match self {
            Self::BrowserOnly => format!(
                "{what} can only be done by a human in a signed-in browser — never with an API \
                 token or a session presented as Bearer — so no confirmation request was raised"
            ),
            Self::NotByToken => format!(
                "{what} can never be done with an API token, so no confirmation request was raised"
            ),
            Self::OutsideTokenScope => format!(
                "{what} acts outside this API token's scope, so it could never run with it and \
                 no confirmation request was raised"
            ),
        }
    }
}

/// May `p`'s machine caller raise a request for `op` — could it ever execute
/// it once confirmed? Checked BEFORE a request is raised. The handler stays
/// the authority on what runs; this only refuses what it is certain to
/// refuse.
pub(crate) fn requestable(op: &Op, p: &Prepared) -> Result<(), Unrequestable> {
    if op.machine == StepUpMachineUse::BrowserOnly {
        return Err(Unrequestable::BrowserOnly);
    }
    if let Some(token) = &p.caller.grant {
        if op.machine == StepUpMachineUse::NotByToken {
            return Err(Unrequestable::NotByToken);
        }
        // An Org-bound operation on a scope the token does not cover (a
        // Project token asking for an Org-level write, another Org, or
        // naming none at all) is refused by its handler for the token's
        // scope, whoever confirms it.
        if op.org_of != OrgOf::None
            && !p
                .requested_scope
                .as_ref()
                .is_some_and(|scope| token.covers(scope))
        {
            return Err(Unrequestable::OutsideTokenScope);
        }
    }
    Ok(())
}

/// What the KEK seals.
#[derive(Serialize, Deserialize)]
struct SealedRequest {
    /// The request body, base64.
    body: String,
    /// A Bearer session requester's session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

fn aad(id: &str) -> Vec<u8> {
    format!("step-up-request:{id}").into_bytes()
}

fn requester_label(caller: &Caller) -> String {
    match &caller.grant {
        Some(t) => format!("API token \u{201c}{}\u{201d} ({})", t.name, t.id),
        None => "a CLI session (Bearer)".to_string(),
    }
}

/// Raise a StepUpConfirmationRequest for `p`'s caller and answer the 403
/// that carries its link (or 429 when the requester has too many waiting).
pub(crate) async fn raise<T>(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    summary: &OpSummary,
    requester: StepUpRequester,
    mut refusal: StepUpRefusal,
) -> Result<T, ApiError> {
    let svc = &p.svc;
    let caller = &p.caller;
    let human = caller.principal.subject.clone();
    let now = st.clock.now().await.0;

    // The sealed body is parked until the request settles or is reaped:
    // bounded per operation, well under the guard's limit.
    if p.body.len() > op.max_request_body {
        let mut ev = event(
            kind::STEP_UP_REFUSED,
            &human,
            caller.event_label(),
            p.org.clone(),
            now,
        );
        ev.op = Some(op.id.to_string());
        ev.target = Some(summary.target.clone());
        ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
        ev.detail = serde_json::json!({
            "mode": refusal.mode,
            "reason": "request body too large",
            "bytes": p.body.len(),
            "limit": op.max_request_body,
        });
        svc.record_refusal(ev).await;
        return Err(super::ceremony::ceremony(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_body_too_large",
            format!(
                "the body ({} bytes) is larger than a confirmation request for {} may carry ({} \
                 bytes), so none was raised; do it from a signed-in browser instead",
                p.body.len(),
                op.label,
                op.max_request_body
            ),
        ));
    }

    let id = random_id();
    let session_id = match &requester {
        StepUpRequester::BearerSession { .. } => caller.session.as_ref().map(|s| s.id.0.clone()),
        StepUpRequester::ApiToken { .. } => None,
    };
    let plaintext = serde_json::to_vec(&SealedRequest {
        body: STANDARD.encode(&p.body),
        session_id,
    })
    .expect("a sealed request serializes");
    let sealed = svc.sealer.seal(&plaintext, &aad(&id)).map_err(|e| {
        ApiError::Unavailable(format!(
            "could not seal the confirmation request, so none was raised: {e}"
        ))
    })?;
    let label = requester_label(caller);
    let session_role_cap = match &requester {
        StepUpRequester::BearerSession { .. } => caller.principal.roles.iter().copied().max(),
        StepUpRequester::ApiToken { .. } => None,
    };
    let expires_at = now + STEP_UP_REQUEST_TTL_MS;
    let row = StepUpRequest {
        id: id.clone(),
        org: p.org.clone(),
        op: op.id.to_string(),
        method: p.method.clone(),
        path: p.path.clone(),
        query: p.query.clone(),
        sealed: Some(sealed),
        action_digest: p.digest.to_vec(),
        summary: serde_json::json!({
            "operation": summary.operation,
            "target": summary.target,
            "changes": summary.changes,
            "requester": label,
        }),
        for_human: human.clone(),
        requester: requester.clone(),
        session_role_cap,
        created_at: now,
        expires_at,
        state: RequestState::Pending,
        confirmed_by: None,
        confirmed_at: None,
        confirmation_id: None,
        executed_at: None,
        result_status: None,
        result_summary: None,
    };
    // The caps are counted atomically with the insert: per credential, and
    // per human across every credential acting for them.
    let full = match svc
        .confirmations
        .raise_request(&row, StepUpRequestPendingCaps::DEFAULT, now)
        .await
        .map_err(store_err)?
    {
        StepUpRequestRaiseOutcome::Raised => None,
        StepUpRequestRaiseOutcome::RequesterFull { pending } => Some((
            pending,
            "too many pending confirmation requests",
            format!(
                "{pending} step-up confirmation requests from this credential are already \
                 waiting for {human}; confirm them, or let them expire (5 minutes), first"
            ),
        )),
        StepUpRequestRaiseOutcome::HumanFull { pending } => Some((
            pending,
            "too many pending confirmation requests for this human",
            format!(
                "{pending} step-up confirmation requests are already waiting for {human}; \
                 confirm them, or let them expire (5 minutes), first"
            ),
        )),
    };
    if let Some((pending, reason, message)) = full {
        let mut ev = event(
            kind::STEP_UP_REFUSED,
            &human,
            caller.event_label(),
            p.org.clone(),
            now,
        );
        ev.op = Some(op.id.to_string());
        ev.target = Some(summary.target.clone());
        ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
        ev.detail = serde_json::json!({
            "mode": refusal.mode,
            "reason": reason,
            "pending": pending,
        });
        svc.record_refusal(ev).await;
        return Err(ApiError::TooManyRequests(message));
    }

    let mut ev = event(
        kind::REQUEST_RAISED,
        &human,
        caller.event_label(),
        p.org.clone(),
        now,
    );
    ev.op = Some(op.id.to_string());
    ev.target = Some(summary.target.clone());
    ev.request_id = Some(id.clone());
    ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({
        "requested_via": requester.via(),
        "requested_by": label,
        "expires_at": expires_at,
        "summary": summary,
    });
    svc.record(ev).await;

    refusal.message = MSG_REQUEST.to_string();
    refusal.confirmation_url = Some(format!(
        "{}/security/confirm/{id}",
        st.public_url.trim_end_matches('/')
    ));
    refusal.request_id = Some(id.clone());
    refusal.expires_at = Some(expires_at);
    super::refuse_with(st, op, p, refusal, "confirmation request raised", Some(id)).await
}

/// The caller a replay runs as (ADR-0073 §6): the requester re-checked NOW
/// and narrowed to its human's live authority. Never from headers.
pub(crate) async fn replay_caller(st: &AppState, a: &ReplayAuthority) -> Result<Caller, ApiError> {
    let now = st.clock.now().await.0;
    match &a.requester {
        ReplayRequester::ApiToken(id) => {
            let store = st.api_tokens.as_ref().ok_or(ApiError::Unauthorized)?;
            let token = store
                .by_id(id)
                .await
                .map_err(|_| ApiError::Unauthorized)?
                .ok_or(ApiError::Unauthorized)?;
            // Revoked or expired since it asked: it asks for nothing now.
            if !token.is_live(now) || token.owner_subject != a.for_human {
                return Err(ApiError::Unauthorized);
            }
            // The token's grant stays attached, so every decision is
            // min(its ceiling and scope, the owner's live role) — and "a token
            // may not mint tokens" still holds.
            let roles = crate::token_owner_roles(st, &token.owner_subject).await;
            Ok(Caller {
                principal: Principal {
                    subject: token.owner_subject.clone(),
                    display_name: Some(format!("API token \u{201c}{}\u{201d}", token.name)),
                    roles,
                },
                grant: Some(token),
                via: Some(AuthVia::Bearer),
                session: None,
            })
        }
        ReplayRequester::BearerSession(sid) => {
            let sessions = st.sessions.as_ref().ok_or(ApiError::Unauthorized)?;
            let session = sessions
                .get(&sid.0)
                .await
                .map_err(|_| ApiError::Unauthorized)?
                .ok_or(ApiError::Unauthorized)?;
            // Logged out or expired since it asked: nothing runs.
            if !session.is_valid(now) || session.principal.subject != a.for_human {
                return Err(ApiError::Unauthorized);
            }
            // Global roles: the human's LIVE bootstrap role, never above what
            // the session held when it asked. Bindings are read live by every
            // scoped decision, as for any session.
            let roles = crate::token_owner_roles(st, &a.for_human)
                .await
                .into_iter()
                .filter(|r| a.role_cap.is_some_and(|cap| *r <= cap))
                .collect();
            Ok(Caller {
                principal: Principal {
                    subject: session.principal.subject.clone(),
                    display_name: session.principal.display_name.clone(),
                    roles,
                },
                grant: None,
                via: Some(AuthVia::Bearer),
                session: Some(SessionFacts {
                    id: sid.clone(),
                    id_hash: crate::session_hash(&session.id),
                    fresh_login_until: None,
                }),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// The confirm endpoints
// ---------------------------------------------------------------------------

/// A StepUpConfirmationRequest as its human (or its polling requester)
/// sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct StepUpRequestDto {
    pub id: String,
    /// The operation id (`secret.put`).
    pub op: String,
    /// What would run, redacted (secret values read `(hidden)`).
    pub summary: OpSummary,
    /// Who asked: `API token “ci-bot” (<id>)` or `a CLI session (Bearer)`.
    pub requested_by: String,
    /// `api_token` or `bearer_session`.
    pub requested_via: String,
    /// The only human who can confirm it.
    pub for_human: String,
    pub org: Option<String>,
    pub method: String,
    pub path: String,
    /// The raw query string the operation runs with (empty = none) — part
    /// of what runs, so the confirm page shows it.
    pub query: String,
    /// `pending`, `executing`, `executed`, `failed` or `expired`.
    pub state: String,
    pub created_at: i64,
    /// A pending request past this can no longer be confirmed.
    pub expires_at: i64,
    pub confirmed_by: Option<String>,
    pub confirmed_at: Option<i64>,
    pub executed_at: Option<i64>,
    /// The replayed operation's HTTP status, once it ran.
    pub result_status: Option<i32>,
}

impl StepUpRequestDto {
    fn new(r: &StepUpRequest, now: i64) -> Self {
        let text = |k: &str| {
            r.summary
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let changes = r
            .summary
            .get("changes")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id: r.id.clone(),
            op: r.op.clone(),
            summary: OpSummary {
                operation: text("operation"),
                target: text("target"),
                changes,
            },
            requested_by: text("requester"),
            requested_via: r.requester.via().to_string(),
            for_human: r.for_human.clone(),
            org: r.org.clone(),
            method: r.method.clone(),
            path: r.path.clone(),
            query: r.query.clone(),
            state: r.state_label(now).to_string(),
            created_at: r.created_at,
            expires_at: r.expires_at,
            confirmed_by: r.confirmed_by.clone(),
            confirmed_at: r.confirmed_at,
            executed_at: r.executed_at,
            result_status: r.result_status,
        }
    }
}

/// What a confirmed request did.
#[derive(Debug, Serialize, ToSchema)]
pub struct StepUpRequestResultDto {
    pub request_id: String,
    /// `executed` (the operation answered 2xx) or `failed`.
    pub state: String,
    /// The operation's HTTP status.
    pub result_status: Option<i32>,
    /// The operation's response body (JSON, or a string), returned once
    /// and never stored — a minted token's plaintext shows only here.
    #[schema(value_type = Object)]
    pub result_body: serde_json::Value,
}

/// Is `subject` the human `for_human` names — itself, or a Principal it was
/// merged into?
async fn is_for_human(st: &AppState, subject: &str, for_human: &str) -> bool {
    if subject == for_human {
        return true;
    }
    match st.identities.as_ref() {
        Some(store) => store
            .aliases_of(subject)
            .await
            .unwrap_or_default()
            .iter()
            .any(|a| a == for_human),
        None => false,
    }
}

/// The request, if `caller` (a browser session) is its human; 404 otherwise,
/// whoever asks — another admin learns nothing, not even that it exists.
async fn load_as_human(
    st: &AppState,
    svc: &StepUpService,
    id: &str,
    caller: &Caller,
) -> Result<StepUpRequest, ApiError> {
    let found = svc.confirmations.get_request(id).await.map_err(store_err)?;
    match found {
        Some(r) if is_for_human(st, &caller.principal.subject, &r.for_human).await => Ok(r),
        other => Err(probe_refused(st, svc, id, caller, other.as_ref()).await),
    }
}

/// The 404 for a request that does not exist or is not the caller's —
/// recorded (git-bug 69e06b5; ADR-0073 §6's confused deputy) as one reason
/// for both, so the caller's own event log is no oracle for existence.
///
/// A probe of a request that exists is ALSO recorded where its owner and
/// its Org can see it (redteam auth-2): as the request's human's event,
/// in the request's Org, naming the prober. That row is never the
/// prober's (another subject), so the prober's own log reads the same for
/// a request that exists and one that does not; an admin of that Org
/// already sees the request's `request.raised` there.
async fn probe_refused(
    st: &AppState,
    svc: &StepUpService,
    id: &str,
    caller: &Caller,
    found: Option<&StepUpRequest>,
) -> ApiError {
    let now = st.clock.now().await.0;
    super::ceremony::record_ceremony_refusal(svc, caller, "request_not_found", Some(id), now).await;
    if let Some(r) = found {
        let mut ev = event(
            kind::STEP_UP_REFUSED,
            &r.for_human,
            caller.event_label(),
            r.org.clone(),
            now,
        );
        ev.op = Some(r.op.clone());
        ev.request_id = Some(id.to_string());
        ev.detail = serde_json::json!({
            "mode": "ceremony",
            "reason": "request_not_found",
            "probed_by": caller.principal.subject,
        });
        svc.record_refusal(ev).await;
    }
    ApiError::NotFound
}

/// 410 once expired, 409 once confirmed.
fn confirmable(r: &StepUpRequest, now: i64) -> Result<(), ApiError> {
    if r.is_expired(now) {
        return Err(ApiError::Gone(
            "this confirmation request expired (5 minutes); run the operation again to raise a \
             new one"
                .into(),
        ));
    }
    if r.state != RequestState::Pending {
        return Err(ApiError::Conflict(format!(
            "this confirmation request was already confirmed (it is {})",
            r.state.as_str()
        )));
    }
    Ok(())
}

/// A StepUpConfirmationRequest: for its human (a browser session), or for
/// the requester polling its state.
#[utoipa::path(
    get,
    path = "/v1/step-up/requests/{id}",
    params(("id" = String, Path, description = "request id (from `confirmation_url`)")),
    summary = "A step-up confirmation request (ADR-0073 §6)",
    responses(
        (status = 200, body = StepUpRequestDto),
        (status = 401, description = "not signed in"),
        (status = 404, description = "no such request, or you are neither its human nor its requester; or step-up is off")
    )
)]
pub(crate) async fn get_request(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<StepUpRequestDto>, ApiError> {
    let svc = service(&st)?;
    let caller = authenticate(&st, &headers, Action::Read).await?;
    let Some(r) = svc
        .confirmations
        .get_request(&id)
        .await
        .map_err(store_err)?
    else {
        return Err(probe_refused(&st, &svc, &id, &caller, None).await);
    };
    let is_human = caller.is_browser_session()
        && is_for_human(&st, &caller.principal.subject, &r.for_human).await;
    let is_requester = requester_of(&caller).is_some_and(|me| me == r.requester);
    if !(is_human || is_requester) {
        return Err(probe_refused(&st, &svc, &id, &caller, Some(&r)).await);
    }
    let now = st.clock.now().await.0;
    Ok(Json(StepUpRequestDto::new(&r, now)))
}

/// A challenge whose answer confirms this request (committed to its digest).
#[utoipa::path(
    post,
    path = "/v1/step-up/requests/{id}/options",
    params(("id" = String, Path, description = "request id")),
    summary = "Begin confirming a step-up confirmation request (ADR-0073 §6)",
    responses(
        (status = 200, body = WebauthnChallengeDto, description = "PublicKeyCredentialRequestOptionsJSON under `public_key`"),
        (status = 400, description = "`no_passkey`: enroll one first"),
        (status = 403, description = "not a browser session"),
        (status = 404, description = "no such request of yours, or step-up is off"),
        (status = 409, description = "already confirmed"),
        (status = 410, description = "expired")
    )
)]
pub(crate) async fn request_options(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<WebauthnChallengeDto>, ApiError> {
    let svc = service(&st)?;
    let (caller, facts) = browser_session(&st, &headers, Action::Write).await?;
    let r = load_as_human(&st, &svc, &id, &caller).await?;
    let now = st.clock.now().await.0;
    confirmable(&r, now)?;
    let subject = caller.principal.subject.as_str();
    let passkeys = svc.usable_passkeys(subject).await?;
    if passkeys.is_empty() {
        return Err(super::ceremony::ceremony(
            StatusCode::BAD_REQUEST,
            "no_passkey",
            "you have no passkey; enroll one first",
        ));
    }
    let digest: [u8; 32] = r
        .action_digest
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::Conflict("the stored request is unreadable".into()))?;
    let (row, sent) = svc
        .mint_challenge(
            subject,
            &facts.id_hash,
            ChallengePurpose::Request,
            Some(digest),
            Some(r.id.clone()),
            None,
            Some(serde_json::json!({ "op": r.op, "summary": r.summary, "request_id": r.id })),
            now,
        )
        .await?;
    Ok(Json(WebauthnChallengeDto {
        id: row.id,
        public_key: scarab_webauthn::request_options(svc.rp(), &sent, &descriptors(&passkeys)),
        expires_at: row.expires_at,
    }))
}

/// Confirm a request with a passkey: it runs once, now, exactly as raised,
/// with the narrower of the requester's scope and your live role.
#[utoipa::path(
    post,
    path = "/v1/step-up/requests/{id}/confirm",
    params(("id" = String, Path, description = "request id")),
    request_body = StepUpConfirmRequest,
    summary = "Confirm a step-up confirmation request: it runs once (ADR-0073 §6)",
    responses(
        (status = 200, body = StepUpRequestResultDto, description = "it ran; `state` says whether the operation succeeded"),
        (status = 400, description = "challenge unknown/used/expired/for another request, or the assertion failed verification (`code` names the check)"),
        (status = 403, description = "not a browser session, or `clone_suspected`"),
        (status = 404, description = "no such request of yours, or step-up is off"),
        (status = 409, description = "already confirmed — it ran (or is running) once"),
        (status = 410, description = "expired")
    )
)]
pub(crate) async fn confirm_request(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<StepUpConfirmRequest>,
) -> Result<Json<StepUpRequestResultDto>, ApiError> {
    let svc = service(&st)?;
    let (caller, facts) = browser_session(&st, &headers, Action::Write).await?;
    let r = load_as_human(&st, &svc, &id, &caller).await?;
    let now = st.clock.now().await.0;
    confirmable(&r, now)?;
    let (row, sent) = consume(
        &svc,
        &req.challenge_id,
        &caller,
        &facts,
        &[ChallengePurpose::Request],
        now,
    )
    .await?;
    // The challenge must have been minted for THIS request: its signature
    // commits to this request's digest and nothing else.
    if row.request_id.as_deref() != Some(id.as_str())
        || row.action_digest.as_deref() != Some(r.action_digest.as_slice())
    {
        return Err(challenge_invalid());
    }
    let passkey = verify_assertion(&svc, &caller, &facts, &sent, req.credential, now).await?;

    // Exactly once: pending → executing, only while unexpired.
    let confirmation_id = random_id();
    let human = caller.principal.subject.clone();
    let r = match svc
        .confirmations
        .begin_execution(&id, &human, &confirmation_id, now)
        .await
        .map_err(store_err)?
    {
        BeginExecution::Begun(r) => *r,
        BeginExecution::NotFound => return Err(ApiError::NotFound),
        BeginExecution::Expired => {
            return Err(ApiError::Gone(
                "this confirmation request expired (5 minutes)".into(),
            ))
        }
        BeginExecution::NotPending(state) => {
            return Err(ApiError::Conflict(format!(
                "this confirmation request was already confirmed (it is {})",
                state.as_str()
            )))
        }
    };
    let confirmation = StepUpConfirmation {
        id: confirmation_id.clone(),
        principal: human.clone(),
        session_hash: facts.id_hash.clone(),
        passkey_id: passkey.id.clone(),
        kind: ConfirmationKind::Request,
        action_digest: Some(r.action_digest.clone()),
        request_id: Some(id.clone()),
        confirmed_at: now,
        expires_at: now + STEP_UP_WINDOW_MS,
        consumed_at: Some(now),
    };
    svc.confirmations
        .put_confirmation(&confirmation)
        .await
        .map_err(store_err)?;
    let requested_by = r
        .summary
        .get("requester")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let target = r
        .summary
        .get("target")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut ev = event(
        kind::STEP_UP_CONFIRMED,
        &human,
        caller.event_label(),
        r.org.clone(),
        now,
    );
    ev.op = Some(r.op.clone());
    ev.target = target.clone();
    ev.confirmation_id = Some(confirmation_id.clone());
    ev.request_id = Some(id.clone());
    ev.passkey_id = Some(passkey.id.clone());
    ev.session_hash = Some(facts.id_hash.clone());
    ev.detail = serde_json::json!({
        "kind": ConfirmationKind::Request.as_str(),
        "summary": r.summary,
    });
    // Logged, or not run: an unrecorded confirmation replays nothing.
    let logged = match svc.events.append(&ev).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::error!(error = %e, request = %id, "step_up.confirmed could not be recorded; not replaying");
            Err("the security event log is unavailable, so the request was not run".to_string())
        }
    };
    let mut ev = event(
        kind::REQUEST_CONFIRMED,
        &human,
        caller.event_label(),
        r.org.clone(),
        now,
    );
    ev.op = Some(r.op.clone());
    ev.target = target.clone();
    ev.confirmation_id = Some(confirmation_id.clone());
    ev.request_id = Some(id.clone());
    ev.passkey_id = Some(passkey.id.clone());
    ev.session_hash = Some(facts.id_hash.clone());
    ev.detail = serde_json::json!({
        "requested_via": r.requester.via(),
        "requested_by": requested_by,
        "confirmed_by": human,
    });
    svc.record(ev).await;

    // Open the sealed body; then replay.
    let opened = logged
        .and_then(|()| {
            r.sealed
                .as_deref()
                .ok_or_else(|| "the request has no sealed body".to_string())
        })
        .and_then(|sealed| {
            svc.sealer
                .open(sealed, &aad(&id))
                .map_err(|e| format!("the sealed request could not be opened: {e}"))
        })
        .and_then(|plain| {
            serde_json::from_slice::<SealedRequest>(&plain)
                .map_err(|e| format!("the sealed request is unreadable: {e}"))
        })
        .and_then(|s| {
            let body = STANDARD
                .decode(s.body.as_bytes())
                .map_err(|e| format!("the sealed request is unreadable: {e}"))?;
            let requester = match &r.requester {
                StepUpRequester::ApiToken { id } => ReplayRequester::ApiToken(id.clone()),
                StepUpRequester::BearerSession { .. } => ReplayRequester::BearerSession(
                    SessionSecret(s.session_id.ok_or("the sealed request lost its session")?),
                ),
            };
            Ok((body, requester))
        });
    let (status, result_body, failure) = match opened {
        Ok((body, requester)) => {
            let authority = ReplayAuthority {
                request_id: id.clone(),
                confirmation_id: confirmation_id.clone(),
                op_id: r.op.clone(),
                digest: r.action_digest.clone(),
                changes: r
                    .summary
                    .get("changes")
                    .and_then(|c| c.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                requester,
                // The confirmer: proven (above) to be the request's human
                // or the live Principal it was merged into — whom a merge
                // moved the requesting token to.
                for_human: human.clone(),
                role_cap: r.session_role_cap,
            };
            let (status, bytes) = replay(&st, authority, &r, body).await;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let body = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| serde_json::Value::String(text.clone()));
            let failure = (!status.is_success()).then_some(text);
            (Some(status), body, failure)
        }
        Err(why) => (None, serde_json::Value::String(why.clone()), Some(why)),
    };
    let state = match failure {
        None => RequestState::Executed,
        Some(_) => RequestState::Failed,
    };
    let result_status = status.map(|s| i32::from(s.as_u16()));
    let result_summary = match (&status, &failure) {
        (Some(s), None) => format!("HTTP {}", s.as_u16()),
        (Some(s), Some(text)) => format!("HTTP {}: {}", s.as_u16(), cut(text, MAX_RESULT_SUMMARY)),
        (None, Some(text)) => cut(text, MAX_RESULT_SUMMARY),
        (None, None) => String::new(),
    };
    let done = st.clock.now().await.0;
    if !svc
        .confirmations
        .finish_request(&id, state, result_status, Some(&result_summary), done)
        .await
        .map_err(store_err)?
    {
        tracing::error!(request = %id, "step-up request was not executing when it finished");
    }
    let mut ev = event(
        match state {
            RequestState::Executed => kind::REQUEST_EXECUTED,
            _ => kind::REQUEST_FAILED,
        },
        &r.for_human,
        caller.event_label(),
        r.org.clone(),
        done,
    );
    ev.op = Some(r.op.clone());
    ev.target = target;
    ev.confirmation_id = Some(confirmation_id);
    ev.request_id = Some(id.clone());
    ev.detail = serde_json::json!({
        "status": result_status,
        "result": result_summary,
        "requested_by": requested_by,
        "confirmed_by": human,
    });
    svc.record(ev).await;
    Ok(Json(StepUpRequestResultDto {
        request_id: id,
        state: state.as_str().to_string(),
        result_status,
        result_body,
    }))
}

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Run the stored request through the real router, under [`REPLAY`].
async fn replay(
    st: &AppState,
    authority: ReplayAuthority,
    r: &StepUpRequest,
    body: Vec<u8>,
) -> (StatusCode, Bytes) {
    use tower::ServiceExt;
    let uri = if r.query.is_empty() {
        r.path.clone()
    } else {
        format!("{}?{}", r.path, r.query)
    };
    let mut builder = Request::builder().method(r.method.as_str()).uri(uri);
    if !body.is_empty() {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let req = match builder.body(Body::from(body)) {
        Ok(req) => req,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Bytes::from(format!("the stored request is malformed: {e}")),
            )
        }
    };
    let router = crate::router(st.clone());
    let resp = match REPLAY.scope(Arc::new(authority), router.oneshot(req)).await {
        Ok(resp) => resp,
        Err(never) => match never {},
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_RESULT_BODY)
        .await
        .unwrap_or_default();
    (status, bytes)
}
