//! Where core calls a Plugin (ADR-0086 §1, §2): a manifest's
//! PluginExtensionRegistrations, approved with it. Nothing registers at run
//! time.
//!
//! The one PluginExtensionPoint this release knows is the **endpoint**: core
//! forwards an authenticated outside request under
//! `/v1/orgs/{org}/plugin-installations/{id}/endpoints/<path>` to the Plugin.
//! Every request must match exactly one of the registration's
//! PluginEndpointOperations, each naming the ApiOperations it **may** use
//! through a DelegatedCallerHandle and how many writes it may make. This
//! module holds the shapes and the rules that need no route table; the
//! server checks each `may` against its ApiOperations at install.
//!
//! A Plugin with an endpoint registration is a `trusted_service` Plugin
//! ([`PluginTrustTier`]): it sees data for many callers, so only an image of
//! a publisher the operator lists may run as one.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::PluginError;

/// The most extension registrations one manifest may declare.
pub const MAX_EXTENSION_REGISTRATIONS: usize = 4;
/// The most operations one endpoint registration may declare.
pub const MAX_ENDPOINT_OPERATIONS: usize = 64;
/// The longest endpoint path, in characters.
pub const MAX_ENDPOINT_PATH_CHARS: usize = 64;
/// The longest operation name, in characters.
pub const MAX_ENDPOINT_OPERATION_NAME_CHARS: usize = 64;
/// The most JSON checks one request rule may make.
pub const MAX_ENDPOINT_JSON_CHECKS: usize = 8;
/// The longest JSON pointer a check may name, in characters.
pub const MAX_ENDPOINT_JSON_POINTER_CHARS: usize = 128;
/// The longest string an `equals` check may compare with, in characters.
pub const MAX_ENDPOINT_EQUALS_CHARS: usize = 256;
/// The most ApiOperations one endpoint operation's `may` may name.
pub const MAX_ENDPOINT_MAY_OPERATIONS: usize = 32;
/// The most writes one endpoint operation may allow per request.
pub const MAX_ENDPOINT_OPERATION_WRITES: u32 = 10;
/// The longest ApiOperation id a `may` may name, in characters.
pub const MAX_API_OPERATION_ID_CHARS: usize = 64;

/// A place core calls whatever Plugin registered there (ADR-0086 §1). The
/// set is closed and grows only with a Scarab release; a manifest naming
/// another point is refused when it is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginExtensionPoint {
    /// Core forwards an authenticated outside request to the Plugin and
    /// returns its answer.
    Endpoint,
}

impl PluginExtensionPoint {
    /// The wire name (`endpoint`).
    pub fn as_str(self) -> &'static str {
        match self {
            PluginExtensionPoint::Endpoint => "endpoint",
        }
    }
}

/// How far an installation is trusted with other people's data (ADR-0086
/// §8). Only the tier this release needs exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginTrustTier {
    /// The Plugin has an endpoint registration: it acts for many callers,
    /// and core cannot stop it repeating one caller's data to another. Only
    /// an image whose repository the operator lists in
    /// `plugins.trustedPublishers`, pinned by digest, runs as one.
    TrustedService,
}

impl PluginTrustTier {
    /// The wire name (`trusted_service`).
    pub fn as_str(self) -> &'static str {
        match self {
            PluginTrustTier::TrustedService => "trusted_service",
        }
    }
}

/// One PluginExtensionRegistration: a point and what the Plugin registers
/// there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginExtensionRegistration {
    pub point: PluginExtensionPoint,
    /// The path the endpoint claims under its installation's `endpoints/`:
    /// one or more DNS-1123 labels joined by `/` (`mcp`).
    pub path: String,
    /// Every request the endpoint takes, by name. A request that matches
    /// none is refused by core and never reaches the Plugin.
    pub operations: BTreeMap<String, PluginEndpointOperation>,
}

/// One kind of request an endpoint takes (ADR-0086 §2): how core recognises
/// it, the ApiOperations a request of it may use, and how many writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEndpointOperation {
    #[serde(rename = "match")]
    pub request_rule: PluginEndpointRequestRule,
    /// The ApiOperation ids (`run.read`, `repo.dispatch`, …) a request of
    /// this kind may use through its DelegatedCallerHandle. Required, so the
    /// manifest names everything the Plugin can be asked, protocol messages
    /// included (`may: []`).
    pub may: Vec<String>,
    /// How many writes one request of this kind may make. Only a POST
    /// operation may write.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub writes: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// How core recognises a request (ADR-0086 §2): an HTTP method plus checks
/// on the JSON body, each at a JSON pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEndpointRequestRule {
    /// `POST`. A `GET` operation would stream from server to client, which
    /// this release does not serve.
    pub method: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json: BTreeMap<String, PluginEndpointJsonCheck>,
}

/// One check on the body at a JSON pointer: a string the value must equal
/// (JSON strings only), or `{"exists": true}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginEndpointJsonCheck {
    Equals(String),
    Presence(PluginEndpointJsonPresence),
}

/// `{"exists": true}`: the pointer names a value, whatever it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEndpointJsonPresence {
    pub exists: bool,
}

