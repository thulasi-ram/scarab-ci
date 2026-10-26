//! Which Environment governance edits **loosen** it (ADR-0073 §3): the rules
//! that decide whether a conditional crown-jewel write needs a step-up.
//!
//! Each function compares what is stored with what is about to be written and
//! returns one [`Loosening`] per way the write widens what may deploy, who
//! must agree, or what a step may do; empty means the write tightens or
//! changes nothing that matters. They are **fail-closed**: when in doubt a
//! change counts as a loosening, because a false positive costs one passkey
//! tap and a false negative is the hole step-up exists to close.

use std::collections::BTreeSet;

use crate::{
    glob_match, EnvironmentDeploymentSource, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules,
};

/// One way a governance write loosens an Environment, in words a human
/// confirming it reads ("removes approver `amy`").
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loosening {
    pub what: String,
}

impl Loosening {
    fn new(what: impl Into<String>) -> Self {
        Self { what: what.into() }
    }
}

/// Does every name `inner` matches also match `outer`? Both are `*`-globs
/// (the only wildcard [`glob_match`] knows).
///
/// Decided by matching `outer` against `inner` **as text**, so each `*` in
/// `inner` is a literal character that only a `*` in `outer` can consume —
/// whatever an inner `*` stands for, the outer `*` that ate it stands for
/// too. That makes a `true` sound (L(inner) ⊆ L(outer)). It is not complete:
/// a few containments it cannot see answer `false`, which reads as a
/// loosening — the fail-closed direction.
pub fn glob_subsumes(outer: &str, inner: &str) -> bool {
    outer == inner || glob_match(outer, inner)
}

/// Is `pattern` covered by some pattern in `old`?
fn covered(old: &[String], pattern: &str) -> bool {
    old.iter().any(|o| glob_subsumes(o, pattern))
}

/// E1 — how replacing protection rules `old` with `new` loosens them (the
/// baseline; an overlay is [`overlay_loosenings`]):
///
/// - an approver dropped;
/// - allowed refs widened: a restricted list made empty (any ref), or a new
///   glob no old one covers. An unrestricted (empty) list can only tighten;
/// - a shorter wait timer;
/// - `require_reason` switched off;
/// - a concurrency limit removed or raised.
pub fn protection_loosenings(
    old: &EnvironmentProtectionRules,
    new: &EnvironmentProtectionRules,
) -> Vec<Loosening> {
    let mut out = Vec::new();
    for a in &old.approvers {
        if !new.approvers.contains(a) {
            out.push(Loosening::new(format!("removes approver `{a}`")));
        }
    }
    if !old.allowed_refs.is_empty() {
        if new.allowed_refs.is_empty() {
            out.push(Loosening::new("allows any ref"));
        } else {
            for g in &new.allowed_refs {
                if !covered(&old.allowed_refs, g) {
                    out.push(Loosening::new(format!("widens allowed refs: `{g}`")));
                }
            }
        }
    }
    if new.wait_timer_s < old.wait_timer_s {
        out.push(Loosening::new(format!(
            "shortens the wait timer from {} s to {} s",
            old.wait_timer_s, new.wait_timer_s
        )));
    }
    if old.require_reason && !new.require_reason {
        out.push(Loosening::new("no longer requires a reason"));
    }
    match (old.deployment_concurrency, new.deployment_concurrency) {
        (Some(a), None) => out.push(Loosening::new(format!(
            "removes the deployment concurrency limit of {a}"
        ))),
        (Some(a), Some(b)) if b > a => out.push(Loosening::new(format!(
            "raises the deployment concurrency limit from {a} to {b}"
        ))),
        _ => {}
    }
    out
}

/// E1 for one Project's overlay: the protection rules as above (no overlay
/// yet reads as the permissive default, so creating one never loosens), plus
/// its DeploymentGate deadline removed or lengthened.
pub fn overlay_loosenings(
    old: Option<&EnvironmentProjectOverlay>,
    new: &EnvironmentProjectOverlay,
) -> Vec<Loosening> {
    let default_rules = EnvironmentProtectionRules::default();
    let old_rules = old.map_or(&default_rules, |o| &o.rules);
    let mut out = protection_loosenings(old_rules, &new.rules);
    match (
        old.and_then(|o| o.deployment_gate_deadline),
        new.deployment_gate_deadline,
    ) {
        (Some(a), None) => out.push(Loosening::new(format!(
            "removes the deployment gate deadline of {} ms",
            a.millis()
        ))),
        (Some(a), Some(b)) if b > a => out.push(Loosening::new(format!(
            "lengthens the deployment gate deadline from {} ms to {} ms",
            a.millis(),
            b.millis()
        ))),
        _ => {}
    }
    out
}

