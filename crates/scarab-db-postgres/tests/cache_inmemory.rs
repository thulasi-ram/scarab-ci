//! ADR-0065 s1 — the keyed directory Cache's launch-time key resolution,
//! driven over the in-memory store with a fake snapshot oracle (DST): the
//! scheduler folds the key from the Run's cache scope and the key files' blob
//! hashes through the merged input roots (last-overlay-wins), fills the
//! restore hints from the mapping, and — the licence for the whole feature —
//! every failure mode DISABLES the cache for the attempt instead of failing or
//! mis-keying it.
//!
//! Cache scopes (git-bug efcec77): an Environment-governed Run reads and
//! writes only `env:<name>`; any other Run writes only its ref's scope and
//! reads the default branch's on a miss. The scopes here come from
//! [`CacheScope::for_run`], the derivation the server stamps at creation
//! (`scarab-server/tests/cache_scope.rs` proves the stamping).

use std::collections::HashMap;
use std::sync::Mutex;

use scarab_engine::ports::ExecState;
use scarab_engine::{
    cache_key, rerun_step, CacheConfig, CacheResolution, CacheScope, Db, RunId, RunStatus,
    Scheduler, StepId, StepSpec, StepStatus, Timestamp, WorkspaceSnapshots,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

/// A fake [`WorkspaceSnapshots`]: `(root, path) → blob hash`, plus an
/// error switch for the transient-failure arm.
#[derive(Default)]
struct FakeOracle {
    files: HashMap<(String, String), String>,
    fail: Mutex<bool>,
}

impl FakeOracle {
    fn with(files: &[(&str, &str, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(root, path, hash)| ((root.to_string(), path.to_string()), hash.to_string()))
                .collect(),
            fail: Mutex::new(false),
        }
    }
}

#[async_trait::async_trait]
impl WorkspaceSnapshots for FakeOracle {
    async fn snapshot_present(&self, _root: &str) -> bool {
        true
    }
    async fn file_blob_hash(&self, root: &str, path: &str) -> Result<Option<String>, String> {
        if *self.fail.lock().unwrap() {
            return Err("injected store blip".to_string());
        }
        Ok(self
            .files
            .get(&(root.to_string(), path.to_string()))
            .cloned())
    }
}

fn spec(cache: Option<CacheConfig>) -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache,
        forge_token: None,
        sandbox: None,
        registry_auth: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        resolved_secrets: Default::default(),
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

fn caching(dirs: &[&str], key_files: &[&str]) -> CacheConfig {
    CacheConfig {
        dirs: dirs.iter().map(|s| s.to_string()).collect(),
        key_files: key_files.iter().map(|s| s.to_string()).collect(),
        key: None,
        restore: Vec::new(),
        fallback_key: None,
        approved: None,
    }
}

/// The scope of an ungoverned push to `main`, the default branch.
fn main_scope() -> CacheScope {
    CacheScope::for_run(None, Some("refs/heads/main"), Some("main"), false).unwrap()
}

/// Two steps: `clone` (produces the input snapshot) → `build` (declares the
/// cache). Returns the run and the ids.
async fn seed(db: &InMemoryDb, cache: CacheConfig, project: Option<&str>) -> (RunId, StepId) {
    seed_run(db, "run-1", cache, project, Some(main_scope())).await
}

/// [`seed`] for a named Run with an explicit cache scope (`None` = a Run
/// stamped with no scope).
async fn seed_run(
    db: &InMemoryDb,
    id: &str,
    cache: CacheConfig,
    project: Option<&str>,
    scope: Option<CacheScope>,
) -> (RunId, StepId) {
    let run = RunId(id.into());
    let clone = StepId("clone".into());
    let build = StepId("build".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    if let Some(project) = project {
        db.set_run_scheduling(&run, project, 0).await.unwrap();
    }
    if let Some(scope) = scope {
        db.set_run_cache_scope(&run, &scope).await.unwrap();
    }
    db.create_step_run(&run, &clone, Some(&spec(None)), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &build,
        Some(&spec(Some(cache))),
        std::slice::from_ref(&clone),
        Timestamp(0),
    )
    .await
    .unwrap();
    (run, build)
}

async fn drive(db: &InMemoryDb, run: &RunId, exec: &FakeExecutor, oracle: &dyn WorkspaceSnapshots) {
    let clock = FakeClock::new(1_000);
    let sched = Scheduler::new(db, &clock, exec, "sched-1").with_snapshots(Some(oracle));
    for _ in 0..6 {
        sched.tick(run).await.expect("tick");
    }
    assert_eq!(
        db.run_status(run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
}

/// The happy path: the key folds from the key file's blob hash (as resolved
/// through the input snapshot), and a recorded mapping row for that exact
/// `(project, key, dir)` becomes a restore hint on the launched spec.
///
/// Mutations killed: skip the enrichment (key stays `None`); look the key up
/// unsalted by project (the recorded row would still be found under a
/// different project — see the isolation test); restore dirs the step never
/// declared (the undeclared row must not ride along).
#[tokio::test]
async fn launch_resolves_the_key_and_restore_hints_through_the_oracle() {
    let db = InMemoryDb::new();
    let (run, build) = seed(
        &db,
        caching(&["node_modules"], &["package-lock.json"]),
        Some("acme/web"),
    )
    .await;
    let oracle = FakeOracle::with(&[("root-clone", "package-lock.json", "blob-lock")]);

    let expected_key = cache_key(
        "acme/web",
        "refs/heads/main",
        &[("package-lock.json".to_string(), "blob-lock".to_string())],
    );
    // A prior save recorded the mapping — plus a row for a dir this step does
    // NOT declare, which must not be restored.
    db.cache_record(
        "acme/web",
        &expected_key,
        "node_modules",
        "tree-warm",
        &run,
        &build,
        &scarab_engine::AttemptId("a0".into()),
        Timestamp(0),
    )
    .await
    .unwrap();
    db.cache_record(
        "acme/web",
        &expected_key,
        "target",
        "tree-other",
        &run,
        &build,
        &scarab_engine::AttemptId("a0".into()),
        Timestamp(0),
    )
    .await
    .unwrap();

    let exec = FakeExecutor::new();
    exec.set_output("clone", "root-clone");
    for _ in 0..2 {
        exec.script_outcome(ExecState::Succeeded);
    }
    drive(&db, &run, &exec, &oracle).await;

    let handle = scarab_engine::ports::ExecHandle("fake://run-1/build/a1".into());
    let launched = exec.launched_spec(&handle).expect("build launched");
    let cache = launched.cache.expect("cache enrichment survived launch");
    assert_eq!(cache.key.as_deref(), Some(expected_key.as_str()));
    assert_eq!(
        cache.restore,
        vec![("node_modules".to_string(), "tree-warm".to_string())],
        "exactly the declared dir's row restores — the undeclared `target` row must not"
    );
}

/// Last-overlay-wins: when two input roots both carry the key file, the LAST
/// root in merge order owns it — the same rule the workspace materialisation
/// applies — so the key folds over the later overlay's hash.
#[tokio::test]
async fn the_key_resolves_last_overlay_wins_across_merged_inputs() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    let a = StepId("a".into());
    let b = StepId("b".into());
    let build = StepId("build".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.set_run_scheduling(&run, "acme/web", 0).await.unwrap();
    db.set_run_cache_scope(&run, &main_scope()).await.unwrap();
    for id in [&a, &b] {
        db.create_step_run(&run, id, Some(&spec(None)), &[], Timestamp(0))
            .await
            .unwrap();
    }
    db.create_step_run(
        &run,
        &build,
        Some(&spec(Some(caching(&["node_modules"], &["lock"])))),
        &[a.clone(), b.clone()],
        Timestamp(0),
    )
    .await
    .unwrap();

    let oracle = FakeOracle::with(&[
        ("root-a", "lock", "blob-early"),
        ("root-b", "lock", "blob-late"),
    ]);
    let exec = FakeExecutor::new();
    exec.set_output("a", "root-a");
    exec.set_output("b", "root-b");
    for _ in 0..3 {
        exec.script_outcome(ExecState::Succeeded);
    }
    drive(&db, &run, &exec, &oracle).await;

    let handle = scarab_engine::ports::ExecHandle("fake://run-1/build/a1".into());
    let cache = exec
        .launched_spec(&handle)
        .and_then(|s| s.cache)
        .expect("cache resolved");
    assert_eq!(
        cache.key.as_deref(),
        Some(
            cache_key(
                "acme/web",
                "refs/heads/main",
                &[("lock".to_string(), "blob-late".to_string())]
            )
            .as_str()
        ),
        "the LAST overlay's blob hash owns the key file"
    );
}

/// The disable arms, each of which must launch the step normally with the
/// cache OFF (`key: None`, no restore) — never an error, never a mis-key:
/// a key file absent from every input; a transient oracle failure; an
/// untenanted run (no project namespace); a Run with no cache scope (no ref
/// and no Environment, or created before scopes were stored).
#[tokio::test]
async fn every_unresolvable_key_disables_the_cache_and_never_fails_the_step() {
    for (name, project, scope, files, fail) in [
        (
            "key file absent",
            Some("acme/web"),
            Some(main_scope()),
            vec![],
            false,
        ),
        (
            "oracle transient error",
            Some("acme/web"),
            Some(main_scope()),
            vec![("root-clone", "package-lock.json", "blob-lock")],
            true,
        ),
        (
            "untenanted run",
            None,
            Some(main_scope()),
            vec![("root-clone", "package-lock.json", "blob-lock")],
            false,
        ),
        (
            "no cache scope",
            Some("acme/web"),
            None,
            vec![("root-clone", "package-lock.json", "blob-lock")],
            false,
        ),
    ] {
        let db = InMemoryDb::new();
        let (run, _build) = seed_run(
            &db,
            "run-1",
            caching(&["node_modules"], &["package-lock.json"]),
            project,
            scope,
        )
        .await;
        let oracle = FakeOracle::with(&files);
        *oracle.fail.lock().unwrap() = fail;

        let exec = FakeExecutor::new();
        exec.set_output("clone", "root-clone");
        for _ in 0..2 {
            exec.script_outcome(ExecState::Succeeded);
        }
        drive(&db, &run, &exec, &oracle).await;

        let handle = scarab_engine::ports::ExecHandle("fake://run-1/build/a1".into());
        let cache = exec
            .launched_spec(&handle)
            .and_then(|s| s.cache)
            .unwrap_or_else(|| panic!("{name}: the authored config still rides the spec"));
        assert_eq!(cache.key, None, "{name}: the cache must be disabled");
        assert!(cache.restore.is_empty(), "{name}: nothing may restore");
        let steps = db.steps_of_run(&run).await.unwrap();
        let build_status = steps
            .iter()
            .find(|s| s.step.0 == "build")
            .map(|s| s.status)
            .expect("build step exists");
        assert_eq!(
            build_status,
            StepStatus::Succeeded,
            "{name}: the step itself is untouched"
        );
    }
}

/// Tenancy isolation (dbe05e5 mandatory test), at the mapping grain: project
/// B never resolves project A's rows — even asking with the very same key
/// string. Belt: the lookup is project-scoped. Braces: the key itself is
/// project-salted (see the key test below), so B cannot even NAME A's key
/// from identical lockfiles.
#[tokio::test]
async fn project_b_never_resolves_project_as_cache_rows() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    let step = StepId("build".into());
    let attempt = scarab_engine::AttemptId("a1".into());
    db.cache_record(
        "acme/web",
        "key-1",
        "node_modules",
        "tree-a",
        &run,
        &step,
        &attempt,
        Timestamp(1),
    )
    .await
    .unwrap();

    assert_eq!(
        db.cache_lookup("acme/web", "key-1").await.unwrap(),
        vec![("node_modules".to_string(), "tree-a".to_string())]
    );
    assert!(
        db.cache_lookup("evil/web", "key-1")
            .await
            .unwrap()
            .is_empty(),
        "another project must not resolve the row, same key string or not"
    );
}

/// One Run of the two-step pipeline under `scope`: `build` launches (with
/// whatever restore the mapping gives it) and, on success, saves
/// `node_modules` as `saved_root` under the key its launch carried. Returns
/// the launched cache config.
async fn run_once(db: &InMemoryDb, id: &str, scope: CacheScope, saved_root: &str) -> CacheConfig {
    let (run, _build) = seed_run(
        db,
        id,
        caching(&["node_modules"], &["package-lock.json"]),
        Some("acme/web"),
        Some(scope),
    )
    .await;
    let oracle = FakeOracle::with(&[("root-clone", "package-lock.json", "blob-lock")]);
    let exec = FakeExecutor::new();
    exec.set_output("clone", "root-clone");
    exec.set_cache_save_roots("build", &[("node_modules", saved_root)]);
    for _ in 0..2 {
        exec.script_outcome(ExecState::Succeeded);
    }
    drive(db, &run, &exec, &oracle).await;
    let handle = scarab_engine::ports::ExecHandle(format!("fake://{id}/build/a1"));
    exec.launched_spec(&handle)
        .and_then(|s| s.cache)
        .expect("build launched with its cache config")
}

fn lock() -> Vec<(String, String)> {
    vec![("package-lock.json".to_string(), "blob-lock".to_string())]
}

/// The `node_modules` row saved under `scope`'s key, if any.
async fn saved(db: &InMemoryDb, scope: &str) -> Option<String> {
    db.cache_lookup("acme/web", &cache_key("acme/web", scope, &lock()))
        .await
        .unwrap()
        .into_iter()
        .find(|(dir, _)| dir == "node_modules")
        .map(|(_, root)| root)
}

/// The poisoning this ticket closes (git-bug efcec77): a pull request whose
/// own YAML names the Environment (so the Environment governs it) saves its
/// Cache, and an Environment Run on `main` with the same key files does not
/// restore it. The PR's save lands in its own ref scope (it is recorded, so
/// the miss below is the scope working, not a save that never happened).
#[tokio::test]
async fn a_pull_request_naming_the_environment_never_writes_what_an_environment_run_restores() {
    let db = InMemoryDb::new();
    let pr =
        CacheScope::for_run(Some("prod"), Some("refs/pull/7/head"), Some("main"), true).unwrap();
    run_once(&db, "run-pr", pr, "tree-from-pr").await;
    assert_eq!(
        saved(&db, "refs/pull/7/head").await.as_deref(),
        Some("tree-from-pr"),
        "a pull request saves under its own ref scope"
    );
    assert_eq!(
        saved(&db, "env:prod").await,
        None,
        "and never under the Environment's"
    );
    assert_eq!(saved(&db, "refs/heads/main").await, None, "or main's");

    // The Environment's resolution, which a Step the approved gate admits
    // launches with (git-bug 31a2533), restores nothing the PR wrote.
    let prod =
        CacheScope::for_run(Some("prod"), Some("refs/heads/main"), Some("main"), false).unwrap();
    let cache = run_once(&db, "run-prod", prod, "tree-from-prod").await;
    assert_eq!(
        cache.approved,
        Some(CacheResolution {
            key: cache_key("acme/web", "env:prod", &lock()),
            restore: vec![],
        }),
        "the Environment Run must not restore the PR's entry"
    );
}

/// A feature-branch Run with no entry of its own restores the default
/// branch's, reads it from that key (`fallback_key`, which picks the Depot
/// replica), and saves under its own scope only: `main`'s row still holds
/// `main`'s tree afterwards. Its next Run hits its own scope, not `main`'s.
#[tokio::test]
async fn a_feature_branch_run_restores_the_default_branch_and_saves_under_its_own_scope() {
    let db = InMemoryDb::new();
    run_once(&db, "run-main", main_scope(), "tree-from-main").await;

    let feature = CacheScope::for_run(None, Some("refs/heads/feat"), Some("main"), false).unwrap();
    let cache = run_once(&db, "run-feat-1", feature.clone(), "tree-from-feat").await;
    assert_eq!(
        cache.restore,
        vec![("node_modules".to_string(), "tree-from-main".to_string())],
        "a miss in the Run's own scope falls back to the default branch's"
    );
    let main_key = cache_key("acme/web", "refs/heads/main", &lock());
    let feat_key = cache_key("acme/web", "refs/heads/feat", &lock());
    assert_eq!(cache.fallback_key.as_deref(), Some(main_key.as_str()));
    assert_eq!(
        cache.key.as_deref(),
        Some(feat_key.as_str()),
        "saves go to its own key"
    );
    assert_eq!(
        saved(&db, "refs/heads/main").await.as_deref(),
        Some("tree-from-main"),
        "a fallback hit is never saved back under the default branch"
    );
    assert_eq!(
        saved(&db, "refs/heads/feat").await.as_deref(),
        Some("tree-from-feat")
    );

    let cache = run_once(&db, "run-feat-2", feature, "tree-from-feat-2").await;
    assert_eq!(
        cache.restore,
        vec![("node_modules".to_string(), "tree-from-feat".to_string())],
        "the Run's own scope wins once it has an entry"
    );
    assert_eq!(cache.fallback_key, None);
}

/// The Environment's scope does not fall back: with `main` holding an entry
/// for the same key files, a Step the approved gate admits restores nothing.
/// A Step of the same Run before the gate uses `main`'s scope, its ref's,
/// and restores that entry (git-bug 31a2533). Without a launch decision the
/// scheduler hands the ref's resolution by default; the Environment's rides
/// beside it.
#[tokio::test]
async fn the_environment_scope_does_not_fall_back_to_the_default_branch() {
    let db = InMemoryDb::new();
    run_once(&db, "run-main", main_scope(), "tree-from-main").await;

    let prod =
        CacheScope::for_run(Some("prod"), Some("refs/heads/main"), Some("main"), false).unwrap();
    let cache = run_once(&db, "run-prod", prod, "tree-from-prod").await;
    assert_eq!(
        cache.key.as_deref(),
        Some(cache_key("acme/web", "refs/heads/main", &lock()).as_str())
    );
    assert_eq!(
        cache.restore,
        vec![("node_modules".to_string(), "tree-from-main".to_string())]
    );
    assert_eq!(
        cache.approved,
        Some(CacheResolution {
            key: cache_key("acme/web", "env:prod", &lock()),
            restore: vec![],
        })
    );
    assert_eq!(saved(&db, "env:prod").await, None, "nothing wrote env:prod");
}

/// A rerun keys its Cache with the scope its Run was created with: the
/// rerun attempt folds the same key as the first, restores the first
/// attempt's save from the Run's own scope, and saves there again.
#[tokio::test]
async fn a_rerun_keeps_the_scope_its_run_started_with() {
    let db = InMemoryDb::new();
    let feature = CacheScope::for_run(None, Some("refs/heads/feat"), Some("main"), false).unwrap();
    let (run, build) = seed_run(
        &db,
        "run-feat",
        caching(&["node_modules"], &["package-lock.json"]),
        Some("acme/web"),
        Some(feature),
    )
    .await;
    let oracle = FakeOracle::with(&[("root-clone", "package-lock.json", "blob-lock")]);
    let exec = FakeExecutor::new();
    exec.set_output("clone", "root-clone");
    exec.set_cache_save_roots("build", &[("node_modules", "tree-a1")]);
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    drive(&db, &run, &exec, &oracle).await;

    let clock = FakeClock::new(2_000);
    rerun_step(&db, &clock, &run, &build, None)
        .await
        .expect("rerun");
    drive(&db, &run, &exec, &oracle).await;

    let feat_key = cache_key("acme/web", "refs/heads/feat", &lock());
    let first = exec
        .launched_spec(&scarab_engine::ports::ExecHandle(
            "fake://run-feat/build/a1".into(),
        ))
        .and_then(|s| s.cache)
        .expect("first attempt");
    let rerun = exec
        .launched_spec(&scarab_engine::ports::ExecHandle(
            "fake://run-feat/build/a2".into(),
        ))
        .and_then(|s| s.cache)
        .expect("rerun attempt");
    assert_eq!(first.key.as_deref(), Some(feat_key.as_str()));
    assert_eq!(
        rerun.key, first.key,
        "the rerun folds the same scope into its key"
    );
    assert_eq!(
        rerun.restore,
        vec![("node_modules".to_string(), "tree-a1".to_string())],
        "and restores the first attempt's save from the Run's own scope"
    );
    assert_eq!(rerun.fallback_key, None);
}

/// The scope a Run gets from its Environment, ref and default branch.
#[test]
fn the_cache_scope_derives_from_the_ref_with_the_environment_beside_it() {
    let scope = |env, git_ref, default| CacheScope::for_run(env, git_ref, default, false);
    let s = |own: &str, fallback: Option<&str>| {
        Some(CacheScope {
            own: Some(own.into()),
            fallback: fallback.map(str::to_string),
            environment: None,
        })
    };
    // An Environment's scope sits beside the ref's (git-bug 31a2533): a Step
    // uses it only once an approved approval gate admits it, and every other
    // Step uses the ref's scope and its fallback.
    assert_eq!(
        scope(Some("prod"), Some("refs/heads/feat"), Some("main")),
        Some(CacheScope {
            own: Some("refs/heads/feat".into()),
            fallback: Some("refs/heads/main".into()),
            environment: Some("env:prod".into()),
        })
    );
    // No ref: only the Environment's scope; Steps before the gate get no
    // Cache.
    assert_eq!(
        scope(Some("prod"), None, None),
        Some(CacheScope {
            own: None,
            fallback: None,
            environment: Some("env:prod".into()),
        })
    );
    // A pull request never gets the Environment's scope: not on a pull or
    // merge request ref, and not when the event is a pull request or a
    // locked-out fork (`pull_request`). It is scoped by its ref instead.
    assert_eq!(
        scope(Some("prod"), Some("refs/pull/7/head"), Some("main")),
        s("refs/pull/7/head", Some("refs/heads/main"))
    );
    assert_eq!(
        scope(
            Some("prod"),
            Some("refs/merge-requests/7/head"),
            Some("main")
        ),
        s("refs/merge-requests/7/head", Some("refs/heads/main"))
    );
    assert_eq!(
        CacheScope::for_run(Some("prod"), Some("refs/heads/feat"), Some("main"), true),
        s("refs/heads/feat", Some("refs/heads/main"))
    );
    assert_eq!(
        CacheScope::for_run(Some("prod"), None, Some("main"), true),
        None
    );
    // A ref reads its own scope, then the default branch's.
    assert_eq!(
        scope(None, Some("refs/pull/7/head"), Some("main")),
        s("refs/pull/7/head", Some("refs/heads/main"))
    );
    assert_eq!(
        scope(None, Some("refs/tags/v1"), Some("trunk")),
        s("refs/tags/v1", Some("refs/heads/trunk"))
    );
    // The default branch itself, or an unknown default branch: no fallback.
    assert_eq!(
        scope(None, Some("refs/heads/main"), Some("main")),
        s("refs/heads/main", None)
    );
    assert_eq!(
        scope(None, Some("refs/heads/feat"), None),
        s("refs/heads/feat", None)
    );
    assert_eq!(
        scope(None, Some("refs/heads/feat"), Some("")),
        s("refs/heads/feat", None)
    );
    // No ref, or a ref that is not a full `refs/…` name (which could spell
    // `env:prod`): no scope, so the Cache is off.
    assert_eq!(scope(None, None, Some("main")), None);
    assert_eq!(scope(None, Some("env:prod"), Some("main")), None);
}

/// The key is project-salted AND the lookup is project-scoped: the same key
/// files resolve to different keys for different projects, so tenant B can
/// never see tenant A's rows even by recording identical lockfiles.
#[test]
fn the_cache_key_is_project_salted_and_deterministic() {
    let pairs = vec![("lock".to_string(), "blob".to_string())];
    let a = cache_key("acme/web", "refs/heads/main", &pairs);
    let b = cache_key("evil/web", "refs/heads/main", &pairs);
    assert_ne!(
        a, b,
        "two tenants with identical lockfiles fold different keys"
    );
    assert_eq!(
        a,
        cache_key("acme/web", "refs/heads/main", &pairs),
        "deterministic"
    );
    // Two scopes of one project with identical lockfiles fold different
    // keys (git-bug efcec77).
    assert_ne!(a, cache_key("acme/web", "env:prod", &pairs));
    assert_ne!(a, cache_key("acme/web", "refs/pull/7/head", &pairs));
    // Length-prefixed: moving a boundary between project and scope must
    // not collide.
    assert_ne!(
        cache_key("acme/web", "refs/heads/main", &pairs),
        cache_key("acme/webr", "efs/heads/main", &pairs),
    );
    // Order-independent over the pairs.
    let two = vec![
        ("b".to_string(), "2".to_string()),
        ("a".to_string(), "1".to_string()),
    ];
    let mut rev = two.clone();
    rev.reverse();
    assert_eq!(cache_key("p", "s", &two), cache_key("p", "s", &rev));
    // Length-prefixed: shifting a boundary between path and hash must not
    // collide.
    assert_ne!(
        cache_key("p", "s", &[("ab".to_string(), "c".to_string())]),
        cache_key("p", "s", &[("a".to_string(), "bc".to_string())]),
    );
}
