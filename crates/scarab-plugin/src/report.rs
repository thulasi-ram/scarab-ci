//! The broker's report wire shape (ADR-0072 §11) and the
//! DeploymentReporterContract's per-item rules (ADR-0070 §11).
//!
//! A [`ReportItem`] has **no `source`**: core sets it from the caller's
//! verified identity. Unknown fields are refused, so a plugin that tries to
//! name its source (or misspells a field) is told so rather than silently
//! half-read.

use serde::{Deserialize, Serialize};

use scarab_deployment::SourceAssertedStatus;
use scarab_project::EnvironmentDeploymentSource;

/// The most items one `POST …/reports` may carry.
pub const MAX_REPORT_ITEMS: usize = 100;
/// Caps on the free-form fields, in characters. A label is shown, a version
/// compared, a link displayed — none needs more, and the record keeps every
/// one forever.
pub const MAX_REPORT_LABEL_CHARS: usize = 512;
pub const MAX_VERSION_CHARS: usize = 4096;
pub const MAX_REPORT_LINK_CHARS: usize = 2048;
/// A Kubernetes object name's limit; an Argo `<namespace>/<name>` fits.
pub const MAX_TARGET_ID_CHARS: usize = 253;
pub const MAX_IDEMPOTENCY_KEY_CHARS: usize = 128;
pub const MAX_VERSION_SCHEME_CHARS: usize = 128;
/// The most owner Projects one report may name for a target.
pub const MAX_OWNER_PROJECTS: usize = 64;

/// The body of `POST /_scarab/plugin/v1/reports`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBatch {
    pub items: Vec<ReportItem>,
}

/// One report. Internally tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReportItem {
    /// A DeploymentStatusReport about a *(target, version)*.
    Status(StatusReportItem),
    /// DeploymentTargetProjectOwnership alone: which Projects own a target.
    Ownership(OwnershipReportItem),
}

/// The statuses a source may assert (INV-DEP-6), as the wire spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedStatus {
    InProgress,
    Live,
    Failed,
}

impl From<ReportedStatus> for SourceAssertedStatus {
    fn from(s: ReportedStatus) -> Self {
        match s {
            ReportedStatus::InProgress => SourceAssertedStatus::InProgress,
            ReportedStatus::Live => SourceAssertedStatus::Live,
            ReportedStatus::Failed => SourceAssertedStatus::Failed,
        }
    }
}

