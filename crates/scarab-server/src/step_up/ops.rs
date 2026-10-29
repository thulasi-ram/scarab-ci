//! The classification of every route (ADR-0073 §3): one [`Op`] per governed
//! route, registered with the route in `crate::routes`, so a route cannot
//! exist without saying whether it needs a step-up.
//!
//! What each class means:
//!
//! - **CrownJewel** — the guard demands a step-up BEFORE the handler runs.
//! - **Conditional** — a crown jewel only when the change loosens; the
//!   handler decides (after loading the current state) and calls
//!   [`GuardCtx::require`](super::GuardCtx::require) or
//!   [`GuardCtx::exempt`](super::GuardCtx::exempt).
//! - **Ceremony** — the passkey ceremony endpoints themselves.
//! - **Exempt** — tightening: never a step-up.
//! - **NotGoverned** — not on ADR-0073 §3's list, with the reason.
//!
//! Every GET is [`READ`].

use scarab_identity::Role;
use serde::Serialize;
use serde_json::Value;

/// Which confirmation a crown jewel needs (ADR-0073 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The session's 5-minute window satisfies it.
    Window,
    /// A confirmation bound to THIS action, every time; a window never does.
    AlwaysAsk,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Window => "window",
            Mode::AlwaysAsk => "action",
        }
    }
}

/// How a route is governed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Read,
    /// Enforced by the guard before the handler.
    CrownJewel(Mode),
    /// The handler decides with the loosening rules and calls
    /// `GuardCtx::require` / `GuardCtx::exempt`.
    Conditional {
        mode: Mode,
        rule: &'static str,
    },
    /// The step-up/passkey ceremony endpoints themselves.
    Ceremony,
    /// Tightening; never a step-up. The reason.
    Exempt(&'static str),
    /// Not on the crown-jewel list. The reason.
    NotGoverned(&'static str),
}

impl Class {
    /// A short code for tables and tests: `READ`, `CJ-W`, `CJ-A`, `COND-W`,
    /// `COND-A`, `CER`, `EX`, `NG`.
    pub fn code(&self) -> &'static str {
        match self {
            Class::Read => "READ",
            Class::CrownJewel(Mode::Window) => "CJ-W",
            Class::CrownJewel(Mode::AlwaysAsk) => "CJ-A",
            Class::Conditional {
                mode: Mode::Window, ..
            } => "COND-W",
            Class::Conditional {
                mode: Mode::AlwaysAsk,
                ..
            } => "COND-A",
            Class::Ceremony => "CER",
            Class::Exempt(_) => "EX",
            Class::NotGoverned(_) => "NG",
        }
    }

    /// The mode a crown jewel (conditional or not) needs.
    pub fn mode(&self) -> Option<Mode> {
        match self {
            Class::CrownJewel(m) | Class::Conditional { mode: m, .. } => Some(*m),
            _ => None,
        }
    }
}

