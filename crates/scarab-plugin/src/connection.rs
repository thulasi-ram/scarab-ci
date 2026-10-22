//! PluginConnections and the CredentialBroker's read-rule matcher
//! (ADR-0072 §5, MMP slice d3).
//!
//! A **PluginConnection** binds a credential to the one destination it may
//! be used against. The destination is part of the authority: the broker
//! sends a Plugin's request to the admin-bound `base_url` and nowhere else,
//! whatever the request names, so a Plugin can never aim the credential at
//! a host of its choosing.
//!
//! [`match_read_request`] decides whether a request a Plugin sends to the
//! broker is one the manifest's **read rules** allow. It is pure so that the
//! subtle part — what counts as "one segment" — is pinned by unit tests:
//!
//! - the path is **percent-decoded before matching**, segment by segment, so
//!   a pattern cannot be dodged by spelling a literal in `%xx`;
//! - a segment that decodes to a separator (`%2F`, `%5C`), to `.` / `..`,
//!   to anything still holding a `%` (double encoding) or to anything not
//!   ASCII (`．．` is `..` to an upstream that NFKC-normalises), and any
//!   segment that is empty — also before a `;` parameter (`;x`, which a
//!   server stripping matrix parameters reads as empty) — is refused
//!   outright: an upstream that normalises paths could otherwise be walked
//!   out of the matched prefix;
//! - `*` in a pattern is **exactly one** non-empty segment;
//! - the **query** may carry only the keys the matched rule lists, each at
//!   most once — an allowlist, since a read path can still have a side
//!   effect behind a query key (Argo CD's `?refresh=hard`), and refused
//!   rather than dropped, since dropping a key can re-target the read;
//! - what is forwarded is the matched segments and the query **re-encoded
//!   canonically**, never the raw text the Plugin sent, so the upstream sees
//!   exactly the request the rule admitted.

use crate::{ConnectionSlot, RequestRule};

/// How long a broker journal entry is kept (owner default 9: 14 days). The
/// retention sweeper prunes older rows under its leader lease.
pub const BROKER_JOURNAL_RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// The longest request path (after the slot) the broker will consider, and
/// the most segments it may have: generous for any vendor API, and a bound
/// on what one request can make the matcher do.
pub const MAX_PROXY_PATH_CHARS: usize = 4096;
pub const MAX_PROXY_PATH_SEGMENTS: usize = 64;
/// The longest raw query the broker will consider, and the most parameters.
pub const MAX_PROXY_QUERY_CHARS: usize = 4096;
pub const MAX_PROXY_QUERY_PARAMS: usize = 32;

/// The longest connection base URL and CA bundle an admin may bind.
pub const MAX_CONNECTION_BASE_URL_CHARS: usize = 2048;
pub const MAX_CONNECTION_CA_PEM_BYTES: usize = 64 * 1024;
/// The longest credential the broker will inject. A bearer token is a
/// header value; a few KiB covers JWTs with fat claims.
pub const MAX_CONNECTION_CREDENTIAL_BYTES: usize = 8 * 1024;

/// How the broker presents a PluginConnection's credential upstream. Only
/// `bearer` exists in the MMP ([`crate::KNOWN_AUTH_SCHEMES`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginConnectionAuthScheme {
    /// `Authorization: Bearer <credential>`.
    Bearer,
}

impl PluginConnectionAuthScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginConnectionAuthScheme::Bearer => "bearer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bearer" => Some(PluginConnectionAuthScheme::Bearer),
            _ => None,
        }
    }
}