impl PluginExtensionRegistration {
    pub(crate) fn validate(&self) -> Result<(), PluginError> {
        let invalid = |m: String| Err(PluginError::InvalidManifest(m));
        if !is_endpoint_path(&self.path) {
            return invalid(format!(
                "endpoint path `{}` must be 1-{MAX_ENDPOINT_PATH_CHARS} characters of \
                 DNS-1123 labels joined by `/`",
                self.path.escape_debug()
            ));
        }
        if self.operations.is_empty() {
            return invalid(format!(
                "endpoint `{}` needs at least one operation",
                self.path
            ));
        }
        if self.operations.len() > MAX_ENDPOINT_OPERATIONS {
            return invalid(format!(
                "endpoint `{}`: at most {MAX_ENDPOINT_OPERATIONS} operations",
                self.path
            ));
        }
        let mut rules = BTreeSet::new();
        for (name, operation) in &self.operations {
            if !is_operation_name(name) {
                return invalid(format!(
                    "endpoint `{}`: operation name `{}` must be 1-\
                     {MAX_ENDPOINT_OPERATION_NAME_CHARS} of `a-z`, `0-9`, `_`, starting with a \
                     letter",
                    self.path,
                    name.escape_debug()
                ));
            }
            operation.validate(&self.path, name)?;
            // Two operations with the same rule match the same requests, and
            // a request that matches two is refused: neither could be used.
            let rule = serde_json::to_string(&operation.request_rule).unwrap_or_default();
            if !rules.insert(rule) {
                return invalid(format!(
                    "endpoint `{}`: operation `{name}` has the same match as another operation",
                    self.path
                ));
            }
        }
        Ok(())
    }
}

impl PluginEndpointOperation {
    fn validate(&self, path: &str, name: &str) -> Result<(), PluginError> {
        let invalid = |m: String| Err(PluginError::InvalidManifest(m));
        let at = format!("endpoint `{path}` operation `{name}`");
        if self.request_rule.method != "POST" {
            return invalid(format!(
                "{at}: method `{}` is not POST; a GET operation streams from server to client, \
                 which this Scarab release does not serve",
                self.request_rule.method.escape_debug()
            ));
        }
        if self.request_rule.json.len() > MAX_ENDPOINT_JSON_CHECKS {
            return invalid(format!(
                "{at}: at most {MAX_ENDPOINT_JSON_CHECKS} JSON checks"
            ));
        }
        for (pointer, check) in &self.request_rule.json {
            let pointer_ok = pointer.starts_with('/')
                && pointer.chars().count() <= MAX_ENDPOINT_JSON_POINTER_CHARS
                && !pointer.chars().any(char::is_control);
            if !pointer_ok {
                return invalid(format!(
                    "{at}: `{}` is not a JSON pointer of at most \
                     {MAX_ENDPOINT_JSON_POINTER_CHARS} characters starting with `/`",
                    pointer.escape_debug()
                ));
            }
            match check {
                PluginEndpointJsonCheck::Equals(value) => {
                    if value.chars().count() > MAX_ENDPOINT_EQUALS_CHARS
                        || value.chars().any(char::is_control)
                    {
                        return invalid(format!(
                            "{at}: the value checked at `{pointer}` must be one line of at most \
                             {MAX_ENDPOINT_EQUALS_CHARS} characters"
                        ));
                    }
                }
                PluginEndpointJsonCheck::Presence(p) if !p.exists => {
                    return invalid(format!(
                        "{at}: the check at `{pointer}` is `{{\"exists\": false}}`; only \
                         `{{\"exists\": true}}` is a check"
                    ));
                }
                PluginEndpointJsonCheck::Presence(_) => {}
            }
        }
        if self.may.len() > MAX_ENDPOINT_MAY_OPERATIONS {
            return invalid(format!(
                "{at}: `may` names at most {MAX_ENDPOINT_MAY_OPERATIONS} operations"
            ));
        }
        let mut seen = BTreeSet::new();
        for op in &self.may {
            if !is_api_operation_id(op) {
                return invalid(format!(
                    "{at}: `may` names `{}`, which is not an operation id",
                    op.escape_debug()
                ));
            }
            if !seen.insert(op.as_str()) {
                return invalid(format!("{at}: `may` names `{op}` twice"));
            }
        }
        if self.writes > MAX_ENDPOINT_OPERATION_WRITES {
            return invalid(format!(
                "{at}: at most {MAX_ENDPOINT_OPERATION_WRITES} writes"
            ));
        }
        Ok(())
    }

    /// One line an admin reads on the approval screen: `POST
    /// "/method"="tools/call" "/params/name"="trigger_run"; may
    /// repo.dispatch, repo.read; writes 1`. Pointers and values are JSON
    /// strings, so a space, `=` or `;` inside one cannot pass for another
    /// check.
    pub fn describe(&self) -> String {
        let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
        let mut line = self.request_rule.method.clone();
        for (pointer, check) in &self.request_rule.json {
            match check {
                PluginEndpointJsonCheck::Equals(v) => {
                    line.push_str(&format!(" {}={}", quoted(pointer), quoted(v)))
                }
                PluginEndpointJsonCheck::Presence(_) => {
                    line.push_str(&format!(" {} exists", quoted(pointer)))
                }
            }
        }
        let mut may = self.may.clone();
        may.sort();
        if may.is_empty() {
            line.push_str("; may nothing");
        } else {
            line.push_str(&format!("; may {}", may.join(", ")));
        }
        if self.writes > 0 {
            line.push_str(&format!("; writes {}", self.writes));
        }
        line
    }
}

/// `path` as an endpoint path: DNS-1123 labels joined by `/`.
fn is_endpoint_path(path: &str) -> bool {
    !path.is_empty()
        && path.chars().count() <= MAX_ENDPOINT_PATH_CHARS
        && path.split('/').all(crate::is_dns1123_label)
}

fn is_operation_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ENDPOINT_OPERATION_NAME_CHARS
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The shape of an ApiOperation id (`run.read`, `environment.overlay.put`);
/// whether one exists is the server's check.
fn is_api_operation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_API_OPERATION_ID_CHARS
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.'))
}
