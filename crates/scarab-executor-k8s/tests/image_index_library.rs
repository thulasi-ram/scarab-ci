//! The script of `libs/image-index.yaml` (git-bug 794b3ac), as the compiler
//! hands it to the executor, run with the host's `/bin/sh` against a stub
//! `crane` on `PATH` that serves manifests and configs from files and
//! records each call. The Step holds the registry credential, so what it
//! refuses matters: images by tag, two images of one platform, an index
//! as a source, `--insecure` beside a registry login.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const INDEX: &str = include_str!("../../../libs/image-index.yaml");
/// The index digest the stub prints unless a case sets `CRANE_DIGEST`.
const PUSHED: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// `registry.example/app@sha256:<c × 64>`.
fn by_digest(c: char) -> String {
    format!("registry.example/app@sha256:{}", c.to_string().repeat(64))
}

fn image_manifest() -> &'static str {
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[]}"#
}

fn config(platform: &str) -> String {
    format!(r#"{{{platform},"rootfs":{{"type":"layers","diff_ids":[]}},"config":{{}}}}"#)
}

/// The Step's `/bin/sh -c` script, with its results directory moved to
/// `results`.
fn script(results: &Path) -> String {
    let ir = scarab_pipeline::compile_with_libraries(
        "steps:\n  - id: app\n    invoke: lib:thulasi-ram::scarab-ci::image-index\n    with: { image: \"registry.example/app:1\", images: \"registry.example/app@sha256:1111111111111111111111111111111111111111111111111111111111111111\" }\n",
        &scarab_pipeline::Libraries {
            installed: std::collections::BTreeMap::from([(
                "thulasi-ram::scarab-ci::image-index".to_string(),
                scarab_pipeline::InstalledLibrary {
                    content: INDEX.to_string(),
                    source: "github.com/thulasi-ram/scarab-ci//libs/image-index.yaml@1".into(),
                },
            )]),
            ..Default::default()
        },
    )
    .unwrap();
    let step = ir.steps.iter().find(|s| s.id == "app/index").unwrap();
    let command = &step.command;
    assert_eq!(command.len(), 3, "{command:?}");
    assert!(
        command[0].ends_with("/sh") && command[1] == "-c",
        "{command:?}"
    );
    assert!(command[2].contains("/scarab/results"));
    command[2].replace("/scarab/results", &results.display().to_string())
}

struct Harness {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    script: String,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        for d in ["bin", "fixtures", "results", "registry"] {
            std::fs::create_dir(base.join(d)).unwrap();
        }
        // A registry login, for the cases that give DOCKER_CONFIG.
        std::fs::write(base.join("registry/config.json"), "{}").unwrap();
        let stub = base.join("bin/crane");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$CRANE_LOG"
case "$1" in
  manifest | config)
    f="$CRANE_FIXTURES/${2##*sha256:}.$1"
    [ -f "$f" ] || { echo "MANIFEST_UNKNOWN: $2" >&2; exit 1; }
    cat "$f"
    ;;
  index) echo "registry.example/app@$CRANE_DIGEST" ;;
  *) exit 99 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = script(&base.join("results"));
        let h = Harness {
            _tmp: tmp,
            base,
            script,
        };
        // amd64 (a), arm64 (b), a second amd64 (d), arm v7 (e) and v6 (f).
        h.image('a', r#""architecture":"amd64","os":"linux""#);
        h.image('b', r#""os":"linux","architecture":"arm64","variant":"v8""#);
        h.image('d', r#""architecture":"amd64","os":"linux""#);
        h.image('e', r#""architecture":"arm","os":"linux","variant":"v7""#);
        h.image('f', r#""architecture":"arm","os":"linux","variant":"v6""#);
        h
    }

    /// Serves an image manifest and a config holding `platform` (JSON
    /// members) for the digest `<c × 64>`.
    fn image(&self, c: char, platform: &str) {
        self.serve(c, image_manifest(), &config(platform));
    }

    fn serve(&self, c: char, manifest: &str, config: &str) {
        let hex = c.to_string().repeat(64);
        let dir = self.base.join("fixtures");
        std::fs::write(dir.join(format!("{hex}.manifest")), manifest).unwrap();
        std::fs::write(dir.join(format!("{hex}.config")), config).unwrap();
    }

    /// Runs the script with `images` and the given env over the defaults
    /// (target `registry.example/app:1`, not insecure, not a fork, no
    /// registry login): its exit code and crane's calls, one per line.
    fn run(&self, images: &str, given: &[(&str, &str)]) -> (Option<i32>, Vec<String>) {
        let log = self.base.join("called");
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(self.base.join("results/index.json"));
        let path = format!(
            "{}:{}",
            self.base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", &self.script, "scarab-index"])
            .current_dir(&self.base)
            .env_remove("DOCKER_CONFIG")
            .env("PATH", path)
            .env("CRANE_LOG", &log)
            .env("CRANE_FIXTURES", self.base.join("fixtures"))
            .env("CRANE_DIGEST", PUSHED)
            .env("SCARAB_INDEX_IMAGE", "registry.example/app:1")
            .env("SCARAB_INDEX_IMAGES", images)
            .env("SCARAB_INDEX_INSECURE", "false")
            .env("SCARAB_FORK_PR", "false");
        for (name, value) in given {
            cmd.env(name, value);
        }
        let out = cmd.output().unwrap();
        let calls = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        (out.status.code(), calls)
    }

    fn result(&self) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(self.base.join("results/index.json")).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }
}

/// Two images of two platforms: crane reads each one's manifest and config,
/// then joins them under the target tag, and the Step writes the index's
/// reference and digest as its `index` result.
#[test]
fn the_step_joins_images_of_distinct_platforms_and_reports_the_digest() {
    let h = Harness::new();
    let (a, b) = (by_digest('a'), by_digest('b'));
    let (code, calls) = h.run(&format!("{a}\n{b}\n"), &[]);
    assert_eq!(code, Some(0), "{calls:?}");
    assert_eq!(
        calls,
        [
            format!("manifest {a}"),
            format!("config {a}"),
            format!("manifest {b}"),
            format!("config {b}"),
            format!("index append -m {a} -m {b} -t registry.example/app:1"),
        ]
    );
    let result = h.result().expect("the index result");
    assert_eq!(result["digest"], PUSHED);
    assert_eq!(
        result["ref"],
        format!("registry.example/app@{PUSHED}").as_str()
    );

    // Variants tell two arm images apart; one image alone is an index too.
    let (e, f) = (by_digest('e'), by_digest('f'));
    assert_eq!(h.run(&format!("{e}\n{f}"), &[]).0, Some(0));
    assert_eq!(h.run(&a, &[]).0, Some(0));
}

/// `insecure` adds `--insecure` to every crane call when the Step holds no
/// registry login, and is refused (exit 2) before crane runs when it holds
/// one, since `--insecure` also skips TLS verification.
#[test]
fn insecure_reaches_every_crane_call_and_is_refused_beside_a_login() {
    let h = Harness::new();
    let (a, b) = (by_digest('a'), by_digest('b'));
    let images = format!("{a}\n{b}");
    let (code, calls) = h.run(&images, &[("SCARAB_INDEX_INSECURE", "true")]);
    assert_eq!(code, Some(0), "{calls:?}");
    assert_eq!(calls.len(), 5);
    assert!(
        calls.iter().all(|c| c.ends_with(" --insecure")),
        "{calls:?}"
    );
    let login = h.base.join("registry").display().to_string();
    let (code, calls) = h.run(
        &images,
        &[
            ("SCARAB_INDEX_INSECURE", "true"),
            ("DOCKER_CONFIG", login.as_str()),
        ],
    );
    assert_eq!((code, calls.len()), (Some(2), 0));
    // The same login without insecure joins, over TLS.
    let (code, calls) = h.run(&images, &[("DOCKER_CONFIG", login.as_str())]);
    assert_eq!(code, Some(0));
    assert!(calls.iter().all(|c| !c.contains("--insecure")), "{calls:?}");
}

// Attack: two images of one platform (a second amd64 build, or the same
// image under two digests) would give the index two entries a client picks
// between by order; an index named as a source would be flattened into the
// new index with whatever it holds (attestations, other platforms). Safe
// outcome: exit 2 before `crane index append` runs, and no result.
#[test]
fn the_step_refuses_two_images_of_one_platform_and_an_index_source() {
    let h = Harness::new();
    let (a, d) = (by_digest('a'), by_digest('d'));
    let (code, calls) = h.run(&format!("{a}\n{d}"), &[]);
    assert_eq!(code, Some(2), "{calls:?}");
    assert!(calls.iter().all(|c| !c.starts_with("index ")), "{calls:?}");
    assert!(h.result().is_none());

    h.serve(
        '9',
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
        &config(r#""architecture":"arm64","os":"linux""#),
    );
    let (code, calls) = h.run(&format!("{a}\n{}", by_digest('9')), &[]);
    assert_eq!(code, Some(2), "{calls:?}");
    assert!(calls.iter().all(|c| !c.starts_with("index ")), "{calls:?}");
}

/// A config that names no architecture would give crane no platform for
/// the image; one that names `os` twice (a label called `os`) is refused
/// rather than guessed at. A manifest crane cannot read fails the Step
/// (exit 1), as a registry error, not a refusal.
#[test]
fn the_step_refuses_a_config_it_cannot_read_a_platform_from() {
    let h = Harness::new();
    let a = by_digest('a');
    h.serve('7', image_manifest(), r#"{"os":"linux","rootfs":{}}"#);
    h.serve(
        '8',
        image_manifest(),
        r#"{"architecture":"arm64","os":"linux","config":{"Labels":{"os":"windows"}}}"#,
    );
    for c in ['7', '8'] {
        let (code, calls) = h.run(&format!("{a}\n{}", by_digest(c)), &[]);
        assert_eq!(code, Some(2), "{c}: {calls:?}");
        assert!(calls.iter().all(|l| !l.starts_with("index ")), "{calls:?}");
    }
    let (code, calls) = h.run(&format!("{a}\n{}", by_digest('6')), &[]);
    assert_eq!(code, Some(1), "{calls:?}");
    assert!(calls.iter().all(|l| !l.starts_with("index ")), "{calls:?}");
}

/// The script holds the inputs to their `validate:` rules on its own: each
/// of these exits 2 before crane runs.
#[test]
fn the_step_refuses_inputs_outside_their_rules_before_crane_runs() {
    let h = Harness::new();
    let (a, b) = (by_digest('a'), by_digest('b'));
    let two = format!("{a}\n{b}");
    let hex = "a".repeat(64);
    for (images, given) in [
        // By tag, a tag beside a digest, and a short digest.
        ("registry.example/app:amd64".to_string(), vec![]),
        (format!("registry.example/app:1@sha256:{hex}"), vec![]),
        ("registry.example/app@sha256:abc".to_string(), vec![]),
        (format!("registry.example/app@sha256:{}", "A".repeat(64)), vec![]),
        // A ',' would split crane's -m argument; a leading '-' is a flag.
        (format!("{a},{b}"), vec![]),
        (format!("-m@sha256:{hex}"), vec![]),
        (format!("{a} {b}"), vec![]),
        // Named twice, or nothing at all.
        (format!("{a}\n{a}"), vec![]),
        ("\n".to_string(), vec![]),
        // The target: no tag, a digest, a flag.
        (two.clone(), vec![("SCARAB_INDEX_IMAGE", "registry.example/app")]),
        (
            two.clone(),
            vec![("SCARAB_INDEX_IMAGE", "registry.example:5000/app")],
        ),
        (
            two.clone(),
            vec![(
                "SCARAB_INDEX_IMAGE",
                "registry.example/app@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            )],
        ),
        (two.clone(), vec![("SCARAB_INDEX_IMAGE", "--insecure")]),
        (two.clone(), vec![("SCARAB_INDEX_INSECURE", "yes")]),
    ] {
        let (code, calls) = h.run(&images, &given);
        assert_eq!((code, calls.len()), (Some(2), 0), "{images:?} {given:?}");
    }
}

/// A fork pull request's Run, or one locked out of its Environment's
/// secrets (SCARAB_FORK_PR anything but `false`, or unset), pushes nothing
/// and exits 0.
#[test]
fn the_step_pushes_nothing_for_a_fork() {
    let h = Harness::new();
    let images = format!("{}\n{}", by_digest('a'), by_digest('b'));
    for fork in ["true", ""] {
        let (code, calls) = h.run(&images, &[("SCARAB_FORK_PR", fork)]);
        assert_eq!((code, calls.len()), (Some(0), 0), "{fork:?}");
        assert!(h.result().is_none());
    }
}

/// git-bug 794b3ac: a fork pull request's builds push nothing, so the
/// documented `has(outputs.<build>.push) ? … : ''` template renders `images`
/// as empty lines. A fork's Step then pushes nothing and exits 0; a Run that
/// is not a fork's fails on the same input (exit 2), before crane runs.
#[test]
fn a_fork_with_no_images_pushes_nothing_and_a_non_fork_fails() {
    let h = Harness::new();
    for images in ["", "\n", "\n\n"] {
        for fork in ["true", ""] {
            let (code, calls) = h.run(images, &[("SCARAB_FORK_PR", fork)]);
            assert_eq!((code, calls.len()), (Some(0), 0), "{images:?} {fork:?}");
            assert!(h.result().is_none());
        }
        let (code, calls) = h.run(images, &[]);
        assert_eq!((code, calls.len()), (Some(2), 0), "{images:?}");
        assert!(h.result().is_none());
    }
}

// Attack: whoever made a source image writes its manifest or config so that
// grep and crane's Go JSON decoder read different things: an index whose
// `manifests` key is spelled with a \u escape or in another case (crane's
// `index append` would flatten it into the new index), or a config whose
// real `os` key is escaped, upper-cased or spelled with U+017F beside a
// decoy `os` label (the platform check would read the label). Safe outcome:
// exit 2 before `crane index append` runs, and no result.
#[test]
fn the_step_refuses_a_manifest_or_config_crane_would_read_differently() {
    let h = Harness::new();
    let a = by_digest('a');
    let arm64 = config(r#""architecture":"arm64","os":"linux""#);
    h.serve(
        '1',
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
        &arm64,
    );
    h.serve(
        '2',
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","Manifests":[]}"#,
        &arm64,
    );
    let decoy = |key: &str| {
        format!(
            r#"{{"architecture":"arm64","{key}":"windows","rootfs":{{"type":"layers","diff_ids":[]}},"config":{{"Labels":{{"os":"linux"}}}}}}"#
        )
    };
    h.serve('3', image_manifest(), &decoy(r"os"));
    h.serve('4', image_manifest(), &decoy("OS"));
    h.serve('5', image_manifest(), &decoy("o\u{17f}"));
    for c in ['1', '2', '3', '4', '5'] {
        let (code, calls) = h.run(&format!("{a}\n{}", by_digest(c)), &[]);
        assert_eq!(code, Some(2), "{c}: {calls:?}");
        assert!(
            calls.iter().all(|l| !l.starts_with("index ")),
            "{c}: {calls:?}"
        );
        assert!(h.result().is_none(), "{c}");
    }
}

/// What image builders write is not refused: Go's encoder escapes `&`, `<`
/// and `>` as `&`, `<` and `>` (a `RUN a && b` in the
/// history), and a label may hold any UTF-8 text.
#[test]
fn the_step_joins_images_whose_config_holds_escaped_html_and_utf8() {
    let h = Harness::new();
    h.serve(
        '0',
        image_manifest(),
        r#"{"architecture":"arm64","os":"linux","config":{"Labels":{"maintainer":"José <jose@example.com>"}},"history":[{"created_by":"RUN apt-get update && apt-get install -y curl"}],"rootfs":{"type":"layers","diff_ids":[]}}"#,
    );
    let (code, calls) = h.run(&format!("{}\n{}", by_digest('a'), by_digest('0')), &[]);
    assert_eq!(code, Some(0), "{calls:?}");
    assert!(h.result().is_some());
}

/// A reference crane reports that is not `<repository>@sha256:<64 hex>`
/// fails the Step (exit 1) and writes no result.
#[test]
fn the_step_fails_on_a_digest_crane_did_not_report() {
    let h = Harness::new();
    let images = format!("{}\n{}", by_digest('a'), by_digest('b'));
    for digest in ["sha256:abc", "latest", "sha256:\"}x"] {
        let (code, _) = h.run(&images, &[("CRANE_DIGEST", digest)]);
        assert_eq!(code, Some(1), "{digest}");
        assert!(h.result().is_none(), "{digest}");
    }
}
