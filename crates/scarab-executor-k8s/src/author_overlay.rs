//! What a step author's `k8s_overlay` may set on a step Pod.
//!
//! An org admin lets a Project's steps carry a `k8s_overlay` in an Environment
//! (`permit_k8s_overlay`), but the overlay is written by whoever edits the
//! Pipeline file. The step Pod runs in the release namespace beside the
//! server's Secret and ServiceAccount, so an open overlay could mount a
//! Secret, take the server's ServiceAccount, share the host's namespaces, or
//! point the sidecars at a listener of its own. The overlay may therefore set
//! only the fields in [`AUTHOR_OVERLAY_FIELDS`], and it may add to the Pod but
//! not change or remove what Scarab, the placement baseline or a placement
//! profile put there ([`refuse_changes`]). The operator's baseline and
//! profiles are not limited by either rule.
//!
//! The server checks the list at Run creation, so the author sees the refusal
//! there; the executor checks it again before it creates the Pod.

use serde_json::Value;

use super::{list_merge, ListMerge, MergeAt, STEP_CONTAINER};

/// Every field an author's `k8s_overlay` may set, with the reason it is safe.
/// A listed field takes any value beneath it (a few have extra checks, in
/// [`author_overlay_refusals`]); an object or list on the way to one (`spec`,
/// `spec.containers[step]`, `spec.volumes[]`, …) may hold only listed fields.
/// Anything else is refused with its path. `[step]` is the step container, the
/// only entry `spec.containers` may hold; `[]` is any item of a list.
pub const AUTHOR_OVERLAY_FIELDS: &[(&str, &str)] = &[
    (
        "metadata.labels",
        "labels for the cluster's own tooling; Scarab's and the chart's keys are reserved",
    ),
    (
        "metadata.annotations",
        "annotations for the cluster's own tooling; Scarab's keys and the AppArmor and seccomp \
         keys are reserved",
    ),
    (
        "spec.nodeSelector",
        "picks nodes by label; it can only narrow where the Pod runs",
    ),
    (
        "spec.affinity",
        "picks nodes and neighbouring Pods for placement; it reads nothing from them",
    ),
    (
        "spec.tolerations",
        "lets the Pod onto a tainted node pool; the list only grows, and a control-plane or \
         keyless toleration is refused",
    ),
    (
        "spec.topologySpreadConstraints",
        "spreads step Pods over zones or nodes",
    ),
    (
        "spec.priorityClassName",
        "picks one of the PriorityClasses the cluster's operator created",
    ),
    (
        "spec.schedulerName",
        "picks a scheduler the cluster runs; it decides placement only",
    ),
    ("spec.volumes[].name", "names a volume the overlay adds"),
    (
        "spec.volumes[].emptyDir",
        "scratch space that starts empty and holds no credential",
    ),
    (
        "spec.containers[step].name",
        "must be `step`: the overlay changes the step container only",
    ),
    (
        "spec.containers[step].resources.requests",
        "requests CPU, memory or an extended resource such as a GPU",
    ),
    (
        "spec.containers[step].resources.limits",
        "caps CPU, memory or an extended resource",
    ),
    (
        "spec.containers[step].env[].name",
        "names a variable the overlay adds",
    ),
    (
        "spec.containers[step].env[].value",
        "a literal value only; `valueFrom` and `envFrom` can read a Secret",
    ),
    (
        "spec.containers[step].volumeMounts[].name",
        "must name an emptyDir volume the same overlay adds",
    ),
    (
        "spec.containers[step].volumeMounts[].mountPath",
        "where that volume appears; not a path Scarab mounts",
    ),
    (
        "spec.containers[step].volumeMounts[].readOnly",
        "mounts it read-only",
    ),
];

/// Label and annotation key prefixes an author's overlay may not set or
/// delete: the Helm chart's Services select Pods on `app.kubernetes.io/name` +
/// `/instance`, so a step Pod carrying them would receive the server's
/// traffic, and the executor reads `scarab.io/*` and `scarab.dev/*` keys to
/// find, harvest and drain step Pods.
pub const RESERVED_METADATA_PREFIXES: [&str; 3] =
    ["app.kubernetes.io/", "scarab.io/", "scarab.dev/"];

/// Annotation key prefixes an author's overlay may not set: the kubelet reads
/// them as AppArmor and seccomp profiles, and `unconfined` lifts the step
/// container's confinement.
const RESERVED_ANNOTATION_PREFIXES: [&str; 3] = [
    "container.apparmor.security.beta.kubernetes.io/",
    "seccomp.security.alpha.kubernetes.io/",
    "container.seccomp.security.alpha.kubernetes.io/",
];

/// Taint key prefixes an author's overlay may not tolerate: the control-plane
/// nodes' taints.
const RESERVED_TOLERATION_PREFIXES: [&str; 1] = ["node-role.kubernetes.io/"];

