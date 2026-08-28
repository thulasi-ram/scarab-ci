//! # scarab-identity — authN/authZ ports
//!
//! Pure domain crate (ADR-0010): identity is **forge-agnostic**. Login happens
//! via an [`Authenticator`] (OAuth/OIDC, e.g. GitHub) that yields a
//! [`Principal`]; a [`Session`] issued for that principal is kept in a
//! [`SessionStore`] and presented on later requests. Authorization is
//! **Scarab-native RBAC** — roles `{Owner, Admin, Member, Viewer}` (ADR-0032),
//! defined in our own terms and merely *seeded* from a forge. [`OidcIssuer`]
//! mints short-lived per-run JWTs for keyless cloud federation (slice 5).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// An authenticated principal (a human user or a machine identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub subject: String,
    pub display_name: Option<String>,
    /// The principal's Scarab-native roles (seeded from the forge at login, but
    /// authoritative in Scarab).
    pub roles: Vec<Role>,
}

impl Principal {
    /// Is this principal allowed to perform `action` (by any of its roles)?
    pub fn can(&self, action: Action) -> bool {
        self.roles.iter().any(|r| r.allows(action))
    }
}

/// A Scarab-native RBAC role (ADR-0032), ordered least→most privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    Viewer,
    Member,
    Admin,
    Owner,
}

/// A capability a caller may need on a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read runs / logs / status.
    Read,
    /// Create or restart runs.
    Write,
    /// Administrative changes (RBAC, environments, settings).
    Administer,
}

impl Role {
    /// Does this role grant `action`? Viewer reads; Member also writes; Admin and
    /// Owner also administer.
    pub fn allows(self, action: Action) -> bool {
        match action {
            Action::Read => true, // every role can read
            Action::Write => self >= Role::Member,
            Action::Administer => self >= Role::Admin,
        }
    }
}

/// The scope a role binding applies to (ADR-0049): an Org, or one of its
/// Projects. **No finer scope exists** — deploy authorization is the
/// Environment's protection rules (ADR-0037), orthogonal to RBAC.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    Org(String),
    Project { org: String, name: String },
}

impl Scope {
    /// The org this scope belongs to (a Project's scope inherits from it).
    pub fn org(&self) -> &str {
        match self {
            Scope::Org(org) => org,
            Scope::Project { org, .. } => org,
        }
    }
}

/// One RBAC grant: `subject` holds `role` within `scope`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub subject: String,
    pub scope: Scope,
    pub role: Role,
}

/// Where a binding came from (ADR-0049): forge imports **seed**; native
/// grants/revokes are **authoritative** — a re-sync never clobbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingOrigin {
    Native,
    Import,
}

/// Scarab-native RBAC: subject→role grants scoped to orgs/projects.
/// Authoritative in Scarab (ADR-0010/0049), even when seeded from a forge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rbac {
    pub bindings: Vec<Binding>,
}

impl Rbac {
    pub fn grant(&mut self, subject: impl Into<String>, scope: Scope, role: Role) {
        self.bindings.push(Binding {
            subject: subject.into(),
            scope,
            role,
        });
    }

    /// The highest role `subject` holds in `scope`. **An Org role inherits
    /// down** to the org's Projects (ADR-0049): asking at a Project scope
    /// also considers the enclosing Org's bindings.
    pub fn role_of(&self, subject: &str, scope: &Scope) -> Option<Role> {
        let org_scope = Scope::Org(scope.org().to_string());
        self.bindings
            .iter()
            .filter(|b| b.subject == subject && (&b.scope == scope || b.scope == org_scope))
            .map(|b| b.role)
            .max()
    }

    /// May `subject` perform `action` in `scope`?
    pub fn can(&self, subject: &str, scope: &Scope, action: Action) -> bool {
        self.role_of(subject, scope)
            .is_some_and(|r| r.allows(action))
    }
}

