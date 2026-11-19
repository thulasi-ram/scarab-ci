//! `libs/image-index.yaml` as the release ships it (git-bug 794b3ac): one
//! Step that joins images built per architecture into one index with
//! `crane index append`. It holds the registry credential, so every input
//! that reaches it is checked, and it takes images by digest only.

use std::collections::BTreeMap;

use scarab_pipeline::{
    check_installed_library, compile_with_libraries, render, Facts, InstalledLibrary, Libraries,
    PipelineError, PipelineIr, Results, StepSpec, Templates,
};

const INDEX: &str = include_str!("../../../libs/image-index.yaml");
const INDEX_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/image-index.yaml@1111111111111111111111111111111111111111";
const BUILD: &str = include_str!("../../../libs/build.yaml");
const BUILD_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build.yaml@1111111111111111111111111111111111111111";

const AMD64: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const ARM64: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

fn libraries() -> Libraries {
    Libraries {
        installed: BTreeMap::from([
            (
                "thulasi-ram::scarab-ci::image-index".to_string(),
                InstalledLibrary {
                    content: INDEX.to_string(),
                    source: INDEX_SOURCE.to_string(),
                },
            ),
            (
                "thulasi-ram::scarab-ci::build".to_string(),
                InstalledLibrary {
                    content: BUILD.to_string(),
                    source: BUILD_SOURCE.to_string(),
                },
            ),
        ]),
        ..Default::default()
    }
}

/// Two builds, one per architecture, joined by the index library from their
/// pushed references, and a deploy that reads the index's digest.
const TWO_ARCH: &str = r#"
on: { push: {} }
steps:
  - id: amd64
    invoke: lib:thulasi-ram::scarab-ci::build
    placement_profiles: { image: [amd64], push: [ecr-push] }
    with: { image: "ghcr.io/acme/app:${{ event.sha }}-amd64", push: "true" }
  - id: arm64
    invoke: lib:thulasi-ram::scarab-ci::build
    placement_profiles: { image: [arm64], push: [ecr-push] }
    with: { image: "ghcr.io/acme/app:${{ event.sha }}-arm64", push: "true" }
  - id: app
    invoke: lib:thulasi-ram::scarab-ci::image-index
    needs: [amd64, arm64]
    placement_profiles: [ecr-push]
    with:
      image: "ghcr.io/acme/app:${{ event.sha }}"
      images: |
        ${{ outputs.amd64.push.ref }}
        ${{ outputs.arm64.push.ref }}
  - id: deploy
    image: busybox
    needs: [app]
    env: [[DIGEST, "${{ outputs.app.index.digest }}"]]
"#;

fn step<'a>(ir: &'a PipelineIr, id: &str) -> &'a StepSpec {
    ir.steps
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("no step {id}"))
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

/// The two builds' `push` results, pushed as `refs`.
fn pushed(amd64: &str, arm64: &str) -> serde_json::Value {
    serde_json::json!({
        "amd64/push": { "push": { "ref": amd64, "digest": AMD64 } },
        "arm64/push": { "push": { "ref": arm64, "digest": ARM64 } },
    })
}

/// The library installs, and compiles to one Step, `app/index`: it holds
/// the registry credential, has no sandbox, runs as uid 1000 from the crane
/// image the build library pins, takes every input through env, needs both
/// builds' push Steps, and gets the invoke's placement profiles. A Step
/// that reads its `index` output needs it.
#[test]
fn the_image_index_library_compiles_to_one_credentialed_step() {
    check_installed_library(
        "thulasi-ram::scarab-ci::image-index",
        INDEX,
        &BTreeMap::new(),
    )
    .expect("libs/image-index.yaml installs");
    let ir = compile_with_libraries(TWO_ARCH, &libraries()).unwrap();
    let index = step(&ir, "app/index");
    assert!(index.registry_auth);
    assert_eq!(index.sandbox(), None);
    assert!(index.from_installed_library());
    let security = index.security.as_ref().expect("a security block");
    assert_eq!(security.run_as_user, Some(1000));
    assert!(!security.run_as_root);
    assert_eq!(
        index.image,
        step(&ir, "amd64/push").image,
        "the crane image the build library's push Step pins"
    );
    assert!(index.image.contains("@sha256:"), "{}", index.image);
    assert!(
        index.command.iter().all(|c| !c.contains("${{")),
        "no input in the command"
    );
    let mut needs = index.needs.0.clone();
    needs.sort();
    assert_eq!(needs, ["amd64/push", "arm64/push"]);
    assert_eq!(index.placement_profiles, vec!["ecr-push"]);
    assert_eq!(step(&ir, "deploy").needs.0, vec!["app/index".to_string()]);
    assert_eq!(
        ir.steps.iter().filter(|s| s.id.starts_with("app/")).count(),
        1
    );
}

