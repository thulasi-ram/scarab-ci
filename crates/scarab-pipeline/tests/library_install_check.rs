//! `check_installed_library` (ADR-0076 §4): what an org admin may install.

use std::collections::BTreeMap;

use scarab_pipeline::{check_installed_library, InstalledLibrary};

fn none() -> BTreeMap<String, InstalledLibrary> {
    BTreeMap::new()
}

#[test]
fn a_library_with_a_required_input_and_no_values_installs() {
    let lib = r#"
kind: library
interface:
  inputs:
    - { name: release, type: string }
    - { name: wait, type: boolean, required: false, default: true }
steps:
  - id: upgrade
    image: alpine/helm:3.16.2
    env: [[RELEASE, "${{ inputs.release }}"]]
    command: [sh, -c, 'helm upgrade --install "$RELEASE" ./chart']
"#;
    check_installed_library("acme/deploy-helm", lib, &none()).unwrap();
}

#[test]
fn a_pipeline_is_not_a_library() {
    let errs = check_installed_library(
        "acme/x",
        "on: { push: {} }\nsteps: [{ id: a, image: busybox }]\n",
        &none(),
    )
    .unwrap_err();
    assert!(errs[0].contains("kind: library"), "{errs:?}");
}

#[test]
fn an_installed_library_may_not_invoke_a_file() {
    let lib = r#"
kind: library
steps:
  - { id: inner, invoke: file:.scarab/lib/x.yaml }
"#;
    let errs = check_installed_library("acme/outer", lib, &none()).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| e.contains("can invoke only `lib:` and `plugin:` targets")),
        "{errs:?}"
    );
}

#[test]
fn a_nested_library_must_be_installed_in_the_org() {
    let lib = r#"
kind: library
steps:
  - { id: inner, invoke: lib:acme/inner }
"#;
    let errs = check_installed_library("acme/outer", lib, &none()).unwrap_err();
    assert!(
        errs.iter()
            .any(|e| e.contains("`lib:acme/inner` is not installed in this org")),
        "{errs:?}"
    );
    let mut installed = none();
    installed.insert(
        "acme/inner".to_string(),
        InstalledLibrary {
            content: "kind: library\nsteps: [{ id: a, image: busybox }]\n".into(),
            source: "github.com/acme/libs//inner.yaml@abc".into(),
        },
    );
    check_installed_library("acme/outer", lib, &installed).unwrap();
}

#[test]
fn a_default_that_does_not_fit_its_type_is_refused() {
    let lib = r#"
kind: library
interface:
  inputs:
    - { name: n, type: number, required: false, default: "many" }
steps:
  - { id: a, image: busybox }
"#;
    assert!(check_installed_library("acme/x", lib, &none()).is_err());
}
