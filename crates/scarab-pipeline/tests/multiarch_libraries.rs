//! Issue #218: a multi-architecture image whose registry writes all happen
//! in one credentialed Step. Four library files share two scripts by copy
//! (the BuildKit build script in `build.yaml`, `build-layout.yaml` and
//! `build-multiarch.yaml`; the index script in `image-index.yaml` and
//! `build-multiarch.yaml`), so these cases read the files through the
//! compiler, as Run creation does, and hold the copies equal; the scripts'
//! behaviour is run in `scarab-executor-k8s`'s tests. The rest is what the
//! compiler and the launch render make of the files: which Step holds the
//! credential, which workspaces reach it, and what its env renders to.

use std::collections::BTreeMap;

use scarab_pipeline::{
    check_installed_library, compile_with_libraries, render, Facts, InstalledLibrary, Libraries,
    PipelineError, PipelineIr, Results, Sandbox, StepSpec, Templates,
};
use sha2::{Digest, Sha256};

const BUILD: &str = include_str!("../../../libs/build.yaml");
const BUILD_LAYOUT: &str = include_str!("../../../libs/build-layout.yaml");
const BUILD_MULTIARCH: &str = include_str!("../../../libs/build-multiarch.yaml");
const IMAGE_INDEX: &str = include_str!("../../../libs/image-index.yaml");

/// Each library file, by the name it is installed as.
const FILES: [(&str, &str); 4] = [
    ("build", BUILD),
    ("build-layout", BUILD_LAYOUT),
    ("build-multiarch", BUILD_MULTIARCH),
    ("image-index", IMAGE_INDEX),
];

const AMD64: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const ARM64: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn libraries() -> Libraries {
    Libraries {
        installed: FILES
            .iter()
            .map(|(name, content)| {
                (
                    format!("thulasi-ram::scarab-ci::{name}"),
                    InstalledLibrary {
                        content: content.to_string(),
                        source: format!(
                            "github.com/thulasi-ram/scarab-ci//libs/{name}.yaml@1111111111111111111111111111111111111111"
                        ),
                    },
                )
            })
            .collect(),
        ..Default::default()
    }
}

/// The first 32 hex characters of the SHA-256 of a build Step's id.
fn key(step: &str) -> String {
    let hex: String = Sha256::digest(step.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex[..32].to_string()
}

/// The layout path the build Step `step` reports in a Pod.
fn layout(step: &str) -> String {
    format!("/workspace/.scarab-build/{}/oci", key(step))
}

/// One invoke `x` of the library `lib`, with `with` (a YAML flow map).
fn invoke(lib: &str, with: &str) -> PipelineIr {
    compile_with_libraries(
        &format!(
            "steps:\n  - id: x\n    invoke: lib:thulasi-ram::scarab-ci::{lib}\n    with: {with}\n"
        ),
        &libraries(),
    )
    .unwrap_or_else(|e| panic!("{lib}: {e:?}"))
}

/// The `with:` each library needs at least.
fn minimal_with(lib: &str) -> String {
    match lib {
        "build" => r#"{ image: "registry.example/app:1", push: "true" }"#.into(),
        "build-layout" => "{}".into(),
        "build-multiarch" => r#"{ image: "registry.example/app:1" }"#.into(),
        "image-index" => {
            format!(
                r#"{{ image: "registry.example/app:1", images: "registry.example/app@{AMD64}" }}"#
            )
        }
        other => panic!("{other}"),
    }
}

fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
    ir.steps.iter().find(|s| s.id == id).unwrap_or_else(|| {
        panic!(
            "no step {id} in {:?}",
            ir.steps.iter().map(|s| &s.id).collect::<Vec<_>>()
        )
    })
}

/// Renders `step` as a launch does, with the results of its needs.
fn launch(step: &StepSpec, results: serde_json::Value) -> Result<Vec<(String, String)>, String> {
    let results: Results = serde_json::from_value(results).unwrap();
    let event = serde_json::json!({ "sha": "0123456789abcdef0123456789abcdef01234567" });
    render(
        Templates {
            image: &step.image,
            command: &step.command,
            env: &step.env,
        },
        step.scope.as_ref(),
        &step.matrix_values,
        Facts::Launch {
            params: &BTreeMap::new(),
            declared: &[],
            results: &results,
            event: event.as_object().unwrap(),
        },
    )
    .map(|r| r.env)
}

