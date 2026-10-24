//! The OrgSecurityEventLog API (ADR-0073 §7, §8). Read-only: the log is
//! append-only and nothing here writes it.
//!
//! - `GET /v1/orgs/{org}/security/events` — an Org's log: its own events,
//!   plus the Org-less ones (passkeys, confirmations) about its members.
//!   Administer on the Org.
//! - `GET /v1/me/security/events` — your own events, whoever you are.
//! - `GET /v1/security/events` — the whole install. Global Administer.
//! - `GET /v1/orgs/{org}/security/summary` — what an Org's Security page
//!   and the "step-up enforced; N admins have no passkey" banner show.
//!   Administer on the Org.
//!
//! Each list is newest first, filtered by `?kind=a,b`, paged by
//! `?before=<seq>` (the `next_before` of the previous page) and `?limit`.

use std::collections::BTreeSet;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_identity::{
    security_event_kind as kind, Action, Role, Scope, SecurityEvent, SecurityEventFilter,
};

use super::store_err;
use crate::{authenticate, ApiError, AppState};

/// The default page size.
pub const DEFAULT_EVENT_LIMIT: u32 = 50;
/// The largest page.
pub const MAX_EVENT_LIMIT: u32 = 200;
/// How many recent refusals the summary carries, and from how far back.
const RECENT_REFUSALS: u32 = 20;
const RECENT_REFUSALS_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// `?kind&before&limit`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    /// Comma-separated event kinds (`step_up.refused,passkey.reset`); none =
    /// every kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// Only events with `seq` below this (the previous page's
    /// `next_before`).
    #[serde(default)]
    pub before: Option<i64>,
    /// At most this many (default 50, at most 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One OrgSecurityEventLog row.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecurityEventDto {
    /// Orders the log; pass the last one's as `before` to page.
    pub seq: i64,
    pub id: String,
    /// `None`: not bound to one Org (a passkey, a confirmation).
    pub org: Option<String>,
    /// Unix ms.
    pub at: i64,
    /// `step_up.confirmed`, `step_up.refused`, `op.authorised`,
    /// `op.completed`, `request.raised|confirmed|executed|failed`,
    /// `passkey.enrolled|removed|reset|break_glass_reset|clone_suspected`,
    /// `guard.not_consulted`.
    pub kind: String,
    /// The human/principal it is about.
    pub subject: String,
    /// `session:cookie`, `session:bearer`, `api_token:<id>` or
    /// `operator:break-glass`.
    pub caller: String,
    pub op: Option<String>,
    pub target: Option<String>,
    /// The StepUpConfirmation it was authorised by / that it records.
    pub confirmation_id: Option<String>,
    /// The StepUpConfirmationRequest it belongs to.
    pub request_id: Option<String>,
    pub passkey_id: Option<String>,
    /// SHA-256 of the session (never the session id).
    pub session_hash: Option<String>,
    #[schema(value_type = Object)]
    pub detail: serde_json::Value,
}

impl From<SecurityEvent> for SecurityEventDto {
    fn from(e: SecurityEvent) -> Self {
        Self {
            seq: e.seq,
            id: e.id,
            org: e.org,
            at: e.at,
            kind: e.kind,
            subject: e.subject,
            caller: e.caller,
            op: e.op,
            target: e.target,
            confirmation_id: e.confirmation_id,
            request_id: e.request_id,
            passkey_id: e.passkey_id,
            session_hash: e.session_hash,
            detail: e.detail,
        }
    }
}

/// A page of the log.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecurityEventsResponse {
    pub events: Vec<SecurityEventDto>,
    /// Pass as `before` for the next page; absent on the last.
    pub next_before: Option<i64>,
}

/// `GET /v1/orgs/{org}/security/summary`.
#[derive(Debug, Serialize, ToSchema)]
pub struct OrgSecuritySummary {
    /// Step-up is enforced on this install.
    pub enforced: bool,
    /// Your role in the Org (`owner`, `admin`, …).
    pub caller_role: Option<String>,
    /// Admins of the Org (Admin/Owner bindings on the Org, and — for a
    /// global administrator only — the bootstrap owners) who hold no usable
    /// passkey: every crown jewel is refused for them until they enroll one.
    pub admins_without_passkey: Vec<String>,
    pub admins_total: u64,
    /// Bootstrap owners entries that match only a verified email at login
    /// and name no Principal: not counted above, since no passkey is ever
    /// stored under one (a global administrator's view; 0 otherwise).
    pub unresolved_owner_entries: u64,
    /// The last 20 refusals in the Org, from the last 24 hours.
    pub recent_refusals: Vec<SecurityEventDto>,
    /// StepUpConfirmationRequests waiting for a human in this Org.
    pub pending_requests: u64,
}

