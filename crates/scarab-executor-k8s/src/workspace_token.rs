//! The **workspace token** (ADR-0061): the fence-scoped credential a Step Pod
//! presents to the workspace service.
//!
//! # Why this is a NEW token and not the results-ingest one
//!
//! The obvious economy — reuse `x-scarab-results-token`, one credential, one
//! mechanism, less surface — is refused on three facts, not on taste:
//!
//! 1. **It carries no verb.** The results token is
//!    `HMAC(secret, "{run}:{step}:{attempt}")` and nothing else. Presenting it
//!    to a content store would silently promote a *results-write* credential
//!    into a *content read+write* credential, with no way to tell the two uses
//!    apart at the verifier.
//! 2. **It never expires.** Materially worse here than there: a ten-second POST
//!    with an eternal credential is a small window of one shape; an eternal
//!    read credential for every snapshot the token's roots name is a different
//!    thing entirely.
//! 3. **It would hand the workspace service the power to forge step results.**
//!    Verification is symmetric — whoever can verify can mint — so giving the
//!    service the results secret would let it fabricate results for any
//!    `{run, step, attempt}`, including other tenants'. The workspace service is
//!    precisely the component ADR-0061 moves *out* of the trusted control plane.
//!    Hard no.
//!
//! The **primitive** is shared, though: [`scarab_forge_github::sign_hex`] /
//! `verify_signature`, which is where this repo's HMAC already lives. (That the
//! HMAC helpers live in the GitHub adapter is a wart. It is noted, not fixed
//! here.)
//!
//! # One codec, one place
//!
//! Minting and verifying both live in this file, and every constant the two
//! sides must agree on is `pub` here. This is deliberate: the results token's
//! message format is duplicated between `egress_sidecar` (which mints it) and
//! `results_token_message` (which verifies it), and that duplication is a
//! standing drift hazard. It must not be repeated.
//!
//! # Delivery
//!
//! A per-Pod k8s Secret on **tmpfs**, at
//! `/scarab/secrets/workspace-token` — the ADR-0045 clone-credential pattern,
//! *not* the results token's plain `env.value` (a value in a PodSpec is readable
//! by anyone with `get pod`). [`WORKSPACE_TOKEN_FILE_ENV`] points at the file.
//!
//! # What the service enforces
//!
//! See [`WorkspaceClaims`] and the service's own module docs. In one line:
//! tree reads are checked against the token's `roots`; blob reads are checked
//! against the **blob closure** of those roots (ticket 52ef3aa — rollout-gated
//! by `SCARAB_DEPOT_BLOB_AUTHZ`).

use base64::Engine;

/// The header a client presents the token in.
pub const WORKSPACE_TOKEN_HEADER: &str = "x-scarab-workspace-token";

/// The tmpfs directory the per-Pod Secret is mounted at. Deliberately the same
/// path the ADR-0045 clone credential uses — one Secret volume can carry both —
/// but under a **distinct key**, so neither credential's presence implies the
/// other's.
pub const WORKSPACE_SECRETS_MOUNT_PATH: &str = "/scarab/secrets";

/// The Secret key (and therefore file name) the token lives under.
pub const WORKSPACE_TOKEN_KEY: &str = "workspace-token";

/// The env var pointing a Step's helpers at the mounted token file. The token
/// itself NEVER rides in env, argv, or a Pod annotation.
pub const WORKSPACE_TOKEN_FILE_ENV: &str = "SCARAB_WORKSPACE_TOKEN_FILE";

/// The env var carrying the workspace service's base URL into a Pod.
pub const WORKSPACE_URL_ENV: &str = "SCARAB_WORKSPACE_URL";

/// Extra lifetime beyond the step deadline, so a Pod that is being torn down
/// can still finish talking to the service.
pub const WORKSPACE_TOKEN_GRACE_SECS: i64 = 600;

/// Hard ceiling on a token's lifetime regardless of the step timeout: a
/// multi-day bearer credential in an untrusted Pod is not a thing this system
/// mints.
///
/// The ceiling is only SAFE because pipeline validation enforces the matching
/// step-timeout ceiling (`scarab_pipeline::MAX_STEP_TIMEOUT_SECS`, 23h —
/// ticket 16a7768 item 3): before that, a step whose `timeout:` reached this
/// cap would run past its own token's expiry and its drain could only 401 →
/// exit 10 → Transient-loop, stranding an open PackSession for the abandoned-
/// session sweep. The compile-time assert below is what keeps the two
/// constants from drifting apart: every legal timeout leaves the token its
/// full [`WORKSPACE_TOKEN_GRACE_SECS`] beyond the deadline, un-truncated.
pub const WORKSPACE_TOKEN_MAX_TTL_SECS: i64 = 24 * 60 * 60;