fn get<'a>(env: &'a [(String, String)], name: &str) -> &'a str {
    env.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {name} in {env:?}"))
}

/// The four files' copies of the BuildKit script, and the two copies of the
/// index script, are equal as the compiler hands them to the executor, and
/// so are the image pins each script runs in. A copy edited alone (a fix to
/// one file's refusal) fails here.
#[test]
fn the_shared_scripts_are_one_script() {
    let build = invoke("build", &minimal_with("build"));
    let layout = invoke("build-layout", &minimal_with("build-layout"));
    let multiarch = invoke("build-multiarch", &minimal_with("build-multiarch"));
    let index = invoke("image-index", &minimal_with("image-index"));

    let builds = [
        ("build x/image", step(&build, "x/image")),
        ("build-layout x/image", step(&layout, "x/image")),
        ("build-multiarch x/amd64", step(&multiarch, "x/amd64")),
        ("build-multiarch x/arm64", step(&multiarch, "x/arm64")),
    ];
    let (_, first) = builds[0];
    assert_eq!(first.command.len(), 3);
    for (what, s) in &builds[1..] {
        assert_eq!(
            s.command, first.command,
            "{what}: the BuildKit script differs"
        );
        assert_eq!(s.image, first.image, "{what}: the BuildKit pin differs");
    }
    assert!(
        first.image.starts_with("moby/buildkit:rootless@sha256:"),
        "{}",
        first.image
    );

    let (a, b) = (step(&index, "x/index"), step(&multiarch, "x/push"));
    assert_eq!(a.command.len(), 3);
    assert_eq!(b.command, a.command, "the index script differs");
    assert_eq!(b.image, a.image, "the crane pin differs");
    assert_eq!(
        step(&build, "x/push").image,
        a.image,
        "build.yaml's push Step pins another crane"
    );
    assert!(
        a.image
            .starts_with("gcr.io/go-containerregistry/crane:debug@sha256:"),
        "{}",
        a.image
    );
}

/// build-multiarch's arm64 Step is amd64's by YAML alias, not a merge key
/// (which serde_yaml and other readers expand differently): the two compile
/// to equal env and command.
#[test]
fn build_multiarch_aliases_resolve_to_equal_steps() {
    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    let (amd64, arm64) = (step(&ir, "x/amd64"), step(&ir, "x/arm64"));
    assert_eq!(arm64.env, amd64.env);
    assert_eq!(arm64.command, amd64.command);
    assert_eq!(arm64.security, amd64.security);
    assert!(!BUILD_MULTIARCH.contains("<<:"), "a merge key in the file");
}

/// Each file installs; exactly the expected Step holds `registry_auth`
/// (none in build-layout); every BuildKit Step runs in the
/// `rootless-buildkit` sandbox with no credential; every Step that holds
/// the credential runs crane as uid 1000 with no sandbox.
#[test]
fn only_the_push_step_holds_registry_auth() {
    for (lib, credentialed, builds) in [
        ("build", vec!["x/push"], vec!["x/image"]),
        ("build-layout", vec![], vec!["x/image"]),
        (
            "build-multiarch",
            vec!["x/push"],
            vec!["x/amd64", "x/arm64"],
        ),
        ("image-index", vec!["x/index"], vec![]),
    ] {
        let content = FILES.iter().find(|(n, _)| *n == lib).unwrap().1;
        check_installed_library(
            &format!("thulasi-ram::scarab-ci::{lib}"),
            content,
            &BTreeMap::new(),
        )
        .unwrap_or_else(|e| panic!("{lib} does not install: {e:?}"));
        let ir = invoke(lib, &minimal_with(lib));
        let holding: Vec<&str> = ir
            .steps
            .iter()
            .filter(|s| s.registry_auth)
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(holding, credentialed, "{lib}");
        for id in &builds {
            let s = step(&ir, id);
            assert_eq!(s.sandbox(), Some(Sandbox::RootlessBuildkit), "{lib} {id}");
            assert!(!s.registry_auth, "{lib} {id}");
            assert!(s.from_installed_library(), "{lib} {id}");
        }
        for id in &credentialed {
            let s = step(&ir, id);
            assert_eq!(s.sandbox(), None, "{lib} {id}");
            let security = s.security.as_ref().expect("a security block");
            assert_eq!(security.run_as_user, Some(1000), "{lib} {id}");
            assert!(!security.runs_as_root(), "{lib} {id}");
            assert!(
                s.secrets.is_empty() && s.forge_token.is_none(),
                "{lib} {id}"
            );
        }
        assert_eq!(
            ir.steps.len(),
            builds.len() + credentialed.len(),
            "{lib}: no other Step"
        );
    }
}

