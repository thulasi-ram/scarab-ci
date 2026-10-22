//! The broker's report wire shape (ADR-0072 §11) and the
//! DeploymentReporterContract's transition rule (ADR-0070 §11), written from
//! the published contract — this crate shares no type with the server.
//!
//! The server parses each item with `deny_unknown_fields` and refuses a
//! `source` anywhere in the body, so these types serialize exactly the
//! contract's fields and nothing else.

use std::collections::BTreeSet;
use std::hash::{BuildHasher, Hasher};

use serde::{Deserialize, Serialize};

use crate::argo::Application;
use crate::assess::{assess, Assessment, Status};
use crate::version::{version_of, VERSION_SCHEME};

/// The most items one `POST …/reports` may carry.
pub const MAX_ITEMS_PER_BATCH: usize = 100;
/// The contract's caps, in characters. An item past one is `rejected`, so
/// the reporter keeps inside them itself.
pub const MAX_LABEL_CHARS: usize = 512;
pub const MAX_VERSION_CHARS: usize = 4096;
pub const MAX_LINK_CHARS: usize = 2048;
pub const MAX_TARGET_ID_CHARS: usize = 253;
pub const MAX_OWNERS: usize = 64;
const MAX_OWNER_SEGMENT_BYTES: usize = 255;
/// Sequences run from 1 to 2^53 − 1, JSON's exact-integer limit.
pub const MAX_SEQUENCE: u64 = (1 << 53) - 1;

/// One report, internally tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReportItem {
    Status(StatusItem),
    Ownership(OwnershipItem),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusItem {
    pub idempotency_key: String,
    pub sequence: u64,
    pub vendor_grouping: String,
    pub target_id: String,
    pub version: String,
    pub version_scheme: String,
    pub status: Status,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Unix-ms by this reporter's clock; shown, never compared.
    pub source_at: i64,
    /// The complete owner set; `[]` clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owners: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipItem {
    pub idempotency_key: String,
    pub sequence: u64,
    pub vendor_grouping: String,
    pub target_id: String,
    pub owners: Vec<String>,
}

impl ReportItem {
    pub fn idempotency_key(&self) -> &str {
        match self {
            ReportItem::Status(s) => &s.idempotency_key,
            ReportItem::Ownership(o) => &o.idempotency_key,
        }
    }

    pub fn target_id(&self) -> &str {
        match self {
            ReportItem::Status(s) => &s.target_id,
            ReportItem::Ownership(o) => &o.target_id,
        }
    }

    pub fn sequence(&self) -> u64 {
        match self {
            ReportItem::Status(s) => s.sequence,
            ReportItem::Ownership(o) => o.sequence,
        }
    }
}

/// What one Application says right now, ready to compare with what was
/// last reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub target_id: String,
    pub vendor_grouping: String,
    pub version: String,
    pub assessment: Assessment,
    /// Sorted, in the installation's org only.
    pub owners: Vec<String>,
    pub link: Option<String>,
}

/// What the reporter last reported (or, after a restart, what the broker's
/// `/targets` says was last accepted) for one DeploymentTarget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    /// The last sequence used; the next report takes the one after it.
    pub last_sequence: u64,
    pub vendor_grouping: Option<String>,
    pub reported: Option<Reported>,
    pub owners: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub version: String,
    pub status: Status,
    pub label: String,
}

/// Which report an observation calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Nothing,
    /// A status report (it carries the owners too).
    Status,
    /// Only the owners changed.
    Ownership,
}

/// The contract's transition rule: report a status only when
/// *(version, status, label)* — or the grouping that routes it — differs
/// from what was last reported; never per reconcile. `resend` is the one
/// exception (the broker flagged the target): the current state again.
pub fn due(cursor: Option<&Cursor>, obs: &Observation, resend: bool) -> Due {
    let Some(cursor) = cursor else {
        return Due::Status;
    };
    let now = Reported {
        version: obs.version.clone(),
        status: obs.assessment.status,
        label: obs.assessment.label.clone(),
    };
    if resend
        || cursor.reported.as_ref() != Some(&now)
        || cursor.vendor_grouping.as_deref() != Some(obs.vendor_grouping.as_str())
    {
        return Due::Status;
    }
    if cursor.owners.as_deref() != Some(obs.owners.as_slice()) {
        return Due::Ownership;
    }
    Due::Nothing
}