/// One refusal per field of `overlay` an author may not set, each naming the
/// field's path. Empty when the overlay keeps to [`AUTHOR_OVERLAY_FIELDS`] and
/// their extra checks. Shared by the server's admission and the executor.
pub fn author_overlay_refusals(overlay: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if !overlay.is_object() {
        out.push("`k8s_overlay` must be a mapping (a Pod fragment)".to_string());
        return out;
    }
    walk(overlay, "", &mut out);
    check_metadata(overlay, &mut out);
    check_tolerations(overlay, &mut out);
    check_volumes_and_mounts(overlay, &mut out);
    out
}

/// Whether `path` is a listed field: anything beneath it is free.
fn is_listed(path: &str) -> bool {
    AUTHOR_OVERLAY_FIELDS
        .iter()
        .any(|(field, _)| *field == path)
}

/// The listed fields directly under `parent`, as the refusal names them.
fn allowed_under(parent: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (field, _) in AUTHOR_OVERLAY_FIELDS {
        let rest = if parent.is_empty() {
            Some(*field)
        } else {
            field.strip_prefix(parent).and_then(|r| r.strip_prefix('.'))
        };
        if let Some(rest) = rest {
            let name = rest.split(['.', '[']).next().unwrap_or_default();
            let name = format!("`{name}`");
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// What a path on the way to a listed field holds: an object, a list, or
/// nothing (no listed field is under it).
enum Holds {
    Object,
    List,
    Nothing,
}

fn holds(path: &str) -> Holds {
    let mut holds = Holds::Nothing;
    for (field, _) in AUTHOR_OVERLAY_FIELDS {
        let rest = if path.is_empty() {
            Some(*field)
        } else {
            field.strip_prefix(path)
        };
        match rest {
            Some(r) if r.starts_with('[') => return Holds::List,
            Some(r) if path.is_empty() || r.starts_with('.') => holds = Holds::Object,
            _ => {}
        }
    }
    holds
}

fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn walk(value: &Value, path: &str, out: &mut Vec<String>) {
    if is_listed(path) {
        return;
    }
    match (holds(path), value) {
        (Holds::Object, Value::Object(map)) => {
            for (key, v) in map {
                let p = child(path, key);
                if matches!(holds(&p), Holds::Nothing) && !is_listed(&p) {
                    let allowed = allowed_under(path).join(", ");
                    let at = if path.is_empty() {
                        "the top level".to_string()
                    } else {
                        format!("`{path}`")
                    };
                    out.push(format!(
                        "`k8s_overlay` may not set `{p}`: at {at} a step's overlay may set only \
                         {allowed}. An operator can set it in a placement profile"
                    ));
                    continue;
                }
                walk(v, &p, out);
            }
        }
        (Holds::List, Value::Array(items)) => {
            for item in items {
                if path == "spec.containers" {
                    match item.get("name").and_then(Value::as_str) {
                        Some(STEP_CONTAINER) => {
                            walk(item, &format!("{path}[{STEP_CONTAINER}]"), out);
                        }
                        other => out.push(format!(
                            "`k8s_overlay` may not add or change container {} in \
                             `spec.containers`: a step's overlay may change only the \
                             `{STEP_CONTAINER}` container. An operator can add a container in a \
                             placement profile",
                            other.map_or("without a name".to_string(), |n| format!("`{n}`"))
                        )),
                    }
                } else {
                    walk(item, &format!("{path}[]"), out);
                }
            }
        }
        (Holds::Object, _) => out.push(format!(
            "`k8s_overlay` may not replace or delete `{}`: it must be a mapping",
            if path.is_empty() { "k8s_overlay" } else { path }
        )),
        (Holds::List, _) => out.push(format!(
            "`k8s_overlay` may not replace or delete `{path}`: it must be a list"
        )),
        // `walk` is called only on a listed path or one that leads to one.
        (Holds::Nothing, _) => {}
    }
}

/// Labels and annotations: each a mapping, and no reserved key set or deleted.
fn check_metadata(overlay: &Value, out: &mut Vec<String>) {
    let reserved = RESERVED_METADATA_PREFIXES
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");
    for field in ["labels", "annotations"] {
        let Some(map) = overlay.pointer(&format!("/metadata/{field}")) else {
            continue;
        };
        let Some(map) = map.as_object() else {
            out.push(format!(
                "`k8s_overlay` may not replace or delete `metadata.{field}`: it holds the \
                 reserved {reserved} keys"
            ));
            continue;
        };
        for key in map.keys() {
            if RESERVED_METADATA_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                out.push(format!(
                    "`k8s_overlay` may not set or delete the reserved `metadata.{field}` key \
                     `{key}` (keys under {reserved} are Scarab's)"
                ));
            } else if field == "annotations"
                && RESERVED_ANNOTATION_PREFIXES
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
            {
                out.push(format!(
                    "`k8s_overlay` may not set the reserved `metadata.annotations` key `{key}`: \
                     it sets the step's AppArmor or seccomp profile"
                ));
            }
        }
    }
}

/// No toleration without a key (it tolerates every taint) and none of a
/// control-plane taint.
fn check_tolerations(overlay: &Value, out: &mut Vec<String>) {
    let Some(items) = overlay
        .pointer("/spec/tolerations")
        .and_then(Value::as_array)
    else {
        return;
    };
    for item in items {
        match item.get("key").and_then(Value::as_str) {
            None | Some("") => out.push(
                "`k8s_overlay` may not add a toleration without a `key` in `spec.tolerations`: \
                 it tolerates every taint, the control plane's among them"
                    .to_string(),
            ),
            Some(key)
                if RESERVED_TOLERATION_PREFIXES
                    .iter()
                    .any(|prefix| key.starts_with(prefix)) =>
            {
                out.push(format!(
                    "`k8s_overlay` may not tolerate the taint `{key}` in `spec.tolerations`: it \
                     keeps step Pods off the control-plane nodes"
                ));
            }
            Some(_) => {}
        }
    }
}

/// Each added volume is a named emptyDir; each variable on the step container
/// has a name; each mount on it names one of the overlay's own emptyDir volumes.
fn check_volumes_and_mounts(overlay: &Value, out: &mut Vec<String>) {
    let mut own = Vec::new();
    for volume in overlay
        .pointer("/spec/volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match (
            volume.get("name").and_then(Value::as_str),
            volume.get("emptyDir").map(Value::is_object),
        ) {
            (Some(name), Some(true)) => own.push(name),
            _ => out.push(
                "`k8s_overlay` may add a volume to `spec.volumes` only as a `name` and an \
                 `emptyDir` mapping"
                    .to_string(),
            ),
        }
    }
    let steps = overlay
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("name").and_then(Value::as_str) == Some(STEP_CONTAINER));
    for step in steps {
        for var in step
            .get("env")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if var.get("name").and_then(Value::as_str).is_none() {
                out.push(format!(
                    "`k8s_overlay` sets a variable without a `name` on the `{STEP_CONTAINER}` \
                     container"
                ));
            }
        }
        for mount in step
            .get("volumeMounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = mount.get("name").and_then(Value::as_str);
            if !name.is_some_and(|n| own.contains(&n)) {
                out.push(format!(
                    "`k8s_overlay` mounts volume {} on the `{STEP_CONTAINER}` container; a \
                     step's overlay may mount only an emptyDir volume it adds in `spec.volumes`",
                    name.map_or("without a name".to_string(), |n| format!("`{n}`"))
                ));
            }
        }
    }
}

