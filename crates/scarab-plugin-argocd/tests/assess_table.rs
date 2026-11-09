//! Table tests of the plugin's pure library code against Argo CD-shaped JSON
//! (`tests/fixtures/argocd/`, synthetic: realistic `Application` and
//! managed-resources shapes, `targetState` as Argo's JSON-in-a-string).
//!
//! - **version** (ADR-0070 §4): target pod-template images, hooks excluded,
//!   initContainers included, live state and `summary.images` ignored;
//! - **assess** (ADR-0070 §5, §11; plan-d's table): `failed` only once Argo
//!   gave up, `live` only synced and healthy;
//! - **multi-source** Applications (`spec.sources`): `status.sync.revisions`
//!   and the operation's `revisions` lists in place of the single
//!   `revision`.

use scarab_plugin_argocd::argo::{Application, ManagedResources};
use scarab_plugin_argocd::assess::{assess, Status};
use scarab_plugin_argocd::version::{target_images, version_of};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/argocd/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn app(name: &str) -> Application {
    serde_json::from_str(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn managed(name: &str) -> ManagedResources {
    serde_json::from_str(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

const WEB_VERSION: &str = "docker.io/library/busybox:1.36,\
ghcr.io/acme/log-shipper:3.1,\
ghcr.io/acme/web-cron:7,\
ghcr.io/acme/web-migrations:1.42.0,\
ghcr.io/acme/web:1.42.0,\
ghcr.io/acme/web:1.42.0-canary";

#[test]
fn the_version_is_the_target_pod_template_images_without_hooks() {
    let images = target_images(&managed("managed-web.json"));
    assert_eq!(version_of(&images).as_deref(), Some(WEB_VERSION));
    for absent in [
        // live only (the old tag, an injected sidecar)
        "ghcr.io/acme/web:1.41.0",
        "docker.io/istio/proxyv2:1.22.3",
        // hooks: Argo-flagged, and Helm-annotated
        "ghcr.io/acme/web-migrations:1.42.0-hook",
        "docker.io/curlimages/curl:8.10.1",
        // a lookalike kind in another group; a resource Argo would prune
        "ghcr.io/evil/not-a-workload:1",
        "ghcr.io/acme/web-legacy:0.9",
    ] {
        assert!(
            !images.contains(absent),
            "{absent} must not be in {images:?}"
        );
    }
    assert!(
        images.contains("ghcr.io/acme/web-migrations:1.42.0"),
        "initContainers count"
    );
}

#[test]
fn a_config_only_change_leaves_the_version_as_it_was() {
    // The documented blind spot (ADR-0070 §4): replicas and ConfigMap data
    // moved, images did not.
    assert_eq!(
        version_of(&target_images(&managed(
            "managed-web-config-only-change.json"
        ))),
        version_of(&target_images(&managed("managed-web.json")))
    );
}

#[test]
fn no_pod_templates_means_no_version() {
    for name in ["managed-config-only-app.json", "managed-empty.json"] {
        let images = target_images(&managed(name));
        assert!(images.is_empty(), "{name}: {images:?}");
        assert_eq!(version_of(&images), None, "{name}");
    }
}

#[test]
fn assess_maps_argo_states_to_statuses() {
    use Status::*;
    // (fixture, status, label must contain)
    let table: &[(&str, Status, &str)] = &[
        ("app-synced-healthy.json", Live, "synced, healthy"),
        // OutOfSync with no operation on its revision: Argo has not started.
        // The failed operation there synced an OLDER revision.
        ("app-out-of-sync-auto.json", InProgress, "awaiting auto-sync"),
        ("app-out-of-sync-manual.json", InProgress, "awaiting sync"),
        ("app-hook-running.json", InProgress, "hook migrate running"),
        (
            "app-rollout-progressing.json",
            InProgress,
            "progressing: Waiting for rollout to finish: 2 of 4",
        ),
        // A failed attempt with retries left is NOT a give-up. Argo's
        // "Retrying attempt #N at T" note is dropped: the prefix says it.
        (
            "app-retrying.json",
            InProgress,
            "retry 2/3: one or more synchronization tasks completed unsuccessfully",
        ),
        // Retries exhausted: the first failed hook is the reason, one line.
        (
            "app-hook-failed-retries-exhausted.json",
            Failed,
            "hook migrate failed: Job has reached the specified backoff limit secret \"db-creds\" not found",
        ),
        // Terminated by the sync timeout: also a give-up.
        (
            "app-sync-timeout.json",
            Failed,
            "sync failed: Operation terminated, triggered by timeout",
        ),
        (
            "app-terminating.json",
            InProgress,
            "terminating: Operation forced to terminate",
        ),
        // Degraded is a give-up only past the progress deadline.
        (
            "app-degraded-progress-deadline.json",
            Failed,
            "exceeded its progress deadline",
        ),
        (
            "app-degraded-crashloop.json",
            InProgress,
            "degraded: Readiness probe failed",
        ),
        // A degraded Argo Rollout has given up, whatever its message says:
        // the Rollouts controller is Degraded only when aborted, timed out
        // or invalid (Argo CD's Rollout health check).
        (
            "app-rollout-aborted.json",
            Failed,
            "degraded: RolloutAborted: Rollout aborted update to revision 7",
        ),
        (
            "app-rollout-progress-deadline.json",
            Failed,
            "has timed out progressing",
        ),
        ("app-suspended.json", InProgress, "paused: Rollout is paused"),
        ("app-missing.json", InProgress, "missing"),
    ];
    for (name, status, label) in table {
        let a = assess(&app(name));
        assert_eq!(a.status, *status, "{name}: {a:?}");
        assert!(
            a.label.contains(label),
            "{name}: {:?} lacks {label:?}",
            a.label
        );
        assert!(
            !a.label.chars().any(char::is_control),
            "{name}: labels are one line"
        );
    }
}

#[test]
fn a_retry_label_drops_argos_retrying_attempt_note() {
    // Argo's controller appends ". Retrying attempt #N at <time>." to the
    // operation message. Kept, it repeats the "retry n/l" prefix and makes
    // every retry a new label.
    let expected = "retry 2/3: one or more synchronization tasks completed unsuccessfully";
    let mut a = app("app-retrying.json");
    // The suffix form Argo writes (the prefix form is the table row above).
    a.status.operation_state.as_mut().unwrap().message = Some(
        "one or more synchronization tasks completed unsuccessfully. Retrying attempt #2 at 9:14AM."
            .into(),
    );
    assert_eq!(assess(&a).label, expected);
    // A message that is only the note leaves the bare prefix.
    a.status.operation_state.as_mut().unwrap().message =
        Some("Retrying attempt #2 at 9:14AM.".into());
    assert_eq!(assess(&a).label, "retry 2/3");
    // Text that merely mentions retrying stays.
    a.status.operation_state.as_mut().unwrap().message =
        Some("hook failed. Retrying attempt is disabled".into());
    assert_eq!(
        assess(&a).label,
        "retry 2/3: hook failed. Retrying attempt is disabled"
    );
}

#[test]
fn a_rollouts_timed_out_message_is_a_give_up_wherever_argo_puts_it() {
    // Argo Rollouts says "timed out progressing", not "progress deadline";
    // an Argo CD that copies it onto the Application (and lists no Rollout
    // resource with it) must still read it as a give-up.
    let mut a = app("app-degraded-crashloop.json");
    assert_eq!(assess(&a).status, Status::InProgress);
    a.status.health.message =
        Some("ReplicaSet \"web-6d8f7c9b5\" has timed out progressing.".into());
    assert_eq!(assess(&a).status, Status::Failed);
    // A Rollout that is merely progressing or paused is not a give-up.
    let mut r = app("app-rollout-aborted.json");
    for res in r.status.resources.iter_mut() {
        if let Some(h) = res.health.as_mut() {
            h.status = "Progressing".into();
        }
    }
    r.status.health.status = "Progressing".into();
    assert_eq!(assess(&r).status, Status::InProgress);
}

#[test]
fn a_finished_operation_on_the_current_revision_decides_even_when_synced() {
    // A PostSync hook failed after the resources applied: Argo says Failed,
    // and the contract follows Argo.
    let mut a = app("app-hook-failed-retries-exhausted.json");
    a.status.sync.status = "Synced".into();
    assert_eq!(assess(&a).status, Status::Failed);
    // The same failure on an older revision says nothing about this one.
    a.status.sync.revision = Some("ffffffffffffffffffffffffffffffffffffffff".into());
    assert_eq!(assess(&a).status, Status::Live);
}

#[test]
fn a_multi_source_application_is_judged_by_its_revision_lists() {
    // Synced and healthy, with only `revisions` set: live.
    let live = app("app-multi-source-synced-healthy.json");
    assert_eq!(live.status.sync.revision, None);
    assert_eq!(live.status.sync.revisions.len(), 2);
    assert_eq!(assess(&live).status, Status::Live);
    // A failed operation whose `revisions` equal the target's: Argo gave up
    // on this version.
    let a = assess(&app("app-multi-source-failed-current.json"));
    assert_eq!(a.status, Status::Failed, "{a:?}");
    assert!(
        a.label
            .starts_with("sync failed: one or more objects failed to apply"),
        "{a:?}"
    );
    // The same failure for an older first source says nothing about the
    // current target: the sync status decides.
    let a = assess(&app("app-multi-source-failed-older.json"));
    assert_eq!(a.status, Status::InProgress, "{a:?}");
    assert_eq!(a.label, "awaiting auto-sync");
}