/// Where an operation's Org comes from, for the OrgSecurityEventLog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgOf {
    /// A path parameter.
    Path(&'static str),
    /// A JSON pointer into the request body.
    Body(&'static str),
    /// A query parameter.
    Query(&'static str),
    /// The tenant of the run named by the `id` path parameter.
    Run,
    /// Not bound to one Org.
    None,
}

/// Where an Org-bound operation's Project comes from, when its handler
/// authorizes at that Project rather than at the Org (`rbac_scope(org,
/// Some(project))`). Together with [`OrgOf`] it is the scope an API token's
/// must cover before a request is raised for it ([`Op::project_of`]).
/// [`OrgOf::Run`] needs none: the run's tenant is the whole scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectOf {
    /// The handler authorizes at the Org.
    None,
    /// A path parameter.
    Path(&'static str),
    /// A JSON pointer into the request body; present (a string) means the
    /// Project, absent or `null` means the Org.
    Body(&'static str),
    /// A query parameter; present means the Project, absent the Org.
    Query(&'static str),
}

/// What a describe function sees: the request, its body already redacted.
#[derive(Debug, Clone)]
pub struct OpInput {
    pub method: String,
    /// The concrete request path.
    pub path: String,
    /// Path parameters, by name, in pattern order.
    pub params: Vec<(String, String)>,
    /// Query parameters, sorted.
    pub query: Vec<(String, String)>,
    /// The JSON body with [`Op::redact`] applied (`Null` when there is none).
    pub body: Value,
}

impl OpInput {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query_value(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// A string at a JSON pointer of the body, if it is a non-empty string,
    /// number or boolean.
    pub fn body_str(&self, pointer: &str) -> Option<String> {
        match self.body.pointer(pointer)? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// Resolve one [`Op::target`] source: `path:<name>`, `query:<name>`, or
    /// `body:<json pointer>`.
    fn source(&self, source: &str) -> Option<String> {
        if let Some(name) = source.strip_prefix("path:") {
            self.param(name).map(str::to_string)
        } else if let Some(name) = source.strip_prefix("query:") {
            self.query_value(name)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        } else if let Some(ptr) = source.strip_prefix("body:") {
            self.body_str(ptr)
        } else {
            None
        }
    }
}

/// What a human is asked to confirm: the server-rendered, redacted
/// description of one operation (shown in the 403, on the confirm page and
/// in the event log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct OpSummary {
    /// What is being done ("Write a secret").
    pub operation: String,
    /// What it is done to ("acme/web/prod/DB_PASSWORD").
    pub target: String,
    /// What changes, one line each. Redacted fields read `(hidden)`.
    pub changes: Vec<String>,
}

/// Who, besides a human in a browser, an operation can ever be performed by
/// — and so whether a machine caller may raise a StepUpConfirmationRequest
/// for it (ADR-0073 §6). A request its requester could never execute would
/// only ever end `failed`, and would hand a compromised machine a plausible
/// prompt to send its human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepUpMachineUse {
    /// An API token or a Bearer session may perform it (once confirmed).
    Requestable,
    /// Only a cookie session: the handler refuses every token and Bearer
    /// session (the identity-link flows, like `passkey.add`).
    BrowserOnly,
    /// Never an API token (a token may not mint tokens); a Bearer session
    /// may.
    NotByToken,
}

/// One governed operation.
#[derive(Debug)]
pub struct Op {
    /// Stable id (`secret.put`), used in events, refusals and requests.
    pub id: &'static str,
    /// Human label (`Write a secret`).
    pub label: &'static str,
    pub class: Class,
    /// The ADR-0073 §3 item (or the reason it is not one).
    pub adr: &'static str,
    /// The lowest role that can ever pass the handler's own authorization.
    /// A caller holding no role at least this high anywhere is passed
    /// straight to the handler, which refuses it for its role — so a Viewer
    /// sees "insufficient role", not "step-up required". `None`: every
    /// authenticated caller may (the caller's own passkeys and identities).
    /// Never declare it higher than the handler needs: a caller below it
    /// skips the step-up.
    pub min_role: Option<Role>,
    pub org_of: OrgOf,
    /// The Project its handler authorizes at, if it is Project-level. Never
    /// leave a Project-level handler at [`ProjectOf::None`]: a token scoped
    /// to that Project would be refused a request it could run.
    pub project_of: ProjectOf,
    /// JSON pointers into the body hidden in summaries and events
    /// (`/value`, `/credential`). The digest still covers them.
    pub redact: &'static [&'static str],
    /// Where the summary's target comes from, joined with `/`:
    /// `path:<name>`, `query:<name>`, `body:<json pointer>`.
    pub target: &'static [&'static str],
    pub describe: fn(&Op, &OpInput) -> OpSummary,
    /// Whether a machine caller may raise a request for it.
    pub machine: StepUpMachineUse,
    /// The largest body a StepUpConfirmationRequest for it may seal (bytes);
    /// a bigger one raises none (413). Well under the guard's 2 MiB: a
    /// request parks its body, sealed, until it settles or is reaped.
    pub max_request_body: usize,
}

/// [`Op::max_request_body`] unless an operation says otherwise.
pub const DEFAULT_MAX_REQUEST_BODY: usize = 64 * 1024;

impl Op {
    pub fn summary(&self, input: &OpInput) -> OpSummary {
        (self.describe)(self, input)
    }

    /// Only a human in a browser can ever perform it.
    const fn browser_only(self) -> Op {
        Op {
            machine: StepUpMachineUse::BrowserOnly,
            ..self
        }
    }

    /// An API token can never perform it.
    const fn not_by_token(self) -> Op {
        Op {
            machine: StepUpMachineUse::NotByToken,
            ..self
        }
    }

    /// Its handler authorizes at the Project named by `project`.
    const fn in_project(self, project: ProjectOf) -> Op {
        Op {
            project_of: project,
            ..self
        }
    }

    /// A request for it may seal a body of up to `bytes`.
    const fn max_request_body(self, bytes: usize) -> Op {
        Op {
            max_request_body: bytes,
            ..self
        }
    }
}

/// How long one rendered value may be in a summary line before it is shown
/// as head…tail.
const MAX_CHANGE_VALUE: usize = 200;
/// How much of each end of a long value is shown.
const CHANGE_VALUE_END: usize = 96;

/// The default description: the label; the target from [`Op::target`]; one
/// `field = value` line per body leaf (nested objects flattened to
/// `a.b = c`, sorted, redacted fields already `(hidden)`; a key that is not
/// plain `[A-Za-z0-9_-]+` is quoted, so `{"a.b": x}` and `{"a": {"b": y}}`
/// never render under one path); for a URL value — however the handler's
/// WHATWG parser would accept it spelled — the URL as the handler will use
/// it, in full, and a `field host = …` line in its ASCII (punycode) form; and
/// one `?key = value` line per query parameter the target does not already
/// name. Every value is escaped ([`escape_invisible`]) so no control, bidi or
/// invisible character reaches the human, and a long value keeps its end
/// visible (a URL is never cut).
pub fn describe_generic(op: &Op, input: &OpInput) -> OpSummary {
    let target: Vec<String> = op.target.iter().filter_map(|s| input.source(s)).collect();
    let mut changes = Vec::new();
    if let Value::Object(map) = &input.body {
        flatten(map, "", &mut changes);
    }
    let named: Vec<&str> = op
        .target
        .iter()
        .filter_map(|s| s.strip_prefix("query:"))
        .collect();
    for (k, v) in &input.query {
        if !named.contains(&k.as_str()) {
            leaf_lines(&format!("?{}", render_key(k)), v, &mut changes);
        }
    }
    OpSummary {
        operation: op.label.to_string(),
        target: escape_invisible(&target.join("/")),
        changes,
    }
}

/// One line per leaf of `map`, keys sorted, under `prefix`.
fn flatten(map: &serde_json::Map<String, Value>, prefix: &str, out: &mut Vec<String>) {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for k in keys {
        let key = format!("{prefix}{}", render_key(k));
        match &map[k] {
            Value::Object(inner) if !inner.is_empty() => flatten(inner, &format!("{key}."), out),
            Value::String(s) => leaf_lines(&key, s, out),
            other => out.push(format!("{key} = {}", long_value(&other.to_string()))),
        }
    }
}

/// One path segment of a flattened key: bare when it is `[A-Za-z0-9_-]+`,
/// else quoted (`"a.b"`, with `\` and `"` escaped) — so a key holding the
/// `.` separator, a space (`"base_url host"`) or a leading `?` can never
/// pass for a nested path, a host line or a query line.
fn render_key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        k.to_string()
    } else {
        format!("\"{}\"", escape_invisible(k).replace('"', "\\\""))
    }
}

