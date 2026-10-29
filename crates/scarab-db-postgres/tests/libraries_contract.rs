//! `LibraryStore` parity contract (ADR-0076 §4-§6): one suite, run against
//! the in-memory fake (`scarab_testkit::InMemoryLibraries`, always) and the
//! Postgres adapter (a throwaway database per test, skipping cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset — see `common`).

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_policy::library::{
    content_hash, DefaultsState, LibraryStore, LibraryStoreError, LibraryVersion, LibraryYank,
    MissingDefault, NewInstallation, OrgLibraryDefaults, Replaces, TrustedSource,
};
use scarab_testkit::InMemoryLibraries;

macro_rules! contract_test {
    ($name:ident, $contract:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn in_memory() {
                $contract(&InMemoryLibraries::new()).await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(tdb) = fresh_db().await else {
                    eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                    return;
                };
                let db = PostgresDb::with_pool(tdb.pool.clone());
                db.migrate().await.unwrap();
                $contract(&db).await;
                tdb.cleanup().await;
            }
        }
    };
}

contract_test!(versions_are_kept_per_org_by_hash, versions);
contract_test!(install_upgrade_and_remove_check_what_they_replace, installs);
contract_test!(yanks_are_per_org_and_keep_the_first_reason, yanks);
contract_test!(defaults_are_claimed_once_per_org, defaults);
contract_test!(
    prune_deletes_the_oldest_versions_never_installed,
    prune_never_installed
);
contract_test!(trust_is_per_org_by_source_and_keeps_the_first_record, trust);

async fn trust(store: &dyn LibraryStore) {
    let t = |by: &str, at: i64| TrustedSource {
        org: "acme".into(),
        source: "github.com/acme/libs//push.yaml".into(),
        trusted_by: by.into(),
        trusted_at: at,
        step_up_ref: Some("su-1".into()),
    };
    assert!(store.trusted_sources("acme").await.unwrap().is_empty());
    assert_eq!(store.trust_source(&t("amy", 1)).await.unwrap(), t("amy", 1));
    // Trusting again keeps the first record.
    assert_eq!(store.trust_source(&t("bob", 2)).await.unwrap(), t("amy", 1));
    assert_eq!(store.trusted_sources("acme").await.unwrap(), vec![t("amy", 1)]);
    assert!(store.trusted_sources("globex").await.unwrap().is_empty(), "per org");
    assert_eq!(
        store
            .untrust_source("acme", "github.com/acme/libs//push.yaml")
            .await
            .unwrap(),
        Some(t("amy", 1))
    );
    assert_eq!(
        store
            .untrust_source("acme", "github.com/acme/libs//push.yaml")
            .await
            .unwrap(),
        None
    );
    assert!(store.trusted_sources("acme").await.unwrap().is_empty());
}

fn version(org: &str, content: &str, source: &str) -> LibraryVersion {
    LibraryVersion {
        org: org.into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source.into(),
        publisher: "acme".into(),
        name: "deploy".into(),
        first_party: false,
        fetched_by: "user:amy".into(),
        fetched_at: 1_000,
    }
}

fn install(org: &str, hash: &str, at: i64) -> NewInstallation {
    NewInstallation {
        org: org.into(),
        publisher: "acme".into(),
        name: "deploy".into(),
        content_hash: hash.into(),
        installed_by: "user:amy".into(),
        installed_at: at,
        step_up_ref: Some("su-1".into()),
    }
}

const V1: &str = "kind: library\nsteps: [{id: a, image: busybox}]\n";
const V2: &str = "kind: library\nsteps: [{id: b, image: busybox}]\n";

async fn versions(store: &dyn LibraryStore) {
    let v = version("acme", V1, "github.com/acme/libs//deploy.yaml@aaaa");
    let stored = store.put_version(&v).await.unwrap();
    assert_eq!(stored, v);
    // A second fetch of the same bytes keeps the first record.
    let again = LibraryVersion {
        fetched_by: "user:bob".into(),
        fetched_at: 2_000,
        ..v.clone()
    };
    assert_eq!(store.put_version(&again).await.unwrap(), v);
    // Per org: the same hash in another org is another row.
    assert_eq!(
        store.version("globex", &v.content_hash).await.unwrap(),
        None
    );
    assert_eq!(
        store.version("acme", &v.content_hash).await.unwrap(),
        Some(v)
    );
}

