//! The script of `libs/image-index.yaml` (git-bug 794b3ac), as the compiler
//! hands it to the executor, run with the host's `/bin/sh` against a stub
//! `crane` on `PATH` that serves manifests and configs from files and
//! records each call. The Step holds the registry credential, so what it
//! refuses matters: images by tag, two images of one platform, an index
//! as a source, `--insecure` beside a registry login, and (issue #218) a
//! layout in the workspace that is not exactly what a build wrote: every
//! check runs before the first registry write.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const INDEX: &str = include_str!("../../../libs/image-index.yaml");
/// The index digest the stub prints unless a case sets `CRANE_DIGEST`.
const PUSHED: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
/// A `given` value that removes the variable instead of setting it.
const UNSET: &str = "\u{0}unset";

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

const AMD64: &str = r#""architecture":"amd64","os":"linux""#;
const ARM64: &str = r#""os":"linux","architecture":"arm64","variant":"v8""#;
const ARM_V7: &str = r#""architecture":"arm","os":"linux","variant":"v7""#;
const ARM_V6: &str = r#""architecture":"arm","os":"linux","variant":"v6""#;

/// The first 32 hex characters of the SHA-256 of a build Step's id: the key
/// its layout's directory is named by, computed here rather than copied
/// from the build script.
fn key(step: &str) -> String {
    scarab_storage::sha256_hex(step.as_bytes())[..32].to_string()
}

/// A manifest naming the config `digest` of `size` bytes and no layers.
fn manifest_naming(digest: &str, size: usize) -> String {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{digest}","size":{size}}},"layers":[]}}"#
    )
}

/// `index.json` as BuildKit's oci exporter writes it: one manifest, with an
/// annotation.
fn buildkit_index(digest: &str, size: usize) -> String {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{digest}","size":{size},"annotations":{{"org.opencontainers.image.created":"2026-11-25T00:00:00Z"}}}}]}}"#
    )
}