const _: () = assert!(
    scarab_pipeline::MAX_STEP_TIMEOUT_SECS as i64 + WORKSPACE_TOKEN_GRACE_SECS
        <= WORKSPACE_TOKEN_MAX_TTL_SECS,
    "the validation-side step-timeout ceiling must leave the workspace token \
     its full post-deadline grace under the 24h cap — raising one constant \
     means re-deciding the other (ticket 16a7768 item 3)"
);

/// The absolute path of the mounted token file — the value of
/// [`WORKSPACE_TOKEN_FILE_ENV`]. One definition so the PodSpec and the reader
/// cannot disagree.
pub fn workspace_token_path() -> String {
    format!("{WORKSPACE_SECRETS_MOUNT_PATH}/{WORKSPACE_TOKEN_KEY}")
}

/// What a token is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A Step Pod's token: read the snapshots named in `roots`, and write
    /// content (a content-addressed write with a verified hash cannot overwrite
    /// or corrupt anything — the worst case is disk consumption, which is the
    /// warm tier's bounded resource). There is deliberately no separate `write`
    /// scope: splitting one would imply the service can be harmed by a write,
    /// and it cannot.
    Read,
    /// The control plane's own token, minted by the API role **for itself** —
    /// the Browse path needs to read arbitrary snapshots and has no single
    /// fence. Never handed to a Pod.
    Browse,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Browse => "browse",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Scope::Read),
            "browse" => Some(Scope::Browse),
            _ => None,
        }
    }
}

/// The `{run, step, attempt}` a token is fenced to (CONTEXT.md §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    pub run: String,
    pub step: String,
    pub attempt: String,
}

/// A verified token's claims. Only ever constructed by [`verify`] — if you hold
/// one, the MAC checked out and `exp` had not passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceClaims {
    pub fence: Fence,
    /// Unix seconds.
    pub exp: i64,
    pub scope: Scope,
    /// The Workspace Snapshot roots this token may read, sorted. Empty for
    /// [`Scope::Browse`], which is not root-limited.
    pub roots: Vec<String>,
}

impl WorkspaceClaims {
    /// May this token read the tree `hash`?
    ///
    /// Exact membership in the declared roots, or any root under
    /// [`Scope::Browse`]. This is cheap and exact **and sufficient in
    /// practice** only because the service exposes `GET
    /// /v1/cas/trees/{hash}/flat`: a Pod gets a whole subtree in one call, so it
    /// never needs to walk sub-trees by hash and never needs a root it was not
    /// given.
    pub fn may_read_tree(&self, hash: &str) -> bool {
        matches!(self.scope, Scope::Browse) || self.roots.iter().any(|r| r == hash)
    }
}

/// Why a token was rejected. Deliberately coarse at the boundary — the caller
/// turns all of these into one 401 — but distinguished here so the service's
/// warning line can say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceTokenError {
    Malformed,
    UnknownVersion,
    BadSignature,
    Expired { exp: i64, now: i64 },
}

// Written by hand rather than derived: `thiserror` is not a dependency of this
// crate and adding one for four variants is not worth the manifest churn.
impl std::fmt::Display for WorkspaceTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed workspace token"),
            Self::UnknownVersion => f.write_str("unknown workspace token version"),
            Self::BadSignature => f.write_str("workspace token signature does not verify"),
            Self::Expired { exp, now } => {
                write!(f, "workspace token expired at {exp} (now {now})")
            }
        }
    }
}

impl std::error::Error for WorkspaceTokenError {}

/// Escape the field separator so the encoding is **injective**.
///
/// Nothing validates the character set of a step id (it comes from authored
/// YAML), so a step literally named `a|b` would otherwise shift every field
/// after it and let one claim set be read as another. `%` first, then `|`, so
/// `a%7Cb` and `a|b` stay distinguishable. Hashes are hex and exp/scope are
/// fixed vocabularies, so only the fence fields can contain anything.
fn esc(field: &str) -> String {
    field.replace('%', "%25").replace('|', "%7C")
}