/// The credentialed push Step receives only the builds' workspaces
/// (`inputs:`), not those of the Steps the invoke needs, while it still
/// waits for those (a `with:` that reads `outputs.meta` adds `meta` to every
/// library Step's needs). A Step that needs the invoke waits for the push
/// Step alone. build.yaml's push Step reads only its build Step's layout.
#[test]
fn the_push_step_receives_only_the_builds() {
    for (lib, push, builds) in [
        ("build-multiarch", "x/push", vec!["x/amd64", "x/arm64"]),
        ("build", "x/push", vec!["x/image"]),
    ] {
        let yaml = format!(
            r#"
steps:
  - id: checkout
    image: alpine
    command: ["true"]
  - id: meta
    image: alpine
    command: ["true"]
    needs: [checkout]
    results: [tag]
  - id: x
    invoke: lib:thulasi-ram::scarab-ci::{lib}
    needs: [checkout, meta]
    with:
      image: "registry.example/app:${{{{ outputs.meta.tag.value }}}}"
      push: "true"
  - id: deploy
    image: alpine
    command: ["true"]
    needs: [x]
"#
        );
        let yaml = if lib == "build" {
            yaml
        } else {
            yaml.replace("      push: \"true\"\n", "")
        };
        let ir =
            compile_with_libraries(&yaml, &libraries()).unwrap_or_else(|e| panic!("{lib}: {e:?}"));
        let p = step(&ir, push);
        assert_eq!(
            p.inputs.as_deref(),
            Some(
                builds
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .as_slice()
            ),
            "{lib}"
        );
        for need in builds.iter().copied().chain(["meta"]) {
            assert!(
                p.needs.0.iter().any(|n| n == need),
                "{lib}: {push} needs {:?}",
                p.needs.0
            );
        }
        for b in &builds {
            let mut needs = step(&ir, b).needs.0.clone();
            needs.sort();
            assert_eq!(needs, ["checkout", "meta"], "{lib} {b}");
        }
        assert_eq!(step(&ir, "deploy").needs.0, [push.to_string()], "{lib}");
    }
}

/// build-multiarch's push Step is told how many layouts to expect (one per
/// build Step), and renders one line per build: `<layout> <digest>` from a
/// result that has both, or a placeholder line the script refuses when a
/// build published no layout (a result missing, or missing a key).
#[test]
fn the_multiarch_layouts_env_renders_a_placeholder_for_a_missing_result() {
    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    let push = step(&ir, "x/push");
    let (la, lb) = (layout("x/amd64"), layout("x/arm64"));
    let full = |l: &str, d: &str| serde_json::json!({ "image": { "containerimage.digest": d, "digest": d, "layout": l } });
    let amd64 = full(&la, AMD64);
    let both = format!("{la} {AMD64}\n{lb} {ARM64}");
    let placeholder = format!("{la} {AMD64}\n(arm64 wrote no layout)");
    for (what, arm64, want) in [
        ("both results", Some(full(&lb, ARM64)), &both),
        ("no arm64 result", None, &placeholder),
        (
            "no layout",
            Some(
                serde_json::json!({ "image": { "containerimage.digest": ARM64, "digest": ARM64 } }),
            ),
            &placeholder,
        ),
        (
            "no digest",
            Some(serde_json::json!({ "image": { "containerimage.digest": ARM64, "layout": lb } })),
            &placeholder,
        ),
        ("no image result", Some(serde_json::json!({})), &placeholder),
    ] {
        let mut results = serde_json::json!({ "x/amd64": amd64 });
        if let Some(r) = arm64 {
            results["x/arm64"] = r;
        }
        let env = launch(push, results).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(get(&env, "SCARAB_INDEX_LAYOUTS"), want.as_str(), "{what}");
        assert_eq!(get(&env, "SCARAB_INDEX_LAYOUT_COUNT"), "2", "{what}");
        assert_eq!(get(&env, "SCARAB_INDEX_IMAGES"), "", "{what}");
        assert_eq!(get(&env, "SCARAB_INDEX_RESULT"), "push", "{what}");
    }
    // Neither build wrote a layout (a fork): two placeholders, which the
    // script never reads, as a fork exits 0 first.
    let env = launch(push, serde_json::json!({})).unwrap();
    assert_eq!(
        get(&env, "SCARAB_INDEX_LAYOUTS"),
        "(amd64 wrote no layout)\n(arm64 wrote no layout)"
    );
}

