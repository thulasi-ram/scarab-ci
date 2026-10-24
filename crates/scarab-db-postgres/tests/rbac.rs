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

/// ADR-0069 §5 re-sync, in SQL: seeds never clobber a native row; a prune
/// removes only this scope's import rows seeded through THIS provider — or
/// before provenance existed — that the listing no longer names; another
/// provider's import rows, other scopes, and native rows are untouched.
#[tokio::test]
async fn import_seeds_prune_only_what_this_provider_seeded_here() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let app = project("acme", "app");
    let other = project("acme", "api");

    // State before the re-sync:
    // - `legacy`: an import row from before provenance (imported_via NULL);
    // - `native`: a native grant; `tomb`: a native revoke;
    // - `okta-only`: seeded through ANOTHER provider;
    // - `elsewhere`: this provider, but another project.
    db.grant(
        &binding("legacy", app.clone(), Role::Member),
        BindingOrigin::Import,
    )
    .await
    .unwrap();
    db.grant(
        &binding("native", app.clone(), Role::Viewer),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.revoke("tomb", &app).await.unwrap();
    db.import_seeds(&app, "okta", &[("okta-only".into(), Role::Viewer)], false)
        .await
        .unwrap();
    db.import_seeds(
        &other,
        "github",
        &[("elsewhere".into(), Role::Admin)],
        false,
    )
    .await
    .unwrap();

    // The re-sync lists `stays`, plus `native` and `tomb` as admins.
    let pruned = db
        .import_seeds(
            &app,
            "github",
            &[
                ("stays".into(), Role::Member),
                ("native".into(), Role::Admin),
                ("tomb".into(), Role::Admin),
            ],
            true,
        )
        .await
        .unwrap();
    assert_eq!(pruned, 1, "only `legacy` goes");
    assert_eq!(db.role_of("legacy", &app).await.unwrap(), None);
    assert_eq!(db.role_of("stays", &app).await.unwrap(), Some(Role::Member));
    assert_eq!(
        db.role_of("native", &app).await.unwrap(),
        Some(Role::Viewer),
        "a native grant is never raised by an import"
    );
    assert_eq!(
        db.role_of("tomb", &app).await.unwrap(),
        None,
        "a native revoke is never resurrected"
    );
    assert_eq!(
        db.role_of("okta-only", &app).await.unwrap(),
        Some(Role::Viewer)
    );
    assert_eq!(
        db.role_of("elsewhere", &other).await.unwrap(),
        Some(Role::Admin)
    );

    // The next re-sync prunes `stays` once the forge drops it — its row now
    // carries this provider's provenance.
    let pruned = db.import_seeds(&app, "github", &[], true).await.unwrap();
    assert_eq!(pruned, 1);
    assert_eq!(db.role_of("stays", &app).await.unwrap(), None);
    tdb.cleanup().await;
}

/// ADR-0073's coarse role skip reads the highest role a subject holds
/// ANYWHERE: every Org and Project counts, a tombstone grants nothing.
#[tokio::test]
async fn max_role_is_the_highest_live_role_in_any_scope() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    assert_eq!(db.max_role("amy").await.unwrap(), None);
    db.grant(
        &binding("amy", Scope::Org("acme".into()), Role::Viewer),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.grant(
        &binding("amy", project("globex", "web"), Role::Admin),
        BindingOrigin::Native,
    )
    .await
    .unwrap();
    db.grant(
        &binding("amy", Scope::Org("initech".into()), Role::Member),
        BindingOrigin::Import,
    )
    .await
    .unwrap();
    // Text order would pick "viewer"; the role order picks Admin.
    assert_eq!(db.max_role("amy").await.unwrap(), Some(Role::Admin));

    db.revoke("amy", &project("globex", "web")).await.unwrap();
    assert_eq!(db.max_role("amy").await.unwrap(), Some(Role::Member));
    assert_eq!(db.max_role("bob").await.unwrap(), None);

    tdb.cleanup().await;
}
