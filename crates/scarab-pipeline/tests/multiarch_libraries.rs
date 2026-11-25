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

/// build-multiarch's build instances for the default `platforms`.
const AMD: &str = "x/build[arch=linux/amd64]";
const ARM: &str = "x/build[arch=linux/arm64]";

/// The ids of the build instances of `ir`, in the order they compiled.
fn build_ids(ir: &PipelineIr) -> Vec<&str> {
    ir.steps
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| id.starts_with("x/build["))
        .collect()
}

/// An `image` result as the BuildKit script publishes it for the build
/// Step `id`: BuildKit's digest, `digest`, and the layout keyed by `id`.
fn image_result(id: &str, digest: &str) -> serde_json::Value {
    serde_json::json!({ "image": {
        "containerimage.digest": digest, "digest": digest, "layout": layout(id),
    } })
}

/// The three files' copies of the BuildKit script, and the two copies of the
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
        ("build-multiarch amd64", step(&multiarch, AMD)),
        ("build-multiarch arm64", step(&multiarch, ARM)),
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

/// build-multiarch writes one build Step; its instances differ only by the
/// platform each builds: same image, command and sandbox, and at launch the
/// same env but SCARAB_BUILD_PLATFORM.
#[test]
fn build_instances_differ_only_by_platform() {
    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    assert_eq!(build_ids(&ir), [AMD, ARM]);
    let (amd64, arm64) = (step(&ir, AMD), step(&ir, ARM));
    assert_eq!(arm64.command, amd64.command);
    assert_eq!(arm64.security, amd64.security);
    assert_eq!(arm64.image, amd64.image);
    let a = launch(amd64, serde_json::json!({})).unwrap();
    let b = launch(arm64, serde_json::json!({})).unwrap();
    let names = |env: &[(String, String)]| env.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
    assert_eq!(names(&a), names(&b));
    let differ: Vec<&str> = a
        .iter()
        .zip(&b)
        .filter(|(x, y)| x.1 != y.1)
        .map(|(x, _)| x.0.as_str())
        .collect();
    assert_eq!(differ, ["SCARAB_BUILD_PLATFORM"]);
    assert!(!BUILD_MULTIARCH.contains("<<:"), "a merge key in the file");
}