/// At launch the Step receives the builds' pushed references, one per
/// line; a reference by tag is refused before the Step launches.
#[test]
fn the_images_reach_the_step_by_digest_only() {
    let ir = compile_with_libraries(TWO_ARCH, &libraries()).unwrap();
    let index = step(&ir, "app/index");
    let env = launch(
        index,
        pushed(
            &format!("ghcr.io/acme/app@{AMD64}"),
            &format!("ghcr.io/acme/app@{ARM64}"),
        ),
    )
    .expect("the Step launches");
    let get = |name: &str| {
        env.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {name} in {env:?}"))
    };
    assert_eq!(
        get("SCARAB_INDEX_IMAGES"),
        format!("ghcr.io/acme/app@{AMD64}\nghcr.io/acme/app@{ARM64}\n")
    );
    assert_eq!(
        get("SCARAB_INDEX_IMAGE"),
        "ghcr.io/acme/app:0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(get("SCARAB_INDEX_INSECURE"), "false");

    for (amd64, arm64) in [
        // A tag can be moved.
        ("ghcr.io/acme/app:1-amd64", "ghcr.io/acme/app:1-arm64"),
        // A tag beside the digest.
        (
            &format!("ghcr.io/acme/app:1-amd64@{AMD64}") as &str,
            &format!("ghcr.io/acme/app@{ARM64}") as &str,
        ),
        // A second line smuggled into one reference.
        (
            &format!("ghcr.io/acme/app@{AMD64}\n--insecure") as &str,
            &format!("ghcr.io/acme/app@{ARM64}") as &str,
        ),
        // A ',' would split one crane -m argument into two.
        (
            &format!("ghcr.io/acme/app@{AMD64},evil.example/x@{ARM64}") as &str,
            &format!("ghcr.io/acme/app@{ARM64}") as &str,
        ),
    ] {
        let err = launch(index, pushed(amd64, arm64)).unwrap_err();
        assert!(err.contains("input `images`"), "{amd64}: {err}");
    }
}

/// A literal `with:` is checked when the Pipeline compiles: the target needs
/// a tag and no digest, and each image a digest and no tag.
#[test]
fn literal_inputs_are_checked_at_compile() {
    let compile = |image: &str, images: &str| {
        compile_with_libraries(
            &format!(
                "steps:\n  - id: app\n    invoke: lib:thulasi-ram::scarab-ci::image-index\n    with:\n      image: \"{image}\"\n      images: \"{images}\"\n"
            ),
            &libraries(),
        )
    };
    let two = format!("registry.example:5000/app@{AMD64}\\nregistry.example:5000/app@{ARM64}\\n");
    compile("registry.example:5000/app:1.2.0", &two).expect("a tag and two digests compile");
    compile("app:latest", &format!("app@{AMD64}")).expect("one image compiles");
    for (image, images, input) in [
        ("registry.example/app", two.as_str(), "image"),
        (
            &format!("registry.example/app@{AMD64}") as &str,
            two.as_str(),
            "image",
        ),
        ("registry.example/app:1,x", two.as_str(), "image"),
        ("-t/app:1", two.as_str(), "image"),
        (
            "registry.example/app:1",
            "registry.example/app:amd64",
            "images",
        ),
        ("registry.example/app:1", "", "images"),
        (
            "registry.example/app:1",
            "registry.example/app@sha256:abc",
            "images",
        ),
        (
            "registry.example/app:1",
            &format!("registry.example/app@{AMD64}\\n\\nregistry.example/app@{ARM64}") as &str,
            "images",
        ),
    ] {
        match compile(image, images) {
            Err(PipelineError::Validation(errs)) => assert!(
                errs.iter().any(|e| e.contains(&format!("input `{input}`"))),
                "{image} / {images}: {errs:?}"
            ),
            other => panic!("{image} / {images}: expected a refusal, got {other:?}"),
        }
    }
}

/// `insecure` is a boolean, and anything else is refused; the library asks
/// for nothing but the registry credential.
#[test]
fn insecure_is_a_boolean_and_the_library_asks_for_one_credential() {
    let errs = match compile_with_libraries(
        &format!(
            "steps:\n  - id: app\n    invoke: lib:thulasi-ram::scarab-ci::image-index\n    with: {{ image: \"app:1\", images: \"app@{AMD64}\", insecure: \"maybe\" }}\n"
        ),
        &libraries(),
    ) {
        Err(PipelineError::Validation(errs)) => errs,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(
        errs.iter().any(|e| e.contains("input `insecure`")),
        "{errs:?}"
    );
    let lib: PipelineIr = serde_yaml::from_str(INDEX).unwrap();
    assert_eq!(lib.steps.len(), 1);
    let s = &lib.steps[0];
    assert!(s.registry_auth);
    assert!(s.forge_token.is_none());
    assert!(s.secrets.is_empty());
    assert!(s.services.is_empty());
}
