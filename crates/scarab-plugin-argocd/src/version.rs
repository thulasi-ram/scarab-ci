//! The DeploymentVersion of an Argo CD Application, in the scheme
//! [`VERSION_SCHEME`] (ADR-0070 §4, PROVISIONAL): the image references in
//! the pod templates of its **target** manifests — what Argo rendered from
//! git — not of its live objects.
//!
//! Why target, not live (`status.summary.images`): during a rolling update
//! live is the union of old and new, a mutating webhook's injected sidecars
//! appear in it, Sync-hook Pods flicker in and out of it, and a
//! scale-to-zero app has none. Each would mint a spurious Deployment that
//! supersedes the requested one.
//!
//! Hooks are excluded (owner default 5): a migration Job's image is not what
//! the Application serves, and it would make the version change whenever a
//! hook does.
//!
//! The documented blind spots stay blind: tags not digests, and a
//! config-only change (values, ConfigMap, replicas) leaves the version as it
//! was.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::argo::{ManagedResources, ResourceDiff};

/// The scheme every version this reporter sends is written in.
pub const VERSION_SCHEME: &str = "argocd.target-images.v1";

/// Annotations that make a manifest a hook even when Argo's `hook` flag is
/// absent (older Argo versions, Helm charts).
const HOOK_ANNOTATIONS: [&str; 2] = ["argocd.argoproj.io/hook", "helm.sh/hook"];

/// Where each workload kind keeps its pod spec, by `(group, kind)`.
fn pod_spec_path(group: &str, kind: &str) -> Option<&'static [&'static str]> {
    const TEMPLATE: &[&str] = &["spec", "template", "spec"];
    match (group, kind) {
        ("", "Pod") => Some(&["spec"]),
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") => Some(TEMPLATE),
        ("batch", "Job") => Some(TEMPLATE),
        ("batch", "CronJob") => Some(&["spec", "jobTemplate", "spec", "template", "spec"]),
        // An Argo Rollout with an inline template. One that points at a
        // Deployment (`workloadRef`) has no template here; the Deployment it
        // names is itself a managed resource, or is not in git.
        ("argoproj.io", "Rollout") => Some(TEMPLATE),
        _ => None,
    }
}

/// A resource's target manifest, if it has one (a resource Argo would
/// prune has none).
fn target_manifest(diff: &ResourceDiff) -> Option<Value> {
    let raw = diff.target_state.as_deref()?.trim();
    if raw.is_empty() || raw == "null" {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                kind = %diff.kind,
                name = %diff.name,
                error = %e,
                "argocd: a managed resource's targetState is not JSON; skipped"
            );
            None
        }
    }
}

fn is_hook(diff: &ResourceDiff, manifest: &Value) -> bool {
    diff.hook
        || HOOK_ANNOTATIONS.iter().any(|a| {
            manifest
                .pointer(&format!(
                    "/metadata/annotations/{}",
                    a.replace('~', "~0").replace('/', "~1")
                ))
                .is_some()
        })
}

/// The group of a manifest: Argo's own field, else the `apiVersion`'s group.
fn group_of<'a>(diff: &'a ResourceDiff, manifest: &'a Value) -> &'a str {
    if !diff.group.is_empty() {
        return &diff.group;
    }
    match manifest.get("apiVersion").and_then(Value::as_str) {
        Some(api) => api.rsplit_once('/').map(|(g, _)| g).unwrap_or(""),
        None => "",
    }
}

fn kind_of<'a>(diff: &'a ResourceDiff, manifest: &'a Value) -> &'a str {
    if !diff.kind.is_empty() {
        return &diff.kind;
    }
    manifest.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// Every image reference in the target pod templates of the Application's
/// non-hook workloads — `containers` and `initContainers` — as written in
/// the manifest.
pub fn target_images(resources: &ManagedResources) -> BTreeSet<String> {
    let mut images = BTreeSet::new();
    for diff in &resources.items {
        let Some(manifest) = target_manifest(diff) else {
            continue;
        };
        if is_hook(diff, &manifest) {
            continue;
        }
        let Some(path) = pod_spec_path(group_of(diff, &manifest), kind_of(diff, &manifest)) else {
            continue;
        };
        let Some(pod_spec) = path.iter().try_fold(&manifest, |v, key| v.get(key)) else {
            continue;
        };
        for list in ["containers", "initContainers"] {
            for container in pod_spec
                .get(list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(image) = container.get("image").and_then(Value::as_str) {
                    let image = image.trim();
                    if !image.is_empty() {
                        images.insert(image.to_string());
                    }
                }
            }
        }
    }
    images
}

/// The version string: the sorted, de-duplicated references joined by `,`.
/// `None` for an empty set — an Application with no pod templates (only
/// ConfigMaps, CRDs, …) has no version in this scheme and is not reported
/// (owner default 7).
pub fn version_of(images: &BTreeSet<String>) -> Option<String> {
    if images.is_empty() {
        return None;
    }
    Some(images.iter().cloned().collect::<Vec<_>>().join(","))
}