/// The `index.json` the Step writes for crane: the checked manifest alone.
fn rewritten_index(digest: &str, size: usize) -> String {
    format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[{{\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"digest\":\"{digest}\",\"size\":{size}}}]}}\n"
    )
}

/// Stores `bytes` in the layout `oci` under their digest, and returns it.
fn put(oci: &Path, bytes: &[u8]) -> String {
    let hex = scarab_storage::sha256_hex(bytes);
    std::fs::write(oci.join("blobs/sha256").join(&hex), bytes).unwrap();
    format!("sha256:{hex}")
}

/// An image layout a build left in the workspace.
struct Layout {
    /// `<workspace>/.scarab-build/<key>/oci`.
    oci: PathBuf,
    /// The manifest's digest, as the build reported it.
    digest: String,
    /// The manifest's size in bytes.
    size: usize,
}

impl Layout {
    /// Its `layouts` line: `<path> <digest>`.
    fn line(&self) -> String {
        format!("{} {}", self.oci.display(), self.digest)
    }

    /// The path of the blob `digest` in it.
    fn blob(&self, digest: &str) -> PathBuf {
        self.oci
            .join("blobs/sha256")
            .join(digest.trim_start_matches("sha256:"))
    }
}

/// The Step's `/bin/sh -c` script, with the workspace moved to `ws` and its
/// results directory to `results`.
fn script(ws: &Path, results: &Path) -> String {
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
    let pinned = "root=/workspace\n";
    assert!(command[2].contains(pinned) && command[2].contains("/scarab/results"));
    command[2]
        .replace(pinned, &format!("root='{}'\n", ws.display()))
        .replace("/scarab/results", &results.display().to_string())
}

struct Harness {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    script: String,
}

/// What one run of the script did.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// crane's calls, one per line; empty when crane did not run.
    calls: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        for d in ["bin", "fixtures", "results", "registry", "workspace"] {
            std::fs::create_dir(base.join(d)).unwrap();
        }
        // A registry login, for the cases that give DOCKER_CONFIG.
        std::fs::write(base.join("registry/config.json"), "{}").unwrap();
        let stub = base.join("bin/crane");
        // `push <dir> <repo>@<digest>` reports the digest it was given (or
        // CRANE_PUSH_DIGEST) and keeps the index.json it was asked to push.
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
  push)
    cat "$2/index.json" >> "$CRANE_SAW"
    echo "registry.example/app@${CRANE_PUSH_DIGEST:-${3##*@}}"
    ;;
  tag) ;;
  *) exit 99 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = script(&base.join("workspace"), &base.join("results"));
        let h = Harness {
            _tmp: tmp,
            base,
            script,
        };
        // amd64 (a), arm64 (b), a second amd64 (d), arm v7 (e) and v6 (f).
        h.image('a', AMD64);
        h.image('b', ARM64);
        h.image('d', AMD64);
        h.image('e', ARM_V7);
        h.image('f', ARM_V6);
        h
    }

    fn ws(&self) -> PathBuf {
        self.base.join("workspace")
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

    /// Writes the layout the build Step `step` leaves: a config holding
    /// `platform`, a manifest naming it, both under their digests, and a
    /// BuildKit `index.json` naming the manifest.
    fn layout(&self, step: &str, platform: &str) -> Layout {
        self.layout_with(step, &config(platform), &manifest_naming)
    }

    /// [`Harness::layout`] with the config `config` and the manifest
    /// `manifest(<config digest>, <config size>)`.
    fn layout_with(
        &self,
        step: &str,
        config: &str,
        manifest: &dyn Fn(&str, usize) -> String,
    ) -> Layout {
        let oci = self.ws().join(".scarab-build").join(key(step)).join("oci");
        std::fs::create_dir_all(oci.join("blobs/sha256")).unwrap();
        std::fs::write(oci.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let c = put(&oci, config.as_bytes());
        let m = manifest(&c, config.len());
        let digest = put(&oci, m.as_bytes());
        std::fs::write(oci.join("index.json"), buildkit_index(&digest, m.len())).unwrap();
        Layout {
            oci,
            digest,
            size: m.len(),
        }
    }

    /// Runs the script with `images` and the given env over the defaults
    /// (target `registry.example/app:1`, not insecure, not a fork, no
    /// registry login, no layouts): its exit code and crane's calls, one
    /// per line.
    fn run(&self, images: &str, given: &[(&str, &str)]) -> (Option<i32>, Vec<String>) {
        let mut all = vec![("SCARAB_INDEX_IMAGES", images)];
        all.extend_from_slice(given);
        let ran = self.ran(&all);
        (ran.code, ran.calls)
    }

    /// Runs the script with the given env over the defaults of
    /// [`Harness::run`] and no `images`; a value of [`UNSET`] removes the
    /// variable.
    fn ran(&self, given: &[(&str, &str)]) -> Ran {
        let log = self.base.join("called");
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(self.saw_path());
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
            .env("CRANE_SAW", self.saw_path())
            .env("CRANE_FIXTURES", self.base.join("fixtures"))
            .env("CRANE_DIGEST", PUSHED)
            .env("SCARAB_INDEX_IMAGE", "registry.example/app:1")
            .env("SCARAB_INDEX_IMAGES", "")
            .env("SCARAB_INDEX_INSECURE", "false")
            .env("SCARAB_FORK_PR", "false");
        for name in [
            "CRANE_PUSH_DIGEST",
            "SCARAB_INDEX_LAYOUTS",
            "SCARAB_INDEX_EXTRA_TAGS",
            "SCARAB_INDEX_ARCH_TAGS",
            "SCARAB_INDEX_RESULT",
            "SCARAB_INDEX_LAYOUT_COUNT",
        ] {
            cmd.env_remove(name);
        }
        for (name, value) in given {
            if *value == UNSET {
                cmd.env_remove(name);
            } else {
                cmd.env(name, value);
            }
        }
        let out = cmd.output().unwrap();
        Ran {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            calls: std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect(),
        }
    }

    fn saw_path(&self) -> PathBuf {
        self.base.join("saw")
    }

    /// The `index.json`s crane was asked to push, one after another.
    fn saw(&self) -> String {
        std::fs::read_to_string(self.saw_path()).unwrap_or_default()
    }

    fn result(&self) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(self.base.join("results/index.json")).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    /// Whether anything is at `.scarab-build` in the workspace.
    fn build_dir_kept(&self) -> bool {
        std::fs::symlink_metadata(self.ws().join(".scarab-build")).is_ok()
    }
}

/// Two images of two platforms: crane reads each one's manifest and config,
/// then joins them under the target tag, and the Step writes the index's
/// reference and digest as its `index` result. A caller of `images` alone
/// (issue #218 added `layouts`, `extra_tags` and `arch_tags`, none set here)
/// sees the calls it saw before, no `push` and no `tag`, and the workspace
/// is not touched: what a build left at `.scarab-build` stays.
#[test]
fn the_step_joins_images_of_distinct_platforms_and_reports_the_digest() {
    let h = Harness::new();
    let keep = h.ws().join(".scarab-build/keep");
    std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
    std::fs::write(&keep, "kept").unwrap();
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

    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "kept");

    // Variants tell two arm images apart; one image alone is an index too.
    let (e, f) = (by_digest('e'), by_digest('f'));
    assert_eq!(h.run(&format!("{e}\n{f}"), &[]).0, Some(0));
    assert_eq!(h.run(&a, &[]).0, Some(0));
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "kept");
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
    let long_tag = format!("registry.example/app:{}", "t".repeat(129));
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
        // The target's tag: a leading '.', over 128 characters, a '+'.
        (two.clone(), vec![("SCARAB_INDEX_IMAGE", "registry.example/app:.1")]),
        (two.clone(), vec![("SCARAB_INDEX_IMAGE", long_tag.as_str())]),
        (two.clone(), vec![("SCARAB_INDEX_IMAGE", "registry.example/app:1+x")]),
        // arch_tags is a boolean; the result name is a file name in
        // /scarab/results.
        (two.clone(), vec![("SCARAB_INDEX_ARCH_TAGS", "yes")]),
        (two.clone(), vec![("SCARAB_INDEX_RESULT", "../x")]),
        (two.clone(), vec![("SCARAB_INDEX_RESULT", "A")]),
        (two.clone(), vec![("SCARAB_INDEX_RESULT", "1x")]),
        (two.clone(), vec![("SCARAB_INDEX_RESULT", "")]),
    ] {
        let (code, calls) = h.run(&images, &given);
        assert_eq!((code, calls.len()), (Some(2), 0), "{images:?} {given:?}");
        assert!(h.result().is_none(), "{images:?} {given:?}");
    }
    assert_eq!(
        std::fs::read_dir(h.base.join("results")).unwrap().count(),
        0,
        "no file written in the results directory"
    );
    assert!(!h.base.join("x.json").exists());
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