/// An org admin's binding of a credential to the one destination it may be
/// used against, for one connection slot of one installation (ADR-0072 §5).
///
/// The credential's bytes live in the SecretProvider; the row holds only
/// `credential_ref`, a server-minted handle that is never shown. Every edit
/// mints a new handle, so a destination is never re-aimed while keeping a
/// credential an admin entered for another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConnection {
    pub installation_id: String,
    /// The manifest's [`ConnectionSlot::name`].
    pub slot: String,
    /// `http(s)://host[:port][/prefix]`, no trailing `/`, no userinfo, query
    /// or fragment. Every proxied request goes here and nowhere else.
    pub base_url: String,
    pub auth_scheme: PluginConnectionAuthScheme,
    /// The SecretProvider key the credential is stored under.
    pub credential_ref: String,
    /// PEM certificates that are the ONLY trust anchors for `base_url` when
    /// set (an internal CA); `None` = the platform's roots. There is no
    /// skip-verify.
    pub ca_pem: Option<String>,
    /// The Principal subject that last bound it, and when (Unix-ms).
    pub updated_by: String,
    pub updated_at: i64,
}

/// Which of a request's journal rows an entry is. Rows are append-only: a
/// forwarded request writes a [`Head`](Self::Head) row before its body
/// streams — so a watch stream open for hours is on record from its first
/// second — and an [`End`](Self::End) row when it ends; both carry the same
/// `request_id`. A request that is never forwarded writes one
/// [`Refused`](Self::Refused) row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrokerJournalPhase {
    Refused,
    Head,
    End,
}

impl BrokerJournalPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            BrokerJournalPhase::Refused => "refused",
            BrokerJournalPhase::Head => "head",
            BrokerJournalPhase::End => "end",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "refused" => Some(BrokerJournalPhase::Refused),
            "head" => Some(BrokerJournalPhase::Head),
            "end" => Some(BrokerJournalPhase::End),
            _ => None,
        }
    }
}

/// One row of the broker's journal of a request it proxied or refused
/// (ADR-0072 §5 step 4). Written asynchronously; read by tests (an audit
/// reader is MLP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerJournalEntry {
    /// Broker-minted, one per request: ties a `head` row to its `end` row.
    pub request_id: String,
    pub phase: BrokerJournalPhase,
    pub installation_id: String,
    /// The slot as the request named it (bounded; may name no real slot).
    pub slot: String,
    pub method: String,
    /// The request path after the slot, as sent (bounded).
    pub path: String,
    /// The path forwarded upstream: the matched path re-encoded canonically.
    /// `None` when it was refused before a rule matched.
    pub upstream_path: Option<String>,
    /// The query (bounded): as forwarded (canonical) once a rule matched it,
    /// as sent on a row refused for its query; `None` when there was none.
    pub query: Option<String>,
    /// The read rule that admitted it (`GET /api/v1/applications/*`); `None`
    /// when it was refused before a rule matched.
    pub rule: Option<String>,
    /// The status the Plugin got: the upstream's, or the broker's refusal.
    pub status: Option<u16>,
    /// Response body bytes delivered to the Plugin (`end` rows).
    pub bytes: Option<u64>,
    /// Unix-ms the request reached the broker.
    pub started_at: i64,
    /// Until the refusal, the response head (`head`), or the end (`end`).
    pub duration_ms: Option<u64>,
    /// Why the broker refused it, or why its stream ended short (`cut: …`
    /// for the kill switch or a re-bind, `upstream body error`); `None` for
    /// a `head` row and a stream proxied to completion.
    pub denied_reason: Option<String>,
}

/// A read request a rule admits: what to forward, and why it was allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRequestMatch<'a> {
    /// The rule that matched — first in manifest order.
    pub rule: &'a RequestRule,
    /// The decoded path re-encoded canonically, `/`-joined, leading `/`:
    /// exactly what the rule admitted, and the only path forwarded.
    pub upstream_path: String,
    /// The decoded query re-encoded canonically (`k=v&…`, in the order
    /// sent): only keys the rule allows, each once. `None` = no query.
    pub upstream_query: Option<String>,
}