/// Durable store of role bindings (ADR-0049): the native model authz reads on
/// the hot path — **never** a live forge call. `role_of` applies Org→Project
/// inheritance, exactly like [`Rbac::role_of`].
#[async_trait]
pub trait RbacStore: Send + Sync {
    /// Upsert a binding. `Native` grants always win; an `Import` grant seeds
    /// or refreshes only rows that are still import-owned — it never clobbers
    /// a native grant or a native revoke.
    async fn grant(&self, binding: &Binding, origin: BindingOrigin) -> Result<(), IdentityError>;
    /// Natively revoke `subject`'s binding at `scope`: a durable tombstone —
    /// later imports cannot resurrect the grant.
    async fn revoke(&self, subject: &str, scope: &Scope) -> Result<(), IdentityError>;
    /// The highest role `subject` holds in `scope`, Org inheritance applied.
    async fn role_of(&self, subject: &str, scope: &Scope) -> Result<Option<Role>, IdentityError>;
    /// All live bindings within `org` (org- and project-scoped).
    async fn bindings(&self, org: &str) -> Result<Vec<Binding>, IdentityError>;
}

/// A server-side login session for a [`Principal`] (ADR-0032: PG-backed in
/// production; the store is a port so the backend is swappable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub principal: Principal,
    /// Unix-ms expiry.
    pub expires_at: i64,
    /// The session's CSRF token (ADR-0049): random, minted with the session,
    /// double-submitted by browsers (readable cookie → `x-csrf-token` header)
    /// on every mutation. Bearer (API/CLI) requests never need it.
    #[serde(default)]
    pub csrf: String,
}

impl Session {
    /// Is the session still valid at `now_ms`?
    pub fn is_valid(&self, now_ms: i64) -> bool {
        now_ms < self.expires_at
    }
}

/// Durable store of login sessions (ADR-0032). A session id (an opaque,
/// unguessable token) maps to its [`Session`].
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn put(&self, session: &Session) -> Result<(), IdentityError>;
    async fn get(&self, id: &str) -> Result<Option<Session>, IdentityError>;
    /// Revoke a session (logout). Deleting an unknown id is a no-op.
    async fn delete(&self, id: &str) -> Result<(), IdentityError>;
}

// ---------------------------------------------------------------------------
// Issued API tokens (ADR-0049 amendment): the credential a machine can hold.
// ---------------------------------------------------------------------------

/// The prefix every issued API token's plaintext carries.
///
/// Two payoffs, both practical. The server routes on it instead of guessing
/// whether a bearer string is a session id or a token, so an expired session id
/// and a bad token cannot be confused for one another. And a secret scanner —
/// GitHub push protection, gitleaks — can be taught exactly one pattern for
/// "this is a Scarab credential, and it is loose".
pub const API_TOKEN_PREFIX: &str = "scarab_pat_";

/// How long a token may be issued for, in days. An expiry is **mandatory**
/// (see [`ApiToken`]); this only bounds how far out it may be pushed.
pub const MAX_API_TOKEN_DAYS: u32 = 365;

