//! PG role-binding store (ADR-0049 C2) against *real* Postgres: native grants
//! are authoritative, imports only seed/refresh import-owned rows, a native
//! revoke is a tombstone imports cannot resurrect, and role resolution
//! applies Org→Project inheritance. Skips without `SCARAB_TEST_DATABASE_URL`.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{Binding, BindingOrigin, RbacStore, Role, Scope};

fn project(org: &str, name: &str) -> Scope {
    Scope::Project {
        org: org.into(),
        name: name.into(),
    }
}

fn binding(subject: &str, scope: Scope, role: Role) -> Binding {
    Binding {
        subject: subject.into(),
        scope,
        role,
    }
}

#[tokio::test]
async fn scoped_roles_inherit_and_origins_respect_authority() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let app = project("acme", "app");
    let web = project("acme", "web");
    let foreign = project("evil", "app");

    // Org role inherits down to the org's projects — and no further.
    db.grant(
        &binding("alice", Scope::Org("acme".into()), Role::Member),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("alice", &app).await.unwrap(), Some(Role::Member));
    assert_eq!(db.role_of("alice", &web).await.unwrap(), Some(Role::Member));
    assert_eq!(db.role_of("alice", &foreign).await.unwrap(), None);

    // A project grant maxes with the inherited org role.
    db.grant(
        &binding("alice", app.clone(), Role::Admin),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("alice", &app).await.unwrap(), Some(Role::Admin));
    assert_eq!(db.role_of("alice", &web).await.unwrap(), Some(Role::Member));

    // Import seeds a fresh subject…
    db.grant(
        &binding("bob", app.clone(), Role::Member),
        BindingOrigin::Import,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("bob", &app).await.unwrap(), Some(Role::Member));
    // …and a re-sync refreshes import-owned rows…
    db.grant(
        &binding("bob", app.clone(), Role::Viewer),
        BindingOrigin::Import,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("bob", &app).await.unwrap(), Some(Role::Viewer));
    // …but NEVER clobbers a native grant.
    db.grant(
        &binding("bob", app.clone(), Role::Admin),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.grant(
        &binding("bob", app.clone(), Role::Viewer),
        BindingOrigin::Import,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("bob", &app).await.unwrap(), Some(Role::Admin));

    // A native revoke is a tombstone: the role is gone AND a later import
    // cannot resurrect it.
    db.revoke("bob", &app).await.unwrap();
    assert_eq!(db.role_of("bob", &app).await.unwrap(), None);
    db.grant(
        &binding("bob", app.clone(), Role::Member),
        BindingOrigin::Import,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("bob", &app).await.unwrap(), None);
    // A native grant clears the tombstone.
    db.grant(
        &binding("bob", app.clone(), Role::Viewer),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    assert_eq!(db.role_of("bob", &app).await.unwrap(), Some(Role::Viewer));

    // bindings() lists live grants of the org (tombstones excluded).
    let listed = db.bindings("acme").await.unwrap();
    let subjects: Vec<(&str, Option<&str>)> = listed
        .iter()
        .map(|b| {
            (
                b.subject.as_str(),
                match &b.scope {
                    Scope::Org(_) => None,
                    Scope::Project { name, .. } => Some(name.as_str()),
                },
            )
        })
        .collect();
    assert!(subjects.contains(&("alice", None)));
    assert!(subjects.contains(&("alice", Some("app"))));
    assert!(subjects.contains(&("bob", Some("app"))));
    assert!(listed.iter().all(|b| b.scope.org() == "acme"));

    tdb.cleanup().await;
}