/// The line(s) for one string value: a URL in full plus its host, anything
/// else head…tail when long.
fn leaf_lines(key: &str, v: &str, out: &mut Vec<String>) {
    // What `normalize_connection_base_url` (and any WHATWG consumer) would
    // accept: surrounding whitespace trimmed, the scheme in any case.
    let trimmed = v.trim();
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        if matches!(url.scheme(), "http" | "https") {
            let host = url.host_str().unwrap_or_default();
            let mut notes = Vec::new();
            if !url.username().is_empty() || url.password().is_some() {
                notes.push("URL carries userinfo");
            }
            if host.split('.').any(|l| l.starts_with("xn--")) {
                notes.push("internationalized host: check every character");
            }
            let note = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join("; "))
            };
            out.push(format!("{key} = {}", escape_invisible(url.as_str())));
            out.push(format!("{key} host = {}{note}", escape_invisible(host)));
            return;
        }
    }
    let lower = trimmed.get(..8).unwrap_or(trimmed).to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        // Not a URL the parser accepts (so no handler will use it), but it
        // reads like one: still never cut, and its host still shown.
        out.push(format!("{key} = {}", escape_invisible(v)));
        let (host, userinfo) = url_host(trimmed);
        let note = if userinfo {
            " (URL carries userinfo)"
        } else {
            ""
        };
        out.push(format!("{key} host = {}{note}", escape_invisible(host)));
    } else {
        out.push(format!("{key} = {}", long_value(v)));
    }
}

/// The host of an `http(s)://` URL (the authority after its last `@`, port
/// stripped) and whether the authority carries userinfo. Parsed by hand: the
/// fallback for a URL-looking value the WHATWG parser refuses.
pub(crate) fn url_host(url: &str) -> (&str, bool) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host_port, userinfo) = match authority.rsplit_once('@') {
        Some((_, h)) => (h, true),
        None => (authority, false),
    };
    let host = if host_port.starts_with('[') {
        host_port.find(']').map_or(host_port, |i| &host_port[..=i])
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    (host, userinfo)
}

/// `v` escaped; over [`MAX_CHANGE_VALUE`] characters, its first and last
/// [`CHANGE_VALUE_END`] characters and its length — the end of a value is
/// where a padded host or a swapped suffix would hide.
fn long_value(v: &str) -> String {
    let n = v.chars().count();
    if n <= MAX_CHANGE_VALUE {
        return escape_invisible(v);
    }
    let head: String = v.chars().take(CHANGE_VALUE_END).collect();
    let tail: String = v.chars().skip(n - CHANGE_VALUE_END).collect();
    format!(
        "{}…{} ({} bytes)",
        escape_invisible(&head),
        escape_invisible(&tail),
        v.len()
    )
}

/// Does `c` render as nothing, or rearrange or break the text around it?
/// Every control (Cc), format (Cf), line- and paragraph-separator (Zl, Zp)
/// character, plus the other default-ignorable code points (variation
/// selectors, Hangul fillers, the combining grapheme joiner) — enumerated by
/// hand (Unicode 15.1) rather than pulling in a Unicode-properties crate.
/// The CLI's `sanitize` mirrors this set.
pub fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF0}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

