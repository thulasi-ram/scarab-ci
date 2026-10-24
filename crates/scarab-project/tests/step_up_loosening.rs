//! ADR-0073 §3: which Environment governance edits loosen it. Unit cases for
//! each rule (E1-E4), and properties: tightening is never flagged, and
//! dropping an approver or adding an uncovered glob always is.

use std::num::NonZeroU32;

use proptest::prelude::*;
use scarab_project::{
    deployment_source_additions, eligibility_loosenings, glob_subsumes, overlay_loosenings,
    privilege_grant_loosenings, protection_loosenings, EnvironmentDeploymentDeadline,
    EnvironmentDeploymentSource, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProtectionRules, ImageGrant, Loosening,
};

fn rules(approvers: &[&str], refs: &[&str]) -> EnvironmentProtectionRules {
    EnvironmentProtectionRules {
        approvers: approvers.iter().map(|s| s.to_string()).collect(),
        allowed_refs: refs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn whats(l: Vec<Loosening>) -> Vec<String> {
    l.into_iter().map(|l| l.what).collect()
}

fn projects(p: &[&str]) -> EnvironmentProjectEligibility {
    EnvironmentProjectEligibility::Projects(p.iter().map(|s| s.to_string()).collect())
}

fn source(installation: &str, grouping: &str) -> EnvironmentDeploymentSource {
    EnvironmentDeploymentSource {
        installation: installation.into(),
        vendor_grouping: grouping.into(),
    }
}

fn image(digest: &str, privileged: bool, caps: &[&str]) -> ImageGrant {
    ImageGrant {
        image_digest: digest.into(),
        privileged,
        capabilities: caps.iter().map(|s| s.to_string()).collect(),
    }
}

fn grant(images: Vec<ImageGrant>, permit_k8s_overlay: bool) -> EnvironmentProjectPrivilegeGrant {
    EnvironmentProjectPrivilegeGrant {
        org: "acme".into(),
        environment: "prod".into(),
        project: "web".into(),
        privileged_images: images,
        permit_k8s_overlay,
    }
}

fn overlay(
    rules: EnvironmentProtectionRules,
    deadline_ms: Option<i64>,
) -> EnvironmentProjectOverlay {
    EnvironmentProjectOverlay {
        org: "acme".into(),
        environment: "prod".into(),
        project: "web".into(),
        rules,
        deployment_gate_deadline: deadline_ms
            .map(|ms| EnvironmentDeploymentDeadline::new(ms).unwrap()),
    }
}

// --- E1: protection ---------------------------------------------------------

#[test]
fn approvers_loosen_only_by_removal() {
    assert_eq!(
        whats(protection_loosenings(
            &rules(&["amy", "bob"], &[]),
            &rules(&["bob"], &[])
        )),
        vec!["removes approver `amy`"]
    );
    assert!(protection_loosenings(&rules(&["bob"], &[]), &rules(&["bob", "cat"], &[])).is_empty());
}

#[test]
fn allowed_refs_loosen_by_widening_or_clearing() {
    // main → refs/heads/* widens.
    assert_eq!(
        whats(protection_loosenings(
            &rules(&[], &["refs/heads/main"]),
            &rules(&[], &["refs/heads/*"])
        )),
        vec!["widens allowed refs: `refs/heads/*`"]
    );
    // * → main narrows.
    assert!(
        protection_loosenings(&rules(&[], &["*"]), &rules(&[], &["refs/heads/main"])).is_empty()
    );
    // Clearing a restricted list allows any ref.
    assert_eq!(
        whats(protection_loosenings(
            &rules(&[], &["refs/heads/main"]),
            &rules(&[], &[])
        )),
        vec!["allows any ref"]
    );
    // From unrestricted, every list is a tightening.
    assert!(protection_loosenings(&rules(&[], &[]), &rules(&[], &["refs/tags/*"])).is_empty());
    // A sibling glob is not covered.
    assert_eq!(
        whats(protection_loosenings(
            &rules(&[], &["refs/heads/main"]),
            &rules(&[], &["refs/heads/main", "refs/tags/*"])
        )),
        vec!["widens allowed refs: `refs/tags/*`"]
    );
}

#[test]
fn timer_reason_and_concurrency_loosen_one_way() {
    let base = EnvironmentProtectionRules {
        wait_timer_s: 60,
        require_reason: true,
        deployment_concurrency: NonZeroU32::new(2),
        ..Default::default()
    };
    let looser = EnvironmentProtectionRules {
        wait_timer_s: 30,
        require_reason: false,
        deployment_concurrency: NonZeroU32::new(3),
        ..Default::default()
    };
    assert_eq!(protection_loosenings(&base, &looser).len(), 3);
    assert!(
        protection_loosenings(&looser, &base).is_empty(),
        "the reverse tightens"
    );
    let unlimited = EnvironmentProtectionRules {
        deployment_concurrency: None,
        ..base.clone()
    };
    assert_eq!(
        whats(protection_loosenings(&base, &unlimited)),
        vec!["removes the deployment concurrency limit of 2"]
    );
    assert!(protection_loosenings(&unlimited, &base).is_empty());
    assert!(protection_loosenings(&base, &base).is_empty());
}

#[test]
fn an_overlay_loosens_against_the_old_overlay_or_the_default() {
    // Creating an overlay never loosens: none reads as the permissive default.
    assert!(
        overlay_loosenings(None, &overlay(rules(&["amy"], &["main"]), Some(60_000))).is_empty()
    );
    let old = overlay(rules(&["amy"], &[]), Some(60_000));
    assert_eq!(
        whats(overlay_loosenings(
            Some(&old),
            &overlay(rules(&[], &[]), Some(60_000))
        )),
        vec!["removes approver `amy`"]
    );
    assert_eq!(
        whats(overlay_loosenings(
            Some(&old),
            &overlay(rules(&["amy"], &[]), None)
        )),
        vec!["removes the deployment gate deadline of 60000 ms"]
    );
    assert_eq!(
        overlay_loosenings(Some(&old), &overlay(rules(&["amy"], &[]), Some(120_000))).len(),
        1
    );
    assert!(
        overlay_loosenings(Some(&old), &overlay(rules(&["amy"], &[]), Some(60_000))).is_empty()
    );
}

// --- E2: eligibility -------------------------------------------------------

#[test]
fn eligibility_loosens_by_opening_up() {
    use EnvironmentProjectEligibility::AllProjects;
    assert!(eligibility_loosenings(&AllProjects, &projects(&["web"])).is_empty());
    assert_eq!(
        whats(eligibility_loosenings(&projects(&["web"]), &AllProjects)),
        vec!["admits every project"]
    );
    assert_eq!(
        whats(eligibility_loosenings(
            &projects(&["web"]),
            &projects(&["web", "api"])
        )),
        vec!["admits projects matching `api`"]
    );
    assert!(eligibility_loosenings(&projects(&["web-*"]), &projects(&["web-a"])).is_empty());
    assert!(eligibility_loosenings(&projects(&["web", "api"]), &projects(&[])).is_empty());
}

// --- E3: deployment sources ------------------------------------------------

#[test]
fn only_an_added_source_is_governed() {
    let argo = source("argocd", "prod");
    let flux = source("flux", "prod");
    assert_eq!(
        whats(deployment_source_additions(
            &[],
            std::slice::from_ref(&argo)
        )),
        vec!["adds deployment source `argocd` / `prod`"]
    );
    assert_eq!(
        whats(deployment_source_additions(
            std::slice::from_ref(&argo),
            &[argo.clone(), flux.clone()]
        )),
        vec!["adds deployment source `flux` / `prod`"]
    );
    assert!(
        deployment_source_additions(&[argo.clone(), flux], std::slice::from_ref(&argo)).is_empty()
    );
    // A different grouping under the same installation is a different source.
    assert_eq!(
        deployment_source_additions(std::slice::from_ref(&argo), &[source("argocd", "staging")])
            .len(),
        1
    );
}

// --- E4: privilege grants --------------------------------------------------

#[test]
fn a_grant_loosens_by_digest_privilege_capability_or_overlay() {
    let old = grant(vec![image("sha256:a", false, &["NET_ADMIN"])], false);
    assert_eq!(
        whats(privilege_grant_loosenings(
            Some(&old),
            &grant(
                vec![
                    image("sha256:a", true, &["NET_ADMIN", "SYS_ADMIN"]),
                    image("sha256:b", false, &[]),
                ],
                true,
            )
        )),
        vec![
            "lets image `sha256:a` run privileged",
            "lets image `sha256:a` add capability `SYS_ADMIN`",
            "grants image `sha256:b`",
            "permits a raw k8s_overlay",
        ]
    );
    // Dropping a digest, a capability or `privileged` tightens.
    let wide = grant(
        vec![
            image("sha256:a", true, &["NET_ADMIN"]),
            image("sha256:b", false, &[]),
        ],
        true,
    );
    assert!(privilege_grant_loosenings(
        Some(&wide),
        &grant(vec![image("sha256:a", false, &[])], false)
    )
    .is_empty());
    // An empty, overlay-less grant where there was none loosens nothing.
    assert!(privilege_grant_loosenings(None, &grant(vec![], false)).is_empty());
    assert_eq!(
        privilege_grant_loosenings(None, &grant(vec![], true)).len(),
        1
    );
}

// --- properties -------------------------------------------------------------

/// Globs over a small alphabet that never START with `*`, `z` or `/`, so a
/// pattern beginning with `z` is provably outside every one of them.
fn anchored_glob() -> impl Strategy<Value = String> {
    "[abc][abc/*]{0,6}"
}

/// A narrowing of `pattern`: each `*` kept or replaced by a literal run.
fn narrow(pattern: &str, fills: &[String]) -> String {
    let mut out = String::new();
    let mut fills = fills.iter().cycle();
    for c in pattern.chars() {
        if c == '*' {
            match fills.next() {
                Some(f) if f.as_str() != "*" => out.push_str(f),
                _ => out.push('*'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn names() -> impl Strategy<Value = Vec<String>> {
    prop::collection::btree_set("[a-e]{1,3}", 0..5).prop_map(|s| s.into_iter().collect())
}

proptest! {
    /// Containment is sound: whatever an inner `*` stands for, every name the
    /// inner glob matches is matched by an outer glob that subsumes it.
    #[test]
    fn subsumption_is_sound(
        outer in "[ab*]{0,5}",
        inner in "[ab*]{0,5}",
        fills in prop::collection::vec("[ab]{0,3}", 1..4),
    ) {
        if glob_subsumes(&outer, &inner) {
            let instance = narrow(&inner, &fills).replace('*', "");
            let eligibility = EnvironmentProjectEligibility::Projects(vec![outer.clone()]);
            prop_assert!(
                eligibility.admits(&instance),
                "`{outer}` subsumes `{inner}` but not its instance `{instance}`"
            );
        }
    }

    /// Adding approvers (keeping every old one) never loosens.
    #[test]
    fn a_superset_of_approvers_is_never_flagged(old in names(), extra in names()) {
        let mut new = old.clone();
        new.extend(extra);
        let o = EnvironmentProtectionRules { approvers: old, ..Default::default() };
        let n = EnvironmentProtectionRules { approvers: new, ..Default::default() };
        prop_assert!(protection_loosenings(&o, &n).is_empty());
    }

    /// Dropping any approver is always flagged, by name.
    #[test]
    fn dropping_any_approver_is_always_flagged(
        old in names().prop_filter("non-empty", |v| !v.is_empty()),
        pick in any::<prop::sample::Index>(),
    ) {
        let dropped = old[pick.index(old.len())].clone();
        let new: Vec<String> = old.iter().filter(|a| **a != dropped).cloned().collect();
        let o = EnvironmentProtectionRules { approvers: old, ..Default::default() };
        let n = EnvironmentProtectionRules { approvers: new, ..Default::default() };
        let flagged = whats(protection_loosenings(&o, &n));
        prop_assert!(flagged.contains(&format!("removes approver `{dropped}`")), "{flagged:?}");
    }

    /// Allowed refs that are each a narrowing of an old one never loosen.
    #[test]
    fn subsumed_refs_are_never_flagged(
        old in prop::collection::vec(anchored_glob(), 1..4),
        picks in prop::collection::vec(any::<prop::sample::Index>(), 1..4),
        fills in prop::collection::vec("[abc/]{0,3}|\\*", 1..4),
    ) {
        let new: Vec<String> = picks.iter().map(|p| narrow(&old[p.index(old.len())], &fills)).collect();
        let o = EnvironmentProtectionRules { allowed_refs: old, ..Default::default() };
        let n = EnvironmentProtectionRules { allowed_refs: new, ..Default::default() };
        prop_assert!(protection_loosenings(&o, &n).is_empty());
    }

    /// A ref glob no old glob can cover is always flagged.
    #[test]
    fn an_unsubsumed_glob_is_always_flagged(
        old in prop::collection::vec(anchored_glob(), 1..4),
        tail in "[abc/*]{0,6}",
    ) {
        let added = format!("z{tail}");
        let mut new = old.clone();
        new.push(added.clone());
        let o = EnvironmentProtectionRules { allowed_refs: old, ..Default::default() };
        let n = EnvironmentProtectionRules { allowed_refs: new, ..Default::default() };
        let flagged = whats(protection_loosenings(&o, &n));
        prop_assert!(flagged.contains(&format!("widens allowed refs: `{added}`")), "{flagged:?}");
    }

    /// Narrower eligibility — a subset of narrowed patterns, from a list or
    /// from every Project — never loosens; adding an uncovered pattern does.
    #[test]
    fn narrower_eligibility_is_never_flagged(
        old in prop::collection::vec(anchored_glob(), 1..4),
        picks in prop::collection::vec(any::<prop::sample::Index>(), 0..4),
        fills in prop::collection::vec("[abc/]{0,3}|\\*", 1..4),
        tail in "[abc*]{0,4}",
    ) {
        let new: Vec<String> = picks.iter().map(|p| narrow(&old[p.index(old.len())], &fills)).collect();
        let o = EnvironmentProjectEligibility::Projects(old.clone());
        let n = EnvironmentProjectEligibility::Projects(new.clone());
        prop_assert!(eligibility_loosenings(&o, &n).is_empty());
        prop_assert!(eligibility_loosenings(&EnvironmentProjectEligibility::AllProjects, &n).is_empty());

        let mut wider = new;
        wider.push(format!("z{tail}"));
        prop_assert_eq!(
            eligibility_loosenings(&o, &EnvironmentProjectEligibility::Projects(wider)).len(),
            1
        );
    }

    /// Removing digests, capabilities, `privileged` or the overlay
    /// permission never loosens a grant.
    #[test]
    fn a_removed_grant_is_never_flagged(
        old in prop::collection::vec(
            ("sha256:[a-d]", any::<bool>(), prop::collection::vec("[A-D]", 0..3)),
            0..5,
        ),
        keep in prop::collection::vec(any::<bool>(), 5),
        keep_caps in any::<bool>(),
        keep_privileged in any::<bool>(),
        permit in any::<bool>(),
        keep_permit in any::<bool>(),
    ) {
        let old_images: Vec<ImageGrant> = old
            .iter()
            .map(|(d, p, c)| ImageGrant { image_digest: d.clone(), privileged: *p, capabilities: c.clone() })
            .collect();
        let new_images: Vec<ImageGrant> = old_images
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(i, _)| ImageGrant {
                image_digest: i.image_digest.clone(),
                privileged: i.privileged && keep_privileged,
                capabilities: if keep_caps { i.capabilities.clone() } else { Vec::new() },
            })
            .collect();
        let o = grant(old_images, permit);
        let n = grant(new_images, permit && keep_permit);
        prop_assert!(privilege_grant_loosenings(Some(&o), &n).is_empty());
    }
}