async fn installs(store: &dyn LibraryStore) {
    let v1 = store
        .put_version(&version(
            "acme",
            V1,
            "github.com/acme/libs//deploy.yaml@aaaa",
        ))
        .await
        .unwrap();
    let v2 = store
        .put_version(&version(
            "acme",
            V2,
            "github.com/acme/libs//deploy.yaml@bbbb",
        ))
        .await
        .unwrap();

    // An unfetched version cannot be installed.
    let err = store
        .install(&install("acme", &"f".repeat(64), 1), &Replaces::Nothing)
        .await
        .unwrap_err();
    assert!(
        matches!(err, LibraryStoreError::UnknownVersion(_)),
        "{err:?}"
    );
    // Nor a version of another org.
    let err = store
        .install(&install("globex", &v1.content_hash, 1), &Replaces::Nothing)
        .await
        .unwrap_err();
    assert!(
        matches!(err, LibraryStoreError::UnknownVersion(_)),
        "{err:?}"
    );

    // An upgrade from a version when nothing is installed is a conflict.
    assert_eq!(
        store
            .install(
                &install("acme", &v2.content_hash, 9),
                &Replaces::Version(v1.content_hash.clone())
            )
            .await
            .unwrap_err(),
        LibraryStoreError::Conflict
    );
    assert_eq!(
        store.installation("acme", "acme", "deploy").await.unwrap(),
        None
    );

    let i = store
        .install(&install("acme", &v1.content_hash, 10), &Replaces::Nothing)
        .await
        .unwrap();
    assert_eq!(i.content_hash, v1.content_hash);
    assert_eq!(i.source, v1.source);
    assert_eq!(i.qualified_name(), "acme/deploy");
    assert_eq!(i.step_up_ref.as_deref(), Some("su-1"));

    // A second first install is a conflict, as is an upgrade from a version
    // that is not the current one.
    assert_eq!(
        store
            .install(&install("acme", &v2.content_hash, 11), &Replaces::Nothing)
            .await
            .unwrap_err(),
        LibraryStoreError::Conflict
    );
    assert_eq!(
        store
            .install(
                &install("acme", &v2.content_hash, 11),
                &Replaces::Version(v2.content_hash.clone())
            )
            .await
            .unwrap_err(),
        LibraryStoreError::Conflict
    );
    // Neither conflict changed what is installed.
    assert_eq!(
        store.installation("acme", "acme", "deploy").await.unwrap(),
        Some(i.clone())
    );
    let up = store
        .install(
            &install("acme", &v2.content_hash, 12),
            &Replaces::Version(v1.content_hash.clone()),
        )
        .await
        .unwrap();
    assert_eq!(up.content_hash, v2.content_hash);
    assert_eq!(up.installed_at, 12);

    let compiled = store.installed_for_compile("acme").await.unwrap();
    assert_eq!(compiled.len(), 1);
    assert_eq!(compiled["acme/deploy"].content, V2);
    assert_eq!(compiled["acme/deploy"].source, v2.source);
    assert!(store
        .installed_for_compile("globex")
        .await
        .unwrap()
        .is_empty());

    assert_eq!(store.installations("acme").await.unwrap(), vec![up.clone()]);
    assert_eq!(
        store.remove("acme", "acme", "deploy").await.unwrap(),
        Some(up)
    );
    assert_eq!(store.remove("acme", "acme", "deploy").await.unwrap(), None);
    assert!(store.installations("acme").await.unwrap().is_empty());
    // The version stays, so Runs that used it still show what they ran.
    assert!(store
        .version("acme", &v1.content_hash)
        .await
        .unwrap()
        .is_some());
}

