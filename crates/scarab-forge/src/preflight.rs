//! **Connection preflight** — what Scarab needs a forge credential to be
//! allowed to do, and the diff against what it actually is.
//!
//! A forge app can be misconfigured in ways that fail *silently*, which is the
//! whole reason this module exists:
//!
//!  - **No webhook events subscribed.** A GitHub App still receives
//!    `installation` / `installation_repositories` regardless of its event
//!    subscription, so the connection registers itself, `GET /v1/repos` lists
//!    Projects, and everything looks wired up — while no push ever starts a run.
//!  - **No `statuses:write` grant.** Every status post 403s deep inside the
//!    status pipeline while the Run itself goes green, so the forge simply never
//!    shows a check and nothing surfaces the refusal.
//!
//! Neither failure produces an error anyone looks at. So the requirement set is
//! **data, in one place** — [`required`] — and the diff ([`missing`]) is a pure
//! function over it, so the health readout is derived rather than a hand-kept
//! list of strings scattered through adapters and UI copy.
//!
//! The requirement vocabulary is deliberately the **forge's own** (`statuses`,
//! `contents`, `push`, `pull_request`): a preflight is only useful if it names
//! the setting the operator must go and change. That vendor naming is what makes
//! the table keyed by [`ForgeKind`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ForgeKind;

/// Which axis of an app's configuration a requirement lives on.
///
/// The two are genuinely different settings on the forge — a permission is a
/// grant on the app, an event is a subscription — and they fail differently
/// (a permission 403s a call, a missing subscription means the call never
/// happens). Keeping them one enum with a discriminator, rather than two
/// parallel lists, is what lets the gap report read as one ordered story.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Permission,
    Event,
}

impl CapabilityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityKind::Permission => "permission",
            CapabilityKind::Event => "event",
        }
    }
}

/// How badly Scarab wants a capability.
///
/// `Required` = a core promise is broken without it (no runs trigger, no checks
/// appear). `Recommended` = an *authored* feature stops working (`on: release`,
/// comment-commands, deployment history) but a repo that does not use it is
/// fine. Reporting both, and distinguishing them, is the difference between a
/// health line an operator trusts and one they learn to ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Required,
    Recommended,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Required => "required",
            Severity::Recommended => "recommended",
        }
    }
}

/// One thing Scarab needs the forge app to allow — a permission at a level, or
/// a webhook event subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeRequirement {
    pub kind: CapabilityKind,
    /// The forge's own name for it (`statuses`, `contents`, `push`).
    pub name: &'static str,
    /// For a permission, the minimum level (`read` / `write` / `admin`);
    /// `None` for an event, which is subscribed or not.
    pub level: Option<&'static str>,
    pub severity: Severity,
    /// What silently breaks without it — the sentence the UI shows. Written as
    /// a consequence, not a restatement of the name, because "statuses: write
    /// is missing" tells an operator nothing they could not read off the label.
    pub why: &'static str,
}

/// What a forge reports a connection's credential is *actually* granted:
/// permissions as `name → level`, plus the webhook events it will deliver.
///
/// Deliberately vendor-shaped strings rather than a Scarab enum — an unmapped
/// permission is still worth showing, and a closed vocabulary would silently
/// drop whatever the forge added last month.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeCapabilities {
    pub permissions: BTreeMap<String, String>,
    pub events: BTreeSet<String>,
}

impl ForgeCapabilities {
    /// Build from borrowed pairs — the shape adapters and tests have on hand.
    pub fn new<'a>(
        permissions: impl IntoIterator<Item = (&'a str, &'a str)>,
        events: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            permissions: permissions
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            events: events.into_iter().map(str::to_string).collect(),
        }
    }
}

/// Permission levels as a ladder, so `write` satisfies a `read` requirement.
/// An unknown token ranks 0 — below every requirement, which fails *closed*:
/// a level we cannot interpret is reported as a gap rather than assumed
/// sufficient.
fn level_rank(level: &str) -> u8 {
    match level {
        "read" => 1,
        "write" => 2,
        "admin" => 3,
        _ => 0,
    }
}