/// Refuse an author's overlay that changed or removed anything in `before`, the
/// Pod as Scarab built it with the placement baseline and profiles merged in;
/// `after` is the Pod with the overlay merged too. The overlay may add fields
/// and list items. A list the strategic merge replaces must be unchanged; in a
/// list merged by key, each existing item must be unchanged (a container may
/// gain fields, but none it has may change); a union may only grow. `path`
/// names the field in the refusal.
pub(crate) fn refuse_changes(before: &Value, after: &Value) -> Result<(), String> {
    kept(before, after, MergeAt::Pod, "")
}

fn kept(before: &Value, after: &Value, at: MergeAt, path: &str) -> Result<(), String> {
    let Value::Object(fields) = before else {
        return if before == after {
            Ok(())
        } else {
            Err(changed(path))
        };
    };
    let Some(now) = after.as_object() else {
        return Err(changed(path));
    };
    for (key, was) in fields {
        let p = child(path, key);
        let Some(is) = now.get(key) else {
            return Err(changed(&p));
        };
        let Value::Array(was_items) = was else {
            let child_at = match (at, key.as_str()) {
                (MergeAt::Pod, "spec") => MergeAt::PodSpec,
                _ => MergeAt::Other,
            };
            kept(was, is, child_at, &p)?;
            continue;
        };
        let Some(is_items) = is.as_array() else {
            return Err(changed(&p));
        };
        let grown = is_items.len() >= was_items.len();
        match list_merge(at, key) {
            ListMerge::Replace => {
                if was_items != is_items {
                    return Err(changed(&p));
                }
            }
            ListMerge::Union => {
                if !grown || is_items[..was_items.len()] != was_items[..] {
                    return Err(changed(&p));
                }
            }
            ListMerge::ByKey(_, item_at) => {
                if !grown {
                    return Err(changed(&p));
                }
                for (i, (w, n)) in was_items.iter().zip(is_items).enumerate() {
                    let item = match w.get("name").and_then(Value::as_str) {
                        Some(name) => format!("{p}[{name}]"),
                        None => format!("{p}[{i}]"),
                    };
                    match item_at {
                        MergeAt::Container => kept(w, n, item_at, &item)?,
                        _ if w != n => return Err(changed(&item)),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn changed(path: &str) -> String {
    format!(
        "the step's `k8s_overlay` changes or removes `{path}`, which Scarab, the placement \
         baseline or a placement profile set; a step's overlay may add to the Pod but not \
         change what is already there"
    )
}
