//! Make Kubernetes pass every string Scarab puts in a Pod through unchanged.
//!
//! Shared by every crate that builds Pods: the k8s executor (step, service and
//! debug Pods) and the plugin runtime (Plugin and isolation-probe Pods).

use k8s_openapi::api::core::v1::PodSpec;

/// Write every `$` in `spec` as `$$`, so the kubelet expands nothing.
///
/// The kubelet expands `$(VAR)` in a container's `command`, `args` and
/// `env[].value` from that container's environment, and reduces `$$` to `$`
/// (see the `Container` and `EnvVar` API docs). It does the same to an exec
/// probe's command (`ExpandContainerCommandOnlyStatic` in the kubelet's
/// prober), though the API docs do not say so. Scarab renders `${{ }}` once at
/// launch, and Step secrets are plain env vars, so without this a rendered
/// value from outside the Pipeline (a dispatch parameter, a webhook field, a
/// Step output) holding `$(DEPLOY_TOKEN)` would become the secret before the
/// program ran, with no shell involved. It also made `sh -c 'echo $$'` run as
/// `echo $` in a Pod while the local executor runs it as written. A Plugin's
/// config value is an org admin's text, and is passed through the same way.
///
/// Writing every `$` as `$$` turns expansion off: Kubernetes reduces each pair
/// back to one `$`, and an escaped `$$(VAR)` is never expanded. Nothing Scarab
/// generates relies on `$(VAR)` expansion; a string that ever needs it must be
/// added after this pass, and say so.
///
/// Not touched, because Kubernetes does not expand them: `env[].valueFrom`,
/// `workingDir`, `image`, HTTP-probe paths and headers, lifecycle hooks (Scarab
/// sets none). Apply it once per Pod, after the Pod is assembled: a second pass
/// would double the escaping.
pub fn pass_through_k8s_expansion(spec: &mut PodSpec) {
    fn escape(s: &mut String) {
        if s.contains('$') {
            *s = s.replace('$', "$$");
        }
    }
    fn escape_all(v: &mut Option<Vec<String>>) {
        v.iter_mut().flatten().for_each(escape);
    }
    let containers = spec
        .containers
        .iter_mut()
        .chain(spec.init_containers.iter_mut().flatten());
    for c in containers {
        escape_all(&mut c.command);
        escape_all(&mut c.args);
        for e in c.env.iter_mut().flatten() {
            if let Some(v) = e.value.as_mut() {
                escape(v);
            }
        }
        for probe in [
            &mut c.startup_probe,
            &mut c.readiness_probe,
            &mut c.liveness_probe,
        ]
        .into_iter()
        .flatten()
        {
            if let Some(exec) = probe.exec.as_mut() {
                escape_all(&mut exec.command);
            }
        }
    }
}