/// Everything Scarab needs from a forge of `kind` — **the** requirement set.
///
/// One table, ordered required-first, so the endpoint, the UI copy and the docs
/// all diff against the same data instead of three drifting lists.
pub fn required(kind: ForgeKind) -> &'static [ForgeRequirement] {
    match kind {
        ForgeKind::GitHub => GITHUB_REQUIREMENTS,
        ForgeKind::Forgejo => FORGEJO_REQUIREMENTS,
    }
}

/// The GitHub App configuration Scarab depends on.
///
/// Permissions are named as the App settings page names them; events are the
/// `X-GitHub-Event` values [`crate::ForgePort::normalize_event`] can turn into a
/// Scarab `Event`. `installation` / `installation_repositories` are absent on
/// purpose: GitHub delivers them to every App regardless of subscription, which
/// is precisely why an App with an empty event list still looks healthy.
const GITHUB_REQUIREMENTS: &[ForgeRequirement] = &[
    ForgeRequirement {
        kind: CapabilityKind::Event,
        name: "push",
        level: None,
        severity: Severity::Required,
        why: "Without it no push starts a run — the App still registers itself and its \
              repositories, so Scarab looks healthy while nothing ever builds.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Event,
        name: "pull_request",
        level: None,
        severity: Severity::Required,
        why: "Without it pull requests never build, so no PR gets a check.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Permission,
        name: "metadata",
        level: Some("read"),
        severity: Severity::Required,
        why: "Without it Scarab cannot see which repositories the installation covers, so \
              repositories never become projects.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Permission,
        name: "contents",
        level: Some("read"),
        severity: Severity::Required,
        why: "Without it Scarab cannot read .scarab pipelines or mint a clone token, so every \
              run fails at checkout.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Permission,
        name: "statuses",
        level: Some("write"),
        severity: Severity::Required,
        why: "Without it every commit status is rejected with 403 while the run itself goes \
              green — the forge simply never shows a check.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Event,
        name: "release",
        level: None,
        severity: Severity::Recommended,
        why: "Needed only by pipelines triggered `on: release`.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Event,
        name: "issue_comment",
        level: None,
        severity: Severity::Recommended,
        why: "Needed only by comment-command triggers (e.g. `/scarab run`).",
    },
    ForgeRequirement {
        kind: CapabilityKind::Permission,
        name: "pull_requests",
        level: Some("write"),
        severity: Severity::Recommended,
        why: "Needed to post run summaries back as pull-request comments.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Permission,
        name: "deployments",
        level: Some("write"),
        severity: Severity::Recommended,
        why: "Needed only by environments that record deployment history on the forge.",
    },
];

/// Forgejo's equivalents. Forgejo tokens carry OAuth-style scopes rather than
/// per-resource grants and its per-repo hooks are subscribed at registration
/// time, so nothing here is introspectable yet — the table names the events a
/// registered hook must carry, and the preflight for a Forgejo connection
/// currently answers "unknown" rather than pretending to have looked.
const FORGEJO_REQUIREMENTS: &[ForgeRequirement] = &[
    ForgeRequirement {
        kind: CapabilityKind::Event,
        name: "push",
        level: None,
        severity: Severity::Required,
        why: "Without it no push starts a run.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Event,
        name: "pull_request",
        level: None,
        severity: Severity::Required,
        why: "Without it pull requests never build.",
    },
    ForgeRequirement {
        kind: CapabilityKind::Permission,
        name: "repository",
        level: Some("write"),
        severity: Severity::Required,
        why: "Without it Scarab cannot read pipelines, clone, or post commit statuses.",
    },
];

/// The requirements `granted` does **not** satisfy, in [`required`] order.
///
/// A permission is satisfied by its level *or better* ([`level_rank`]); an event
/// by being subscribed. Unknown extras in `granted` are ignored — this answers
/// "what is missing", never "what is excessive", because a forge app is
/// routinely installed for more than Scarab.
pub fn missing(kind: ForgeKind, granted: &ForgeCapabilities) -> Vec<&'static ForgeRequirement> {
    required(kind)
        .iter()
        .filter(|req| !satisfied(req, granted))
        .collect()
}