/// E2 — how replacing eligibility `old` with `new` loosens it: opening it to
/// every Project, or a new pattern no old one covers. From every Project,
/// nothing can loosen.
pub fn eligibility_loosenings(
    old: &EnvironmentProjectEligibility,
    new: &EnvironmentProjectEligibility,
) -> Vec<Loosening> {
    use EnvironmentProjectEligibility::{AllProjects, Projects};
    match (old, new) {
        (AllProjects, _) => Vec::new(),
        (Projects(_), AllProjects) => vec![Loosening::new("admits every project")],
        (Projects(p), Projects(q)) => q
            .iter()
            .filter(|g| !covered(p, g))
            .map(|g| Loosening::new(format!("admits projects matching `{g}`")))
            .collect(),
    }
}

/// E3 — the EnvironmentDeploymentSources `new` adds over `old` (a new
/// Environment's `old` is empty): each is a new outside party whose reports
/// the Environment will accept. Removals, descriptions and deadlines are not
/// governed.
pub fn deployment_source_additions(
    old: &[EnvironmentDeploymentSource],
    new: &[EnvironmentDeploymentSource],
) -> Vec<Loosening> {
    let old: BTreeSet<&EnvironmentDeploymentSource> = old.iter().collect();
    let mut added: Vec<&EnvironmentDeploymentSource> =
        new.iter().filter(|s| !old.contains(s)).collect();
    added.sort();
    added.dedup();
    added
        .into_iter()
        .map(|s| {
            Loosening::new(format!(
                "adds deployment source `{}` / `{}`",
                s.installation, s.vendor_grouping
            ))
        })
        .collect()
}

/// E4 — how replacing one Project's privilege grant `old` (none = an empty
/// whitelist without the overlay permission) with `new` loosens it: a digest
/// not granted before, `privileged` switched on or a capability added for a
/// digest that was, or the raw `k8s_overlay` permission switched on.
pub fn privilege_grant_loosenings(
    old: Option<&EnvironmentProjectPrivilegeGrant>,
    new: &EnvironmentProjectPrivilegeGrant,
) -> Vec<Loosening> {
    let old_images = old.map_or(&[][..], |g| &g.privileged_images[..]);
    let mut out = Vec::new();
    for image in &new.privileged_images {
        let digest = &image.image_digest;
        // A digest listed twice before is read as the union of its entries.
        let before: Vec<_> = old_images
            .iter()
            .filter(|o| &o.image_digest == digest)
            .collect();
        if before.is_empty() {
            out.push(Loosening::new(format!("grants image `{digest}`")));
            continue;
        }
        if image.privileged && !before.iter().any(|o| o.privileged) {
            out.push(Loosening::new(format!(
                "lets image `{digest}` run privileged"
            )));
        }
        for cap in &image.capabilities {
            if !before.iter().any(|o| o.capabilities.contains(cap)) {
                out.push(Loosening::new(format!(
                    "lets image `{digest}` add capability `{cap}`"
                )));
            }
        }
    }
    if new.permit_k8s_overlay && !old.is_some_and(|g| g.permit_k8s_overlay) {
        out.push(Loosening::new("permits a raw k8s_overlay"));
    }
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsumption_reads_inner_stars_literally() {
        assert!(glob_subsumes("refs/heads/*", "refs/heads/main"));
        assert!(glob_subsumes("refs/heads/*", "refs/heads/release-*"));
        assert!(glob_subsumes("*", "anything*at*all"));
        assert!(glob_subsumes("main", "main"));
        // An inner `*` is not a literal an outer literal can match.
        assert!(!glob_subsumes("refs/heads/main", "refs/heads/*"));
        assert!(!glob_subsumes("refs/heads/r*", "refs/heads/*"));
        assert!(!glob_subsumes("refs/tags/*", "refs/heads/main"));
    }
}
