//! Red team, round 1, ADR-0076 step 4: `wait-live`'s judge. It passes only
//! when Argo CD reports the Application synced, healthy, with no operation
//! in flight, AND its target (git) manifests render exactly the image the
//! gate was given in a workload's pod template. Each case asserts the SECURE
//! answer.

use scarab_plugin_argocd::argo::{Application, ManagedResources};
use scarab_plugin_argocd::gates::{judge, Judgement};
use serde_json::{json, Value};

const IMAGE: &str = "ghcr.io/acme/web:1.4.2";
const REV: &str = "9f1c2e7b9f1c2e7b9f1c2e7b9f1c2e7b9f1c2e7b";

fn app(sync: &str, health: &str, op: Option<Value>) -> Application {
    let mut status = json!({
        "sync": { "status": sync, "revision": REV },
        "health": { "status": health },
    });
    if let Some(op) = op {
        status["operationState"] = op;
    }
    serde_json::from_value(json!({
        "metadata": { "name": "web", "namespace": "argocd" },
        "spec": { "project": "default" },
        "status": status,
    }))
    .unwrap()
}

fn deployment(images: &[&str], init: &[&str]) -> Value {
    let c = |v: &[&str]| v.iter().map(|i| json!({ "name": "c", "image": i })).collect::<Vec<_>>();
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": { "name": "web" },
        "spec": { "template": { "spec": { "containers": c(images), "initContainers": c(init) } } }
    })
}

fn managed(items: Vec<Value>) -> ManagedResources {
    serde_json::from_value(json!({ "items": items })).unwrap()
}

/// A managed resource whose target (git) state is `target`, live is `live`.
fn diff(group: &str, kind: &str, target: Option<&Value>, live: Option<&Value>, hook: bool) -> Value {
    json!({
        "group": group,
        "kind": kind,
        "name": "web",
        "namespace": "web",
        "targetState": target.map(|t| t.to_string()).unwrap_or_else(|| "null".into()),
        "liveState": live.map(|t| t.to_string()).unwrap_or_else(|| "null".into()),
        "hook": hook,
    })
}

fn live_app() -> Application {
    app("Synced", "Healthy", None)
}

fn is_pass(j: &Judgement) -> bool {
    matches!(j, Judgement::Pass { .. })
}

#[test]
fn the_image_is_matched_exactly_never_as_a_substring_or_prefix() {
    // Control: the exact image in a Deployment's target passes, with the revision.
    let ok = managed(vec![diff("apps", "Deployment", Some(&deployment(&[IMAGE], &[])), None, false)]);
    assert_eq!(judge(&live_app(), &ok, IMAGE), Judgement::Pass { revision: REV.into() });
    for (target, wanted) in [
        ("ghcr.io/acme/web:1.4.2-evil", IMAGE),
        ("ghcr.io/acme/web:1.4.2", "ghcr.io/acme/web:1.4"),
        ("ghcr.io/acme/web:1.4.2", "acme/web:1.4.2"),
        ("evil.io/ghcr.io/acme/web:1.4.2", IMAGE),
        ("ghcr.io/acme/web:1.4.2@sha256:0000", IMAGE),
        ("ghcr.io/acme/web@sha256:0000", "ghcr.io/acme/web@sha256:000"),
        ("ghcr.io/acme/web:1.4.2", ""),
        ("ghcr.io/acme/web:1.4.2", " ghcr.io/acme/web:1.4.2"),
        ("ghcr.io/acme/web:1.4.2", "GHCR.IO/acme/web:1.4.2"),
    ] {
        let m = managed(vec![diff("apps", "Deployment", Some(&deployment(&[target], &[])), None, false)]);
        let j = judge(&live_app(), &m, wanted);
        assert!(!is_pass(&j), "target {target:?} vs wanted {wanted:?}: {j:?}");
    }
    // Several images: the input is one reference, not the joined version.
    let m = managed(vec![diff(
        "apps",
        "Deployment",
        Some(&deployment(&["a:1", "b:2"], &[])),
        None,
        false,
    )]);
    assert!(!is_pass(&judge(&live_app(), &m, "a:1,b:2")));
}

#[test]
fn only_the_target_pod_templates_of_real_workloads_count() {
    let with_image = deployment(&[IMAGE], &[]);
    let hooked = {
        let mut d = with_image.clone();
        d["metadata"]["annotations"] = json!({ "argocd.argoproj.io/hook": "PreSync" });
        d
    };
    let config_map = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "spec": { "template": { "spec": { "containers": [{ "image": IMAGE }] } } },
        "data": { "image": IMAGE }
    });
    for (what, items) in [
        // Only in the live state: what runs now, not what git says.
        ("live only", vec![diff("apps", "Deployment", None, Some(&with_image), false)]),
        // A hook Job (by flag, and by annotation).
        ("hook flag", vec![diff("batch", "Job", Some(&with_image), None, true)]),
        ("hook annotation", vec![diff("apps", "Deployment", Some(&hooked), None, false)]),
        // A kind that has no pod template, however its JSON is shaped.
        ("configmap", vec![diff("", "ConfigMap", Some(&config_map), None, false)]),
        // A CRD that borrows the name `Deployment` in another group.
        ("other group", vec![diff("evil.io", "Deployment", Some(&with_image), None, false)]),
        // A targetState that is a string holding JSON twice, or not an object.
        (
            "double-encoded",
            vec![json!({ "group": "apps", "kind": "Deployment", "targetState": with_image.to_string().replace('"', "\\\""), "hook": false })],
        ),
    ] {
        let j = judge(&live_app(), &managed(items), IMAGE);
        assert!(!is_pass(&j), "{what}: {j:?}");
    }
}

#[test]
fn not_live_is_never_a_pass_even_at_the_image() {
    let m = managed(vec![diff("apps", "Deployment", Some(&deployment(&[IMAGE], &[])), None, false)]);
    let running = json!({ "phase": "Running", "operation": { "sync": { "revision": REV } } });
    let terminating = json!({ "phase": "Terminating", "operation": { "sync": { "revision": REV } } });
    for (what, a) in [
        ("out of sync", app("OutOfSync", "Healthy", None)),
        ("unknown sync", app("Unknown", "Healthy", None)),
        ("synced lower-case", app("synced", "Healthy", None)),
        ("healthy lower-case", app("Synced", "healthy", None)),
        ("healthy padded", app("Synced", "Healthy ", None)),
        ("progressing", app("Synced", "Progressing", None)),
        ("degraded", app("Synced", "Degraded", None)),
        ("missing", app("Synced", "Missing", None)),
        ("suspended", app("Synced", "Suspended", None)),
        ("no status", serde_json::from_value(json!({ "metadata": { "name": "web" } })).unwrap()),
        ("operation running", app("Synced", "Healthy", Some(running))),
        ("operation terminating", app("Synced", "Healthy", Some(terminating))),
    ] {
        let j = judge(&a, &m, IMAGE);
        assert!(!is_pass(&j), "{what}: {j:?}");
    }
}

#[test]
fn a_failed_sync_of_the_current_revision_fails_the_gate() {
    let m = managed(vec![diff("apps", "Deployment", Some(&deployment(&[IMAGE], &[])), None, false)]);
    let failed = json!({
        "phase": "Failed",
        "message": "hook migrate failed",
        "operation": { "sync": { "revision": REV } },
        "syncResult": { "revision": REV }
    });
    let j = judge(&app("Synced", "Healthy", Some(failed)), &m, IMAGE);
    assert!(matches!(j, Judgement::Fail { .. }), "{j:?}");
}