/// A build-layout invoke feeding image-index through the documented line.
const LAYOUT_TO_INDEX: &str = r#"
steps:
  - id: amd64
    invoke: lib:thulasi-ram::scarab-ci::build-layout
  - id: app
    invoke: lib:thulasi-ram::scarab-ci::image-index
    needs: [amd64]
    with:
      image: "registry.example/app:1"
      extra_tags: latest
      layouts: |
        ${{ has(outputs.amd64.image) && has(outputs.amd64.image.layout) ? outputs.amd64.image.layout + ' ' + outputs.amd64.image.digest : '' }}
"#;

/// The documented consumer line renders `<layout> <digest>` from a build's
/// result, and an empty line from a fork's (a `digest` and no `layout`),
/// and both pass the `layouts` rule at launch.
#[test]
fn the_build_layout_consumer_line_renders_empty_for_a_fork() {
    let ir = compile_with_libraries(LAYOUT_TO_INDEX, &libraries()).unwrap();
    let index = step(&ir, "app/index");
    let l = layout("amd64/image");
    let fork = serde_json::json!({
        "amd64/image": { "image": { "containerimage.digest": AMD64, "digest": AMD64 } },
    });
    let env = launch(index, fork).expect("a fork's result launches");
    assert_eq!(get(&env, "SCARAB_INDEX_LAYOUTS"), "\n");
    let built = serde_json::json!({
        "amd64/image": { "image": { "containerimage.digest": AMD64, "digest": AMD64, "layout": l } },
    });
    let env = launch(index, built).expect("a build's result launches");
    assert_eq!(get(&env, "SCARAB_INDEX_LAYOUTS"), format!("{l} {AMD64}\n"));
    assert_eq!(get(&env, "SCARAB_INDEX_EXTRA_TAGS"), "latest");
    assert_eq!(get(&env, "SCARAB_INDEX_ARCH_TAGS"), "true");
    assert_eq!(get(&env, "SCARAB_INDEX_IMAGES"), "");
}

// Attack: a build result (written in the build Pod, where a Dockerfile RUN's
// process can reach it) names a layout path that is not a build's layout
// under /workspace/.scarab-build, so the credentialed Step would upload
// another directory. Safe outcome: the `layouts` rule refuses the launch,
// naming the input, before the Step starts.
#[test]
fn layouts_input_is_checked_at_launch() {
    let ir = compile_with_libraries(LAYOUT_TO_INDEX, &libraries()).unwrap();
    let index = step(&ir, "app/index");
    let k = key("amd64/image");
    for path in [
        "/tmp/x/oci".to_string(),
        format!("/workspace/.scarab-build/../{k}/oci"),
        format!("/workspace/.scarab-build/{}/oci", &k[..31]),
        format!("/workspace/.scarab-build/{k}/oci "),
        format!("/workspace/.scarab-build/{k}/oci\n/etc/x"),
        format!("/data/.scarab-build/{k}/oci"),
    ] {
        let result = serde_json::json!({
            "amd64/image": { "image": { "digest": AMD64, "layout": path } },
        });
        let err = launch(index, result).expect_err(&path);
        assert!(err.contains("input `layouts`"), "{path:?}: {err}");
    }
    // A digest of another form is refused the same way.
    let result = serde_json::json!({
        "amd64/image": { "image": { "digest": "sha256:ABC", "layout": layout("amd64/image") } },
    });
    let err = launch(index, result).unwrap_err();
    assert!(err.contains("input `layouts`"), "{err}");
}

