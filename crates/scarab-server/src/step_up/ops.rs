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
    /// JSON pointers into the body hidden in summaries and events
    /// (`/value`, `/credential`). The digest still covers them.
    pub redact: &'static [&'static str],
    /// Where the summary's target comes from, joined with `/`:
    /// `path:<name>`, `query:<name>`, `body:<json pointer>`.
    pub target: &'static [&'static str],
    pub describe: fn(&Op, &OpInput) -> OpSummary,
}

impl Op {
    pub fn summary(&self, input: &OpInput) -> OpSummary {
        (self.describe)(self, input)
    }
}

/// How long one rendered value may be in a summary line.
const MAX_CHANGE_VALUE: usize = 200;

/// The default description: the label; the target from [`Op::target`]; one
/// `field = value` line per top-level body field (sorted, redacted fields
/// already `(hidden)`).
pub fn describe_generic(op: &Op, input: &OpInput) -> OpSummary {
    let target: Vec<String> = op.target.iter().filter_map(|s| input.source(s)).collect();
    let mut changes = Vec::new();
    if let Value::Object(map) = &input.body {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            let v = &map[k];
            let mut rendered = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if rendered.len() > MAX_CHANGE_VALUE {
                let mut cut = MAX_CHANGE_VALUE;
                while !rendered.is_char_boundary(cut) {
                    cut -= 1;
                }
                rendered.truncate(cut);
                rendered.push('…');
            }
            changes.push(format!("{k} = {rendered}"));
        }
    }
    OpSummary {
        operation: op.label.to_string(),
        target: target.join("/"),
        changes,
    }
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
        redact,
        target,
        describe: describe_generic,
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
    Class::NotGoverned("re-applies what the credential and installation already cover"),
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
);
pub static IDENTITY_UNLINK: Op = op(
    "identity.unlink",
    "Unlink a login identity",
    Class::CrownJewel(W),
    "§3 identity links (AMBIG)",
    ANYONE,
    OrgOf::None,
    &[],
    &["path:provider"],
);
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
);
pub static SECRET_PUT: Op = op(
    "secret.put",
    "Write a secret",
    Class::CrownJewel(W),
    "§3 secret writes",
    ADMIN,
    OrgOf::Body("/org"),
    &["/value"],
    &["body:/org", "body:/repo", "body:/environment", "body:/name"],
);
pub static SECRET_DELETE: Op = op(
    "secret.delete",
    "Delete a secret",
    Class::CrownJewel(W),
    "§3 secret writes; irreversible destruction",
    ADMIN,
    OrgOf::Query("org"),
    &[],
    &["query:org", "query:repo", "query:environment", "query:name"],
);
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
);
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
);
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
    Class::Conditional {
        mode: W,
        rule: "E3: adds a deployment source",
    },
    "§3 EnvironmentDeploymentSources",
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
);
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
);

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
        assert!(s.changes[0].ends_with('…'));
    }
}