fn satisfied(req: &ForgeRequirement, granted: &ForgeCapabilities) -> bool {
    match req.kind {
        CapabilityKind::Event => granted.events.contains(req.name),
        CapabilityKind::Permission => {
            let want = req.level.map(level_rank).unwrap_or(1);
            granted
                .permissions
                .get(req.name)
                .map(|have| level_rank(have) >= want)
                .unwrap_or(false)
        }
    }
}

/// Is anything **required** missing? The one bit a health line needs — a
/// connection with only recommended gaps is configured, just not for every
/// optional trigger.
pub fn is_degraded(kind: ForgeKind, granted: &ForgeCapabilities) -> bool {
    missing(kind, granted)
        .iter()
        .any(|req| req.severity == Severity::Required)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GitHub App configured the way the docs say.
    fn healthy_github() -> ForgeCapabilities {
        ForgeCapabilities::new(
            [
                ("metadata", "read"),
                ("contents", "read"),
                ("statuses", "write"),
                ("pull_requests", "write"),
                ("deployments", "write"),
            ],
            ["push", "pull_request", "release", "issue_comment"],
        )
    }

    #[test]
    fn a_fully_configured_app_has_no_gaps() {
        assert!(missing(ForgeKind::GitHub, &healthy_github()).is_empty());
        assert!(!is_degraded(ForgeKind::GitHub, &healthy_github()));
    }

    #[test]
    fn an_app_with_no_event_subscription_is_degraded_on_the_trigger_events() {
        // The silent failure the whole module exists for: permissions are fine,
        // the installation registered itself, and no run will ever trigger.
        let mut caps = healthy_github();
        caps.events.clear();
        let gaps = missing(ForgeKind::GitHub, &caps);
        let names: Vec<&str> = gaps.iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["push", "pull_request", "release", "issue_comment"]);
        assert!(gaps
            .iter()
            .all(|r| r.kind == CapabilityKind::Event));
        assert!(is_degraded(ForgeKind::GitHub, &caps));
    }

    #[test]
    fn a_missing_statuses_grant_is_reported_as_required() {
        let mut caps = healthy_github();
        caps.permissions.remove("statuses");
        let gaps = missing(ForgeKind::GitHub, &caps);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].name, "statuses");
        assert_eq!(gaps[0].severity, Severity::Required);
        assert!(is_degraded(ForgeKind::GitHub, &caps));
    }

    #[test]
    fn statuses_granted_read_only_does_not_satisfy_a_write_requirement() {
        // The nastier shape of the same bug: the grant exists, at a level that
        // still 403s every post.
        let mut caps = healthy_github();
        caps.permissions
            .insert("statuses".into(), "read".into());
        let gaps = missing(ForgeKind::GitHub, &caps);
        assert_eq!(gaps.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["statuses"]);
    }

    #[test]
    fn a_higher_level_satisfies_a_lower_requirement() {
        let mut caps = healthy_github();
        caps.permissions.insert("contents".into(), "write".into());
        assert!(missing(ForgeKind::GitHub, &caps).is_empty());
    }

    #[test]
    fn recommended_gaps_alone_do_not_degrade_a_connection() {
        let mut caps = healthy_github();
        caps.events.remove("release");
        caps.permissions.remove("deployments");
        let gaps = missing(ForgeKind::GitHub, &caps);
        assert_eq!(gaps.len(), 2, "both reported");
        assert!(gaps.iter().all(|r| r.severity == Severity::Recommended));
        assert!(!is_degraded(ForgeKind::GitHub, &caps));
    }

    #[test]
    fn an_unknown_permission_level_fails_closed() {
        let mut caps = healthy_github();
        caps.permissions
            .insert("statuses".into(), "sometimes".into());
        assert!(is_degraded(ForgeKind::GitHub, &caps));
    }
}