fn filter_of(q: &EventsQuery) -> SecurityEventFilter {
    SecurityEventFilter {
        org: None,
        subject: None,
        kinds: q
            .kind
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .collect(),
        before_seq: q.before,
        limit: q
            .limit
            .unwrap_or(DEFAULT_EVENT_LIMIT)
            .clamp(1, MAX_EVENT_LIMIT),
    }
}

async fn page(
    st: &AppState,
    filter: SecurityEventFilter,
) -> Result<SecurityEventsResponse, ApiError> {
    // Step-up off: there is no log to read (nothing is recorded).
    let Some(svc) = st.step_up.service() else {
        return Ok(SecurityEventsResponse {
            events: vec![],
            next_before: None,
        });
    };
    let events = svc.events.list(&filter).await.map_err(store_err)?;
    let next_before = (events.len() == filter.limit as usize)
        .then(|| events.last().map(|e| e.seq))
        .flatten();
    Ok(SecurityEventsResponse {
        events: events.into_iter().map(SecurityEventDto::from).collect(),
        next_before,
    })
}

/// An Org's security event log.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/security/events",
    params(
        ("org" = String, Path, description = "org"),
        ("kind" = Option<String>, Query, description = "comma-separated event kinds"),
        ("before" = Option<i64>, Query, description = "only events with seq below this (paging)"),
        ("limit" = Option<u32>, Query, description = "page size (default 50, max 200)")
    ),
    summary = "An Org's security event log (ADR-0073 §7)",
    responses(
        (status = 200, body = SecurityEventsResponse),
        (status = 401, description = "not signed in"),
        (status = 403, description = "not an admin of the Org")
    )
)]
pub(crate) async fn org_security_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<SecurityEventsResponse>, ApiError> {
    let caller = authenticate(&st, &headers, Action::Administer).await?;
    let scope = Scope::Org(org.clone());
    if !(caller.may_globally(Action::Administer)
        || caller.may(&st, Action::Administer, &scope).await?)
    {
        return Err(ApiError::Forbidden);
    }
    let mut filter = filter_of(&q);
    filter.org = Some(org);
    let mut page = page(&st, filter).await?;
    // An API token never reads its owner's passkeys (fe777ed): not on
    // `/v1/me/security`, and not here either (redteam auth-3).
    let token = caller.grant.is_some();
    for e in &mut page.events {
        redact_orgless_confirmation(e);
        if token {
            redact_passkey(e);
        }
    }
    Ok(Json(page))
}

/// A row as an API token sees it: which passkey (its id, its label) is
/// left out — the passkey lifecycle and every confirmation still show
/// who, when and what.
fn redact_passkey(e: &mut SecurityEventDto) {
    e.passkey_id = None;
    if !e.kind.starts_with("passkey.") {
        return;
    }
    if let Some(d) = e.detail.as_object_mut() {
        d.remove("label");
    }
    // Enrolment's target is the passkey's label; a reset's names the member.
    if e.kind != kind::PASSKEY_RESET && e.kind != kind::PASSKEY_BREAK_GLASS_RESET {
        e.target = None;
    }
}

/// An Org-less `step_up.confirmed` in an Org's view: who confirmed, when,
/// with which passkey and in which session — not what it was for, which may
/// be another Org's business (or none of this Org's).
fn redact_orgless_confirmation(e: &mut SecurityEventDto) {
    if e.org.is_some() || e.kind != kind::STEP_UP_CONFIRMED {
        return;
    }
    e.op = None;
    e.target = None;
    let mut kept = serde_json::Map::new();
    for k in ["kind", "expires_at"] {
        if let Some(v) = e.detail.get(k) {
            kept.insert(k.to_string(), v.clone());
        }
    }
    e.detail = serde_json::Value::Object(kept);
}

/// Your own security events.
#[utoipa::path(
    get,
    path = "/v1/me/security/events",
    params(
        ("kind" = Option<String>, Query, description = "comma-separated event kinds"),
        ("before" = Option<i64>, Query, description = "only events with seq below this (paging)"),
        ("limit" = Option<u32>, Query, description = "page size (default 50, max 200)")
    ),
    summary = "My security events (ADR-0073 §7)",
    responses(
        (status = 200, body = SecurityEventsResponse),
        (status = 401, description = "not signed in"),
        (status = 403, description = "an API token: it never reads its owner's security history")
    )
)]
pub(crate) async fn my_security_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Json<SecurityEventsResponse>, ApiError> {
    let caller = authenticate(&st, &headers, Action::Read).await?;
    // A token is scoped to one Org or Project; its owner's whole security
    // history (every Org, every passkey, every confirmation) is not in that
    // scope (git-bug fe777ed). A session — cookie or Bearer — reads it.
    if caller.grant.is_some() {
        return Err(ApiError::Denied(
            "an API token cannot read its owner's security events; use a signed-in session".into(),
        ));
    }
    let mut filter = filter_of(&q);
    filter.subject = Some(caller.principal.subject);
    Ok(Json(page(&st, filter).await?))
}

