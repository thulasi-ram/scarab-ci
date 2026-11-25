//! The push Step's script of `libs/build.yaml` (git-bug 23a29ab), as the
//! compiler hands it to the executor, run with the host's `/bin/sh` against
//! a stub `crane` on `PATH` that records its arguments and keeps a copy of
//! the `index.json` it was asked to push. These cases are about what crane
//! reads from the layout: the script reads `index.json` with grep, crane
//! with Go's JSON decoder, and the two must not name different manifests.
//! The script's other cases sit beside the executor's unit tests.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const BUILD: &str = include_str!("../../../libs/build.yaml");

/// The manifest the layout holds, and its digest: sha256("{}").
const MANIFEST: &str = "{}";
const DIGEST: &str = "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
/// Another manifest, stored under its own digest: sha256("[]").
const OTHER: &str = "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

/// The push Step's `/bin/sh -c` script, with the workspace moved to `ws`
/// and the results directory to `results`.
fn script(ws: &Path, results: &Path) -> String {
    let ir = scarab_pipeline::compile_with_libraries(
        "steps:\n  - id: image\n    invoke: lib:thulasi-ram::scarab-ci::build\n    with: { image: registry.example/app:1, push: true }\n",
        &scarab_pipeline::Libraries {
            installed: std::collections::BTreeMap::from([(
                "thulasi-ram::scarab-ci::build".to_string(),
                scarab_pipeline::InstalledLibrary {
                    content: BUILD.to_string(),
                    source: "github.com/thulasi-ram/scarab-ci//libs/build.yaml@1".into(),
                },
            )]),
            ..Default::default()
        },
    )
    .unwrap();
    let step = ir.steps.iter().find(|s| s.id == "image/push").unwrap();
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
    stderr: String,
    /// crane's arguments, one per line; empty when crane did not run.
    crane: Vec<String>,
    /// The `index.json` crane was asked to push, when it ran.
    saw: Option<String>,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        for d in ["bin", "workspace", "results"] {
            std::fs::create_dir(base.join(d)).unwrap();
        }
        let stub = base.join("bin/crane");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$CRANE_LOG"
[ "$1" != push ] || cp "$2/index.json" "$CRANE_SAW"
echo "registry.example/app@$CRANE_DIGEST"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = script(&base.join("workspace"), &base.join("results"));
        Harness {
            _tmp: tmp,
            base,
            script,
        }
    }

    /// The layout the build Step `image/image` writes:
    /// `workspace/.scarab-build/<key>/oci`.
    fn oci(&self) -> PathBuf {
        self.base
            .join("workspace/.scarab-build")
            .join(key("image/image"))
            .join("oci")
    }

    /// Writes the layout the build Step leaves: [`MANIFEST`] under
    /// [`DIGEST`], [`OTHER`]'s manifest beside it, and `index` as its
    /// `index.json`.
    fn layout(&self, index: &str) {
        let oci = self.oci();
        let _ = std::fs::remove_dir_all(self.base.join("workspace/.scarab-build"));
        let blobs = oci.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(oci.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        std::fs::write(oci.join("index.json"), index).unwrap();
        std::fs::write(blobs.join(DIGEST.trim_start_matches("sha256:")), MANIFEST).unwrap();
        std::fs::write(blobs.join(OTHER.trim_start_matches("sha256:")), "[]").unwrap();
    }

    /// Runs the script over the defaults (push `registry.example/app:1`,
    /// not a fork, not insecure, no registry login, the build Step reported
    /// [`DIGEST`] and the layout at [`Harness::oci`], crane reports
    /// [`DIGEST`]; SCARAB_STEP unset, as the script no longer reads it),
    /// then `given`.
    fn run(&self, given: &[(&str, &str)]) -> Ran {
        let log = self.base.join("called");
        let saw = self.base.join("saw");
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(&saw);
        let _ = std::fs::remove_file(self.base.join("results/push.json"));
        let path = format!(
            "{}:{}",
            self.base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", &self.script, "scarab-push"])
            .current_dir(&self.base)
            .env_remove("DOCKER_CONFIG")
            .env("PATH", path)
            .env("CRANE_LOG", &log)
            .env("CRANE_SAW", &saw)
            .env("CRANE_DIGEST", DIGEST)
            .env("SCARAB_BUILD_IMAGE", "registry.example/app:1")
            .env("SCARAB_BUILD_PUSH", "true")
            .env("SCARAB_BUILD_INSECURE_PUSH", "false")
            .env("SCARAB_BUILD_DIGEST", DIGEST)
            .env("SCARAB_BUILD_LAYOUT", self.oci())
            .env_remove("SCARAB_STEP")
            .env("SCARAB_FORK_PR", "false");
        for (name, value) in given {
            cmd.env(name, value);
        }
        let out = cmd.output().unwrap();
        Ran {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            crane: std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect(),
            saw: std::fs::read_to_string(&saw).ok(),
        }
    }

    fn pushed(&self) -> bool {
        self.base.join("results/push.json").exists()
    }
}

/// The first 32 hex characters of the SHA-256 of the build Step's id: the
/// key its layout's directory is named by, computed here rather than copied
/// from the script.
fn key(step: &str) -> String {
    scarab_storage::sha256_hex(step.as_bytes())[..32].to_string()
}

/// A JSON backslash-u escape of the code point `hex4`, written with the
/// backslash as a char so no editor or tool reads the escape on the way.
fn esc(hex4: &str) -> String {
    format!("{}u{hex4}", char::from(92u8))
}

/// `index.json` as BuildKit writes it, naming `digest`.
fn buildkit_index(digest: &str) -> String {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{digest}","size":2,"annotations":{{"org.opencontainers.image.created":"2026-11-19T00:00:00Z"}}}}]}}"#
    )
}