impl ReportedStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportedStatus::InProgress => "in_progress",
            ReportedStatus::Live => "live",
            ReportedStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReportItem {
    /// Unique per installation; a repeat is a duplicate, never a second fact.
    pub idempotency_key: String,
    /// Per *(installation, target)*, strictly increasing in source order.
    pub sequence: u64,
    /// The vendor's grouping (an Argo CD project) — what routes the report
    /// to an Environment (ADR-0071 §5).
    pub vendor_grouping: String,
    pub target_id: String,
    pub version: String,
    /// The scheme `version` is written in (`argocd.target-images.v1`),
    /// declared by the source (ADR-0070 §4).
    pub version_scheme: String,
    pub status: ReportedStatus,
    /// Free-form, shown never interpreted. May be empty.
    pub label: String,
    /// Where the source shows this stage; `http`/`https` only.
    #[serde(default)]
    pub link: Option<String>,
    /// Unix-ms by the source's clock. Shown, never compared (ADR-0070 §8).
    pub source_at: i64,
    /// The Projects (`org/repo`) that own the target, if the source knows;
    /// absent leaves the stored owners unchanged, `[]` clears them.
    #[serde(default)]
    pub owners: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnershipReportItem {
    pub idempotency_key: String,
    pub sequence: u64,
    pub vendor_grouping: String,
    pub target_id: String,
    /// The complete owner set (`org/repo`); `[]` clears it.
    pub owners: Vec<String>,
}

impl ReportItem {
    pub fn idempotency_key(&self) -> &str {
        match self {
            ReportItem::Status(s) => &s.idempotency_key,
            ReportItem::Ownership(o) => &o.idempotency_key,
        }
    }

    pub fn sequence(&self) -> u64 {
        match self {
            ReportItem::Status(s) => s.sequence,
            ReportItem::Ownership(o) => o.sequence,
        }
    }

    pub fn vendor_grouping(&self) -> &str {
        match self {
            ReportItem::Status(s) => &s.vendor_grouping,
            ReportItem::Ownership(o) => &o.vendor_grouping,
        }
    }

    pub fn target_id(&self) -> &str {
        match self {
            ReportItem::Status(s) => &s.target_id,
            ReportItem::Ownership(o) => &o.target_id,
        }
    }

    pub fn owners(&self) -> Option<&[String]> {
        match self {
            ReportItem::Status(s) => s.owners.as_deref(),
            ReportItem::Ownership(o) => Some(&o.owners),
        }
    }

    /// Check one item as `installation` (a name) in `org` would send it: the
    /// caps, a link that is `http(s)`, a grouping that is a valid
    /// EnvironmentDeploymentSource grouping, and owners that are Projects in
    /// the installation's own Org — an installation cannot claim ownership
    /// for another tenant's Project. The `Err` text is the per-item
    /// `rejected` reason.
    pub fn validate(&self, installation: &str, org: &str) -> Result<(), String> {
        let key = self.idempotency_key();
        bounded_text("idempotency_key", key, MAX_IDEMPOTENCY_KEY_CHARS, false)?;
        // Postgres stores the sequence as BIGINT; 0 is "nothing accepted yet".
        if self.sequence() == 0 || self.sequence() > i64::MAX as u64 {
            return Err(format!(
                "sequence must be between 1 and {}, got {}",
                i64::MAX,
                self.sequence()
            ));
        }
        EnvironmentDeploymentSource {
            installation: installation.to_string(),
            vendor_grouping: self.vendor_grouping().to_string(),
        }
        .validate()
        .map_err(|e| e.to_string())?;
        bounded_text("target_id", self.target_id(), MAX_TARGET_ID_CHARS, false)?;
        if let Some(owners) = self.owners() {
            validate_owners(owners, org)?;
        }
        if let ReportItem::Status(s) = self {
            bounded_text("version", &s.version, MAX_VERSION_CHARS, false)?;
            bounded_text(
                "version_scheme",
                &s.version_scheme,
                MAX_VERSION_SCHEME_CHARS,
                false,
            )?;
            // A label may be empty, but not multi-line junk.
            bounded_text("label", &s.label, MAX_REPORT_LABEL_CHARS, true)?;
            if let Some(link) = &s.link {
                validate_link(link)?;
            }
            if s.source_at < 0 {
                return Err(format!("source_at must be Unix-ms, got {}", s.source_at));
            }
        }
        Ok(())
    }
}

fn bounded_text(field: &str, value: &str, max: usize, may_be_empty: bool) -> Result<(), String> {
    if value.is_empty() && !may_be_empty {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().count() > max {
        return Err(format!("{field} is longer than {max} characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} contains a control character"));
    }
    Ok(())
}

/// Only an absolute `http`/`https` URL: a `javascript:` or `data:` link
/// rendered as an href would run on Scarab's origin, and sources are
/// untrusted (ADR-0072). Refused at ingest, so the record never holds one.
fn validate_link(link: &str) -> Result<(), String> {
    bounded_text("link", link, MAX_REPORT_LINK_CHARS, false)?;
    let lower = link.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .ok_or_else(|| "link must be an http:// or https:// URL".to_string())?;
    if rest.is_empty() || link.chars().any(char::is_whitespace) {
        return Err("link must be an http:// or https:// URL with a host".into());
    }
    Ok(())
}

fn validate_owners(owners: &[String], org: &str) -> Result<(), String> {
    if owners.len() > MAX_OWNER_PROJECTS {
        return Err(format!(
            "at most {MAX_OWNER_PROJECTS} owners, got {}",
            owners.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for owner in owners {
        let Some((owner_org, project)) = owner.split_once('/') else {
            return Err(format!("owner `{owner}` must be `org/repo`"));
        };
        let well_formed = |s: &str| {
            !s.is_empty()
                && s.len() <= 255
                && !s.contains('/')
                && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        if !well_formed(owner_org) || !well_formed(project) {
            return Err(format!("owner `{owner}` must be `org/repo`"));
        }
        if owner_org != org {
            return Err(format!(
                "owner `{owner}` is not in this installation's org `{org}`"
            ));
        }
        if !seen.insert(owner.as_str()) {
            return Err(format!("owner `{owner}` is listed twice"));
        }
    }
    Ok(())
}

/// One validated item on its way into the store, with the hash its receipt
/// is checked against: a reused idempotency key with different content is a
/// plugin bug and is refused, never treated as a duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportIngest {
    pub item: ReportItem,
    /// SHA-256 (hex) of the item's canonical JSON.
    pub content_sha256: String,
}

/// What became of one report item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReportOutcome {
    /// Recorded: a status was appended to its Environment's log (an
    /// ownership item updated the target's owners).
    Accepted,
    /// Its idempotency key was seen before with the same content; nothing
    /// changed.
    Duplicate,
    /// Its sequence is not newer than the last accepted one for the target;
    /// dropped, so a late retry cannot flip `live` back (ADR-0070 §11).
    Stale,
    /// No Environment lists its *(installation, grouping)*; dropped from the
    /// record and counted (ADR-0071 §5). Its sequence still advanced.
    Unmapped,
    /// Failed validation; nothing was stored, not even a receipt.
    Rejected,
}

impl ReportOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportOutcome::Accepted => "accepted",
            ReportOutcome::Duplicate => "duplicate",
            ReportOutcome::Stale => "stale",
            ReportOutcome::Unmapped => "unmapped",
            ReportOutcome::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "accepted" => Some(ReportOutcome::Accepted),
            "duplicate" => Some(ReportOutcome::Duplicate),
            "stale" => Some(ReportOutcome::Stale),
            "unmapped" => Some(ReportOutcome::Unmapped),
            "rejected" => Some(ReportOutcome::Rejected),
            _ => None,
        }
    }
}

/// One item's result, in request order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportItemOutcome {
    pub outcome: ReportOutcome,
    /// The Deployment-record seq a status landed at (for a duplicate, the
    /// seq the first copy landed at).
    pub input_seq: Option<u64>,
    /// Why, for `rejected`; what the first copy became, for `duplicate`.
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> StatusReportItem {
        StatusReportItem {
            idempotency_key: "k1".into(),
            sequence: 1,
            vendor_grouping: "prod".into(),
            target_id: "argocd/web".into(),
            version: "ghcr.io/acme/web@sha256:abc".into(),
            version_scheme: "argocd.target-images.v1".into(),
            status: ReportedStatus::Live,
            label: "synced".into(),
            link: Some("https://argo.example/applications/argocd/web".into()),
            source_at: 1_000,
            owners: Some(vec!["acme/web".into()]),
        }
    }

    fn check(item: StatusReportItem) -> Result<(), String> {
        ReportItem::Status(item).validate("argocd", "acme")
    }

    #[test]
    fn a_well_formed_status_and_ownership_item_validate() {
        assert_eq!(check(status()), Ok(()));
        let ownership = ReportItem::Ownership(OwnershipReportItem {
            idempotency_key: "k2".into(),
            sequence: 2,
            vendor_grouping: "prod".into(),
            target_id: "argocd/web".into(),
            owners: vec![],
        });
        assert_eq!(ownership.validate("argocd", "acme"), Ok(()));
    }

    #[test]
    fn the_wire_shape_is_tagged_and_has_no_source() {
        let json = serde_json::json!({
            "type": "status", "idempotency_key": "k", "sequence": 1,
            "vendor_grouping": "prod", "target_id": "t", "version": "v",
            "version_scheme": "s", "status": "in_progress", "label": "",
            "source_at": 0
        });
        let item: ReportItem = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(item.sequence(), 1);
        assert_eq!(serde_json::to_value(&item).unwrap()["type"], "status");

        let mut with_source = json.clone();
        with_source["source"] = "argocd-other".into();
        assert!(serde_json::from_value::<ReportItem>(with_source).is_err());
        let mut unknown = json;
        unknown["environment"] = "prod".into();
        assert!(serde_json::from_value::<ReportItem>(unknown).is_err());
        assert!(serde_json::from_value::<ReportItem>(serde_json::json!({
            "type": "delete", "idempotency_key": "k"
        }))
        .is_err());
    }

    #[test]
    fn caps_are_enforced_at_their_boundary() {
        fn at(n: usize) -> String {
            "x".repeat(n)
        }
        type SetAt = Box<dyn Fn(&mut StatusReportItem, usize)>;
        let cases: [(&str, SetAt, usize); 5] = [
            (
                "label",
                Box::new(|s, n| s.label = at(n)),
                MAX_REPORT_LABEL_CHARS,
            ),
            (
                "version",
                Box::new(|s, n| s.version = at(n)),
                MAX_VERSION_CHARS,
            ),
            (
                "target_id",
                Box::new(|s, n| s.target_id = at(n)),
                MAX_TARGET_ID_CHARS,
            ),
            (
                "idempotency_key",
                Box::new(|s, n| s.idempotency_key = at(n)),
                MAX_IDEMPOTENCY_KEY_CHARS,
            ),
            (
                "link",
                Box::new(|s, n| s.link = Some(format!("https://{}", at(n - 8)))),
                MAX_REPORT_LINK_CHARS,
            ),
        ];
        for (field, set, max) in cases {
            let mut ok = status();
            set(&mut ok, max);
            assert_eq!(check(ok), Ok(()), "{field} at {max}");
            let mut over = status();
            set(&mut over, max + 1);
            assert!(check(over).is_err(), "{field} at {}", max + 1);
        }
    }

    #[test]
    fn only_http_and_https_links_are_kept() {
        for link in [
            "javascript:alert(1)",
            "data:text/html,x",
            "ftp://x",
            "https://",
            "//x",
        ] {
            let mut s = status();
            s.link = Some(link.into());
            assert!(check(s).is_err(), "{link}");
        }
        for link in ["http://argo.local/app", "HTTPS://Argo.example/a"] {
            let mut s = status();
            s.link = Some(link.into());
            assert_eq!(check(s), Ok(()), "{link}");
        }
    }

    #[test]
    fn owners_must_be_projects_in_the_installations_own_org() {
        for owners in [
            vec!["initech/web".to_string()],
            vec!["web".into()],
            vec!["acme/web/x".into()],
            vec!["acme/".into()],
            vec!["acme/web".into(), "acme/web".into()],
        ] {
            let mut s = status();
            s.owners = Some(owners.clone());
            assert!(check(s).is_err(), "{owners:?}");
        }
    }

    #[test]
    fn sequence_grouping_and_text_rules() {
        let mut s = status();
        s.sequence = 0;
        assert!(check(s).is_err(), "sequence 0");
        let mut s = status();
        s.sequence = i64::MAX as u64 + 1;
        assert!(check(s).is_err(), "sequence beyond BIGINT");
        let mut s = status();
        s.vendor_grouping = " prod".into();
        assert!(check(s).is_err(), "edge space in the grouping");
        let mut s = status();
        s.label = "line\nbreak".into();
        assert!(check(s).is_err(), "control character");
        let mut s = status();
        s.label = String::new();
        assert_eq!(check(s), Ok(()), "an empty label is fine");
        let mut s = status();
        s.source_at = -1;
        assert!(check(s).is_err(), "negative source_at");
    }

    #[test]
    fn outcomes_round_trip_by_name() {
        for o in [
            ReportOutcome::Accepted,
            ReportOutcome::Duplicate,
            ReportOutcome::Stale,
            ReportOutcome::Unmapped,
            ReportOutcome::Rejected,
        ] {
            assert_eq!(ReportOutcome::parse(o.as_str()), Some(o));
        }
    }
}