/// Each file installs; exactly the expected Step holds `registry_auth`
/// (none in build-layout); every BuildKit Step (each build-multiarch
/// instance included) runs in the `rootless-buildkit` sandbox with no
/// credential; every Step that holds the credential runs crane as uid 1000
/// with no sandbox.
#[test]
fn only_the_push_step_holds_registry_auth() {
    for (lib, credentialed, builds) in [
        ("build", vec!["x/push"], vec!["x/image"]),
        ("build-layout", vec![], vec!["x/image"]),
        ("build-multiarch", vec!["x/push"], vec![AMD, ARM]),
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
/// (`inputs:`, every build instance), not those of the Steps the invoke
/// needs, while it still waits for those (a `with:` that reads
/// `outputs.meta` adds `meta` to every library Step's needs). A Step that
/// needs the invoke waits for the push Step alone. build.yaml's push Step
/// reads only its build Step's layout.
#[test]
fn the_push_step_receives_only_the_builds() {
    for (lib, push, builds) in [
        ("build-multiarch", "x/push", vec![AMD, ARM]),
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
        let mut inputs = p.inputs.clone().expect("the push Step names its inputs");
        inputs.sort();
        assert_eq!(inputs, builds, "{lib}");
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

/// The push Step's `SCARAB_INDEX_LAYOUTS` template (a `map` over the list
/// of build instances, `has()` on each, and Scarab's `join`) renders one
/// `<layout> <digest>` line per build instance, in the order the instances
/// compiled, and `SCARAB_INDEX_LAYOUT_COUNT` is how many there are, for
/// one, two and three platforms. A build that published no `image`, or one
/// without `layout` or `digest`, renders a placeholder line naming its
/// platform, which the script refuses; the count does not change. An
/// `image` result that is not an object fails the render.
#[test]
fn the_push_template_renders_one_line_per_instance() {
    let digests = [AMD64, ARM64, &format!("sha256:{}", "3".repeat(64))];
    for (platforms, want) in [
        ("linux/amd64", vec!["linux/amd64"]),
        (
            "linux/amd64,linux/arm64",
            vec!["linux/amd64", "linux/arm64"],
        ),
        (
            "linux/amd64 linux/arm64,linux/riscv64",
            vec!["linux/amd64", "linux/arm64", "linux/riscv64"],
        ),
        (
            "linux/arm64,linux/amd64",
            vec!["linux/arm64", "linux/amd64"],
        ),
    ] {
        let ir = invoke(
            "build-multiarch",
            &format!(r#"{{ image: "registry.example/app:1", platforms: "{platforms}" }}"#),
        );
        let ids = build_ids(&ir);
        let expected: Vec<String> = want.iter().map(|p| format!("x/build[arch={p}]")).collect();
        assert_eq!(ids, expected, "{platforms}");
        let mut results = serde_json::json!({});
        for (id, digest) in ids.iter().zip(digests) {
            results[*id] = image_result(id, digest);
        }
        let env =
            launch(step(&ir, "x/push"), results).unwrap_or_else(|e| panic!("{platforms}: {e}"));
        let lines: Vec<String> = ids
            .iter()
            .zip(digests)
            .map(|(id, d)| format!("{} {d}", layout(id)))
            .collect();
        assert_eq!(
            get(&env, "SCARAB_INDEX_LAYOUTS"),
            lines.join("\n"),
            "{platforms}"
        );
        assert_eq!(
            get(&env, "SCARAB_INDEX_LAYOUT_COUNT"),
            ids.len().to_string(),
            "{platforms}"
        );
        assert_eq!(get(&env, "SCARAB_INDEX_IMAGES"), "", "{platforms}");
        assert_eq!(get(&env, "SCARAB_INDEX_RESULT"), "push", "{platforms}");
    }

    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    let push = step(&ir, "x/push");
    let first = format!("{} {AMD64}", layout(AMD));
    let placeholder = format!("{first}\n(linux/arm64 wrote no layout)");
    for (what, arm64) in [
        ("no arm64 result", None),
        ("an empty result", Some(serde_json::json!({}))),
        (
            "no layout",
            Some(
                serde_json::json!({ "image": { "containerimage.digest": ARM64, "digest": ARM64 } }),
            ),
        ),
        (
            "no digest",
            Some(
                serde_json::json!({ "image": { "containerimage.digest": ARM64, "layout": layout(ARM) } }),
            ),
        ),
    ] {
        let mut results = serde_json::json!({ AMD: image_result(AMD, AMD64) });
        if let Some(r) = arm64 {
            results[ARM] = r;
        }
        let env = launch(push, results).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(get(&env, "SCARAB_INDEX_LAYOUTS"), placeholder, "{what}");
        assert_eq!(get(&env, "SCARAB_INDEX_LAYOUT_COUNT"), "2", "{what}");
    }
    // Neither build published (a fork): two placeholders, which the script
    // never reads, as a fork exits 0 first.
    let env = launch(push, serde_json::json!({})).unwrap();
    assert_eq!(
        get(&env, "SCARAB_INDEX_LAYOUTS"),
        "(linux/amd64 wrote no layout)\n(linux/arm64 wrote no layout)"
    );
    // An `image` result that is text, not an object: the render fails
    // rather than guess.
    let results = serde_json::json!({
        AMD: image_result(AMD, AMD64),
        ARM: { "image": "/workspace/.scarab-build/x/oci sha256:2" },
    });
    assert!(launch(push, results).is_err());
}

/// Each build instance is told its platform, keeps a cache per
/// architecture, and has its own Step id, which is the layout key's input
/// (SCARAB_STEP is the Step id).
#[test]
fn each_build_instance_gets_its_platform() {
    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    for (id, platform) in [(AMD, "linux/amd64"), (ARM, "linux/arm64")] {
        let s = step(&ir, id);
        assert_eq!(
            s.matrix_values.get("arch").map(String::as_str),
            Some(platform)
        );
        let env = launch(s, serde_json::json!({})).unwrap();
        assert_eq!(get(&env, "SCARAB_BUILD_PLATFORM"), platform, "{id}");
        assert_eq!(get(&env, "SCARAB_BUILD_CACHE_PER_ARCH"), "true", "{id}");
        assert_eq!(get(&env, "SCARAB_BUILD_PUSH"), "true", "{id}");
    }
    assert_ne!(layout(AMD), layout(ARM));
}

/// With no `platforms` the default builds amd64 and arm64; one platform
/// gives one build instance, which is all the push Step needs and reads.
/// The library's output is `push` either way.
#[test]
fn platforms_default_and_with_choose_the_instances() {
    let ir = invoke("build-multiarch", &minimal_with("build-multiarch"));
    assert_eq!(build_ids(&ir), [AMD, ARM]);
    let ir = invoke(
        "build-multiarch",
        r#"{ image: "registry.example/app:1", platforms: "linux/arm64" }"#,
    );
    assert_eq!(build_ids(&ir), [ARM]);
    let push = step(&ir, "x/push");
    assert_eq!(push.needs.0, [ARM]);
    assert_eq!(push.inputs.as_deref(), Some(&[ARM.to_string()][..]));
    assert_eq!(ir.steps.len(), 2);
    let lib: PipelineIr = serde_yaml::from_str(BUILD_MULTIARCH).unwrap();
    assert_eq!(lib.interface.output_names(), ["push"]);
}

/// `platforms` takes `linux/<architecture>[/<variant>]` of the
/// architectures the build script knows, separated by commas or white
/// space; anything else is refused when the Pipeline compiles, naming the
/// input. A doubled comma is an empty piece the engine drops.
#[test]
fn platforms_validate_accepts_and_refuses() {
    let with = |platforms: &str| {
        compile_with_libraries(
            &format!(
                "steps:\n  - id: x\n    invoke: lib:thulasi-ram::scarab-ci::build-multiarch\n    with: {{ image: \"registry.example/app:1\", platforms: \"{platforms}\" }}\n"
            ),
            &libraries(),
        )
    };
    for (platforms, instances) in [
        ("linux/amd64", 1),
        ("linux/amd64, linux/arm64", 2),
        ("linux/amd64/v3", 1),
        ("linux/arm/v7", 1),
        ("linux/amd64\\tlinux/arm64", 2),
        ("linux/amd64\\nlinux/arm64\\n", 2),
        ("linux/amd64,,linux/arm64", 2),
    ] {
        let ir = with(platforms).unwrap_or_else(|e| panic!("{platforms}: {e:?}"));
        assert_eq!(build_ids(&ir).len(), instances, "{platforms}");
    }
    for platforms in [
        "linux/arm64/v8",
        "linux/amd64/v1",
        "windows/amd64",
        "amd64",
        "",
        "linux/amd64;id",
        // Platforms the build script's node check cannot decide.
        "linux/386",
        "linux/arm",
        "linux/mipsle",
        "linux/arm,linux/arm/v7",
    ] {
        match with(platforms) {
            Err(PipelineError::Validation(errs)) => assert!(
                errs.iter().any(|e| e.contains("input `platforms`")
                    && e.contains("failed validation `value.matches(")),
                "{platforms:?}: {errs:?}"
            ),
            other => panic!("{platforms:?}: expected a refusal, got {other:?}"),
        }
    }
}

/// A caller places each build instance by its coordinate; every compiled
/// Step holds a plain list.
#[test]
fn coordinate_keys_place_each_build_instance() {
    let ir = compile_with_libraries(
        r#"
steps:
  - id: x
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    placement_profiles:
      "build[arch=linux/amd64]": [amd64]
      "build[arch=linux/arm64]": [arm64]
      push: [amd64, ecr-push]
    with: { image: "registry.example/app:1" }
"#,
        &libraries(),
    )
    .unwrap();
    for (id, want) in [
        (AMD, vec!["amd64"]),
        (ARM, vec!["arm64"]),
        ("x/push", vec!["amd64", "ecr-push"]),
    ] {
        assert_eq!(
            step(&ir, id).placement_profiles.compiled().unwrap(),
            want.as_slice(),
            "{id}"
        );
    }
}

/// The map keys a caller wrote before builds were a matrix (`amd64:`,
/// `arm64:`) are refused, naming the coordinate to write instead.
#[test]
fn old_arch_keys_are_refused_naming_the_coordinate() {
    let errs = match compile_with_libraries(
        r#"
steps:
  - id: x
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    placement_profiles: { amd64: [amd64], arm64: [arm64], push: [ecr-push] }
    with: { image: "registry.example/app:1" }
"#,
        &libraries(),
    ) {
        Err(PipelineError::Validation(errs)) => errs,
        other => panic!("expected a refusal, got {other:?}"),
    };
    for arch in ["amd64", "arm64"] {
        let want = format!(
            "step `x`: `placement_profiles` names step `{arch}`, which library \
             `lib:thulasi-ram::scarab-ci::build-multiarch` does not have; to place one \
             instance of a matrix Step, write its coordinate: `build[arch=linux/{arch}]`"
        );
        assert!(errs.iter().any(|e| e == &want), "{want}\n{errs:?}");
    }
}

/// What Run creation stores and a Policy reads is unchanged in shape: no
/// Step holds a matrix or a placement map, every `placement_profiles` is a
/// list, and no `placement` key appears anywhere in the IR.
#[test]
fn compiled_ir_has_plain_lists_only() {
    let ir = compile_with_libraries(
        r#"
steps:
  - id: x
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    placement_profiles:
      "build[arch=linux/amd64]": [amd64]
      build: [other]
      push: [ecr-push]
    with: { image: "registry.example/app:1" }
"#,
        &libraries(),
    )
    .unwrap();
    let json = serde_json::to_value(&ir).unwrap();
    for s in json["steps"].as_array().unwrap() {
        assert!(s.get("matrix").is_none_or(|m| m.is_null()), "{s}");
        assert!(
            s.get("placement_profiles").is_none_or(|p| p.is_array()),
            "{s}"
        );
    }
    fn no_placement_key(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => {
                !m.contains_key("placement") && m.values().all(no_placement_key)
            }
            serde_json::Value::Array(a) => a.iter().all(no_placement_key),
            _ => true,
        }
    }
    assert!(no_placement_key(&json));
    assert_eq!(
        step(&ir, ARM).placement_profiles.compiled().unwrap(),
        ["other"]
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
    for id in [AMD, ARM] {
        assert_eq!(per_arch(step(&ir, id)), ["true"], "{id}");
    }
    for lib in ["build", "build-layout"] {
        let ir = invoke(lib, &minimal_with(lib));
        assert!(per_arch(step(&ir, "x/image")).is_empty(), "{lib}");
    }
}

// Attack (red team): a Pipeline writes its own matrix Step `app/build`
// after the build-multiarch invoke `app`, so its instances would take the
// names of the library's build instances, and the credentialed push Step
// would read their results and workspaces. Safe outcome: the Pipeline
// does not compile, naming the id; a `test/unit` id beside no invoke `test`
// still compiles.
#[test]
fn an_authored_step_under_the_invoke_prefix_is_refused() {
    let errs = match compile_with_libraries(
        r#"
steps:
  - id: app
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    with: { image: "registry.example/app:1" }
  - id: app/build
    image: alpine
    command: [echo]
    matrix: { dimensions: { arch: [linux/amd64] } }
"#,
        &libraries(),
    ) {
        Err(PipelineError::Validation(errs)) => errs,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let want = "step `app/build`: its id starts with `app/`, the name of the Steps that invoke \
                `app` inlines; give it another id";
    assert!(errs.iter().any(|e| e == want), "{errs:?}");

    compile_with_libraries(
        r#"
steps:
  - id: app
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    with: { image: "registry.example/app:1" }
  - { id: test/unit, image: alpine, command: [echo] }
"#,
        &libraries(),
    )
    .expect("an id under no invoke's prefix compiles");
}

/// The repository's own Pipelines compile at the IR version they did
/// before instance lists and `join`: none of them invokes build-multiarch.
#[test]
fn the_repository_pipelines_stay_below_version_4() {
    let mut libraries = libraries();
    libraries.installed.insert(
        "thulasi-ram::scarab-ci::clone".to_string(),
        InstalledLibrary {
            content: include_str!("../../../libs/clone.yaml").to_string(),
            source: "github.com/thulasi-ram/scarab-ci//libs/clone.yaml@1111111111111111111111111111111111111111".to_string(),
        },
    );
    libraries.files.insert(
        ".scarab/lib/notify.yaml".to_string(),
        include_str!("../../../.scarab/lib/notify.yaml").to_string(),
    );
    for (name, yaml) in [
        ("ci.yaml", include_str!("../../../.scarab/ci.yaml")),
        (
            "dogfood.yaml",
            include_str!("../../../.scarab/dogfood.yaml"),
        ),
    ] {
        assert!(!yaml.contains("build-multiarch"), "{name}");
        let ir = compile_with_libraries(yaml, &libraries)
            .unwrap_or_else(|e| panic!("{name} compiles: {e:?}"));
        assert!(
            ir.ir_version < scarab_pipeline::IR_VERSION_INSTANCES,
            "{name}: version {}",
            ir.ir_version
        );
    }
}
