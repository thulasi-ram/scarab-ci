//! ADR-0073 §3 guard coverage: every route of the main router is registered
//! with a classification, the classification is a visible literal table, and
//! the table is what the server serves, documents and enforces.

mod step_up_harness;

use std::collections::BTreeSet;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_identity::{Role, Scope};
use scarab_server::step_up::Class;
use scarab_server::{route_table, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

use step_up_harness::*;

/// (a) The whole classification, as a literal: any reclassification, and any
/// new route, is a visible diff here. `(method, path, op id, class)`; class
/// codes: READ, CJ-W / CJ-A (crown jewel; window / always-ask), COND-W /
/// COND-A (crown jewel only when it loosens; the handler decides), CER
/// (the ceremony endpoints), EX (tightening), NG (not governed).
const EXPECTED: &[(&str, &str, &str, &str)] = &[
    ("GET", "/healthz", "read", "READ"),
    ("GET", "/readyz", "read", "READ"),
    ("GET", "/metrics", "read", "READ"),
    ("GET", "/openapi.json", "read", "READ"),
    ("GET", "/.well-known/jwks.json", "read", "READ"),
    ("GET", "/.well-known/openid-configuration", "read", "READ"),
    ("POST", "/v1/auth/login", "auth.login", "NG"),
    ("GET", "/v1/auth/login", "read", "READ"),
    ("GET", "/v1/auth/callback", "read", "READ"),
    ("GET", "/v1/auth/providers", "read", "READ"),
    ("POST", "/v1/auth/logout", "auth.logout", "NG"),
    ("GET", "/v1/me", "read", "READ"),
    ("POST", "/v1/me/identities/link", "identity.link", "CJ-W"),
    (
        "DELETE",
        "/v1/me/identities/{provider}",
        "identity.unlink",
        "CJ-W",
    ),
    ("GET", "/v1/me/security", "read", "READ"),
    (
        "POST",
        "/v1/me/passkeys/registration/options",
        "passkey.add",
        "COND-A",
    ),
    (
        "POST",
        "/v1/me/passkeys/registration",
        "passkey.registration",
        "CER",
    ),
    ("DELETE", "/v1/me/passkeys/{id}", "passkey.remove", "CJ-A"),
    ("POST", "/v1/me/step-up/options", "step_up.options", "CER"),
    ("POST", "/v1/me/step-up", "step_up.confirm", "CER"),
    ("GET", "/v1/me/security/events", "read", "READ"),
    ("GET", "/v1/security/events", "read", "READ"),
    ("GET", "/v1/orgs/{org}/security/events", "read", "READ"),
    ("GET", "/v1/orgs/{org}/security/summary", "read", "READ"),
    (
        "POST",
        "/v1/orgs/{org}/members/{subject}/passkeys/reset",
        "passkey.reset",
        "CJ-A",
    ),
    ("GET", "/v1/step-up/requests/{id}", "read", "READ"),
    (
        "POST",
        "/v1/step-up/requests/{id}/options",
        "step_up.request_options",
        "CER",
    ),
    (
        "POST",
        "/v1/step-up/requests/{id}/confirm",
        "step_up.request_confirm",
        "CER",
    ),
    ("GET", "/v1/orgs/{org}/bindings", "read", "READ"),
    ("PUT", "/v1/orgs/{org}/bindings", "binding.put", "CJ-W"),
    (
        "DELETE",
        "/v1/orgs/{org}/bindings",
        "binding.delete",
        "CJ-W",
    ),
    ("GET", "/v1/orgs/{org}/principals", "read", "READ"),
    ("GET", "/v1/orgs/{org}/tokens", "read", "READ"),
    ("POST", "/v1/orgs/{org}/tokens", "token.mint", "CJ-A"),
    ("DELETE", "/v1/orgs/{org}/tokens/{id}", "token.revoke", "EX"),
    (
        "POST",
        "/v1/repos/{org}/{repo}/bindings/import",
        "binding.import",
        "CJ-W",
    ),
    ("GET", "/v1/repos", "read", "READ"),
    ("GET", "/v1/repos/{org}/{repo}/runs", "read", "READ"),
    ("POST", "/v1/runs", "run.create", "NG"),
    ("GET", "/v1/runs", "read", "READ"),
    ("GET", "/v1/runs/{id}", "read", "READ"),
    ("GET", "/v1/runs/{id}/events", "read", "READ"),
    ("GET", "/v1/runs/{id}/logs", "read", "READ"),
    (
        "POST",
        "/v1/runs/{id}/steps/{step}/rerun",
        "step.rerun",
        "NG",
    ),
    (
        "POST",
        "/v1/runs/{id}/steps/{step}/restart",
        "step.rerun",
        "NG",
    ),
    (
        "POST",
        "/v1/runs/{id}/steps/{step}/retry",
        "step.retry",
        "NG",
    ),
    (
        "GET",
        "/v1/runs/{id}/steps/{step}/rerun-plan",
        "read",
        "READ",
    ),
    (
        "POST",
        "/v1/runs/{id}/snapshots-pin",
        "run.snapshots_pin",
        "NG",
    ),
    (
        "DELETE",
        "/v1/runs/{id}/snapshots-pin",
        "run.snapshots_unpin",
        "NG",
    ),
    ("POST", "/v1/runs/{id}/cancel", "run.cancel", "EX"),
    ("GET", "/v1/runs/{id}/artifacts", "read", "READ"),
    ("GET", "/v1/runs/{id}/artifacts/{*name}", "read", "READ"),
    ("GET", "/v1/runs/{id}/steps/{step}/logs", "read", "READ"),
    ("GET", "/v1/runs/{id}/services", "read", "READ"),
    (
        "GET",
        "/v1/runs/{id}/services/{service}/logs",
        "read",
        "READ",
    ),
    ("GET", "/v1/runs/{id}/steps/{step}/attach", "read", "READ"),
    (
        "GET",
        "/v1/runs/{id}/steps/{step}/debug-pod",
        "read",
        "READ",
    ),
    (
        "GET",
        "/v1/runs/{id}/steps/{step}/workspace",
        "read",
        "READ",
    ),
    (
        "GET",
        "/v1/runs/{id}/steps/{step}/workspace/file",
        "read",
        "READ",
    ),
    (
        "POST",
        "/v1/runs/{id}/gates/{step}/approve",
        "gate.approve",
        "COND-W",
    ),
    (
        "POST",
        "/v1/runs/{id}/gates/{step}/reject",
        "gate.reject",
        "EX",
    ),
    (
        "POST",
        "/v1/runs/{id}/gates/{step}/release",
        "gate.release_external",
        "NG",
    ),
    ("GET", "/v1/runs/{id}/steps/{step}/results", "read", "READ"),
    (
        "POST",
        "/v1/runs/{id}/steps/{step}/results",
        "step.results_ingest",
        "NG",
    ),
    ("GET", "/v1/runs/{id}/steps/{step}/consumed", "read", "READ"),
    ("POST", "/v1/secrets", "secret.put", "CJ-W"),
    ("GET", "/v1/secrets", "read", "READ"),
    ("DELETE", "/v1/secrets", "secret.delete", "CJ-W"),
    ("GET", "/v1/repos/{org}/{repo}/environments", "read", "READ"),
    (
        "PUT",
        "/v1/repos/{org}/{repo}/environments/{name}",
        "environment.legacy_put",
        "NG",
    ),
    (
        "GET",
        "/v1/repos/{org}/{repo}/environments/{name}",
        "read",
        "READ",
    ),
    (
        "DELETE",
        "/v1/repos/{org}/{repo}/environments/{name}",
        "environment.legacy_delete",
        "NG",
    ),
    (
        "GET",
        "/v1/repos/{org}/{repo}/environments/{name}/deployments",
        "read",
        "READ",
    ),
    ("GET", "/v1/orgs/{org}/environments", "read", "READ"),
    (
        "PUT",
        "/v1/orgs/{org}/environments/{env}",
        "environment.put",
        "NG",
    ),
    ("GET", "/v1/orgs/{org}/environments/{env}", "read", "READ"),
    (
        "DELETE",
        "/v1/orgs/{org}/environments/{env}",
        "environment.delete",
        "CJ-W",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/environments/{env}/eligibility",
        "environment.eligibility.put",
        "COND-W",
    ),
    (
        "GET",
        "/v1/orgs/{org}/environments/{env}/eligibility",
        "read",
        "READ",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/environments/{env}/protection",
        "environment.protection.put",
        "COND-W",
    ),
    (
        "GET",
        "/v1/orgs/{org}/environments/{env}/protection",
        "read",
        "READ",
    ),
    (
        "GET",
        "/v1/orgs/{org}/environments/{env}/projects",
        "read",
        "READ",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
        "environment.overlay.put",
        "COND-W",
    ),
    (
        "GET",
        "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
        "read",
        "READ",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
        "environment.overlay.delete",
        "CJ-W",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
        "environment.privilege_grant.put",
        "COND-W",
    ),
    (
        "GET",
        "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
        "read",
        "READ",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
        "environment.privilege_grant.delete",
        "EX",
    ),
    (
        "GET",
        "/v1/orgs/{org}/environment-migration-report",
        "read",
        "READ",
    ),
    (
        "POST",
        "/v1/orgs/{org}/environment-migration-report/{env}/acknowledge",
        "environment.migration_report_ack",
        "NG",
    ),
    // ADR-0075 §10: adding tightens; an edit or delete of an enforced Policy
    // loosens (the handler decides from the stored mode).
    ("GET", "/v1/orgs/{org}/policies", "read", "READ"),
    ("POST", "/v1/orgs/{org}/policies", "policy.create", "EX"),
    ("GET", "/v1/orgs/{org}/policies/{id}", "read", "READ"),
    (
        "PUT",
        "/v1/orgs/{org}/policies/{id}",
        "policy.update",
        "COND-W",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/policies/{id}",
        "policy.delete",
        "COND-W",
    ),
    (
        "GET",
        "/v1/orgs/{org}/policies/{id}/versions",
        "read",
        "READ",
    ),
    (
        "GET",
        "/v1/orgs/{org}/projects/{project}/policies",
        "read",
        "READ",
    ),
    (
        "POST",
        "/v1/orgs/{org}/projects/{project}/policies",
        "project_policy.create",
        "EX",
    ),
    (
        "GET",
        "/v1/orgs/{org}/projects/{project}/policies/{id}",
        "read",
        "READ",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/projects/{project}/policies/{id}",
        "project_policy.update",
        "COND-W",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/projects/{project}/policies/{id}",
        "project_policy.delete",
        "COND-W",
    ),
    (
        "GET",
        "/v1/orgs/{org}/projects/{project}/policies/{id}/versions",
        "read",
        "READ",
    ),
    ("POST", "/v1/orgs/{org}/policy-test", "policy.test", "NG"),
    ("GET", "/v1/orgs/{org}/policy-decisions", "read", "READ"),
    ("GET", "/v1/runs/{id}/policy-decisions", "read", "READ"),
    ("GET", "/v1/orgs/{org}/libraries", "read", "READ"),
    (
        "POST",
        "/v1/orgs/{org}/libraries/fetch",
        "library.fetch",
        "NG",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/libraries/{publisher}/{name}",
        "library.install",
        "CJ-W",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/libraries/{publisher}/{name}",
        "library.remove",
        "EX",
    ),
    ("POST", "/v1/orgs/{org}/library-yanks", "library.yank", "EX"),
    (
        "DELETE",
        "/v1/orgs/{org}/library-yanks/{hash}",
        "library.unyank",
        "CJ-W",
    ),
    (
        "POST",
        "/v1/orgs/{org}/library-defaults/retry",
        "library.defaults_retry",
        "CJ-W",
    ),
    ("POST", "/v1/orgs/{org}/library-trust", "library.trust", "CJ-W"),
    ("DELETE", "/v1/orgs/{org}/library-trust", "library.untrust", "EX"),
    ("GET", "/v1/orgs/{org}/plugin-catalog", "read", "READ"),
    ("GET", "/v1/orgs/{org}/plugins", "read", "READ"),
    // ADR-0076 §7: a plugin's open gate waits; the handler takes only its
    // scarab-api token, in its own org.
    ("GET", "/v1/orgs/{org}/gates", "read", "READ"),
    ("POST", "/v1/orgs/{org}/plugins", "plugin.install", "CJ-A"),
    ("GET", "/v1/orgs/{org}/plugins/{name}", "read", "READ"),
    (
        "DELETE",
        "/v1/orgs/{org}/plugins/{name}",
        "plugin.uninstall",
        "EX",
    ),
    (
        "POST",
        "/v1/orgs/{org}/plugins/{name}/kill",
        "plugin.kill",
        "EX",
    ),
    (
        "POST",
        "/v1/orgs/{org}/plugins/{name}/enable",
        "plugin.enable",
        "CJ-W",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/plugins/{name}/storage",
        "plugin.storage.put",
        "COND-W",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/plugins/{name}/storage",
        "plugin.storage.delete",
        "EX",
    ),
    (
        "GET",
        "/v1/orgs/{org}/plugins/{name}/connections",
        "read",
        "READ",
    ),
    (
        "PUT",
        "/v1/orgs/{org}/plugins/{name}/connections/{slot}",
        "plugin_connection.put",
        "CJ-A",
    ),
    (
        "DELETE",
        "/v1/orgs/{org}/plugins/{name}/connections/{slot}",
        "plugin_connection.delete",
        "EX",
    ),
    ("GET", "/v1/connections", "read", "READ"),
    ("POST", "/v1/connections", "connection.create", "CJ-W"),
    (
        "DELETE",
        "/v1/connections/{id}",
        "connection.delete",
        "CJ-W",
    ),
    (
        "POST",
        "/v1/connections/{id}/resync",
        "connection.resync",
        "CJ-W",
    ),
    ("GET", "/v1/connections/{id}/preflight", "read", "READ"),
    (
        "GET",
        "/v1/connections/{id}/available-repos",
        "read",
        "READ",
    ),
    (
        "POST",
        "/v1/connections/{id}/repos",
        "connection.bind_repo",
        "CJ-W",
    ),
    (
        "DELETE",
        "/v1/connections/{id}/repos/{owner}/{name}",
        "connection.unbind_repo",
        "CJ-W",
    ),
    (
        "POST",
        "/v1/connections/{id}/repos/{owner}/{name}/webhook",
        "connection.webhook_register",
        "NG",
    ),
    (
        "GET",
        "/v1/repos/{org}/{repo}/secrets/matrix",
        "read",
        "READ",
    ),
    (
        "PUT",
        "/v1/repos/{org}/{repo}/secrets/matrix/silenced",
        "secret_matrix.silence",
        "NG",
    ),
    (
        "DELETE",
        "/v1/repos/{org}/{repo}/secrets/matrix/silenced",
        "secret_matrix.unsilence",
        "NG",
    ),
    ("GET", "/v1/repos/{org}/{repo}/refs", "read", "READ"),
    ("GET", "/v1/repos/{org}/{repo}/pipelines", "read", "READ"),
    (
        "GET",
        "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
        "read",
        "READ",
    ),
    (
        "POST",
        "/v1/repos/{org}/{repo}/dispatch",
        "repo.dispatch",
        "NG",
    ),
    ("POST", "/webhooks/github", "webhook.github", "NG"),
    ("POST", "/webhooks/forgejo", "webhook.forgejo", "NG"),
];

#[test]
fn the_route_table_is_exactly_the_reviewed_classification() {
    let actual: Vec<(String, String, String, String)> = route_table()
        .iter()
        .map(|r| {
            (
                r.method.as_str().to_string(),
                r.path.to_string(),
                r.op.id.to_string(),
                r.op.class.code().to_string(),
            )
        })
        .collect();
    let expected: Vec<(String, String, String, String)> = EXPECTED
        .iter()
        .map(|(m, p, i, c)| (m.to_string(), p.to_string(), i.to_string(), c.to_string()))
        .collect();
    let a: BTreeSet<_> = actual.iter().cloned().collect();
    let e: BTreeSet<_> = expected.iter().cloned().collect();
    let unexpected: Vec<_> = a.difference(&e).collect();
    let missing: Vec<_> = e.difference(&a).collect();
    assert!(
        unexpected.is_empty() && missing.is_empty(),
        "route classification changed (ADR-0073 §3) — review it, then update EXPECTED.\n\
         new/changed: {unexpected:#?}\nno longer present: {missing:#?}"
    );
    assert_eq!(
        actual.len(),
        a.len(),
        "a (method, path) is registered twice"
    );
    // Every mutating route names a non-READ op; every GET is READ.
    for r in route_table() {
        assert_eq!(
            r.method == axum::http::Method::GET,
            matches!(r.op.class, Class::Read),
            "{} {} is {:?}",
            r.method,
            r.path,
            r.op.class
        );
    }
}

/// ADR-0073 C5: the CredentialBroker is a separate listener (PluginPrincipal,
/// ADR-0072 §11) that never passes through the step-up guard — no broker
/// path is in the main table, so the plugin paths stay exactly as they were.
#[test]
fn no_broker_path_is_in_the_main_route_table() {
    let prefix = scarab_server::broker::BROKER_PREFIX;
    let leaked: Vec<_> = route_table()
        .iter()
        .filter(|r| r.path.starts_with(prefix) || r.path.starts_with("/_scarab"))
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert!(
        leaked.is_empty(),
        "broker paths in the main router: {leaked:?}"
    );
}

/// ADR-0073 C8: the operator break-glass has no route. The only passkey
/// reset reachable over HTTP is the Owner's, an always-ask crown jewel.
#[test]
fn the_break_glass_is_not_reachable_over_http() {
    let resets: Vec<_> = route_table()
        .iter()
        .filter(|r| r.op.id.contains("reset") || r.path.contains("reset"))
        .map(|r| {
            (
                r.method.as_str().to_string(),
                r.path,
                r.op.id,
                r.op.class.code(),
            )
        })
        .collect();
    assert_eq!(
        resets,
        vec![(
            "POST".to_string(),
            "/v1/orgs/{org}/members/{subject}/passkeys/reset",
            "passkey.reset",
            "CJ-A"
        )]
    );
    assert!(route_table()
        .iter()
        .all(|r| !r.op.id.contains("break_glass") && !r.path.contains("admin")));
}

/// (b) No route is registered anywhere but `src/routes.rs` (the broker and
/// the workspace service are separate listeners with their own routers and
/// never pass through the guard).
#[test]
fn no_route_is_registered_outside_routes_rs() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if matches!(rel.as_str(), "routes.rs" | "broker.rs" | "workspaced.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            if text.contains(".route(")
                || text.contains(".route_service(")
                || text.contains(".nest(")
            {
                offenders.push(rel);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "routes registered outside src/routes.rs (they would skip the step-up classification): {offenders:?}"
    );
}

/// (c) Full-route OpenAPI coverage (ADR-0054), from the route table rather
/// than a source scan: every registered (method, path) appears in the spec
/// and vice versa. Exemptions: `/openapi.json` (the spec serving itself) and
/// the deprecated `.../steps/{step}/restart` alias of `.../rerun`.
#[test]
fn route_table_equals_the_openapi_spec() {
    let routes: BTreeSet<(String, String)> = route_table()
        .iter()
        .filter(|r| r.path != "/openapi.json" && r.path != "/v1/runs/{id}/steps/{step}/restart")
        .map(|r| {
            (
                r.method.as_str().to_ascii_lowercase(),
                r.path.replace("{*name}", "{name}"),
            )
        })
        .collect();

    let spec: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();
    let mut documented = BTreeSet::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for method in ["get", "put", "post", "delete", "patch", "head", "options"] {
            if item.get(method).is_some() {
                documented.insert((method.to_string(), path.clone()));
            }
        }
    }
    let missing: Vec<_> = routes.difference(&documented).collect();
    assert!(
        missing.is_empty(),
        "routes missing from the OpenAPI spec: {missing:?}"
    );
    let extra: Vec<_> = documented.difference(&routes).collect();
    assert!(
        extra.is_empty(),
        "spec operations with no registered route: {extra:?}"
    );
}

/// A concrete URI for a route pattern: every `{param}` gets a sample value.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|seg| match seg {
            "{org}" => "acme".to_string(),
            "{repo}" | "{project}" => "web".to_string(),
            "{env}" | "{name}" => "prod".to_string(),
            s if s.starts_with('{') => "x1".to_string(),
            s => s.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn crown_jewels() -> Vec<&'static scarab_server::RouteEntry> {
    route_table()
        .iter()
        .filter(|r| matches!(r.op.class, Class::CrownJewel(_)))
        .collect()
}