/// Why the broker will not forward a request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadRequestRefusal {
    /// Read rules admit `GET` and `HEAD` only; writes are MLP, per action.
    #[error("method `{0}` is not a read: the broker forwards GET and HEAD only")]
    MethodNotAllowed(String),
    /// The path cannot be read one way only.
    #[error("path refused: {0}")]
    MalformedPath(String),
    /// Well-formed, but no read rule of the slot admits it.
    #[error("no read rule of connection `{0}` admits this request")]
    NoRuleMatches(String),
    /// A rule admits the path, but not the query: a key it does not list, a
    /// key twice, or a query that cannot be read one way only.
    #[error("query refused: {0}")]
    QueryRefused(String),
}

/// Does a read rule of `slot` admit `method raw_path?raw_query`?
///
/// `raw_path` is the request path **after** the slot, exactly as it arrived
/// (still percent-encoded), with its leading `/` and without the query;
/// `raw_query` is the query as it arrived, without the `?`. A `HEAD` is
/// admitted by a `GET` rule as well as a `HEAD` one: it is the same read
/// without the body. The first rule whose method and path match decides;
/// its query allowlist then has to admit the query.
pub fn match_read_request<'a>(
    slot: &'a ConnectionSlot,
    method: &str,
    raw_path: &str,
    raw_query: Option<&str>,
) -> Result<ReadRequestMatch<'a>, ReadRequestRefusal> {
    if !matches!(method, "GET" | "HEAD") {
        return Err(ReadRequestRefusal::MethodNotAllowed(method.to_string()));
    }
    let segments = decode_path(raw_path)?;
    let rule = slot
        .read_rules
        .iter()
        .find(|rule| {
            (rule.method == method || (method == "HEAD" && rule.method == "GET"))
                && pattern_matches(&rule.path, &segments)
        })
        .ok_or_else(|| ReadRequestRefusal::NoRuleMatches(slot.name.clone()))?;
    let upstream_query = canonical_query(rule, raw_query)?;
    let mut upstream_path = String::with_capacity(raw_path.len());
    for segment in &segments {
        upstream_path.push('/');
        upstream_path.push_str(&encode_path_segment(segment));
    }
    Ok(ReadRequestMatch {
        rule,
        upstream_path,
        upstream_query,
    })
}

/// The query re-encoded canonically, if `rule` admits every parameter of
/// it. Parameters are read the way an upstream reads a form-encoded query
/// (`&`-separated, `+` = space, then percent-decoded) — so the key compared
/// is the key the upstream sees — and written back with every byte that is
/// not unreserved percent-encoded, so a `;`, `&` or `=` inside a value stays
/// inside it.
fn canonical_query(
    rule: &RequestRule,
    raw_query: Option<&str>,
) -> Result<Option<String>, ReadRequestRefusal> {
    let refused = |m: String| Err(ReadRequestRefusal::QueryRefused(m));
    let raw = match raw_query {
        None | Some("") => return Ok(None),
        Some(raw) => raw,
    };
    if raw.len() > MAX_PROXY_QUERY_CHARS {
        return refused("longer than the broker considers".into());
    }
    let mut keys: Vec<String> = Vec::new();
    let mut out = String::with_capacity(raw.len());
    for pair in raw.split('&') {
        if keys.len() == MAX_PROXY_QUERY_PARAMS {
            return refused("too many parameters".into());
        }
        if pair.is_empty() {
            return refused("an empty parameter (`&&`, or a trailing `&`)".into());
        }
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let (Some(key), Some(value)) = (form_decode(raw_key), form_decode(raw_value)) else {
            return refused("an invalid percent-encoding, or one that is not UTF-8".into());
        };
        if key.is_empty() {
            return refused("a parameter without a key".into());
        }
        if key.chars().chain(value.chars()).any(char::is_control) {
            return refused("a control character".into());
        }
        if !rule.query.contains(&key) {
            let shown: String = key.escape_debug().take(64).collect();
            return refused(format!(
                "query key `{shown}` is not one the rule `{}` allows ({})",
                rule_label(rule),
                if rule.query.is_empty() {
                    "it allows no query".to_string()
                } else {
                    rule.query.join(", ")
                }
            ));
        }
        if keys.contains(&key) {
            return refused(format!("query key `{key}` given more than once"));
        }
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&encode_query_component(&key));
        out.push('=');
        out.push_str(&encode_query_component(&value));
        keys.push(key);
    }
    Ok(Some(out))
}