fn unesc(field: &str) -> String {
    field.replace("%7C", "|").replace("%25", "%")
}

/// The canonical MAC message. **This format is the contract**; changing it
/// invalidates every live token, so it carries an explicit version tag.
///
/// `roots` is sorted and `,`-joined so a token's claim does not depend on the
/// order the executor happened to collect the inputs in.
fn message(claims: &WorkspaceClaims) -> String {
    format!(
        "wsv1|{}|{}|{}|{}|{}|{}",
        esc(&claims.fence.run),
        esc(&claims.fence.step),
        esc(&claims.fence.attempt),
        claims.exp,
        claims.scope.as_str(),
        claims.roots.join(","),
    )
}

/// When a token minted for a Pod launching now should expire: the step deadline
/// plus [`WORKSPACE_TOKEN_GRACE_SECS`], capped at
/// [`WORKSPACE_TOKEN_MAX_TTL_SECS`].
///
/// The cap looks like it could truncate the grace for a long-enough timeout —
/// it cannot: pipeline validation refuses any `timeout:` above
/// `scarab_pipeline::MAX_STEP_TIMEOUT_SECS`, and the compile-time assert on
/// [`WORKSPACE_TOKEN_MAX_TTL_SECS`] holds `ceiling + grace <= cap`. The `min`
/// stays as defense in depth for a caller that bypasses validation (the
/// operator-set default timeout is clamped in server config for the same
/// reason).
pub fn expiry_for(launch_unix: i64, step_timeout_secs: u32) -> i64 {
    let ttl = i64::from(step_timeout_secs)
        .saturating_add(WORKSPACE_TOKEN_GRACE_SECS)
        .min(WORKSPACE_TOKEN_MAX_TTL_SECS);
    launch_unix.saturating_add(ttl)
}

/// Mint a token.
///
/// Wire form: `wsv1.<claims-b64url>.<sha256=hex>`, where `<claims-b64url>` is
/// the base64url (unpadded) encoding of the canonical message above.
///
/// **Deviation from the original spec, recorded on purpose.** The design
/// sketched `wsv1.{exp}.{scope}.{roots_b64url}.{sig}` and said "the verifier
/// reconstructs the message from the token plus the path". It cannot: the
/// message includes `{run}|{step}|{attempt}` and the service's paths are
/// `/v1/cas/...`, which carry no run, step or attempt anywhere. Rather than
/// invent a second header for the fence (the results token's shape, which is
/// how its format came to be duplicated in the first place), the whole
/// canonical message travels inside the token, base64url'd. That keeps the
/// message format *exactly* as specified, keeps the token self-describing,
/// removes any possibility of a delimiter colliding with a step name, and keeps
/// every claim under the MAC — a tampered claim still fails verification.
pub fn mint(secret: &[u8], claims: &WorkspaceClaims) -> String {
    let msg = message(claims);
    let sig = scarab_forge_github::sign_hex(secret, msg.as_bytes());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(msg.as_bytes());
    format!("wsv1.{encoded}.{sig}")
}

/// Verify a token and return its claims. `now_unix` is injected rather than
/// read from the clock so expiry is testable without sleeping.
///
/// Order matters: the signature is checked **before** expiry, so an attacker
/// cannot learn anything from the difference between "expired" and "forged".
pub fn verify(
    secret: &[u8],
    token: &str,
    now_unix: i64,
) -> Result<WorkspaceClaims, WorkspaceTokenError> {
    let mut parts = token.split('.');
    let version = parts.next().ok_or(WorkspaceTokenError::Malformed)?;
    let encoded = parts.next().ok_or(WorkspaceTokenError::Malformed)?;
    let sig = parts.next().ok_or(WorkspaceTokenError::Malformed)?;
    if parts.next().is_some() {
        return Err(WorkspaceTokenError::Malformed);
    }
    if version != "wsv1" {
        return Err(WorkspaceTokenError::UnknownVersion);
    }

    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| WorkspaceTokenError::Malformed)?;
    let msg = String::from_utf8(raw).map_err(|_| WorkspaceTokenError::Malformed)?;

    // Constant-time, and over the bytes as received — never over a message we
    // re-derive from parsed fields, which is how signature-verification bugs
    // are born.
    scarab_forge_github::verify_signature(secret, msg.as_bytes(), Some(sig))
        .map_err(|_| WorkspaceTokenError::BadSignature)?;

    let claims = parse_message(&msg)?;
    if now_unix > claims.exp {
        return Err(WorkspaceTokenError::Expired {
            exp: claims.exp,
            now: now_unix,
        });
    }
    Ok(claims)
}

