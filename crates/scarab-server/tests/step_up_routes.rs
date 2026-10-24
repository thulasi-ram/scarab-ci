//! ADR-0073 §3 guard coverage: every route of the main router is registered
//! with a classification, the classification is a visible literal table, and
//! the table is what the server serves and documents.

use std::collections::BTreeSet;

use scarab_server::route_table;

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