/// Decode one form-encoded query component: `+` is a space, then `%xx`.
fn form_decode(raw: &str) -> Option<String> {
    percent_decode_segment(&raw.replace('+', " "))
}

/// Encode one decoded query key or value: unreserved bytes stay literal,
/// everything else — `&`, `=`, `;`, `+`, space, non-ASCII — is `%XX`.
fn encode_query_component(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for byte in component.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `rule` as the journal names it: `GET /api/v1/applications/*`.
pub fn rule_label(rule: &RequestRule) -> String {
    format!("{} {}", rule.method, rule.path)
}

/// A rule pattern (validated at install: absolute, no empty/`.`/`..`
/// segment, `*` only whole) against decoded segments: same count, each a
/// literal equal to it or a `*`.
fn pattern_matches(pattern: &str, segments: &[String]) -> bool {
    let Some(rest) = pattern.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == segments.len()
        && parts
            .iter()
            .zip(segments)
            .all(|(part, segment)| *part == "*" || *part == segment)
}

/// The decoded segments of `raw_path`, or why it cannot be read one way only.
fn decode_path(raw_path: &str) -> Result<Vec<String>, ReadRequestRefusal> {
    let malformed = |m: &str| Err(ReadRequestRefusal::MalformedPath(m.to_string()));
    if raw_path.len() > MAX_PROXY_PATH_CHARS {
        return malformed("longer than the broker considers");
    }
    let Some(rest) = raw_path.strip_prefix('/') else {
        return malformed("must start with `/`");
    };
    let mut segments = Vec::new();
    for raw in rest.split('/') {
        if segments.len() == MAX_PROXY_PATH_SEGMENTS {
            return malformed("too many segments");
        }
        // An empty segment (`//`, a trailing `/`) is one an upstream may
        // collapse, so `/a//b` could reach `/a/b` while matching neither.
        if raw.is_empty() {
            return malformed("an empty segment");
        }
        let decoded = percent_decode_segment(raw).ok_or(ReadRequestRefusal::MalformedPath(
            "an invalid percent-encoding, or one that is not UTF-8".into(),
        ))?;
        if decoded.contains(['/', '\\']) {
            // `%2F` / `%5C`: one segment here, two to an upstream that
            // decodes before routing.
            return malformed("an encoded `/` or `\\` inside a segment");
        }
        if decoded.contains('%') {
            // Double encoding (`%252e%252e`): an upstream that decodes twice
            // would see `..`.
            return malformed("a `%` left after decoding (double encoding)");
        }
        if decoded.chars().any(char::is_control) {
            return malformed("a control character");
        }
        // Fullwidth `．．` is `..` to an upstream that NFKC-normalises; no
        // vendor path a rule names needs anything but ASCII.
        if !decoded.is_ascii() {
            return malformed("a character that is not ASCII");
        }
        // What a server that strips a `;` parameter before resolving the
        // path sees: `..;x` is `..`, and `;x` an empty segment it collapses.
        let before_param = decoded.split(';').next().unwrap_or_default();
        if before_param.is_empty() {
            return malformed("an empty segment before a `;` parameter");
        }
        if before_param == "." || before_param == ".." {
            return malformed("a `.` or `..` segment");
        }
        segments.push(decoded);
    }
    Ok(segments)
}

/// Decode `%xx` escapes in one raw segment. `None` on a malformed escape or
/// a result that is not UTF-8.
fn percent_decode_segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |b: u8| (b as char).to_digit(16);
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Encode one decoded segment for the upstream path: RFC 3986 `pchar`s that
/// carry no structure (unreserved, sub-delims, `:` and `@`) stay literal,
/// everything else — `/`, `?`, `#`, `%`, space, non-ASCII — is `%XX`.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        let literal = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
            );
        if literal {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(method: &str, path: &str) -> RequestRule {
        RequestRule {
            method: method.into(),
            path: path.into(),
            query: vec![],
        }
    }

    fn rule_q(method: &str, path: &str, query: &[&str]) -> RequestRule {
        RequestRule {
            query: query.iter().map(|k| k.to_string()).collect(),
            ..rule(method, path)
        }
    }

    /// The first-party Argo CD slot's shape.
    fn argo() -> ConnectionSlot {
        ConnectionSlot {
            name: "argocd".into(),
            auth_schemes: vec!["bearer".into()],
            read_rules: vec![
                rule("GET", "/api/version"),
                rule_q(
                    "GET",
                    "/api/v1/applications",
                    &["projects", "selector", "appNamespace"],
                ),
                rule_q(
                    "GET",
                    "/api/v1/applications/*",
                    &["appNamespace", "project"],
                ),
                rule("GET", "/api/v1/applications/*/managed-resources"),
                rule("HEAD", "/api/v1/stream/applications"),
            ],
        }
    }

    fn forwarded(method: &str, path: &str) -> Result<(String, String), ReadRequestRefusal> {
        let slot = argo();
        match_read_request(&slot, method, path, None).map(|m| (rule_label(m.rule), m.upstream_path))
    }

    /// The query forwarded for `GET path?query`.
    fn query(path: &str, query: &str) -> Result<Option<String>, ReadRequestRefusal> {
        let slot = argo();
        match_read_request(&slot, "GET", path, Some(query)).map(|m| m.upstream_query)
    }

    #[test]
    fn a_read_a_rule_admits_is_forwarded_as_the_matched_path() {
        assert_eq!(
            forwarded("GET", "/api/v1/applications"),
            Ok((
                "GET /api/v1/applications".into(),
                "/api/v1/applications".into()
            ))
        );
        assert_eq!(
            forwarded("GET", "/api/v1/applications/web/managed-resources"),
            Ok((
                "GET /api/v1/applications/*/managed-resources".into(),
                "/api/v1/applications/web/managed-resources".into()
            ))
        );
    }

    #[test]
    fn a_star_is_exactly_one_segment() {
        assert!(forwarded("GET", "/api/v1/applications/web").is_ok());
        for path in [
            // zero segments where `*` stands
            "/api/v1/applications/",
            // two segments where `*` stands
            "/api/v1/applications/a/b",
            "/api/v1/applications/a/b/managed-resources",
            // a prefix of a rule is not the rule
            "/api/v1",
            // nor is an extension of one
            "/api/version/x",
        ] {
            assert!(forwarded("GET", path).is_err(), "{path}");
        }
    }

    #[test]
    fn traversal_is_refused_however_it_is_spelled() {
        for path in [
            "/api/v1/applications/..",
            "/api/v1/applications/.",
            "/api/v1/applications/%2e%2e",
            "/api/v1/applications/%2E%2E",
            "/api/v1/applications/.%2e",
            "/api/v1/applications/..;x",
            "/api/v1/applications/%2e%2e;x/managed-resources",
            // nothing before a `;` parameter: empty once it is stripped
            "/api/v1/applications/;x",
            "/api/v1/applications/%3Bx/managed-resources",
            // fullwidth dots: `..` after NFKC
            "/api/v1/applications/%EF%BC%8E%EF%BC%8E",
            "/api/v1/../v1/applications",
            "/api/./v1/applications",
        ] {
            assert!(
                matches!(
                    forwarded("GET", path),
                    Err(ReadRequestRefusal::MalformedPath(_))
                ),
                "{path}"
            );
        }
    }

    #[test]
    fn encoded_separators_and_double_encoding_are_refused() {
        for path in [
            // one segment to the matcher, two to an upstream that decodes
            "/api/v1/applications/a%2Fb",
            "/api/v1/applications/a%2fb",
            "/api/v1/applications/a%5Cb",
            "/api/v1/applications/..%2F..%2Fsession",
            // decodes once to `%2e%2e`
            "/api/v1/applications/%252e%252e",
            "/api/v1/applications/a%252Fb",
            // a raw backslash
            "/api/v1/applications/a\\b",
        ] {
            assert!(
                matches!(
                    forwarded("GET", path),
                    Err(ReadRequestRefusal::MalformedPath(_))
                ),
                "{path}"
            );
        }
    }

    #[test]
    fn empty_segments_and_bad_escapes_are_refused() {
        for path in [
            "/api//v1/applications",
            "//api/v1/applications",
            "/api/v1/applications/",
            "api/v1/applications",
            "",
            "/api/v1/applications/%",
            "/api/v1/applications/%4",
            "/api/v1/applications/%zz",
            // not UTF-8
            "/api/v1/applications/%ff",
            // a decoded control character
            "/api/v1/applications/a%00b",
            "/api/v1/applications/a%0Ab",
        ] {
            assert!(
                matches!(
                    forwarded("GET", path),
                    Err(ReadRequestRefusal::MalformedPath(_))
                ),
                "{path:?}"
            );
        }
    }

    #[test]
    fn the_path_is_decoded_before_matching_and_re_encoded_canonically() {
        // A literal spelled in %xx still matches the literal…
        assert_eq!(
            forwarded("GET", "/api/v1/%61pplications"),
            Ok((
                "GET /api/v1/applications".into(),
                "/api/v1/applications".into()
            ))
        );
        // …and what is forwarded is the canonical form, whatever was sent:
        // a `?` or `#` inside a segment stays inside it.
        assert_eq!(
            forwarded("GET", "/api/v1/applications/a%3Fb%23c%20d").map(|(_, p)| p),
            Ok("/api/v1/applications/a%3Fb%23c%20d".into())
        );
        // A `;` parameter after a real name is data, and stays put.
        assert_eq!(
            forwarded("GET", "/api/v1/applications/web;v=1").map(|(_, p)| p),
            Ok("/api/v1/applications/web;v=1".into())
        );
        // Anything not ASCII is refused, not re-encoded.
        assert!(matches!(
            forwarded("GET", "/api/v1/applications/caf%C3%A9"),
            Err(ReadRequestRefusal::MalformedPath(_))
        ));
    }

    #[test]
    fn only_the_query_keys_a_rule_lists_are_forwarded_canonically() {
        assert_eq!(query("/api/v1/applications", ""), Ok(None));
        assert_eq!(
            query("/api/v1/applications/web", "appNamespace=argo&project=prod"),
            Ok(Some("appNamespace=argo&project=prod".into()))
        );
        // Decoded the way the upstream decodes, re-encoded canonically: a
        // `+` is a space, and a `&`, `=` or `;` inside a value stays inside.
        assert_eq!(
            query(
                "/api/v1/applications",
                "selector=team%3Dweb+env%21%3Dprod&projects=a%3Bb%26refresh%3Dhard"
            ),
            Ok(Some(
                "selector=team%3Dweb%20env%21%3Dprod&projects=a%3Bb%26refresh%3Dhard".into()
            ))
        );
        assert_eq!(
            query("/api/v1/applications", "%70rojects=a"),
            Ok(Some("projects=a".into()))
        );
        // A key with no `=` is a key with an empty value.
        assert_eq!(
            query("/api/v1/applications", "projects"),
            Ok(Some("projects=".into()))
        );
    }

    #[test]
    fn an_undeclared_repeated_or_malformed_query_is_refused() {
        for (path, q) in [
            // Argo's hard refresh: a side effect behind a read path
            ("/api/v1/applications/web", "refresh=hard"),
            ("/api/v1/applications/web", "project=prod&refresh=normal"),
            ("/api/v1/applications/web", "%72efresh=hard"),
            // a rule with no query list allows no query
            ("/api/version", "x=1"),
            (
                "/api/v1/applications/web/managed-resources",
                "appNamespace=argo",
            ),
            // case and spelling are exact
            ("/api/v1/applications/web", "appnamespace=argo"),
            ("/api/v1/applications/web", "app_namespace=argo"),
            // once each: two would be read either way
            ("/api/v1/applications/web", "project=a&project=b"),
            ("/api/v1/applications/web", "project=a&%70roject=b"),
            // malformed
            ("/api/v1/applications/web", "project=a&"),
            ("/api/v1/applications/web", "&project=a"),
            ("/api/v1/applications/web", "project=a&&project=b"),
            ("/api/v1/applications/web", "=a"),
            ("/api/v1/applications/web", "project=%zz"),
            ("/api/v1/applications/web", "project=%ff"),
            ("/api/v1/applications/web", "project=a%00b"),
        ] {
            assert!(
                matches!(query(path, q), Err(ReadRequestRefusal::QueryRefused(_))),
                "{path}?{q}"
            );
        }
        let long = format!("projects={}", "a".repeat(MAX_PROXY_QUERY_CHARS));
        assert!(matches!(
            query("/api/v1/applications", &long),
            Err(ReadRequestRefusal::QueryRefused(_))
        ));
        // The path is still checked first: an unmatched path is not a
        // query refusal.
        assert!(matches!(
            query("/api/v1/session", "refresh=hard"),
            Err(ReadRequestRefusal::NoRuleMatches(_))
        ));
    }

    #[test]
    fn only_reads_are_forwarded_and_head_rides_on_get() {
        for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS", "get", "TRACE"] {
            assert_eq!(
                forwarded(method, "/api/v1/applications"),
                Err(ReadRequestRefusal::MethodNotAllowed(method.into())),
                "{method}"
            );
        }
        assert!(forwarded("HEAD", "/api/v1/applications").is_ok());
        // A HEAD rule does not admit a GET.
        assert!(matches!(
            forwarded("GET", "/api/v1/stream/applications"),
            Err(ReadRequestRefusal::NoRuleMatches(_))
        ));
        assert!(forwarded("HEAD", "/api/v1/stream/applications").is_ok());
    }

    #[test]
    fn segment_and_parameter_counts_and_lengths_are_bounded() {
        let slot = ConnectionSlot {
            name: "wide".into(),
            auth_schemes: vec!["bearer".into()],
            read_rules: vec![rule("GET", &"/*".repeat(MAX_PROXY_PATH_SEGMENTS))],
        };
        let at_cap = "/a".repeat(MAX_PROXY_PATH_SEGMENTS);
        assert!(match_read_request(&slot, "GET", &at_cap, None).is_ok());
        let over = "/a".repeat(MAX_PROXY_PATH_SEGMENTS + 1);
        assert!(matches!(
            match_read_request(&slot, "GET", &over, None),
            Err(ReadRequestRefusal::MalformedPath(_))
        ));
        let long = format!("/{}", "a".repeat(MAX_PROXY_PATH_CHARS));
        assert!(matches!(
            match_read_request(&slot, "GET", &long, None),
            Err(ReadRequestRefusal::MalformedPath(_))
        ));
        let slot = ConnectionSlot {
            read_rules: vec![rule_q("GET", "/a", &["k"])],
            ..slot
        };
        let many = vec!["k=1"; MAX_PROXY_QUERY_PARAMS + 1].join("&");
        assert!(matches!(
            match_read_request(&slot, "GET", "/a", Some(&many)),
            Err(ReadRequestRefusal::QueryRefused(_))
        ));
    }

    #[test]
    fn journal_phases_round_trip() {
        for phase in [
            BrokerJournalPhase::Refused,
            BrokerJournalPhase::Head,
            BrokerJournalPhase::End,
        ] {
            assert_eq!(BrokerJournalPhase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(BrokerJournalPhase::parse("killed"), None);
    }

    #[test]
    fn auth_schemes_round_trip() {
        assert_eq!(
            PluginConnectionAuthScheme::parse("bearer"),
            Some(PluginConnectionAuthScheme::Bearer)
        );
        assert_eq!(PluginConnectionAuthScheme::Bearer.as_str(), "bearer");
        assert_eq!(PluginConnectionAuthScheme::parse("basic"), None);
    }
}