/// crane does not read the layout's own `index.json`: the script writes one
/// naming only the manifest it checked, by digest and size, and crane is
/// asked to push that.
#[test]
fn crane_reads_an_index_json_naming_only_the_checked_manifest() {
    let h = Harness::new();
    h.layout(&buildkit_index(DIGEST));
    let ran = h.run(&[]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let layout = h.oci().display().to_string();
    assert_eq!(
        ran.crane,
        ["push", layout.as_str(), "registry.example/app:1"]
    );
    assert_eq!(
        ran.saw.as_deref(),
        Some(
            format!(
                "{{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[{{\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"digest\":\"{DIGEST}\",\"size\":{}}}]}}\n",
                MANIFEST.len()
            )
            .as_str()
        )
    );
    assert!(h.pushed());
}

// Attack: a process a Dockerfile RUN left behind rewrites index.json so that
// grep finds the built manifest in a decoy top-level "digest" while crane's
// Go decoder, which matches keys in any case, decodes \u escapes and folds
// U+017F onto "s", reads another manifest from the descriptor: a "Digest"
// key, a "digest" key, a "digeſt" key. Safe outcome: exit 2 before
// crane runs, and no `push` result.
#[test]
fn an_index_json_crane_would_read_differently_is_refused() {
    let h = Harness::new();
    let decoy = |key: &str| {
        format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","{key}":"{OTHER}","size":2}}],"digest":"{DIGEST}"}}"#
        )
    };
    for (what, index) in [
        ("a Digest key", decoy("Digest")),
        (
            "an escaped digest key",
            decoy(&format!("di{}est", esc("0067"))),
        ),
        ("a non-ASCII digest key", decoy("dige\u{17f}t")),
    ] {
        h.layout(&index);
        let ran = h.run(&[]);
        assert_eq!(ran.code, Some(2), "{what}: {}", ran.stderr);
        assert!(ran.crane.is_empty(), "{what}: crane ran: {:?}", ran.crane);
        assert!(!h.pushed(), "{what}");
    }
}

/// #219: when Scarab does not collect results (no /scarab/results) the
/// build Step's digest cannot reach this Step, so nothing could check the
/// layout against it: the push is refused (exit 2) before crane runs, and
/// the message names the setting that turns results on.
#[test]
fn without_results_the_push_is_refused() {
    let h = Harness::new();
    std::fs::remove_dir(h.base.join("results")).unwrap();
    for given in [
        &[][..],
        &[("SCARAB_BUILD_DIGEST", ""), ("SCARAB_BUILD_LAYOUT", "")][..],
    ] {
        h.layout(&buildkit_index(DIGEST));
        let ran = h.run(given);
        assert_eq!(ran.code, Some(2), "{given:?}: {}", ran.stderr);
        assert_eq!(
            ran.stderr.trim_end(),
            "scarab: push refused: results are not collected, so the image's digest cannot be \
             checked against the build step's; set the server's results token secret \
             (SCARAB_RESULTS_TOKEN_SECRET, the chart value secrets.resultsTokenSecret)",
            "{given:?}"
        );
        assert!(
            ran.crane.is_empty(),
            "{given:?}: crane ran: {:?}",
            ran.crane
        );
        assert!(!h.base.join("called").exists(), "{given:?}");
    }
}