/// Parse an already-authenticated message back into claims. The fence fields
/// are un-escaped (see [`esc`]).
fn parse_message(msg: &str) -> Result<WorkspaceClaims, WorkspaceTokenError> {
    let fields: Vec<&str> = msg.split('|').collect();
    if fields.len() != 7 || fields[0] != "wsv1" {
        return Err(WorkspaceTokenError::Malformed);
    }
    Ok(WorkspaceClaims {
        fence: Fence {
            run: unesc(fields[1]),
            step: unesc(fields[2]),
            attempt: unesc(fields[3]),
        },
        exp: fields[4]
            .parse::<i64>()
            .map_err(|_| WorkspaceTokenError::Malformed)?,
        scope: Scope::parse(fields[5]).ok_or(WorkspaceTokenError::Malformed)?,
        roots: fields[6]
            .split(',')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    })
}

/// Claims for a Step Pod: `read` scope, fenced, limited to the snapshot roots
/// the step actually inherits. `roots` is sorted here so the caller cannot make
/// the message depend on collection order.
pub fn step_claims(
    fence: Fence,
    exp: i64,
    mut roots: Vec<String>,
) -> WorkspaceClaims {
    roots.sort();
    roots.dedup();
    WorkspaceClaims {
        fence,
        exp,
        scope: Scope::Read,
        roots,
    }
}