/// build.yaml's `with:` shape is the one v0.7.6 shipped (the dogfood
/// Pipeline's invoke compiles against it); image-index's v0.7.6 `with:`
/// still compiles and `images` may now be left out; build-layout takes no
/// `image`, `push` or `insecure_push`.
#[test]
fn the_with_shapes_callers_use_still_compile() {
    let lib: PipelineIr = serde_yaml::from_str(BUILD).unwrap();
    let inputs: Vec<serde_json::Value> = lib
        .interface
        .inputs
        .iter()
        .map(|p| {
            serde_json::json!([
                p.name,
                serde_json::to_value(p.r#type).unwrap(),
                p.required,
                p.default,
            ])
        })
        .collect();
    assert_eq!(
        serde_json::Value::Array(inputs),
        serde_json::json!([
            ["image", "string", true, null],
            ["context", "string", false, "."],
            ["dockerfile", "string", false, "Dockerfile"],
            ["push", "boolean", false, false],
            ["insecure_push", "boolean", false, false],
            ["build_args", "string", false, ""],
            ["cache", "choice", false, "none"],
            ["cache_bucket", "string", false, ""],
            ["cache_region", "string", false, ""],
            ["cache_name", "string", false, ""],
        ])
    );
    assert_eq!(lib.interface.output_names(), ["image", "push"]);

    // The dogfood Pipeline's build invoke.
    invoke(
        "build",
        r#"{ context: docker/clone, dockerfile: Dockerfile, image: ghcr.io/thulasi-ram/scarab-clone, push: "false" }"#,
    );
    // image-index as v0.7.6 callers write it, and with `layouts` alone.
    invoke(
        "image-index",
        &format!(
            r#"{{ image: "registry.example/app:1", images: "registry.example/app@{AMD64}\nregistry.example/app@{ARM64}" }}"#
        ),
    );
    invoke(
        "image-index",
        &format!(
            r#"{{ image: "registry.example/app:1", layouts: "{} {AMD64}" }}"#,
            layout("a/image")
        ),
    );
    let index: PipelineIr = serde_yaml::from_str(IMAGE_INDEX).unwrap();
    let images = index
        .interface
        .inputs
        .iter()
        .find(|p| p.name == "images")
        .unwrap();
    assert!(!images.required, "images is optional");

    for with in [
        r#"{ image: "registry.example/app:1" }"#,
        r#"{ push: "true" }"#,
        r#"{ insecure_push: "true" }"#,
    ] {
        match compile_with_libraries(
            &format!("steps:\n  - id: x\n    invoke: lib:thulasi-ram::scarab-ci::build-layout\n    with: {with}\n"),
            &libraries(),
        ) {
            Err(PipelineError::Validation(errs)) => {
                let name = with.split(':').next().unwrap().trim_start_matches("{ ");
                assert!(
                    errs.iter()
                        .any(|e| e.contains(&format!("unknown input `{name}`"))),
                    "{with}: {errs:?}"
                );
            }
            other => panic!("{with}: expected a refusal, got {other:?}"),
        }
    }
}

/// build.yaml's push Step is told where its build Step wrote the layout,
/// from that Step's `image` result, and told nothing (an empty value, which
/// its script refuses while results are collected) when the result has no
/// `layout`.
#[test]
fn the_build_push_step_reads_the_layout_from_the_image_result() {
    let ir = invoke("build", &minimal_with("build"));
    let push = step(&ir, "x/push");
    let l = layout("x/image");
    let env = launch(
        push,
        serde_json::json!({
            "x/image": { "image": { "containerimage.digest": AMD64, "digest": AMD64, "layout": l } },
        }),
    )
    .unwrap();
    assert_eq!(get(&env, "SCARAB_BUILD_LAYOUT"), l);
    assert_eq!(get(&env, "SCARAB_BUILD_DIGEST"), AMD64);
    for results in [
        serde_json::json!({ "x/image": { "image": { "containerimage.digest": AMD64, "digest": AMD64 } } }),
        serde_json::json!({}),
    ] {
        let env = launch(push, results.clone()).unwrap();
        assert_eq!(get(&env, "SCARAB_BUILD_LAYOUT"), "", "{results}");
    }
}

/// build-multiarch's two builds share one set of inputs, so the library
/// asks the BuildKit script for a cache per architecture; build and
/// build-layout build one image and keep the cache name they are given.
#[test]
fn only_build_multiarch_keeps_a_cache_per_architecture() {
    let per_arch = |s: &StepSpec| {
        s.env
            .iter()
            .filter(|(k, _)| k == "SCARAB_BUILD_CACHE_PER_ARCH")
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    for id in ["x/amd64", "x/arm64"] {
        assert_eq!(per_arch(step(&ir, id)), ["true"], "{id}");
    }
    for lib in ["build", "build-layout"] {
        let ir = invoke(lib, &minimal_with(lib));
        assert!(per_arch(step(&ir, "x/image")).is_empty(), "{lib}");
    }
}