// ---------------------------------------------------------------------------
// Layouts (issue #218): images a build left in the workspace, pushed here.
// ---------------------------------------------------------------------------

/// Two layouts, amd64 and arm64/v8, and two extra tags: every image is
/// pushed by digest into the index's repository, then tagged per
/// architecture, then joined under the target tag, then the index is
/// tagged with each extra tag, in that order. crane is asked to push an
/// `index.json` naming only the checked manifest, and the layouts are
/// removed from the workspace after.
#[test]
fn layouts_are_pushed_by_digest_then_arch_tagged_then_indexed_then_extra_tagged() {
    let h = Harness::new();
    let one = h.layout("app/amd64", AMD64);
    let two = h.layout("app/arm64", ARM64);
    let ran = h.ran(&[
        (
            "SCARAB_INDEX_LAYOUTS",
            &format!("{}\n{}\n", one.line(), two.line()),
        ),
        ("SCARAB_INDEX_EXTRA_TAGS", "latest stable"),
    ]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let repo = "registry.example/app";
    let (d1, d2) = (&one.digest, &two.digest);
    assert_eq!(
        ran.calls,
        [
            format!("push {} {repo}@{d1}", one.oci.display()),
            format!("push {} {repo}@{d2}", two.oci.display()),
            format!("tag {repo}@{d1} 1-amd64"),
            format!("tag {repo}@{d2} 1-arm64-v8"),
            format!("index append -m {repo}@{d1} -m {repo}@{d2} -t {repo}:1"),
            format!("tag {repo}@{PUSHED} latest"),
            format!("tag {repo}@{PUSHED} stable"),
        ]
    );
    assert_eq!(
        h.saw(),
        format!(
            "{}{}",
            rewritten_index(d1, one.size),
            rewritten_index(d2, two.size)
        )
    );
    let result = h.result().expect("the index result");
    assert_eq!(
        result,
        serde_json::json!({ "ref": format!("{repo}@{PUSHED}"), "digest": PUSHED })
    );
    assert!(!h.build_dir_kept(), "the layouts are removed");
}

/// One way a second layout is not what a build wrote; it gets the layout
/// and returns the `layouts` line to give for it, when not its own.
type Break = Box<dyn Fn(&Harness, &Layout) -> Option<String>>;

// Attack: a Step before this one, or a Dockerfile RUN, changes the second
// of two layouts after its build wrote it: symlinks at any level (to the
// credential mount, which crane would upload as a blob), a FIFO, a
// directory find cannot read, a missing or rewritten index.json, a manifest
// or config that does not hash to its name or is not the one `layouts`
// names, an index where an image should be, or a config crane would read
// differently than grep does. Safe outcome: exit 2 with no crane call at
// all (not even for the first, valid layout), no result, and the workspace
// left as it was.
#[test]
fn every_layout_check_runs_before_the_first_registry_write() {
    use std::os::unix::fs::symlink;

    fn elsewhere(h: &Harness) -> PathBuf {
        let out = h.base.join("elsewhere");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        out
    }
    let rows: Vec<(&str, Break)> = vec![
        (
            ".scarab-build is a symlink",
            Box::new(|h: &Harness, _: &Layout| {
                let out = elsewhere(h);
                std::fs::rename(h.ws().join(".scarab-build"), out.join(".scarab-build")).unwrap();
                symlink(out.join(".scarab-build"), h.ws().join(".scarab-build")).unwrap();
                None
            }),
        ),
        (
            ".scarab-build/<key> is a symlink",
            Box::new(|h: &Harness, l: &Layout| {
                let keyed = l.oci.parent().unwrap();
                let out = elsewhere(h);
                std::fs::rename(keyed, out.join("k")).unwrap();
                symlink(out.join("k"), keyed).unwrap();
                None
            }),
        ),
        (
            ".scarab-build/<key>/oci is a symlink",
            Box::new(|h: &Harness, l: &Layout| {
                let out = elsewhere(h);
                std::fs::rename(&l.oci, out.join("oci")).unwrap();
                symlink(out.join("oci"), &l.oci).unwrap();
                None
            }),
        ),
        (
            "a blob is a symlink to the credential",
            Box::new(|h: &Harness, l: &Layout| {
                symlink(
                    h.base.join("registry/config.json"),
                    l.oci.join("blobs/sha256").join("cd".repeat(32)),
                )
                .unwrap();
                None
            }),
        ),
        (
            "a FIFO in the layout",
            Box::new(|_: &Harness, l: &Layout| {
                let ok = std::process::Command::new("mkfifo")
                    .arg(l.oci.join("blobs/sha256").join("ef".repeat(32)))
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "mkfifo");
                None
            }),
        ),
        // A failing find is a refusal, not an empty list. (Run as root,
        // find reads the directory and reports the symlink: refused too.)
        (
            "a directory find cannot read, hiding a symlink",
            Box::new(|h: &Harness, l: &Layout| {
                let hidden = l.oci.join("ingest");
                std::fs::create_dir(&hidden).unwrap();
                symlink(h.base.join("registry/config.json"), hidden.join("x")).unwrap();
                std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
                None
            }),
        ),
        (
            "no oci-layout",
            Box::new(|_: &Harness, l: &Layout| {
                std::fs::remove_file(l.oci.join("oci-layout")).unwrap();
                None
            }),
        ),
        (
            "no index.json",
            Box::new(|_: &Harness, l: &Layout| {
                std::fs::remove_file(l.oci.join("index.json")).unwrap();
                None
            }),
        ),
        (
            "index.json holds a backslash",
            Box::new(|_: &Harness, l: &Layout| {
                let index = buildkit_index(&l.digest, l.size).replace("2026", r"202");
                std::fs::write(l.oci.join("index.json"), index).unwrap();
                None
            }),
        ),
        (
            "index.json holds a byte that is not ASCII",
            Box::new(|_: &Harness, l: &Layout| {
                let index = buildkit_index(&l.digest, l.size).replace("2026", "2026\u{e9}");
                std::fs::write(l.oci.join("index.json"), index).unwrap();
                None
            }),
        ),
        (
            "index.json names two manifests",
            Box::new(|_: &Harness, l: &Layout| {
                let other = put(&l.oci, b"[]");
                let index = format!(
                    r#"{{"schemaVersion":2,"manifests":[{{"digest":"{}","size":{}}},{{"digest":"{other}","size":2}}]}}"#,
                    l.digest, l.size
                );
                std::fs::write(l.oci.join("index.json"), index).unwrap();
                None
            }),
        ),
        (
            "the manifest does not hash to its name",
            Box::new(|_: &Harness, l: &Layout| {
                let text = std::fs::read_to_string(l.blob(&l.digest)).unwrap();
                std::fs::write(
                    l.blob(&l.digest),
                    text.replace("\"layers\":[]", "\"layers\":[ ]"),
                )
                .unwrap();
                None
            }),
        ),
        (
            "layouts names another digest",
            Box::new(|_: &Harness, l: &Layout| {
                Some(format!(
                    "{} sha256:{}",
                    l.oci.display(),
                    scarab_storage::sha256_hex(b"another image")
                ))
            }),
        ),
        (
            "the manifest is an index",
            Box::new(|_: &Harness, l: &Layout| {
                let index = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
                let d = put(&l.oci, index.as_bytes());
                std::fs::write(l.oci.join("index.json"), buildkit_index(&d, index.len())).unwrap();
                Some(format!("{} {d}", l.oci.display()))
            }),
        ),
        (
            "the manifest names two configs",
            Box::new(|_: &Harness, l: &Layout| {
                let m = std::fs::read_to_string(l.blob(&l.digest)).unwrap();
                let m = m.replace("\"layers\":[]", "\"Config\":{},\"layers\":[]");
                let d = put(&l.oci, m.as_bytes());
                std::fs::write(l.oci.join("index.json"), buildkit_index(&d, m.len())).unwrap();
                Some(format!("{} {d}", l.oci.display()))
            }),
        ),
        (
            "the manifest's config holds braces",
            Box::new(|_: &Harness, l: &Layout| {
                let m = std::fs::read_to_string(l.blob(&l.digest)).unwrap();
                let m = m.replace(
                    "\"mediaType\":\"application/vnd.oci.image.config.v1+json\",",
                    "\"mediaType\":\"application/vnd.oci.image.config.v1+json\",\"annotations\":{\"a\":\"b\"},",
                );
                let d = put(&l.oci, m.as_bytes());
                std::fs::write(l.oci.join("index.json"), buildkit_index(&d, m.len())).unwrap();
                Some(format!("{} {d}", l.oci.display()))
            }),
        ),
        (
            "the config blob is missing",
            Box::new(|_: &Harness, l: &Layout| {
                let config = scarab_storage::sha256_hex(config(ARM64).as_bytes());
                std::fs::remove_file(l.blob(&config)).unwrap();
                None
            }),
        ),
        (
            "the config does not hash to its name",
            Box::new(|_: &Harness, l: &Layout| {
                let config = scarab_storage::sha256_hex(config(ARM64).as_bytes());
                std::fs::write(l.blob(&config), crate::config(AMD64)).unwrap();
                None
            }),
        ),
        (
            "the config names no os or architecture",
            Box::new(|h: &Harness, _: &Layout| {
                let l = h.layout_with("app/arm64", r#"{"rootfs":{}}"#, &manifest_naming);
                Some(l.line())
            }),
        ),
        (
            "the config spells a key with a \\u escape",
            Box::new(|h: &Harness, _: &Layout| {
                let l = h.layout_with(
                    "app/arm64",
                    r#"{"architecture":"arm64","os":"linux","rootfs":{}}"#,
                    &manifest_naming,
                );
                Some(l.line())
            }),
        ),
    ];
    for (what, broken) in &rows {
        let h = Harness::new();
        let one = h.layout("app/amd64", AMD64);
        let two = h.layout("app/arm64", ARM64);
        let line = broken(&h, &two).unwrap_or_else(|| two.line());
        let ran = h.ran(&[("SCARAB_INDEX_LAYOUTS", &format!("{}\n{line}\n", one.line()))]);
        let _ = std::fs::set_permissions(
            two.oci.join("ingest"),
            std::fs::Permissions::from_mode(0o755),
        );
        assert_eq!(ran.code, Some(2), "{what}: {}", ran.stderr);
        assert!(ran.calls.is_empty(), "{what}: crane ran: {:?}", ran.calls);
        assert!(h.result().is_none(), "{what}");
        assert!(
            h.build_dir_kept(),
            "{what}: the workspace is left as it was"
        );
        assert!(
            ran.stderr.contains("scarab: index refused: "),
            "{what}: {}",
            ran.stderr
        );
    }
}

// Attack: a `layouts` line names a path that passes a loose prefix check
// but is not a build's layout: outside the build directory, up and back
// into it, one level deeper, a key of the wrong length or case, a space, a
// path that does not end in the layout, another root's build directory.
// Safe outcome: exit 2 before crane runs, though a valid layout exists for
// the build.
#[test]
fn a_layout_path_outside_the_build_directory_is_refused() {
    let h = Harness::new();
    let l = h.layout("app/amd64", AMD64);
    let root = h.ws().display().to_string();
    let k = key("app/amd64");
    let d = &l.digest;
    std::fs::create_dir_all(h.ws().join(".scarab-build/a")).unwrap();
    for path in [
        format!("{root}/x/{k}/oci"),
        format!("{root}/.scarab-build/../.scarab-build/{k}/oci"),
        format!("{root}/.scarab-build/a/{k}/oci"),
        format!("{root}/.scarab-build/{}/oci", &k[..31]),
        format!("{root}/.scarab-build/{k}0/oci"),
        format!("{root}/.scarab-build/{}/oci", k.to_uppercase()),
        format!("{root}/.scarab-build/{k} /oci"),
        format!("{root}/.scarab-build/{k}/oci/blobs"),
        format!("{root}/.scarab-build/{k}"),
        format!("/etc/.scarab-build/{k}/oci"),
    ] {
        let ran = h.ran(&[("SCARAB_INDEX_LAYOUTS", &format!("{path} {d}"))]);
        assert_eq!(ran.code, Some(2), "{path}: {}", ran.stderr);
        assert!(ran.calls.is_empty(), "{path}: crane ran: {:?}", ran.calls);
    }
    // The same layout by its own path pushes.
    let ran = h.ran(&[("SCARAB_INDEX_LAYOUTS", &l.line())]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
}

// Attack: `.scarab-build` is a real directory, but the key's directory is
// a symlink to a valid layout elsewhere in the workspace (another Step's
// output), so the path stays inside the workspace and is still not the
// build's. Safe outcome: exit 2 before crane runs.
#[test]
fn a_layout_resolving_elsewhere_is_refused() {
    let h = Harness::new();
    let l = h.layout("app/amd64", AMD64);
    let keyed = l.oci.parent().unwrap().to_path_buf();
    std::fs::rename(&keyed, h.ws().join("other")).unwrap();
    std::os::unix::fs::symlink("../other", &keyed).unwrap();
    let ran = h.ran(&[("SCARAB_INDEX_LAYOUTS", &l.line())]);
    assert_eq!(ran.code, Some(2), "{}", ran.stderr);
    assert!(ran.calls.is_empty(), "crane ran: {:?}", ran.calls);
}

/// `layouts` is one `<path> sha256:<64 lower-case hex>` per line; only line
/// breaks (what a fork's builds render) is no layout. Anything else (a
/// missing or doubled space, a digest of another form, an empty line among
/// layouts, more than one at the end, a path twice, the placeholder
/// build-multiarch renders for a build with no layout) exits 2 before crane
/// runs, so a build that published no layout is not left out of the index
/// unnoticed.
#[test]
fn malformed_layouts_lines_are_refused() {
    let h = Harness::new();
    let one = h.layout("app/amd64", AMD64);
    let two = h.layout("app/arm64", ARM64);
    let (p, d) = (one.oci.display().to_string(), one.digest.clone());
    let hex = d.trim_start_matches("sha256:");
    let (l1, l2) = (one.line(), two.line());
    for layouts in [
        format!("{p}{d}"),
        format!("{p}  {d}"),
        format!("{p} sha256:{}", hex.to_uppercase()),
        format!("{p} sha256:{}", &hex[..63]),
        format!("{p} sha512:{hex}"),
        format!("{p} {hex}"),
        format!("{l1}\n\n{l2}"),
        format!("\n{l1}"),
        format!("{l1}\n{l2}\n\n"),
        format!("{l1}\n{l1}"),
        "(amd64 wrote no layout)".to_string(),
        format!("{l1}\n(arm64 wrote no layout)"),
    ] {
        let ran = h.ran(&[("SCARAB_INDEX_LAYOUTS", &layouts)]);
        assert_eq!(ran.code, Some(2), "{layouts:?}: {}", ran.stderr);
        assert!(
            ran.calls.is_empty(),
            "{layouts:?}: crane ran: {:?}",
            ran.calls
        );
    }
    // One line break at the end is the form a `|` block renders.
    let ran = h.ran(&[("SCARAB_INDEX_LAYOUTS", &format!("{l1}\n{l2}\n"))]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    // Only line breaks beside images: no layout, and the images are joined.
    let ran = h.ran(&[
        ("SCARAB_INDEX_LAYOUTS", "\n\n"),
        ("SCARAB_INDEX_IMAGES", &by_digest('a')),
    ]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(
        ran.calls.last().map(String::as_str),
        Some(
            format!(
                "index append -m {} -t registry.example/app:1",
                by_digest('a')
            )
            .as_str()
        )
    );
}

// Attack: two layouts of one platform (two builds placed on one
// architecture's nodes), or a layout of the platform an `images` entry also
// is: the index would hold two entries a client picks between by order.
// Safe outcome: exit 2 before any registry write; with an `images` entry,
// only its manifest and config were read.
#[test]
fn two_layouts_of_one_platform_are_refused_before_any_write() {
    let h = Harness::new();
    let one = h.layout("app/amd64", AMD64);
    let two = h.layout("app/arm64", AMD64);
    let ran = h.ran(&[(
        "SCARAB_INDEX_LAYOUTS",
        &format!("{}\n{}", one.line(), two.line()),
    )]);
    assert_eq!(ran.code, Some(2), "{}", ran.stderr);
    assert!(ran.calls.is_empty(), "crane ran: {:?}", ran.calls);

    let a = by_digest('a');
    let ran = h.ran(&[
        ("SCARAB_INDEX_LAYOUTS", &one.line()),
        ("SCARAB_INDEX_IMAGES", &a),
    ]);
    assert_eq!(ran.code, Some(2), "{}", ran.stderr);
    assert_eq!(ran.calls, [format!("manifest {a}"), format!("config {a}")]);
    assert!(h.build_dir_kept());
}

/// The tags the Step would write are all checked first: an arch tag over
/// 128 characters, an extra tag that is an arch tag too, or one that is not
/// a tag name, exits 2 before crane runs. Variants keep two arm images'
/// arch tags apart.
#[test]
fn arch_and_extra_tags_over_128_or_colliding_are_refused() {
    let image = |n: usize| format!("registry.example/app:{}", "t".repeat(n));
    let (fits, over) = (image(122), image(123));
    for (what, target, extra, code) in [
        ("an arch tag of 128 characters", fits.as_str(), "", 0),
        ("an arch tag of 129 characters", over.as_str(), "", 2),
        (
            "an extra tag that is an arch tag",
            "registry.example/app:1",
            "1-amd64",
            2,
        ),
        (
            "an extra tag of 129 characters",
            "registry.example/app:1",
            &"e".repeat(129) as &str,
            2,
        ),
        (
            "an extra tag starting '.'",
            "registry.example/app:1",
            ".latest",
            2,
        ),
        (
            "an extra tag starting '-'",
            "registry.example/app:1",
            "-latest",
            2,
        ),
        (
            "an extra tag holding '/'",
            "registry.example/app:1",
            "a/b",
            2,
        ),
        (
            "an extra tag holding ':'",
            "registry.example/app:1",
            "a:b",
            2,
        ),
        (
            "an extra tag holding '@'",
            "registry.example/app:1",
            "a@b",
            2,
        ),
    ] {
        let h = Harness::new();
        let l = h.layout("app/amd64", AMD64);
        let ran = h.ran(&[
            ("SCARAB_INDEX_LAYOUTS", &l.line()),
            ("SCARAB_INDEX_IMAGE", target),
            ("SCARAB_INDEX_EXTRA_TAGS", extra),
        ]);
        assert_eq!(ran.code, Some(code), "{what}: {}", ran.stderr);
        if code == 2 {
            assert!(ran.calls.is_empty(), "{what}: crane ran: {:?}", ran.calls);
        } else {
            let tag = format!("{}-amd64", "t".repeat(122));
            assert_eq!(tag.len(), 128);
            assert!(
                ran.calls
                    .iter()
                    .any(|c| c == &format!("tag registry.example/app@{} {tag}", l.digest)),
                "{what}: {:?}",
                ran.calls
            );
        }
    }

    let h = Harness::new();
    let v6 = h.layout("app/v6", ARM_V6);
    let v7 = h.layout("app/v7", ARM_V7);
    let ran = h.ran(&[(
        "SCARAB_INDEX_LAYOUTS",
        &format!("{}\n{}", v6.line(), v7.line()),
    )]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let tags: Vec<&str> = ran
        .calls
        .iter()
        .filter(|c| c.starts_with("tag "))
        .map(String::as_str)
        .collect();
    assert_eq!(
        tags,
        [
            format!("tag registry.example/app@{} 1-arm-v6", v6.digest),
            format!("tag registry.example/app@{} 1-arm-v7", v7.digest),
        ]
    );
}

/// An extra tag named twice, or the same as the target's own tag, is
/// written once, and the target's tag only by `index append`.
#[test]
fn extra_tags_are_written_once() {
    let h = Harness::new();
    let l = h.layout("app/amd64", AMD64);
    let ran = h.ran(&[
        ("SCARAB_INDEX_LAYOUTS", &l.line()),
        ("SCARAB_INDEX_EXTRA_TAGS", "1 latest\nlatest"),
        ("SCARAB_INDEX_ARCH_TAGS", "false"),
    ]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let tags: Vec<&str> = ran
        .calls
        .iter()
        .filter(|c| c.starts_with("tag "))
        .map(String::as_str)
        .collect();
    assert_eq!(tags, [format!("tag registry.example/app@{PUSHED} latest")]);
}

/// A fork pull request's Run (SCARAB_FORK_PR `true`, or unset) exits 0
/// before it reads any input: garbage in `layouts` and no target are not
/// refused, nothing is pushed, and the workspace is not touched.
#[test]
fn a_fork_reads_no_input() {
    let h = Harness::new();
    let l = h.layout("app/amd64", AMD64);
    for fork in ["true", UNSET] {
        let ran = h.ran(&[
            ("SCARAB_FORK_PR", fork),
            ("SCARAB_INDEX_LAYOUTS", "../../etc oops"),
            ("SCARAB_INDEX_IMAGE", ""),
            ("SCARAB_INDEX_ARCH_TAGS", "maybe"),
        ]);
        assert_eq!(ran.code, Some(0), "{fork:?}: {}", ran.stderr);
        assert!(
            ran.stdout.starts_with("scarab: not pushed"),
            "{fork:?}: {}",
            ran.stdout
        );
        assert!(ran.calls.is_empty(), "{fork:?}: crane ran: {:?}", ran.calls);
        assert!(h.result().is_none());
        assert!(l.oci.join("index.json").is_file(), "{fork:?}");
    }
}

/// crane must report the digest the layout holds; another one fails the
/// Step (exit 1) before any tag or index is written, with no result.
#[test]
fn a_crane_push_reporting_another_digest_fails_the_step() {
    let h = Harness::new();
    let l = h.layout("app/amd64", AMD64);
    let ran = h.ran(&[
        ("SCARAB_INDEX_LAYOUTS", &l.line()),
        ("CRANE_PUSH_DIGEST", PUSHED),
    ]);
    assert_eq!(ran.code, Some(1), "{}", ran.stderr);
    assert_eq!(
        ran.calls,
        [format!(
            "push {} registry.example/app@{}",
            l.oci.display(),
            l.digest
        )]
    );
    assert!(h.result().is_none());
}

/// `arch_tags: false` writes no arch tag: the layout is pushed by digest and
/// joined, and nothing else.
#[test]
fn arch_tags_false_writes_no_arch_tag() {
    let h = Harness::new();
    let l = h.layout("app/amd64", AMD64);
    let ran = h.ran(&[
        ("SCARAB_INDEX_LAYOUTS", &l.line()),
        ("SCARAB_INDEX_ARCH_TAGS", "false"),
    ]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let d = &l.digest;
    assert_eq!(
        ran.calls,
        [
            format!("push {} registry.example/app@{d}", l.oci.display()),
            format!("index append -m registry.example/app@{d} -t registry.example/app:1"),
        ]
    );
}

// Attack: a build result whose `layout` holds a line break (written in the
// build Pod, where a Dockerfile RUN's process can reach it) adds a line to
// `layouts`, and so an image of its choosing to the index. Safe outcome:
// with SCARAB_INDEX_LAYOUT_COUNT set (build-multiarch sets 2, one per build
// Step), any other count of layouts exits 2 before crane runs; a count that
// is not a number is refused; unset or empty, the count is not checked.
#[test]
fn a_layout_count_other_than_the_one_expected_is_refused() {
    let h = Harness::new();
    let one = h.layout("app/amd64", AMD64);
    let two = h.layout("app/arm64", ARM64);
    let three = h.layout("app/arm", ARM_V7);
    let (l1, l2, l3) = (one.line(), two.line(), three.line());
    for (count, layouts, code) in [
        ("2", format!("{l1}\n{l2}\n{l3}"), 2),
        ("2", l1.clone(), 2),
        ("2", "\n".to_string(), 2),
        ("two", format!("{l1}\n{l2}"), 2),
        ("-1", format!("{l1}\n{l2}"), 2),
        ("", l1.clone(), 0),
        (UNSET, format!("{l1}\n{l2}\n{l3}"), 0),
        ("2", format!("{l1}\n{l2}"), 0),
    ] {
        // Each accepted run removes the layouts; put them back.
        let _ = h.layout("app/amd64", AMD64);
        let _ = h.layout("app/arm64", ARM64);
        let _ = h.layout("app/arm", ARM_V7);
        let ran = h.ran(&[
            ("SCARAB_INDEX_LAYOUT_COUNT", count),
            ("SCARAB_INDEX_LAYOUTS", &layouts),
        ]);
        assert_eq!(
            ran.code,
            Some(code),
            "{count:?} {layouts:?}: {}",
            ran.stderr
        );
        if code == 2 {
            assert!(
                ran.calls.is_empty(),
                "{count:?} {layouts:?}: crane ran: {:?}",
                ran.calls
            );
        }
    }
}