/// The stored identity of a token plaintext: its SHA-256, lowercase hex.
///
/// Hashed at rest rather than stored verbatim because a Scarab deployment backs
/// its Postgres up — the public demo dumps to R2 nightly — and a plaintext
/// column would replicate every live credential into object storage on a
/// schedule. A backup outlives a token by a long way.
///
/// A plain digest, not a slow KDF: this is 32 bytes of CSPRNG output, not a
/// password. There is nothing to brute-force and no user-chosen weakness to
/// stretch. Verification is a lookup keyed on this digest, so no comparison in
/// Scarab's own code branches on secret material.
pub fn api_token_hash(plaintext: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(plaintext.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// An issued API token's **record** — everything about a token except the one
/// thing that would let someone use it. The plaintext exists exactly once, in
/// the response to the mint call; what survives here is
/// [`api_token_hash`] of it, held by the store rather than on this struct.
///
/// A token is not a second authorization model. It is a *narrowing* of an
/// existing [`Principal`]'s authority along two axes at once — down to one
/// [`Scope`], and down to a [`Role`] at or below what its owner holds there —
/// and everything downstream ([`Rbac`], tenancy scoping) is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiToken {
    /// Opaque row id. Safe to log, safe to show, safe to put in a revoke URL —
    /// it is not the credential.
    pub id: String,
    /// The human label chosen at mint ("demo keepalive", "laptop CLI"). This is
    /// what makes a token revocable in practice: nobody dares revoke a row they
    /// cannot identify.
    pub name: String,
    /// The [`Principal`] whose authority this token draws on. The token can
    /// never exceed what this subject holds *right now* — so offboarding a
    /// human disarms their tokens without anyone having to remember they
    /// existed.
    pub owner_subject: String,
    /// The single scope this token may act in. Org-scoped inherits down to the
    /// org's projects, exactly as a binding does; project-scoped covers only
    /// that project. There is no unscoped token: an untenanted resource sits in
    /// no scope for a token to cover, and a credential that can reach
    /// *everything* is the thing this design exists to avoid.
    pub scope: Scope,
    /// The **ceiling** the minter set, not the token's effective role. What the
    /// token can actually do is this capped by the owner's live authority — see
    /// [`ApiToken::effective_role`].
    pub role: Role,
    /// Unix-ms expiry. **Mandatory.** `values.yaml` already records why, about
    /// the workspace results token: it "carries no verb and never expires", and
    /// that pairing is exactly why it must never be reused for anything else.
    /// This credential carries a verb (`role`) and an expiry, both required, so
    /// that mistake is not made twice.
    pub expires_at: i64,
    /// Who minted it — kept separately from `owner_subject` so the audit
    /// question "who issued this?" survives even once the two can differ (a
    /// machine-owned token, should `Principal` grow that kind).
    pub created_by: String,
    /// Unix-ms mint time.
    pub created_at: i64,
    /// Unix-ms of the last request that presented this token, written back
    /// coarsely rather than on every request. A token whose last use nobody can
    /// see is a token nobody will ever dare revoke.
    pub last_used_at: Option<i64>,
    /// Unix-ms revocation. Set means dead, forever — revocation is why this is
    /// an opaque secret in a table rather than a self-describing JWT, which
    /// could not be withdrawn before its own expiry.
    pub revoked_at: Option<i64>,
}

impl ApiToken {
    /// Is this token still usable at `now_ms`? Revocation beats expiry beats
    /// everything; neither is a judgement call.
    pub fn is_live(&self, now_ms: i64) -> bool {
        self.revoked_at.is_none() && now_ms < self.expires_at
    }

    /// Does this token's scope reach `scope`? Org→Project inheritance only —
    /// the same direction [`Rbac::role_of`] applies, never upward. A token
    /// scoped to one Project cannot read a sibling's runs, which is the
    /// ADR-0049 cross-tenant leak re-asserted on this credential.
    pub fn covers(&self, scope: &Scope) -> bool {
        match &self.scope {
            Scope::Org(org) => scope.org() == org,
            Scope::Project { org, name } => {
                matches!(scope, Scope::Project { org: o, name: n } if o == org && n == name)
            }
        }
    }

    /// The role this token actually carries, given `live` — the role its owner
    /// holds in the requested scope *at this moment*.
    ///
    /// The minimum of the two, and `None` if the owner holds nothing. Both
    /// halves matter. The ceiling is what makes least-privilege possible: an
    /// Owner can issue a token that only dispatches one repo's pipeline, which
    /// the easy implementation — copying the minter's principal into the row,
    /// exactly as `sessions` denormalises it — could never express. The live
    /// half is what keeps that grant honest afterwards: demote the owner and
    /// every token they hold demotes with them, in the same instant, with no
    /// list to walk.
    pub fn effective_role(&self, live: Option<Role>) -> Option<Role> {
        live.map(|live| live.min(self.role))
    }
}

/// Durable store of issued API tokens (ADR-0049 amendment). Keyed on the
/// [`api_token_hash`] of the plaintext, so the store never holds a usable
/// credential either.
#[async_trait]
pub trait ApiTokenStore: Send + Sync {
    /// Record a freshly minted token. `hash` is [`api_token_hash`] of the
    /// plaintext the caller is about to return once and forget.
    async fn put(&self, token: &ApiToken, hash: &str) -> Result<(), IdentityError>;
    /// The token a presented plaintext hashes to, live or not — expiry and
    /// revocation are the caller's decision ([`ApiToken::is_live`]) so that
    /// "this token is revoked" stays distinguishable from "no such token" in
    /// the code, even though both answer 401 on the wire.
    async fn by_hash(&self, hash: &str) -> Result<Option<ApiToken>, IdentityError>;
    /// Every token issued within `org` — org- and project-scoped alike —
    /// newest first. Records only; there is no plaintext to return.
    async fn list(&self, org: &str) -> Result<Vec<ApiToken>, IdentityError>;
    /// Revoke `id` at `now_ms`. `false` if no such token, or it was already
    /// revoked — revocation is idempotent but not silent.
    async fn revoke(&self, id: &str, now_ms: i64) -> Result<bool, IdentityError>;
    /// Record a use. Called coarsely, and best-effort: a failed write here must
    /// never fail the request it was observing.
    async fn touch(&self, id: &str, now_ms: i64) -> Result<(), IdentityError>;
}

/// Claims to embed in a minted per-run OIDC JWT for keyless cloud federation
/// (ADR-0015, 0032). Minted per **attempt**, short-lived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// `iss` — the configured Scarab issuer URL.
    pub issuer: String,
    /// `sub` — `scarab:org/<org>/repo/<repo>/env/<env>/ref/<ref>` (see
    /// [`Claims::run_subject`]).
    pub subject: String,
    /// `aud` — configurable per cloud.
    pub audience: String,
    pub run_id: String,
    pub attempt: String,
    pub event: String,
    pub git_ref: String,
    pub sha: String,
    /// `exp` — unix-seconds expiry (short TTL).
    pub expires_at: i64,
}

