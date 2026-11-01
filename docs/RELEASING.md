# Releasing

How to cut Scarab release `X.Y.Z`. A release is a `vX.Y.Z` tag on main; the
workflows do the rest.

## 1. Keep the changelog current

Every pull request with a change users can see adds a line to
[CHANGELOG.md](../CHANGELOG.md) under `[Unreleased]`, in one of its parts:
**Upgrading**, **New**, **Fixes** or **Development**. A New entry links the
docs page that explains it, as a docs-site URL
(`https://thulasi-ram.github.io/scarab-ci/usage/libraries/`, anchors allowed)
or `README.md#<anchor>`. Write the entries the way
[WRITING.md](WRITING.md) asks, and claim only what
[positioning.md](positioning.md) allows.

## 2. Prepare the release pull request

On a branch from main:

1. Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, add an empty
   `## [Unreleased]` above it, and update the compare links at the end of the
   file.
2. Bump the version in `Cargo.toml` (`workspace.package.version`), in
   `deploy/helm/scarab/Chart.yaml` (`version` and `appVersion`), in the install
   line of `deploy/helm/scarab/README.md`, and in `openapi.json`
   (`cargo run -p scarab-server -- --emit-openapi openapi.json`). Run
   `cargo check` so `Cargo.lock` follows.
3. If `docker/clone` changed since the last release, pin `libs/clone.yaml` to
   a published build of it: `scripts/pin_clone_image.sh sha-<commit>`. A
   release cannot pin the image it builds itself.
4. If the UI changed, run `just release screenshots` and look at the images
   before you commit them.
5. Run `just release X.Y.Z`. It checks:
   - `Cargo.toml` and both `Chart.yaml` fields say `X.Y.Z`;
   - `libs/clone.yaml` pins one `scarab-clone` image by digest, and not the
     `X.Y.Z` build;
   - `CHANGELOG.md` has a `[X.Y.Z]` section, and prints the Release text
     release.yml will write from it;
   - every New entry under `[Unreleased]` and `[X.Y.Z]` links a docs page
     that exists;
   - a fresh capture of the mock UI matches every committed screenshot
     (diffs go to `target/release-screenshots/`).

   It runs every check, then lists each as PASS or FAIL.

Merge the pull request once `just release X.Y.Z` and the required checks
(`just pr <n>`) pass.

## 3. Tag

```sh
git checkout main && git pull
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin vX.Y.Z
```

The tag starts three workflows:

- `image.yml` checks the versions again, builds and pushes the four images,
  and publishes the chart to `oci://ghcr.io/thulasi-ram/charts/scarab`.
- `release.yml` creates the GitHub Release. Its text is the `[X.Y.Z]` section
  of CHANGELOG.md, the install lines, and GitHub's list of merged pull
  requests. It fails when the section is missing. It also uploads
  `libs-manifest.json` and the library files.
- `docs.yml` publishes the docs for the `X.Y` line.

To change the Release text afterwards, edit CHANGELOG.md on main and edit the
Release by hand; a re-run of release.yml leaves an existing Release's text as
it is.
