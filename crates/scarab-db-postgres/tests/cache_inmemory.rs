//! ADR-0065 s1 — the keyed directory Cache's launch-time key resolution,
//! driven over the in-memory store with a fake snapshot oracle (DST): the
//! scheduler folds the key from the key files' blob hashes through the merged
//! input roots (last-overlay-wins), fills the restore hints from the mapping,
//! and — the licence for the whole feature — every failure mode DISABLES the
//! cache for the attempt instead of failing or mis-keying it.

use std::collections::HashMap;
use std::sync::Mutex;

use scarab_engine::ports::ExecState;
use scarab_engine::{
    cache_key, CacheConfig, Db, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus,
    Timestamp, WorkspaceSnapshots,
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
                .map(|(root, path, hash)| {
                    ((root.to_string(), path.to_string()), hash.to_string())
                })
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
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    }
}

fn caching(dirs: &[&str], key_files: &[&str]) -> CacheConfig {
    CacheConfig {
        dirs: dirs.iter().map(|s| s.to_string()).collect(),
        key_files: key_files.iter().map(|s| s.to_string()).collect(),
        key: None,
        restore: Vec::new(),
    }
}

/// Two steps: `clone` (produces the input snapshot) → `build` (declares the
/// cache). Returns the run and the ids.
async fn seed(db: &InMemoryDb, cache: CacheConfig, project: Option<&str>) -> (RunId, StepId) {
    let run = RunId("run-1".into());
    let clone = StepId("clone".into());
    let build = StepId("build".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    if let Some(project) = project {
        db.set_run_scheduling(&run, project, 0).await.unwrap();
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
    assert_eq!(db.run_status(run).await.unwrap(), Some(RunStatus::Succeeded));
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
    let (run, build) = seed(&db, caching(&["node_modules"], &["package-lock.json"]), Some("acme/web")).await;
    let oracle = FakeOracle::with(&[("root-clone", "package-lock.json", "blob-lock")]);

    let expected_key = cache_key(
        "acme/web",
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

    let oracle = FakeOracle::with(&[("root-a", "lock", "blob-early"), ("root-b", "lock", "blob-late")]);
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
        Some(cache_key("acme/web", &[("lock".to_string(), "blob-late".to_string())]).as_str()),
        "the LAST overlay's blob hash owns the key file"
    );
}

/// The disable arms, each of which must launch the step normally with the
/// cache OFF (`key: None`, no restore) — never an error, never a mis-key:
/// a key file absent from every input; a transient oracle failure; an
/// untenanted run (no project namespace).
#[tokio::test]
async fn every_unresolvable_key_disables_the_cache_and_never_fails_the_step() {
    for (name, project, files, fail) in [
        ("key file absent", Some("acme/web"), vec![], false),
        (
            "oracle transient error",
            Some("acme/web"),
            vec![("root-clone", "package-lock.json", "blob-lock")],
            true,
        ),
        (
            "untenanted run",
            None,
            vec![("root-clone", "package-lock.json", "blob-lock")],
            false,
        ),
    ] {
        let db = InMemoryDb::new();
        let (run, _build) = seed(
            &db,
            caching(&["node_modules"], &["package-lock.json"]),
            project,
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

/// The key is project-salted AND the lookup is project-scoped: the same key
/// files resolve to different keys for different projects, so tenant B can
/// never see tenant A's rows even by recording identical lockfiles.
#[test]
fn the_cache_key_is_project_salted_and_deterministic() {
    let pairs = vec![("lock".to_string(), "blob".to_string())];
    let a = cache_key("acme/web", &pairs);
    let b = cache_key("evil/web", &pairs);
    assert_ne!(a, b, "two tenants with identical lockfiles fold different keys");
    assert_eq!(a, cache_key("acme/web", &pairs), "deterministic");
    // Order-independent over the pairs.
    let two = vec![
        ("b".to_string(), "2".to_string()),
        ("a".to_string(), "1".to_string()),
    ];
    let mut rev = two.clone();
    rev.reverse();
    assert_eq!(cache_key("p", &two), cache_key("p", &rev));
    // Length-prefixed: shifting a boundary between path and hash must not
    // collide.
    assert_ne!(
        cache_key("p", &[("ab".to_string(), "c".to_string())]),
        cache_key("p", &[("a".to_string(), "bc".to_string())]),
    );
}