impl Claims {
    /// The workload-identity subject a cloud's trust policy matches against
    /// (ADR-0015, 0032): `scarab:org/<org>/repo/<repo>/env/<env>/ref/<ref>`.
    pub fn run_subject(org: &str, repo: &str, env: &str, git_ref: &str) -> String {
        format!("scarab:org/{org}/repo/{repo}/env/{env}/ref/{git_ref}")
    }
}

/// A signed JSON Web Token (compact serialization).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwt(pub String);

/// Errors from identity operations.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("authentication failed")]
    AuthFailed,
    #[error("token issuance failed: {0}")]
    Issuance(String),
    #[error("access denied")]
    Denied,
}

/// Inbound login via OAuth / OIDC.
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Exchange an OAuth/OIDC credential (code or token) for a [`Principal`].
    async fn authenticate(&self, credential: &str) -> Result<Principal, IdentityError>;
}

/// Mints per-run JWTs for keyless federation.
#[async_trait]
pub trait OidcIssuer: Send + Sync {
    async fn issue(&self, claims: Claims) -> Result<Jwt, IdentityError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_capabilities_follow_the_hierarchy() {
        // Everyone reads.
        for r in [Role::Viewer, Role::Member, Role::Admin, Role::Owner] {
            assert!(r.allows(Action::Read));
        }
        // Write is Member and up.
        assert!(!Role::Viewer.allows(Action::Write));
        assert!(Role::Member.allows(Action::Write));
        assert!(Role::Owner.allows(Action::Write));
        // Administer is Admin and up.
        assert!(!Role::Member.allows(Action::Administer));
        assert!(Role::Admin.allows(Action::Administer));
        assert!(Role::Owner.allows(Action::Administer));
    }

    #[test]
    fn role_ordering_is_least_to_most_privileged() {
        assert!(Role::Viewer < Role::Member);
        assert!(Role::Member < Role::Admin);
        assert!(Role::Admin < Role::Owner);
    }

    #[test]
    fn rbac_resolves_highest_role_in_scope_and_decides() {
        let scope = Scope::Project {
            org: "acme".into(),
            name: "app".into(),
        };
        let other = Scope::Org("acme".into());
        let mut rbac = Rbac::default();
        rbac.grant("alice", scope.clone(), Role::Viewer);
        rbac.grant("alice", scope.clone(), Role::Admin); // highest wins
        rbac.grant("bob", scope.clone(), Role::Viewer);

        assert_eq!(rbac.role_of("alice", &scope), Some(Role::Admin));
        assert!(rbac.can("alice", &scope, Action::Write));
        assert!(rbac.can("alice", &scope, Action::Administer));

        // bob is only a Viewer here → may read but not write.
        assert!(rbac.can("bob", &scope, Action::Read));
        assert!(!rbac.can("bob", &scope, Action::Write));

        // A Project binding does NOT bubble up to the Org scope, and unknown
        // subjects are denied.
        assert_eq!(rbac.role_of("alice", &other), None);
        assert!(!rbac.can("carol", &scope, Action::Read));
    }

    #[test]
    fn org_role_inherits_down_to_its_projects_only() {
        let mut rbac = Rbac::default();
        rbac.grant("alice", Scope::Org("acme".into()), Role::Member);

        let app = Scope::Project {
            org: "acme".into(),
            name: "app".into(),
        };
        let foreign = Scope::Project {
            org: "evil".into(),
            name: "app".into(),
        };
        // The org role reaches every project of THAT org…
        assert_eq!(rbac.role_of("alice", &app), Some(Role::Member));
        assert!(rbac.can("alice", &app, Action::Write));
        // …and no project of any other org (cross-tenant denial).
        assert_eq!(rbac.role_of("alice", &foreign), None);
        assert!(!rbac.can("alice", &foreign, Action::Read));

        // A project binding maxes with the inherited org role.
        rbac.grant("alice", app.clone(), Role::Admin);
        assert_eq!(rbac.role_of("alice", &app), Some(Role::Admin));
    }

