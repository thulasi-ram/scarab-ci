//! The step-up guard (ADR-0073 §2): one middleware on every route of the
//! main router, deciding from the route's registered [`Op`].
//!
//! 1. A mutating request on a (method, path) the table does not know is
//!    refused 500 — fail-closed at runtime, whatever the tests missed.
//! 2. Reads, ceremonies, tightenings and ungoverned routes pass untouched.
//! 3. With step-up off (dev-insecure, hand-built test states) everything
//!    passes; a conditional route still gets a [`GuardCtx`] that answers
//!    "off".
//! 4. Otherwise the body is buffered (within the default body limit), the
//!    caller authenticated exactly as the handler will (401, CSRF), and the
//!    request's canonical form digested under `K_digest`.
//! 5. A caller who holds no role reaching the op's `min_role` anywhere goes
//!    straight to the handler, which refuses it for its role.
//! 6. A crown jewel needs a step-up BEFORE the handler; `op.authorised` is
//!    written before it runs (fail-closed) and `op.completed` after
//!    (best-effort).
//! 7. A conditional route's handler decides; a 2xx with the question
//!    unanswered is a bug (debug: panic; release: `guard.not_consulted`).

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use scarab_identity::{security_event_kind as kind, Action};

use super::ops::{self, Class, Op, OpInput, OrgOf};
use super::{event, GuardCtx, Prepared, Ticket};
use crate::{authenticate, route_table, run_scope, AppState, Caller, RouteEntry};

/// The body limit the guard buffers within: axum's default
/// `DefaultBodyLimit` (2 MiB), which the handlers' extractors apply too.
pub const GUARD_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// The middleware, over [`route_table`].
pub(crate) async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    guard_with(route_table(), st, req, next).await
}

/// [`guard`] over an explicit table — so a test can prove an unclassified
/// mutating route is refused.
#[doc(hidden)]
pub async fn guard_with(table: &[RouteEntry], st: AppState, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    if matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned());
    let entry = matched
        .as_deref()
        .and_then(|m| table.iter().find(|e| e.method == method && e.path == m));
    let Some(entry) = entry else {
        tracing::error!(
            method = %method,
            path = matched.as_deref().unwrap_or("<unmatched>"),
            "step-up guard: unclassified mutating route; refusing (ADR-0073)"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "unclassified route: refused by the step-up guard",
        )
            .into_response();
    };
    let op: &'static Op = entry.op;
    let conditional = matches!(op.class, Class::Conditional { .. });
    match op.class {
        Class::Read | Class::Ceremony | Class::Exempt(_) | Class::NotGoverned(_) => {
            return next.run(req).await
        }
        Class::CrownJewel(_) | Class::Conditional { .. } => {}
    }

    let Some(svc) = st.step_up.service().cloned() else {
        // Off. A conditional handler still takes its GuardCtx.
        if conditional {
            let ctx = GuardCtx::off(op);
            let mut req = req;
            req.extensions_mut().insert(ctx.clone());
            let resp = next.run(req).await;
            tripwire(&st, &ctx, None, resp.status()).await;
            return resp;
        }
        return next.run(req).await;
    };

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, GUARD_BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    // Authenticate exactly as the handler will: an unauthenticated caller or
    // a cookie mutation without its CSRF token is refused here, the same way.
    let caller = match authenticate(&st, &parts.headers, Action::Write).await {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let rebuild = |parts, bytes: Bytes| Request::from_parts(parts, Body::from(bytes));
    // A crown jewel asks before its handler authorizes, so a caller the
    // handler will refuse for its role is passed straight through: a Viewer
    // sees "insufficient role", not "step-up required". (A conditional
    // handler authorizes before it asks, so it needs no such skip.)
    if !conditional && !reaches_min_role(&st, &caller, op).await {
        return next.run(rebuild(parts, bytes)).await;
    }

    let pattern = matched.unwrap_or_default();
    let path = parts.uri.path().to_string();
    let raw_query = parts.uri.query().unwrap_or_default().to_string();
    let params = path_params(&pattern, &path);
    let query = sorted_query(&raw_query);
    let json: Option<Value> = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&bytes).ok()
    };
    let canonical = canonical_form(parts.method.as_str(), &path, &query, &bytes, json.as_ref());
    let digest = svc.digest(&canonical);
    let body_view = json.clone().unwrap_or(Value::Null);
    let org = match op.org_of {
        OrgOf::Path(name) => params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone()),
        OrgOf::Query(name) => query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone()),
        OrgOf::Body(ptr) => body_view
            .pointer(ptr)
            .and_then(Value::as_str)
            .map(str::to_string),
        OrgOf::Run => match params.iter().find(|(k, _)| k == "id") {
            Some((_, id)) => run_scope(&st, &scarab_engine::RunId(id.clone()))
                .await
                .map(|s| s.org().to_string()),
            None => None,
        },
        OrgOf::None => None,
    }
    .filter(|o| !o.is_empty());
    let summary = op.summary(&OpInput {
        method: parts.method.as_str().to_string(),
        path: path.clone(),
        params,
        query,
        body: ops::redact(&body_view, op.redact),
    });
    let prepared = Arc::new(Prepared {
        svc: svc.clone(),
        caller,
        org,
        summary: summary.clone(),
        digest,
        method: parts.method.as_str().to_string(),
        path,
        query: raw_query,
        body: bytes.clone(),
    });
    let ctx = GuardCtx {
        op,
        prepared: Some(prepared.clone()),
        ticket: Arc::new(Mutex::new(Ticket::Unresolved)),
    };
    let mut req = rebuild(parts, bytes);

    if conditional {
        req.extensions_mut().insert(ctx.clone());
        let resp = next.run(req).await;
        tripwire(&st, &ctx, Some(&prepared), resp.status()).await;
        if let Ticket::Authorised(confirmation) = ctx.ticket() {
            completed(&st, op, &prepared, confirmation, resp.status()).await;
        }
        return resp;
    }

    // A crown jewel: confirmed before the handler runs. The handler may
    // read the GuardCtx to link its own event to the confirmation.
    let confirmation = match super::authorise(&st, op, &prepared, &summary).await {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    *ctx.ticket.lock().unwrap() = Ticket::Authorised(confirmation.clone());
    req.extensions_mut().insert(ctx.clone());
    let resp = next.run(req).await;
    completed(&st, op, &prepared, confirmation, resp.status()).await;
    resp
}