/// The install-wide security event log.
#[utoipa::path(
    get,
    path = "/v1/security/events",
    params(
        ("kind" = Option<String>, Query, description = "comma-separated event kinds"),
        ("before" = Option<i64>, Query, description = "only events with seq below this (paging)"),
        ("limit" = Option<u32>, Query, description = "page size (default 50, max 200)")
    ),
    summary = "The install-wide security event log (ADR-0073 §7)",
    responses(
        (status = 200, body = SecurityEventsResponse),
        (status = 401, description = "not signed in"),
        (status = 403, description = "not a global administrator")
    )
)]
pub(crate) async fn security_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Json<SecurityEventsResponse>, ApiError> {
    let caller = authenticate(&st, &headers, Action::Read).await?;
    if !caller.may_globally(Action::Administer) {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(page(&st, filter_of(&q)).await?))
}

fn role_name(r: Role) -> String {
    format!("{r:?}").to_lowercase()
}

/// The Org's admins: Admin/Owner bindings on the Org scope — plus, for a
/// global caller only, the bootstrap owners entries. Who the install's
/// owners are, and which of them hold no passkey, is install posture: an Org
/// admin must not learn it from their Org's summary.
///
/// Returns the admins and how many owners entries match only by email
/// (not counted: no passkey is ever stored under one — git-bug 08d676d).
async fn org_admins(
    st: &AppState,
    org: &str,
    include_owners: bool,
) -> Result<(BTreeSet<String>, u64), ApiError> {
    let mut admins = BTreeSet::new();
    if let Some(rbac) = st.rbac.as_ref() {
        for b in rbac.bindings(org).await.map_err(store_err)? {
            if b.scope == Scope::Org(org.to_string()) && b.role >= Role::Admin {
                admins.insert(b.subject);
            }
        }
    }
    let mut email_matchers = 0;
    if include_owners {
        let entries: Vec<String> = st
            .oauth_providers
            .iter()
            .flat_map(|p| p.owners().iter().cloned())
            .collect();
        let owners = super::resolve_owner_entries(st.identities.as_deref(), &entries).await;
        admins.extend(owners.principals);
        email_matchers = owners.email_matchers.len() as u64;
    }
    Ok((admins, email_matchers))
}

/// What an Org's Security page shows.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/security/summary",
    params(("org" = String, Path, description = "org")),
    summary = "An Org's step-up posture: admins without a passkey, recent refusals (ADR-0073 §8)",
    responses(
        (status = 200, body = OrgSecuritySummary),
        (status = 401, description = "not signed in"),
        (status = 403, description = "not an admin of the Org")
    )
)]
pub(crate) async fn org_security_summary(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<OrgSecuritySummary>, ApiError> {
    let scope = Scope::Org(org.clone());
    let caller = authenticate(&st, &headers, Action::Read).await?;
    let role = caller.role_in(&st, &scope).await?;
    let global = caller.may_globally(Action::Administer);
    if !(global || role.is_some_and(|r| r.allows(Action::Administer))) {
        return Err(ApiError::Forbidden);
    }
    let (admins, unresolved_owner_entries) = org_admins(&st, &org, global).await?;
    let admins_total = admins.len() as u64;
    let Some(svc) = st.step_up.service() else {
        return Ok(Json(OrgSecuritySummary {
            enforced: false,
            caller_role: role.map(role_name),
            admins_without_passkey: vec![],
            admins_total,
            unresolved_owner_entries,
            recent_refusals: vec![],
            pending_requests: 0,
        }));
    };
    let list: Vec<String> = admins.into_iter().collect();
    let admins_without_passkey = super::admins_without_passkey(svc.passkeys.as_ref(), &list)
        .await
        .map_err(store_err)?;
    let now = st.clock.now().await.0;
    let recent_refusals = svc
        .events
        .list(&SecurityEventFilter {
            org: Some(org.clone()),
            subject: None,
            kinds: vec![kind::STEP_UP_REFUSED.to_string()],
            before_seq: None,
            limit: RECENT_REFUSALS,
        })
        .await
        .map_err(store_err)?
        .into_iter()
        .filter(|e| e.at >= now - RECENT_REFUSALS_WINDOW_MS)
        .map(SecurityEventDto::from)
        .collect();
    let pending_requests = svc
        .confirmations
        .pending_in_org(&org, now)
        .await
        .map_err(store_err)?;
    Ok(Json(OrgSecuritySummary {
        enforced: true,
        caller_role: role.map(role_name),
        admins_without_passkey,
        admins_total,
        unresolved_owner_entries,
        recent_refusals,
        pending_requests,
    }))
}