    #[test]
    fn principal_can_checks_any_held_role() {
        let p = Principal {
            subject: "alice".into(),
            display_name: None,
            roles: vec![Role::Viewer, Role::Member],
        };
        assert!(p.can(Action::Write)); // via Member
        assert!(!p.can(Action::Administer));
    }

    #[test]
    fn run_subject_encodes_org_repo_env_ref() {
        assert_eq!(
            Claims::run_subject("acme", "app", "prod", "refs/heads/main"),
            "scarab:org/acme/repo/app/env/prod/ref/refs/heads/main"
        );
    }

    #[test]
    fn session_validity_tracks_expiry() {
        let s = Session {
            id: "sid".into(),
            principal: Principal {
                subject: "alice".into(),
                display_name: None,
                roles: vec![Role::Owner],
            },
            expires_at: 1_000,
            csrf: String::new(),
        };
        assert!(s.is_valid(999));
        assert!(!s.is_valid(1_000));
        assert!(!s.is_valid(1_001));
    }

    #[test]
    fn a_token_hash_is_stable_and_never_the_plaintext() {
        let secret = "scarab_pat_0123456789abcdef";
        let hash = api_token_hash(secret);
        assert_eq!(hash, api_token_hash(secret), "hashing is deterministic");
        assert_eq!(hash.len(), 64, "sha-256 as hex");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            !hash.contains("scarab_pat"),
            "the plaintext must not survive"
        );
        assert_ne!(hash, api_token_hash("scarab_pat_0123456789abcdee"));
    }

    fn token(scope: Scope, role: Role) -> ApiToken {
        ApiToken {
            id: "t1".into(),
            name: "ci".into(),
            owner_subject: "amy".into(),
            scope,
            role,
            expires_at: 2_000,
            created_by: "amy".into(),
            created_at: 1_000,
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn a_token_scope_inherits_down_but_never_up_or_sideways() {
        let org = token(Scope::Org("acme".into()), Role::Member);
        assert!(org.covers(&Scope::Org("acme".into())));
        assert!(org.covers(&Scope::Project {
            org: "acme".into(),
            name: "web".into()
        }));
        assert!(!org.covers(&Scope::Org("other".into())));

        let project = token(
            Scope::Project {
                org: "acme".into(),
                name: "web".into(),
            },
            Role::Member,
        );
        assert!(project.covers(&Scope::Project {
            org: "acme".into(),
            name: "web".into()
        }));
        // The ADR-0049 cross-tenant leak, re-asserted on this credential.
        assert!(!project.covers(&Scope::Project {
            org: "acme".into(),
            name: "api".into()
        }));
        // ...and no climbing to the enclosing org, which would hand a
        // repo-scoped token the org's secrets and forge connections.
        assert!(!project.covers(&Scope::Org("acme".into())));
    }

    #[test]
    fn a_token_is_capped_by_its_owners_live_role_in_both_directions() {
        let t = token(Scope::Org("acme".into()), Role::Member);
        // The ceiling binds: an Owner's token still only writes.
        assert_eq!(t.effective_role(Some(Role::Owner)), Some(Role::Member));
        // ...and so does the live role: demote the owner and the token demotes.
        assert_eq!(t.effective_role(Some(Role::Viewer)), Some(Role::Viewer));
        // Offboard the owner entirely and the token holds nothing at all.
        assert_eq!(t.effective_role(None), None);
    }

    #[test]
    fn expiry_and_revocation_each_kill_a_token_on_their_own() {
        let t = token(Scope::Org("acme".into()), Role::Member);
        assert!(t.is_live(1_999));
        assert!(!t.is_live(2_000), "expiry is exclusive at the instant");
        assert!(!t.is_live(2_001));

        let revoked = ApiToken {
            revoked_at: Some(1_500),
            ..t
        };
        assert!(
            !revoked.is_live(1_400),
            "revocation kills it even before the stamp — the row is dead, not scheduled"
        );
    }
}
