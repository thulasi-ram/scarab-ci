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
    async fn grant(
        &self,
        binding: &Binding,
        origin: BindingOrigin,
    ) -> Result<(), IdentityError>;
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

        let app = Scope::Project { org: "acme".into(), name: "app".into() };
        let foreign = Scope::Project { org: "evil".into(), name: "app".into() };
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
}