/// Why an Application cannot be reported in this scheme. Logged, never
/// sent: the broker would reject it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreportable {
    /// No pod-template image in its target manifests (owner default 7).
    NoImages,
    TargetIdTooLong,
    VersionTooLong,
    NoGrouping,
}

/// The observation of `app`, whose target manifests hold `images`.
pub fn observe(
    app: &Application,
    images: &BTreeSet<String>,
    org: &str,
    owner_annotation: &str,
    ui_url: Option<&str>,
) -> Result<Observation, Unreportable> {
    let version = version_of(images).ok_or(Unreportable::NoImages)?;
    if version.chars().count() > MAX_VERSION_CHARS {
        return Err(Unreportable::VersionTooLong);
    }
    let target_id = app.target_id();
    if target_id.chars().count() > MAX_TARGET_ID_CHARS {
        return Err(Unreportable::TargetIdTooLong);
    }
    if app.spec.project.trim().is_empty() {
        return Err(Unreportable::NoGrouping);
    }
    let (owners, ignored) = owners_from_annotation(
        app.metadata
            .annotations
            .get(owner_annotation)
            .map(String::as_str),
        org,
    );
    if !ignored.is_empty() {
        tracing::warn!(
            target_id = %target_id,
            annotation = %owner_annotation,
            ignored = ?ignored,
            "argocd: owner entries that are not `{org}/<repo>` were ignored"
        );
    }
    Ok(Observation {
        target_id,
        vendor_grouping: app.spec.project.clone(),
        version,
        assessment: assess(app),
        owners,
        link: app_link(ui_url, app),
    })
}

/// The owner Projects an annotation names (`org/repo, org/repo`): sorted and
/// de-duplicated, only well-formed ones in the installation's own `org`
/// (the broker rejects a whole item over one foreign owner), at most
/// [`MAX_OWNERS`]. Returns (kept, ignored).
pub fn owners_from_annotation(value: Option<&str>, org: &str) -> (Vec<String>, Vec<String>) {
    let well_formed = |s: &str| {
        !s.is_empty()
            && s.len() <= MAX_OWNER_SEGMENT_BYTES
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    let mut kept = BTreeSet::new();
    let mut ignored = Vec::new();
    for entry in value.unwrap_or("").split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry.split_once('/') {
            Some((o, repo)) if o == org && well_formed(o) && well_formed(repo) => {
                kept.insert(entry.to_string());
            }
            _ => ignored.push(entry.to_string()),
        }
    }
    let mut kept: Vec<String> = kept.into_iter().collect();
    if kept.len() > MAX_OWNERS {
        ignored.extend(kept.split_off(MAX_OWNERS));
    }
    (kept, ignored)
}

/// `{ARGOCD_UI_URL}/applications/{namespace}/{name}`, when the UI URL is an
/// `http(s)` URL (the broker refuses any other link).
pub fn app_link(ui_url: Option<&str>, app: &Application) -> Option<String> {
    let base = ui_url?.trim().trim_end_matches('/');
    let lower = base.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return None;
    }
    let link = format!(
        "{base}/applications/{}/{}",
        app.metadata.namespace, app.metadata.name
    );
    (link.chars().count() <= MAX_LINK_CHARS && !link.chars().any(char::is_whitespace))
        .then_some(link)
}