/// `push: false` and a fork's Run exit 0 before the results check: they
/// push nothing, so nothing needs checking.
#[test]
fn push_false_and_fork_exit_before_the_results_check() {
    let h = Harness::new();
    std::fs::remove_dir(h.base.join("results")).unwrap();
    for given in [
        &[("SCARAB_BUILD_PUSH", "false")][..],
        &[("SCARAB_FORK_PR", "true")][..],
        &[("SCARAB_FORK_PR", "")][..],
    ] {
        h.layout(&buildkit_index(DIGEST));
        let ran = h.run(given);
        assert_eq!(ran.code, Some(0), "{given:?}: {}", ran.stderr);
        assert!(
            ran.crane.is_empty(),
            "{given:?}: crane ran: {:?}",
            ran.crane
        );
    }
}

/// The layout comes from the build Step's result, never from this Step's
/// id: an unrelated SCARAB_STEP changes nothing.
#[test]
fn with_results_the_path_ignores_scarab_step() {
    let h = Harness::new();
    h.layout(&buildkit_index(DIGEST));
    let ran = h.run(&[("SCARAB_STEP", "other/thing")]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    let layout = h.oci().display().to_string();
    assert_eq!(
        ran.crane,
        ["push", layout.as_str(), "registry.example/app:1"]
    );
    assert!(h.pushed());
}

// Attack: the build Step's `image` result is written in the build Pod, where
// a process a Dockerfile RUN left behind can rewrite it, so its `layout`
// names a directory the build did not write: elsewhere on the Pod's disk,
// up out of .scarab-build, a key that is not one, or the key's directory
// rather than the layout. Safe outcome: exit 2 before crane runs, though a
// valid layout sits at the build Step's own path.
#[test]
fn a_layout_path_the_result_names_outside_the_build_directory_is_refused() {
    let h = Harness::new();
    let root = h.base.join("workspace").display().to_string();
    let k = key("image/image");
    for layout in [
        format!("/etc/x/{k}/oci"),
        format!("{root}/.scarab-build/../{k}/oci"),
        format!("{root}/.scarab-build/{}/oci", &k[..31]),
        format!("{root}/.scarab-build/{}/oci", k.to_uppercase()),
        format!("{root}/.scarab-build/{k}"),
        format!("{root}/.scarab-build/{k}/oci/"),
    ] {
        h.layout(&buildkit_index(DIGEST));
        let ran = h.run(&[("SCARAB_BUILD_LAYOUT", layout.as_str())]);
        assert_eq!(ran.code, Some(2), "{layout}: {}", ran.stderr);
        assert!(ran.crane.is_empty(), "{layout}: crane ran: {:?}", ran.crane);
        assert!(!h.pushed(), "{layout}");
    }
}

// Attack: results are collected but the build Step's result names no layout,
// or no digest (a rewritten result, or a build of another library). Pushing
// whatever sits at the build Step's path, or pushing unchecked, would push
// what the build may not have built. Safe outcome: exit 2 before crane
// runs, naming what the result lacks, though a valid layout sits there.
#[test]
fn empty_layout_or_digest_is_refused_with_results() {
    let h = Harness::new();
    for (given, want) in [
        (
            ("SCARAB_BUILD_LAYOUT", ""),
            "scarab: push refused: the build step reported no image layout (its image result has no layout)",
        ),
        (
            ("SCARAB_BUILD_DIGEST", ""),
            "scarab: push refused: the build step reported no image digest (its image result has no containerimage.digest)",
        ),
    ] {
        h.layout(&buildkit_index(DIGEST));
        let ran = h.run(&[given]);
        assert_eq!(ran.code, Some(2), "{given:?}: {}", ran.stderr);
        assert_eq!(ran.stderr.trim_end(), want, "{given:?}");
        assert!(ran.crane.is_empty(), "{given:?}: crane ran: {:?}", ran.crane);
        assert!(!h.pushed());
    }
}