/// `op.completed`, best-effort, linked to the confirmation that authorised it.
async fn completed(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    confirmation: Option<String>,
    status: StatusCode,
) {
    let now = st.clock.now().await.0;
    let mut ev = event(
        kind::OP_COMPLETED,
        &p.caller.principal.subject,
        p.caller.event_label(),
        p.org.clone(),
        now,
    );
    ev.op = Some(op.id.to_string());
    ev.target = Some(p.summary.target.clone());
    ev.confirmation_id = confirmation;
    ev.session_hash = p.caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({ "status": status.as_u16() });
    p.svc.record(ev).await;
}

/// A conditional handler answered 2xx without asking: a bug that would let
/// a loosening through unconfirmed.
async fn tripwire(st: &AppState, ctx: &GuardCtx, p: Option<&Prepared>, status: StatusCode) {
    if !status.is_success() || ctx.ticket() != Ticket::Unresolved {
        return;
    }
    if cfg!(debug_assertions) {
        panic!(
            "step-up guard: conditional route `{}` answered {status} without calling \
             GuardCtx::require or GuardCtx::exempt (ADR-0073 §3)",
            ctx.op.id
        );
    }
    tracing::error!(
        op = ctx.op.id,
        "step-up guard: conditional handler did not consult the guard"
    );
    if let Some(p) = p {
        let now = st.clock.now().await.0;
        let mut ev = event(
            kind::GUARD_NOT_CONSULTED,
            &p.caller.principal.subject,
            p.caller.event_label(),
            p.org.clone(),
            now,
        );
        ev.op = Some(ctx.op.id.to_string());
        ev.target = Some(p.summary.target.clone());
        ev.detail = serde_json::json!({ "status": status.as_u16() });
        p.svc.record(ev).await;
    }
}

/// Does `caller` hold, anywhere, a role at least `op.min_role`? Coarse on
/// purpose: it only decides whether to ask the step-up question before the
/// handler's own (scoped) authorization. A store error answers "yes" —
/// asking is the fail-closed direction.
async fn reaches_min_role(st: &AppState, caller: &Caller, op: &Op) -> bool {
    let Some(min) = op.min_role else {
        return true;
    };
    let global = caller.principal.roles.iter().copied().max();
    let bound = match st.rbac.as_ref() {
        Some(rbac) => match rbac.max_role(&caller.principal.subject).await {
            Ok(r) => r,
            Err(_) => return true,
        },
        None => None,
    };
    let live = global.into_iter().chain(bound).max();
    let effective = match &caller.grant {
        Some(token) => token.effective_role(live),
        None => live,
    };
    effective.is_some_and(|r| r >= min)
}

/// The path parameters of `path` under axum pattern `pattern`, decoded.
fn path_params(pattern: &str, path: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut segs = path.trim_start_matches('/').split('/');
    for p in pattern.trim_start_matches('/').split('/') {
        if let Some(name) = p.strip_prefix("{*").and_then(|n| n.strip_suffix('}')) {
            let rest: Vec<&str> = segs.by_ref().collect();
            out.push((name.to_string(), percent_decode(&rest.join("/"), false)));
            break;
        }
        let seg = segs.next().unwrap_or_default();
        if let Some(name) = p.strip_prefix('{').and_then(|n| n.strip_suffix('}')) {
            out.push((name.to_string(), percent_decode(seg, false)));
        }
    }
    out
}