/// Unicode format characters the broker refuses in any text field (they
/// change how text reads without being seen).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// A label the broker accepts: Argo's messages are often multi-line, and a
/// control character would get the item rejected. Whitespace and control
/// characters collapse to one space, invisible format characters go, and
/// the result is cut to [`MAX_LABEL_CHARS`].
pub fn clean_label(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_LABEL_CHARS * 4));
    let mut pending_space = false;
    for c in raw.chars() {
        if is_invisible_format(c) {
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.chars().count() > MAX_LABEL_CHARS {
        out = out.chars().take(MAX_LABEL_CHARS - 1).collect();
        out.push('…');
    }
    out
}

/// Mints idempotency keys: `<process nonce>-<sequence>-<target hash>`.
///
/// Unique per installation because the sequence is per target and strictly
/// increasing, and the nonce differs per process: a reporter restarted
/// before its seeded cursor caught up can never reuse a key for other
/// content (which the broker answers with a 409 for the whole batch).
#[derive(Debug, Clone)]
pub struct KeyMinter {
    nonce: String,
}

impl KeyMinter {
    pub fn new() -> Self {
        // `RandomState` is seeded randomly per process: a nonce without a
        // random-number dependency.
        let random = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        Self::with_nonce(format!("{random:016x}"))
    }

    pub fn with_nonce(nonce: impl Into<String>) -> Self {
        Self {
            nonce: nonce.into(),
        }
    }

    pub fn key(&self, target_id: &str, sequence: u64) -> String {
        format!("{}-{sequence}-{:016x}", self.nonce, fnv1a64(target_id))
    }
}

impl Default for KeyMinter {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a: a stable 64-bit hash, so a key names its target compactly (a
/// target id may be 253 characters; a key at most 128).
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The status item for `obs` at `sequence`.
pub fn status_item(
    obs: &Observation,
    keys: &KeyMinter,
    sequence: u64,
    source_at: i64,
) -> ReportItem {
    ReportItem::Status(StatusItem {
        idempotency_key: keys.key(&obs.target_id, sequence),
        sequence,
        vendor_grouping: obs.vendor_grouping.clone(),
        target_id: obs.target_id.clone(),
        version: obs.version.clone(),
        version_scheme: VERSION_SCHEME.to_string(),
        status: obs.assessment.status,
        label: obs.assessment.label.clone(),
        link: obs.link.clone(),
        source_at,
        owners: Some(obs.owners.clone()),
    })
}

/// The ownership item for `obs` at `sequence`.
pub fn ownership_item(obs: &Observation, keys: &KeyMinter, sequence: u64) -> ReportItem {
    ReportItem::Ownership(OwnershipItem {
        idempotency_key: keys.key(&obs.target_id, sequence),
        sequence,
        vendor_grouping: obs.vendor_grouping.clone(),
        target_id: obs.target_id.clone(),
        owners: obs.owners.clone(),
    })
}

/// The key a 409 names: the broker writes it between the first pair of
/// backticks (`idempotency key `<key>` was already used …`).
pub fn conflicting_key(body: &str) -> Option<&str> {
    let start = body.find('`')? + 1;
    let len = body[start..].find('`')?;
    Some(&body[start..start + len]).filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_one_line_inside_the_cap() {
        assert_eq!(
            clean_label("  one\n\ttwo\u{200B}  three\r\n"),
            "one two three"
        );
        let long = clean_label(&"x".repeat(MAX_LABEL_CHARS + 10));
        assert_eq!(long.chars().count(), MAX_LABEL_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn keys_stay_under_the_cap_and_differ_per_target_and_sequence() {
        let keys = KeyMinter::with_nonce("0123456789abcdef");
        let long_target = "n".repeat(MAX_TARGET_ID_CHARS);
        let k = keys.key(&long_target, MAX_SEQUENCE);
        assert!(k.len() <= 128, "{k}");
        assert_ne!(keys.key("argocd/web", 1), keys.key("argocd/web", 2));
        assert_ne!(keys.key("argocd/web", 1), keys.key("argocd/api", 1));
        assert_ne!(
            KeyMinter::new().key("argocd/web", 1),
            KeyMinter::new().key("argocd/web", 1),
            "another process never reuses a key"
        );
    }

    #[test]
    fn the_conflicting_key_is_read_from_the_409_body() {
        assert_eq!(
            conflicting_key(
                "idempotency key `abc-3-00ff` was already used for a different report; …"
            ),
            Some("abc-3-00ff")
        );
        assert_eq!(conflicting_key("no key here"), None);
    }
}