async fn yanks(store: &dyn LibraryStore) {
    let v1 = store.put_version(&version("acme", V1, "s1")).await.unwrap();
    store
        .put_version(&version("globex", V1, "s1"))
        .await
        .unwrap();
    let y = LibraryYank {
        org: "acme".into(),
        content_hash: v1.content_hash.clone(),
        reason: "pins a vulnerable image".into(),
        yanked_by: "user:amy".into(),
        yanked_at: 5,
    };
    assert_eq!(store.yank(&y).await.unwrap(), y);
    // A second yank keeps the first.
    let later = LibraryYank {
        reason: "other".into(),
        yanked_at: 6,
        ..y.clone()
    };
    assert_eq!(store.yank(&later).await.unwrap(), y);
    // A version this org never fetched cannot be yanked.
    let err = store
        .yank(&LibraryYank {
            content_hash: "e".repeat(64),
            ..y.clone()
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, LibraryStoreError::UnknownVersion(_)),
        "{err:?}"
    );

    let hashes = vec![v1.content_hash.clone(), "d".repeat(64)];
    assert_eq!(
        store.yanked("acme", &hashes).await.unwrap(),
        vec![y.clone()]
    );
    // A yank applies to one org.
    assert!(store.yanked("globex", &hashes).await.unwrap().is_empty());
    assert!(store.yanked("acme", &[]).await.unwrap().is_empty());
    assert_eq!(store.yanks("acme").await.unwrap(), vec![y.clone()]);

    assert_eq!(
        store.unyank("acme", &v1.content_hash).await.unwrap(),
        Some(y)
    );
    assert_eq!(store.unyank("acme", &v1.content_hash).await.unwrap(), None);
    assert!(store.yanked("acme", &hashes).await.unwrap().is_empty());
}

async fn defaults(store: &dyn LibraryStore) {
    assert_eq!(store.defaults("acme").await.unwrap(), None);
    assert!(store.claim_defaults("acme", 7).await.unwrap());
    assert!(!store.claim_defaults("acme", 8).await.unwrap());
    let d = store.defaults("acme").await.unwrap().unwrap();
    assert_eq!(d.state, DefaultsState::Pending);
    assert_eq!(d.updated_at, 7);

    let missing = OrgLibraryDefaults {
        org: "acme".into(),
        state: DefaultsState::Missing,
        missing: vec![MissingDefault {
            publisher: "thulasi-ram".into(),
            name: "clone".into(),
            error: "connection refused".into(),
        }],
        mirror: Some("https://git.example/mirror".into()),
        updated_at: 9,
    };
    store.set_defaults(&missing).await.unwrap();
    assert_eq!(store.defaults("acme").await.unwrap(), Some(missing));
    // A record set by hand also blocks a claim.
    assert!(!store.claim_defaults("acme", 10).await.unwrap());
}

async fn prune_never_installed(store: &dyn LibraryStore) {
    let content = |n: u32| format!("kind: library\nsteps: [{{id: s{n}, image: busybox}}]\n");
    let mut hashes = Vec::new();
    for n in 1..=5 {
        let mut v = version(
            "acme",
            &content(n),
            "github.com/acme/libs//deploy.yaml@aaaa",
        );
        v.fetched_at = i64::from(n);
        hashes.push(store.put_version(&v).await.unwrap().content_hash);
    }
    let other = store
        .put_version(&version(
            "globex",
            &content(1),
            "github.com/globex/libs//x.yaml@aaaa",
        ))
        .await
        .unwrap();
    // 1: installed once, then removed. 2: yanked. 3: just fetched again.
    store
        .install(&install("acme", &hashes[0], 10), &Replaces::Nothing)
        .await
        .unwrap();
    store.remove("acme", "acme", "deploy").await.unwrap();
    store
        .yank(&LibraryYank {
            org: "acme".into(),
            content_hash: hashes[1].clone(),
            reason: "r".into(),
            yanked_by: "amy".into(),
            yanked_at: 11,
        })
        .await
        .unwrap();

    // Never installed and not yanked: 3, 4, 5. Keep the newest one besides
    // 3, which is 5; 4 goes.
    assert_eq!(
        store
            .prune_never_installed("acme", 1, &hashes[2])
            .await
            .unwrap(),
        1
    );
    let mut left = Vec::new();
    for (n, h) in hashes.iter().enumerate() {
        if store.version("acme", h).await.unwrap().is_some() {
            left.push(n + 1);
        }
    }
    assert_eq!(left, vec![1, 2, 3, 5]);
    assert!(store
        .version("globex", &other.content_hash)
        .await
        .unwrap()
        .is_some());
    // An installed version is never pruned, even with nothing kept.
    store
        .install(&install("acme", &hashes[4], 12), &Replaces::Nothing)
        .await
        .unwrap();
    assert_eq!(
        store
            .prune_never_installed("acme", 0, &"0".repeat(64))
            .await
            .unwrap(),
        1
    );
    assert!(store.version("acme", &hashes[2]).await.unwrap().is_none());
    assert!(store.version("acme", &hashes[4]).await.unwrap().is_some());
}