/// The query's pairs, decoded and sorted (so the order a client wrote them
/// in does not change the digest).
fn sorted_query(raw: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = raw
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect();
    pairs.sort();
    pairs
}

/// Percent-decoding: a `%` followed by exactly two hex digits is a byte;
/// any other `%` is kept literally. `plus_is_space` is the query's
/// `application/x-www-form-urlencoded` rule; a path segment (RFC 3986, as
/// axum's `Path` decodes it) keeps its `+`.
fn percent_decode(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' if plus_is_space => out.push(b' '),
            b'%' if i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit() =>
            {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 3;
                continue;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The canonical form an action digest is computed over: method, concrete
/// path, sorted decoded query, and the body — canonical JSON (keys sorted,
/// compact) when it is JSON, the raw bytes otherwise. Length-prefixed, so no
/// field can bleed into the next.
pub(crate) fn canonical_form(
    method: &str,
    path: &str,
    query: &[(String, String)],
    raw_body: &[u8],
    json: Option<&Value>,
) -> Vec<u8> {
    let mut out = b"scarab-step-up-canonical/v1\n".to_vec();
    let mut field = |tag: &str, bytes: &[u8]| {
        out.extend_from_slice(tag.as_bytes());
        out.extend_from_slice(format!(":{}:", bytes.len()).as_bytes());
        out.extend_from_slice(bytes);
        out.push(b'\n');
    };
    field("method", method.as_bytes());
    field("path", path.as_bytes());
    for (k, v) in query {
        field("qk", k.as_bytes());
        field("qv", v.as_bytes());
    }
    match json {
        Some(v) => field("json", canonical_json(v).as_bytes()),
        None => field("raw", raw_body),
    }
    out
}

/// JSON with every object's keys sorted, compact.
fn canonical_json(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(v).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params_follow_the_pattern_including_a_wildcard() {
        assert_eq!(
            path_params(
                "/v1/orgs/{org}/plugins/{name}",
                "/v1/orgs/acme/plugins/argo%20cd"
            ),
            vec![
                ("org".to_string(), "acme".to_string()),
                ("name".to_string(), "argo cd".to_string())
            ]
        );
        assert_eq!(
            path_params(
                "/v1/runs/{id}/artifacts/{*name}",
                "/v1/runs/r1/artifacts/a/b.txt"
            ),
            vec![
                ("id".to_string(), "r1".to_string()),
                ("name".to_string(), "a/b.txt".to_string())
            ]
        );
    }

    #[test]
    fn the_canonical_form_ignores_key_and_query_order_but_nothing_else() {
        let a = json!({"b": 1, "a": {"y": 2, "x": 3}});
        let b = json!({"a": {"x": 3, "y": 2}, "b": 1});
        let q1 = sorted_query("org=acme&name=X");
        let q2 = sorted_query("name=X&org=acme");
        assert_eq!(
            canonical_form("POST", "/p", &q1, b"", Some(&a)),
            canonical_form("POST", "/p", &q2, b"", Some(&b))
        );
        let c = json!({"a": {"x": 3, "y": 2}, "b": 2});
        assert_ne!(
            canonical_form("POST", "/p", &q1, b"", Some(&a)),
            canonical_form("POST", "/p", &q1, b"", Some(&c))
        );
        assert_ne!(
            canonical_form("POST", "/p", &q1, b"", Some(&a)),
            canonical_form("PUT", "/p", &q1, b"", Some(&a))
        );
    }

    #[test]
    fn percent_decoding_keeps_a_bad_escape_literally() {
        assert_eq!(percent_decode("a%2Fb+c", true), "a/b c");
        assert_eq!(percent_decode("100%", true), "100%");
        assert_eq!(percent_decode("%zz", true), "%zz");
        assert_eq!(percent_decode("%+1", true), "% 1");
    }

    #[test]
    fn a_signed_escape_is_not_a_byte() {
        // `u8::from_str_radix` accepts a leading `+`: `%+1` must not decode
        // to 0x01, or two different queries share one digest.
        assert_ne!(sorted_query("a=%+1"), sorted_query("a=%01"));
    }

    #[test]
    fn a_path_parameter_keeps_its_plus() {
        assert_eq!(
            path_params("/v1/orgs/{org}/plugins/{name}", "/v1/orgs/acme/plugins/argo+cd"),
            vec![
                ("org".to_string(), "acme".to_string()),
                ("name".to_string(), "argo+cd".to_string())
            ]
        );
    }
}