/// (d) Every unconditional crown jewel, called by an Owner from a browser
/// with a valid CSRF token and no confirmation, answers 403
/// `step_up_required` — whatever the body. First with no passkey (mode
/// `enroll`), then with one (the op's own mode, with a ready challenge).
#[tokio::test]
async fn every_crown_jewel_needs_a_step_up_even_for_an_owner() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let body = || Some(json!({ "anything": "at all" }));
    let jewels = crown_jewels();
    assert!(jewels.len() >= 15, "the sweep must cover the list");
    for r in &jewels {
        let (st, b) = h
            .cookie(r.method.as_str(), &concrete(r.path), body(), &s)
            .await;
        assert!(is_step_up(st, &b), "{} {}: {st} {b}", r.method, r.path);
        assert_eq!(b["mode"], "enroll", "{} {}: {b}", r.method, r.path);
        assert_eq!(b["op"], r.op.id);
        assert_eq!(b["enroll_url"], "/account/security?enroll=1");
        assert!(b["message"].as_str().unwrap().contains("step-up"));
    }

    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    for r in &jewels {
        let (st, b) = h
            .cookie(r.method.as_str(), &concrete(r.path), body(), &s)
            .await;
        assert!(is_step_up(st, &b), "{} {}: {st} {b}", r.method, r.path);
        let want = match r.op.class {
            Class::CrownJewel(m) => m.as_str(),
            _ => unreachable!(),
        };
        assert_eq!(b["mode"], want, "{} {}", r.method, r.path);
        assert!(b["challenge"]["id"].is_string(), "a ready challenge: {b}");
        assert_eq!(b["challenge"]["public_key"]["rpId"], "ci.example.com");
    }
    // Every refusal was recorded.
    let refused = h.events_of("step_up.refused");
    assert_eq!(refused.len(), 2 * jewels.len());
    assert!(refused.iter().all(|e| e.caller == "session:cookie"));
}