/// `s` with every [`is_invisible`] character written as `\u{XXXX}` and every
/// `\` as `\\` — so the escaping is unambiguous (a literal `\u{202E}` typed
/// by a user reads `\\u{202E}`, never like the real character) and what the
/// human reads is what runs.
pub fn escape_invisible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if is_invisible(c) {
            out.push_str(&format!("\\u{{{:04X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// The marker a redacted field reads as.
pub const HIDDEN: &str = "(hidden)";

/// `body` with every pointer of `redact` replaced by [`HIDDEN`].
pub fn redact(body: &Value, redact: &[&str]) -> Value {
    let mut out = body.clone();
    for ptr in redact {
        if let Some(v) = out.pointer_mut(ptr) {
            *v = Value::String(HIDDEN.into());
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
const fn op(
    id: &'static str,
    label: &'static str,
    class: Class,
    adr: &'static str,
    min_role: Option<Role>,
    org_of: OrgOf,
    redact: &'static [&'static str],
    target: &'static [&'static str],
) -> Op {
    Op {
        id,
        label,
        class,
        adr,
        min_role,
        org_of,
        project_of: ProjectOf::None,
        redact,
        target,
        describe: describe_generic,
        machine: StepUpMachineUse::Requestable,
        max_request_body: DEFAULT_MAX_REQUEST_BODY,
    }
}

const ADMIN: Option<Role> = Some(Role::Admin);
const MEMBER: Option<Role> = Some(Role::Member);
const ANYONE: Option<Role> = None;
const W: Mode = Mode::Window;
const A: Mode = Mode::AlwaysAsk;

// --- reads ------------------------------------------------------------------

/// Every GET route.
pub static READ: Op = op(
    "read",
    "Read",
    Class::Read,
    "reads are never governed",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);

// --- not governed -----------------------------------------------------------

pub static AUTH_LOGIN: Op = op(
    "auth.login",
    "Log in",
    Class::NotGoverned("authentication; a login never counts as a step-up"),
    "§1",
    ANYONE,
    OrgOf::None,
    &["/credential", "/code_verifier"],
    &[],
);
pub static AUTH_LOGOUT: Op = op(
    "auth.logout",
    "Log out",
    Class::NotGoverned("ends the session"),
    "-",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);
pub static RUN_CREATE: Op = op(
    "run.create",
    "Create a run",
    Class::NotGoverned("dispatching a run is off the list"),
    "§3 not on the list",
    MEMBER,
    OrgOf::None,
    &[],
    &[],
);
pub static STEP_RERUN: Op = op(
    "step.rerun",
    "Rerun a step",
    Class::NotGoverned("a run write"),
    "§3 not on the list",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id", "path:step"],
);
pub static STEP_RETRY: Op = op(
    "step.retry",
    "Retry a step",
    Class::NotGoverned("re-asks its gates, never releases one"),
    "§3 not on the list",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id", "path:step"],
);
pub static SNAPSHOTS_PIN: Op = op(
    "run.snapshots_pin",
    "Pin a run's snapshots",
    Class::NotGoverned("extends retention"),
    "§3 not on the list",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id"],
);
pub static SNAPSHOTS_UNPIN: Op = op(
    "run.snapshots_unpin",
    "Unpin a run's snapshots",
    Class::NotGoverned("returns to the declared retention, never below it"),
    "§3 shortening retention (AMBIG: not below declared)",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id"],
);
pub static GATE_RELEASE: Op = op(
    "gate.release_external",
    "Release an external gate",
    Class::NotGoverned(
        "the external-gate HMAC token is the approval (ADR-0034); a machine, no session",
    ),
    "§5",
    ANYONE,
    OrgOf::Run,
    &[],
    &["path:id", "path:step"],
);
pub static STEP_RESULTS: Op = op(
    "step.results_ingest",
    "Ingest step results",
    Class::NotGoverned("the sidecar's fence token (ADR-0042)"),
    "-",
    ANYONE,
    OrgOf::Run,
    &[],
    &["path:id", "path:step"],
);
pub static ENV_LEGACY_PUT: Op = op(
    "environment.legacy_put",
    "Write a per-repo environment",
    Class::NotGoverned("retired, always 410 (ADR-0071)"),
    "-",
    ANYONE,
    OrgOf::Path("org"),
    &[],
    &[],
);
pub static ENV_LEGACY_DELETE: Op = op(
    "environment.legacy_delete",
    "Delete a per-repo environment",
    Class::NotGoverned("retired, always 410 (ADR-0071)"),
    "-",
    ANYONE,
    OrgOf::Path("org"),
    &[],
    &[],
);
pub static MIGRATION_ACK: Op = op(
    "environment.migration_report_ack",
    "Acknowledge an environment migration report",
    Class::NotGoverned("an acknowledgement record; no governance change"),
    "-",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env"],
);
pub static CONNECTION_RESYNC: Op = op(
    "connection.resync",
    "Resync a forge connection",
    // Binds every reported-but-unbound repo through the same store call as
    // `connection.bind_repo`, so it is governed the same way (else it is a
    // bulk bypass of that crown jewel).
    Class::CrownJewel(W),
    "§3 ForgeConnection edits (AMBIG)",
    ADMIN,
    OrgOf::None,
    &[],
    &["path:id"],
);
pub static CONNECTION_WEBHOOK: Op = op(
    "connection.webhook_register",
    "Register a repo webhook",
    Class::NotGoverned("idempotent re-registration for an already-bound repo"),
    "§3 ForgeConnection edits (AMBIG)",
    ADMIN,
    OrgOf::None,
    &[],
    &["path:id", "path:owner", "path:name"],
);
pub static SECRET_CELL_SILENCE: Op = op(
    "secret_matrix.silence",
    "Silence a secret-matrix cell",
    Class::NotGoverned("an advisory annotation"),
    "-",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:repo"],
);
pub static SECRET_CELL_UNSILENCE: Op = op(
    "secret_matrix.unsilence",
    "Unsilence a secret-matrix cell",
    Class::NotGoverned("an advisory annotation"),
    "-",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:repo"],
);
pub static DISPATCH: Op = op(
    "repo.dispatch",
    "Dispatch a pipeline",
    Class::NotGoverned("dispatching a Deploy Run is off the list; its Environment governs it"),
    "§3 not on the list",
    MEMBER,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:repo"],
);
pub static WEBHOOK_GITHUB: Op = op(
    "webhook.github",
    "GitHub webhook delivery",
    Class::NotGoverned("an HMAC-verified machine delivery"),
    "-",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);
pub static WEBHOOK_FORGEJO: Op = op(
    "webhook.forgejo",
    "Forgejo webhook delivery",
    Class::NotGoverned("an HMAC-verified machine delivery"),
    "-",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);

// --- exempt (tightening) ----------------------------------------------------

pub static TOKEN_REVOKE: Op = op(
    "token.revoke",
    "Revoke an API token",
    Class::Exempt("revocation tightens"),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:id"],
);
pub static RUN_CANCEL: Op = op(
    "run.cancel",
    "Cancel a run",
    Class::Exempt("cancelling a Run"),
    "§3 not on the list",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id"],
);
pub static GATE_REJECT: Op = op(
    "gate.reject",
    "Reject a gate",
    Class::Exempt("a reject can only fail a Gate, never release one"),
    "§3 not on the list",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id", "path:step"],
);
pub static PRIVILEGE_GRANT_DELETE: Op = op(
    "environment.privilege_grant.delete",
    "Withdraw a privilege grant",
    Class::Exempt("withdrawing a grant tightens"),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env", "path:project"],
);
pub static PLUGIN_UNINSTALL: Op = op(
    "plugin.uninstall",
    "Uninstall a plugin",
    Class::Exempt("uninstall"),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:name"],
);
pub static PLUGIN_KILL: Op = op(
    "plugin.kill",
    "Kill a plugin",
    Class::Exempt("the kill switch"),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:name"],
);
pub static PLUGIN_CONNECTION_DELETE: Op = op(
    "plugin_connection.delete",
    "Remove a plugin connection",
    Class::Exempt("removes an outbound channel"),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:name", "path:slot"],
);

pub static PLUGIN_STORAGE_DELETE: Op = op(
    "plugin.storage.delete",
    "Stop mounting a plugin's storage",
    Class::Exempt("the plugin loses access to the claim; the claim and its data stay"),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:name"],
);

// --- crown jewels, the window satisfies them ---------------------------------

pub static IDENTITY_LINK: Op = op(
    "identity.link",
    "Link a login identity",
    Class::CrownJewel(W),
    "§3 identity links",
    ANYONE,
    OrgOf::None,
    &[],
    &["body:/provider"],
)
.browser_only();
pub static IDENTITY_UNLINK: Op = op(
    "identity.unlink",
    "Unlink a login identity",
    Class::CrownJewel(W),
    "§3 identity links (AMBIG)",
    ANYONE,
    OrgOf::None,
    &[],
    &["path:provider"],
)
.browser_only();
pub static BINDING_PUT: Op = op(
    "binding.put",
    "Change a role binding",
    Class::CrownJewel(W),
    "§3 role changes",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/project", "body:/subject"],
);
pub static BINDING_DELETE: Op = op(
    "binding.delete",
    "Revoke a role binding",
    Class::CrownJewel(W),
    "§3 role changes (AMBIG: a revoke is a lock-out vector)",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "query:project", "query:subject"],
);
pub static BINDINGS_IMPORT: Op = op(
    "binding.import",
    "Import role bindings from the forge",
    Class::CrownJewel(W),
    "§3 role changes",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:repo"],
)
.in_project(ProjectOf::Path("repo"));
pub static SECRET_PUT: Op = op(
    "secret.put",
    "Write a secret",
    Class::CrownJewel(W),
    "§3 secret writes",
    ADMIN,
    OrgOf::Body("/org"),
    &["/value"],
    &["body:/org", "body:/repo", "body:/environment", "body:/name"],
)
.in_project(ProjectOf::Body("/repo"))
// A secret value (a certificate chain, a kubeconfig) may be bigger.
.max_request_body(256 * 1024);
pub static SECRET_DELETE: Op = op(
    "secret.delete",
    "Delete a secret",
    Class::CrownJewel(W),
    "§3 secret writes; irreversible destruction",
    ADMIN,
    OrgOf::Query("org"),
    &[],
    &["query:org", "query:repo", "query:environment", "query:name"],
)
.in_project(ProjectOf::Query("repo"));
pub static ENVIRONMENT_DELETE: Op = op(
    "environment.delete",
    "Delete an Environment",
    Class::CrownJewel(W),
    "§3 irreversible destruction",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env"],
);
pub static OVERLAY_DELETE: Op = op(
    "environment.overlay.delete",
    "Delete a project's Environment overlay",
    Class::CrownJewel(W),
    "§3 Environment loosening (back to the baseline)",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env", "path:project"],
)
.in_project(ProjectOf::Path("project"));
pub static PLUGIN_ENABLE: Op = op(
    "plugin.enable",
    "Re-enable a killed plugin",
    Class::CrownJewel(W),
    "§3 plugin install (AMBIG: the inverse of the kill switch)",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:name"],
);
pub static CONNECTION_CREATE: Op = op(
    "connection.create",
    "Add a forge connection",
    Class::CrownJewel(W),
    "§3 ForgeConnection edits",
    ADMIN,
    OrgOf::None,
    &["/credential"],
    &["body:/kind", "body:/base_url"],
);
pub static CONNECTION_DELETE: Op = op(
    "connection.delete",
    "Delete a forge connection",
    Class::CrownJewel(W),
    "§3 ForgeConnection edits; Project deletion",
    ADMIN,
    OrgOf::None,
    &[],
    &["path:id"],
);
pub static CONNECTION_BIND_REPO: Op = op(
    "connection.bind_repo",
    "Bind a repository",
    Class::CrownJewel(W),
    "§3 ForgeConnection edits (AMBIG)",
    ADMIN,
    OrgOf::None,
    &[],
    &["path:id", "body:/owner", "body:/name"],
);
pub static CONNECTION_UNBIND_REPO: Op = op(
    "connection.unbind_repo",
    "Unbind a repository (delete its Project)",
    Class::CrownJewel(W),
    "§3 irreversible destruction (Project)",
    ADMIN,
    OrgOf::None,
    &[],
    &["path:id", "path:owner", "path:name"],
);

// --- crown jewels, always ask ------------------------------------------------

pub static TOKEN_MINT: Op = op(
    "token.mint",
    "Create an API token",
    Class::CrownJewel(A),
    "§3/§4 API token creation",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/project", "body:/name"],
)
.not_by_token();
pub static PLUGIN_INSTALL: Op = op(
    "plugin.install",
    "Install a plugin",
    Class::CrownJewel(A),
    "§3/§4 plugin install",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/name"],
);
pub static PLUGIN_CONNECTION_PUT: Op = op(
    "plugin_connection.put",
    "Set a plugin connection",
    Class::CrownJewel(A),
    "§3/§4 PluginConnection create/edit and its destination",
    ADMIN,
    OrgOf::Path("org"),
    &["/credential"],
    &["path:org", "path:name", "path:slot"],
);
pub static PASSKEY_REMOVE: Op = op(
    "passkey.remove",
    "Remove a passkey",
    Class::CrownJewel(A),
    "§2/§4 passkey removal",
    ANYONE,
    OrgOf::None,
    &[],
    &["path:id"],
);
pub static PASSKEY_RESET: Op = op(
    "passkey.reset",
    "Reset a member's passkeys",
    Class::CrownJewel(A),
    "§2/§4 passkey reset (an Owner's; the operator break-glass has no route)",
    Some(Role::Owner),
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:subject"],
);

// --- conditional (the handler decides) ---------------------------------------

pub static GATE_APPROVE: Op = op(
    "gate.approve",
    "Approve a gate",
    Class::Conditional {
        mode: W,
        rule: "the run's Environment has approvers",
    },
    "§3 approvals on Gates of Environments with approvers",
    MEMBER,
    OrgOf::Run,
    &[],
    &["path:id", "path:step"],
);
pub static ENVIRONMENT_PUT: Op = op(
    "environment.put",
    "Write an Environment",
    Class::NotGoverned(
        "a name and a description; the deployment sources it once governed (E3) went with \
         ADR-0074",
    ),
    "§3 not on the list",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env"],
);
pub static ELIGIBILITY_PUT: Op = op(
    "environment.eligibility.put",
    "Change an Environment's eligible projects",
    Class::Conditional {
        mode: W,
        rule: "E2: widens eligibility",
    },
    "§3 EnvironmentProjectEligibility",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env"],
);
pub static PROTECTION_PUT: Op = op(
    "environment.protection.put",
    "Change an Environment's protection rules",
    Class::Conditional {
        mode: W,
        rule: "E1: loosens the protection rules",
    },
    "§3 protection rules",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env"],
);
pub static OVERLAY_PUT: Op = op(
    "environment.overlay.put",
    "Change a project's Environment overlay",
    Class::Conditional {
        mode: W,
        rule: "E1 (+ gate deadline): loosens the overlay",
    },
    "§3 protection rules (overlay)",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env", "path:project"],
)
.in_project(ProjectOf::Path("project"));
pub static PRIVILEGE_GRANT_PUT: Op = op(
    "environment.privilege_grant.put",
    "Change a project's privilege grant",
    Class::Conditional {
        mode: W,
        rule: "E4: widens the privilege whitelist",
    },
    "§3 privilege whitelist",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:env", "path:project"],
);
pub static PLUGIN_STORAGE_PUT: Op = op(
    "plugin.storage.put",
    "Set a plugin's storage",
    Class::Conditional {
        mode: W,
        rule: "moves existing storage to another claim or mount path; adding storage to an \
               installation that has none is exempt",
    },
    "§3 ADR-0074 §7 plugin storage (AMBIG: closest to a plugin upgrade)",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:name", "body:/claim"],
);
pub static PASSKEY_ADD: Op = op(
    "passkey.add",
    "Add a passkey",
    Class::Conditional {
        mode: A,
        rule: "a later passkey is always-ask; the first needs a fresh browser login instead",
    },
    "§2/§4 passkey add after the first",
    ANYONE,
    OrgOf::None,
    &[],
    &["body:/label"],
)
.browser_only();

// --- the ceremony endpoints ----------------------------------------------------

pub static PASSKEY_REGISTRATION: Op = op(
    "passkey.registration",
    "Finish enrolling a passkey",
    Class::Ceremony,
    "§2 (consumes the challenge minted under the passkey.add rule)",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);
pub static STEP_UP_OPTIONS: Op = op(
    "step_up.options",
    "Begin a step-up",
    Class::Ceremony,
    "§4",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);
pub static STEP_UP_CONFIRM: Op = op(
    "step_up.confirm",
    "Confirm a step-up",
    Class::Ceremony,
    "§4",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);
pub static STEP_UP_REQUEST_OPTIONS: Op = op(
    "step_up.request_options",
    "Begin confirming a step-up request",
    Class::Ceremony,
    "§6 (only the request's own human, in a browser)",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);
pub static STEP_UP_REQUEST_CONFIRM: Op = op(
    "step_up.request_confirm",
    "Confirm a step-up request",
    Class::Ceremony,
    "§6 (only the request's own human, in a browser; the replay is guarded as the op itself)",
    ANYONE,
    OrgOf::None,
    &[],
    &[],
);

// --- Policies (ADR-0075 §10) ------------------------------------------------
//
// Op ids say `policy.*`, never a checkpoint name: `run.create` is already the
// id of POST /v1/runs.

pub static POLICY_CREATE: Op = op(
    "policy.create",
    "Add an org Policy",
    Class::Exempt("adding a Policy tightens"),
    "ADR-0075 §10",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/policy/name"],
);
pub static POLICY_UPDATE: Op = op(
    "policy.update",
    "Change an org Policy",
    Class::Conditional {
        mode: W,
        rule: "ADR-0075 §10: any change to an enforced Policy; none to an audit-mode one",
    },
    "ADR-0075 §10",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:id"],
);
pub static POLICY_DELETE: Op = op(
    "policy.delete",
    "Delete an org Policy",
    Class::Conditional {
        mode: W,
        rule: "ADR-0075 §10: deleting an enforced Policy; not an audit-mode one",
    },
    "ADR-0075 §10",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:id"],
);
pub static PROJECT_POLICY_CREATE: Op = op(
    "project_policy.create",
    "Add a Project typed setting",
    Class::Exempt("adding a Policy tightens"),
    "ADR-0075 §10",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:project", "body:/policy/name"],
)
.in_project(ProjectOf::Path("project"));
pub static PROJECT_POLICY_UPDATE: Op = op(
    "project_policy.update",
    "Change a Project typed setting",
    Class::Conditional {
        mode: W,
        rule: "ADR-0075 §10: any change to an enforced Policy; none to an audit-mode one",
    },
    "ADR-0075 §10",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:project", "path:id"],
)
.in_project(ProjectOf::Path("project"));
pub static PROJECT_POLICY_DELETE: Op = op(
    "project_policy.delete",
    "Delete a Project typed setting",
    Class::Conditional {
        mode: W,
        rule: "ADR-0075 §10: deleting an enforced Policy; not an audit-mode one",
    },
    "ADR-0075 §10",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:project", "path:id"],
)
.in_project(ProjectOf::Path("project"));
pub static POLICY_TEST: Op = op(
    "policy.test",
    "Test Policies",
    Class::NotGoverned("evaluates without enforcing or recording"),
    "ADR-0075 §9",
    ANYONE,
    OrgOf::Path("org"),
    &[],
    &["path:org"],
);

pub static LIBRARY_FETCH: Op = op(
    "library.fetch",
    "Fetch a library version to review",
    Class::NotGoverned("stores a version for review; installs nothing"),
    "ADR-0076 §4",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/source"],
);
pub static LIBRARY_INSTALL: Op = op(
    "library.install",
    "Install or upgrade a library",
    Class::CrownJewel(W),
    "ADR-0076 §4: installing and upgrading loosen",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &[
        "path:org",
        "path:publisher",
        "path:name",
        "body:/content_hash",
    ],
);
pub static LIBRARY_REMOVE: Op = op(
    "library.remove",
    "Remove a library",
    Class::Exempt("removing a library tightens"),
    "ADR-0076 §4",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:publisher", "path:name"],
);
pub static LIBRARY_YANK: Op = op(
    "library.yank",
    "Yank a library version",
    Class::Exempt("yanking tightens"),
    "ADR-0076 §6",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/content_hash"],
);
pub static LIBRARY_UNYANK: Op = op(
    "library.unyank",
    "Lift a library yank",
    Class::CrownJewel(W),
    "ADR-0076 §6: un-yanking loosens",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "path:hash"],
);
pub static LIBRARY_DEFAULTS_RETRY: Op = op(
    "library.defaults_retry",
    "Install the first-party libraries",
    Class::CrownJewel(W),
    "ADR-0076 §5: installing loosens",
    ADMIN,
    OrgOf::Path("org"),
    &[],
    &["path:org", "body:/mirror"],
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(body: Value) -> OpInput {
        OpInput {
            method: "POST".into(),
            path: "/v1/secrets".into(),
            params: vec![],
            query: vec![],
            body,
        }
    }

    /// The operations a machine caller may never raise a request for: the
    /// browser-only flows, and token minting for a token (git-bug ff7a014).
    #[test]
    fn only_the_browser_flows_and_token_minting_refuse_a_machine() {
        let mut browser_only = Vec::new();
        let mut not_by_token = Vec::new();
        for e in crate::route_table() {
            match e.op.machine {
                StepUpMachineUse::BrowserOnly => browser_only.push(e.op.id),
                StepUpMachineUse::NotByToken => not_by_token.push(e.op.id),
                StepUpMachineUse::Requestable => {}
            }
        }
        browser_only.sort_unstable();
        browser_only.dedup();
        assert_eq!(
            browser_only,
            vec!["identity.link", "identity.unlink", "passkey.add"]
        );
        assert_eq!(not_by_token, vec!["token.mint"]);
    }

    #[test]
    fn a_redacted_field_never_reaches_the_summary() {
        let body = json!({"org": "acme", "repo": "web", "name": "DB_PASSWORD", "value": "hunter2"});
        let s = SECRET_PUT.summary(&input(redact(&body, SECRET_PUT.redact)));
        assert_eq!(s.operation, "Write a secret");
        assert_eq!(s.target, "acme/web/DB_PASSWORD");
        let all = format!("{s:?}");
        assert!(!all.contains("hunter2"), "{all}");
        assert!(s.changes.contains(&"value = (hidden)".to_string()));
    }

    #[test]
    fn long_values_are_cut_on_a_char_boundary() {
        let body = json!({"x": "é".repeat(300)});
        let s = describe_generic(&READ, &input(body));
        assert!(s.changes[0].contains('…'), "{:?}", s.changes);
        assert!(s.changes[0].ends_with("(600 bytes)"), "{:?}", s.changes);
    }

    fn changes(body: Value) -> Vec<String> {
        describe_generic(&READ, &input(body)).changes
    }

    #[test]
    fn a_long_url_is_shown_in_full_with_its_host() {
        let url = format!("https://{}attacker.net/", "a".repeat(300));
        let c = changes(json!({ "base_url": url }));
        assert!(c[0].contains("attacker.net"), "{c:?}");
        assert!(c
            .iter()
            .any(|l| l == &format!("base_url host = {}attacker.net", "a".repeat(300))));
    }

    #[test]
    fn a_long_plain_value_keeps_its_end_visible() {
        let v = format!("{}TAIL", "x".repeat(300));
        let c = changes(json!({ "name": v }));
        assert!(c[0].contains("TAIL"), "{c:?}");
    }

    #[test]
    fn a_bidi_override_is_escaped() {
        let c = changes(json!({ "name": "evil\u{202E}txt.exe" }));
        assert!(c[0].contains("\\u{202E}"), "{c:?}");
        assert!(!c[0].contains('\u{202E}'), "{c:?}");
        let mut op_input = input(Value::Null);
        op_input.params = vec![("org".into(), "a\u{1b}[31mb".into())];
        let s = describe_generic(&BINDING_PUT, &op_input);
        assert!(!s.target.contains('\u{1b}'), "{}", s.target);
        assert!(s.target.contains("\\u{001B}"), "{}", s.target);
    }

    #[test]
    fn userinfo_cannot_disguise_a_url_s_host() {
        let url = format!(
            "https://hooks.acme.example{}@evil.example/x",
            "z".repeat(190)
        );
        let c = changes(json!({ "base_url": url }));
        assert!(
            c.iter()
                .any(|l| l == "base_url host = evil.example (URL carries userinfo)"),
            "{c:?}"
        );
    }

    #[test]
    fn a_nested_body_is_flattened_to_its_leaves() {
        assert_eq!(changes(json!({"a": {"b": "c"}})), vec!["a.b = c"]);
    }

    #[test]
    fn query_parameters_are_shown_unless_the_target_names_them() {
        let mut i = input(Value::Null);
        i.query = vec![
            ("org".into(), "acme".into()),
            ("unbind_repos".into(), "true".into()),
        ];
        let c = describe_generic(&CONNECTION_DELETE, &i).changes;
        assert_eq!(c, vec!["?org = acme", "?unbind_repos = true"]);
        let c = describe_generic(&SECRET_DELETE, &i).changes;
        assert_eq!(c, vec!["?unbind_repos = true"], "org is a target source");
    }
}