/// Claims the control plane mints **for itself** (Browse, ADR-0056): no root
/// limit, because Browse is asked for arbitrary snapshots and its authorization
/// is the API's own RBAC, upstream of this token. Never handed to a Pod.
pub fn browse_claims(exp: i64) -> WorkspaceClaims {
    WorkspaceClaims {
        fence: Fence {
            run: "-".into(),
            step: "-".into(),
            attempt: "-".into(),
        },
        exp,
        scope: Scope::Browse,
        roots: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"workspace-secret";

    fn fence() -> Fence {
        Fence {
            run: "run-1".into(),
            step: "build".into(),
            attempt: "a1".into(),
        }
    }

    #[test]
    fn a_minted_token_verifies_and_round_trips_every_claim() {
        let claims = step_claims(fence(), 2_000, vec!["bbb".into(), "aaa".into()]);
        let token = mint(SECRET, &claims);
        let out = verify(SECRET, &token, 1_000).expect("verifies");
        assert_eq!(out.fence, fence());
        assert_eq!(out.exp, 2_000);
        assert_eq!(out.scope, Scope::Read);
        // Sorted at mint, so two executors that collected the same inputs in a
        // different order produce the SAME token.
        assert_eq!(out.roots, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[test]
    fn a_different_secret_does_not_verify() {
        let token = mint(SECRET, &step_claims(fence(), 2_000, vec!["aaa".into()]));
        assert_eq!(
            verify(b"other", &token, 1_000),
            Err(WorkspaceTokenError::BadSignature)
        );
    }

    /// The whole point of putting the claims under the MAC: a Pod cannot widen
    /// its own root allowlist.
    #[test]
    fn tampering_with_the_roots_claim_fails_the_mac() {
        let claims = step_claims(fence(), 2_000, vec!["aaa".into()]);
        let token = mint(SECRET, &claims);
        let forged = message(&WorkspaceClaims {
            roots: vec!["aaa".into(), "secret-tree".into()],
            ..claims
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(forged.as_bytes());
        let sig = token.rsplit('.').next().unwrap();
        assert_eq!(
            verify(SECRET, &format!("wsv1.{encoded}.{sig}"), 1_000),
            Err(WorkspaceTokenError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_the_expiry_claim_fails_the_mac() {
        let claims = step_claims(fence(), 1_000, vec!["aaa".into()]);
        let token = mint(SECRET, &claims);
        let forged = message(&WorkspaceClaims {
            exp: i64::MAX,
            ..claims
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(forged.as_bytes());
        let sig = token.rsplit('.').next().unwrap();
        assert!(matches!(
            verify(SECRET, &format!("wsv1.{encoded}.{sig}"), 5_000),
            Err(WorkspaceTokenError::BadSignature)
        ));
    }

    #[test]
    fn an_expired_token_is_rejected_but_a_token_expiring_exactly_now_is_not() {
        let token = mint(SECRET, &step_claims(fence(), 1_000, vec!["aaa".into()]));
        assert!(verify(SECRET, &token, 1_000).is_ok());
        assert_eq!(
            verify(SECRET, &token, 1_001),
            Err(WorkspaceTokenError::Expired {
                exp: 1_000,
                now: 1_001
            })
        );
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        for bad in [
            "",
            "wsv1",
            "wsv1.abc",
            "wsv1.!!!.sha256=00",
            "wsv2.abc.sha256=00",
            "wsv1.abc.sha256=00.extra",
        ] {
            assert!(verify(SECRET, bad, 0).is_err(), "{bad:?} should be an error");
        }
    }

    #[test]
    fn a_read_token_may_only_read_the_roots_it_names() {
        let claims = step_claims(fence(), 2_000, vec!["aaa".into()]);
        assert!(claims.may_read_tree("aaa"));
        assert!(!claims.may_read_tree("bbb"));
    }

    /// Browse is minted by the server for itself and is deliberately NOT
    /// root-limited — its authorization is the API's RBAC, upstream of here.
    #[test]
    fn a_browse_token_may_read_any_root() {
        let claims = browse_claims(2_000);
        assert!(claims.may_read_tree("anything"));
        let token = mint(SECRET, &claims);
        assert_eq!(verify(SECRET, &token, 0).unwrap().scope, Scope::Browse);
    }

    /// Every timeout pipeline validation admits leaves the drain its FULL
    /// grace (ticket 16a7768 item 3): at the 23h ceiling, expiry is exactly
    /// timeout + grace — the 24h cap does not truncate it — so the
    /// expired-token 401 → exit 10 → Transient loop is unreachable for any
    /// valid pipeline. The cap only bites timeouts validation already refuses.
    #[test]
    fn the_maximal_valid_timeout_keeps_the_full_drain_grace() {
        let ceiling = scarab_pipeline::MAX_STEP_TIMEOUT_SECS;
        let exp = expiry_for(0, ceiling);
        assert_eq!(exp, i64::from(ceiling) + WORKSPACE_TOKEN_GRACE_SECS);
        assert!(exp <= WORKSPACE_TOKEN_MAX_TTL_SECS);
    }

    #[test]
    fn expiry_is_the_step_deadline_plus_grace_capped_at_a_day() {
        assert_eq!(expiry_for(1_000, 60), 1_000 + 60 + 600);
        // A step with a week-long timeout still gets a one-day token.
        assert_eq!(expiry_for(0, 7 * 24 * 3600), WORKSPACE_TOKEN_MAX_TTL_SECS);
    }

    /// A step name containing the message delimiter must not be able to shift
    /// the field boundaries and forge a different claim set. Nothing validates
    /// the charset of an authored step id, so this is reachable, not theoretical.
    #[test]
    fn a_pipe_in_a_fence_field_round_trips_and_stays_distinguishable() {
        for (run, step, attempt) in [
            ("r", "weird|name", "a"),
            ("r|1", "s", "a"),
            ("r", "a%7Cb", "a"),
            ("r", "100%|done", "a"),
        ] {
            let fence = Fence {
                run: run.into(),
                step: step.into(),
                attempt: attempt.into(),
            };
            let claims = step_claims(fence.clone(), 2_000, vec!["aaa".into()]);
            let out = verify(SECRET, &mint(SECRET, &claims), 0)
                .unwrap_or_else(|e| panic!("legal ids must not break the codec: {e}"));
            assert_eq!(out.fence, fence);
            assert_eq!(out.roots, vec!["aaa".to_string()]);
            assert_eq!(out.scope, Scope::Read);
        }
        // Injective: the escaped form of one id is not the plain form of another.
        assert_ne!(
            mint(SECRET, &step_claims(fence_named("a|b"), 1, vec![])),
            mint(SECRET, &step_claims(fence_named("a%7Cb"), 1, vec![])),
        );
    }

    fn fence_named(step: &str) -> Fence {
        Fence {
            run: "r".into(),
            step: step.into(),
            attempt: "a".into(),
        }
    }
}