/// `connection.resync` binds every reported repo through the same path as
/// `connection.bind_repo`, so it needs the same window step-up: under
/// enforcement an Owner without one is refused, exactly like bind_repo.
#[tokio::test]
async fn resync_needs_the_same_step_up_as_bind_repo() {
    let h = harness();
    let (s, _soft) = {
        h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
        let s = h.browser_session(owner("amy")).await;
        let mut soft = passkey();
        h.enroll(&s, &mut soft, "Laptop").await;
        (s, soft)
    };
    let (st, resync) = h
        .cookie("POST", "/v1/connections/gh/resync", None, &s)
        .await;
    assert_step_up(st, &resync, "window");
    assert_eq!(resync["op"], "connection.resync");
    let (st, bind) = h
        .cookie(
            "POST",
            "/v1/connections/gh/repos",
            Some(json!({"owner": "acme", "name": "web"})),
            &s,
        )
        .await;
    assert_step_up(st, &bind, "window");
}

/// The 403 (and the refusal it records) never echoes a redacted field.
#[tokio::test]
async fn a_refusal_never_echoes_a_secret_value() {
    let h = harness();
    let s = h.browser_session(owner("amy")).await;
    let cases = [
        (
            "POST",
            "/v1/secrets",
            json!({"org": "acme", "name": "DB_PASSWORD", "value": "S3CR3T-hunter2"}),
        ),
        (
            "POST",
            "/v1/connections",
            json!({"kind": "github", "base_url": "https://github.com", "credential": "S3CR3T-pat"}),
        ),
        (
            "PUT",
            "/v1/orgs/acme/plugins/argo/connections/api",
            json!({"base_url": "https://argo.example", "credential": "S3CR3T-bearer"}),
        ),
    ];
    for (m, uri, body) in cases {
        let (st, b) = h.cookie(m, uri, Some(body), &s).await;
        assert!(is_step_up(st, &b), "{m} {uri}: {st} {b}");
        assert!(
            !b.to_string().contains("S3CR3T"),
            "{m} {uri} echoed a secret: {b}"
        );
        assert!(
            b["summary"]["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().unwrap().ends_with("= (hidden)")),
            "{b}"
        );
    }
    let log = format!("{:?}", h.events.all());
    assert!(!log.contains("S3CR3T"), "an event recorded a secret: {log}");
    // The secret's target is still legible.
    let (_, b) = h
        .cookie(
            "POST",
            "/v1/secrets",
            Some(json!({"org": "acme", "repo": "web", "name": "DB_PASSWORD", "value": "x"})),
            &s,
        )
        .await;
    assert_eq!(b["summary"]["target"], "acme/web/DB_PASSWORD");
    assert_eq!(b["summary"]["operation"], "Write a secret");
}

/// A request each crown jewel's handler would accept the SHAPE of — so the
/// one-role-below caller reaches the handler's own authorization.
fn well_formed(r: &scarab_server::RouteEntry) -> (String, Option<Value>) {
    let uri = concrete(r.path);
    let body = match (r.method.as_str(), r.path) {
        ("PUT", "/v1/orgs/{org}/bindings") => Some(json!({"subject": "carl", "role": "member"})),
        ("DELETE", "/v1/orgs/{org}/bindings") => return (format!("{uri}?subject=carl"), None),
        ("POST", "/v1/orgs/{org}/tokens") => {
            Some(json!({"name": "ci", "role": "member", "expires_in_days": 1}))
        }
        ("POST", "/v1/repos/{org}/{repo}/bindings/import") => Some(json!({"subjects": []})),
        ("POST", "/v1/secrets") => Some(json!({"org": "acme", "name": "X", "value": "v"})),
        ("DELETE", "/v1/secrets") => return ("/v1/secrets?org=acme&name=X".into(), None),
        ("POST", "/v1/orgs/{org}/plugins") => {
            Some(json!({"name": "argo", "plugin": "argocd", "image": "img@sha256:00"}))
        }
        ("PUT", "/v1/orgs/{org}/plugins/{name}/connections/{slot}") => {
            Some(json!({"base_url": "https://argo.example", "credential": "c"}))
        }
        ("POST", "/v1/connections") => {
            Some(json!({"kind": "github", "base_url": "https://github.com", "credential": "c"}))
        }
        ("POST", "/v1/connections/{id}/repos") => Some(json!({"owner": "acme", "name": "web"})),
        ("PUT", "/v1/orgs/{org}/libraries/{publisher}/{name}") => {
            Some(json!({"content_hash": "a".repeat(64)}))
        }
        ("POST", "/v1/orgs/{org}/library-defaults/retry") => Some(json!({})),
        ("POST", "/v1/orgs/{org}/library-trust") => {
            Some(json!({"source": "github.com/acme/libs//push.yaml"}))
        }
        ("DELETE", _)
        | ("POST", "/v1/connections/{id}/resync")
        | ("POST", "/v1/orgs/{org}/plugins/{name}/enable")
        | ("POST", "/v1/orgs/{org}/members/{subject}/passkeys/reset") => None,
        (m, p) => panic!("add a well-formed request for the crown jewel {m} {p}"),
    };
    (uri, body)
}

/// (e) `min_role` is never overstated: a caller holding exactly it is asked
/// for a step-up; one holding the role below is refused by the HANDLER for
/// its role — so nobody the handler would accept ever skips the step-up.
#[tokio::test]
async fn min_role_is_exactly_the_lowest_role_the_handler_accepts() {
    let h = harness();
    for r in crown_jewels() {
        let Some(min) = r.op.min_role else { continue };
        let below = match min {
            Role::Owner => Role::Admin,
            Role::Admin => Role::Member,
            Role::Member => Role::Viewer,
            Role::Viewer => continue,
        };
        let (uri, body) = well_formed(r);

        let at = format!("at-{}", r.op.id);
        h.grant(&at, Scope::Org("acme".into()), min).await;
        let s = h.browser_session(scoped(&at)).await;
        let (st, b) = h.cookie(r.method.as_str(), &uri, body.clone(), &s).await;
        assert!(
            is_step_up(st, &b),
            "{} {} at {min:?}: {st} {b}",
            r.method,
            r.path
        );

        let under = format!("below-{}", r.op.id);
        h.grant(&under, Scope::Org("acme".into()), below).await;
        let s = h.browser_session(scoped(&under)).await;
        let (st, b) = h.cookie(r.method.as_str(), &uri, body, &s).await;
        assert_eq!(
            (st, b.clone()),
            (
                StatusCode::FORBIDDEN,
                Value::String("insufficient role".into())
            ),
            "{} {} at {below:?}: the handler must refuse it for its role",
            r.method,
            r.path
        );
    }
}

/// (f) Tightenings and ungoverned routes never answer `step_up_required`.
#[tokio::test]
async fn exempt_and_ungoverned_routes_never_ask_for_a_step_up() {
    let h = harness();
    let mut checked = 0;
    for r in route_table() {
        if !matches!(r.op.class, Class::Exempt(_) | Class::NotGoverned(_)) {
            continue;
        }
        // A fresh session each time: `logout` ends the one it is called with.
        let s = h.browser_session(owner("amy")).await;
        let (st, b) = h
            .cookie(r.method.as_str(), &concrete(r.path), Some(json!({})), &s)
            .await;
        assert!(
            !is_step_up(st, &b),
            "{} {} ({}): {b}",
            r.method,
            r.path,
            r.op.id
        );
        checked += 1;
    }
    assert_eq!(checked, 35, "the EX + NG mutating routes");
    assert!(h.events_of("step_up.refused").is_empty());
}

async fn empty_table_guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    scarab_server::step_up::guard::guard_with(&[], st, req, next).await
}

/// (g) A mutating route the table does not know is refused 500 at runtime —
/// whatever the tests missed, an unclassified route cannot run unguarded.
/// Reads still pass. This holds with step-up OFF too.
#[tokio::test]
async fn an_unclassified_mutating_route_is_refused() {
    let db = std::sync::Arc::new(InMemoryDb::new());
    let logs = std::sync::Arc::new(LogService::new(
        std::sync::Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db, std::sync::Arc::new(FakeClock::new(0)), logs);
    let app: axum::Router = axum::Router::new()
        .route(
            "/x",
            axum::routing::post(|| async { "handled" }).get(|| async { "read" }),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            st.clone(),
            empty_table_guard,
        ))
        .with_state(st);
    let call = |m: &str| {
        axum::http::Request::builder()
            .method(m)
            .uri("/x")
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let resp = app.clone().oneshot(call("POST")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = app.oneshot(call("GET")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
