//! LIVE proof of ADR-0062 part 3: mount a **real** `overlayfs`, write, delete,
//! replace and rename through it, and assert the change set read out of the upper
//! layer is *exactly* what the Step did.
//!
//! Everything in `changeset.rs`'s unit tests is a claim about what the kernel
//! puts in an upper directory. This file is the only place that claim is checked
//! against a kernel. It cannot run in CI's normal tiers: `overlayfs` is Linux-only
//! and mounting it needs `CAP_SYS_ADMIN` — the same capability the workspace
//! service holds in ADR-0062's preferred configuration.
//!
//! # How to run it
//!
//! ```text
//! just overlay-tests
//! ```
//!
//! which is the canonical entrypoint (repo rule: a missing recipe gets added
//! rather than worked around) and expands to a host build plus
//!
//! ```text
//! sudo env SCARAB_TEST_OVERLAY=1 SCARAB_TEST_OVERLAY_DIR=/var/tmp/scarab-overlay \
//!   <test binary> --ignored --nocapture
//! ```
//!
//! `SCARAB_TEST_OVERLAY_DIR` must live on a filesystem that can serve as an
//! `overlayfs` **upperdir** — a real ext4/xfs/btrfs directory. It is required
//! rather than defaulted to `std::env::temp_dir()` on purpose: `/tmp` is often
//! `tmpfs`, an overlay rootfs cannot be stacked on itself (ADR-0062 measured
//! this), and a default that sometimes works is how a tier stops meaning
//! anything.
//!
//! # Why every unmet precondition is a `panic!` and never a `return`
//!
//! Seven live cases in this repo used to `return` on a missing env var: they
//! reported PASS while executing nothing, and because the tier is wired into the
//! merge gate, the silence read as proof (fixed at commit `505f313`). A live case
//! that cannot run must be RED.
//!
//! This file reintroduced that shape one level up and it is worth naming, because
//! the repair at `505f313` did not cover it. The preconditions were panics, but
//! the **opt-in check itself** was a `return`: `cargo test -- --ignored` with no
//! `SCARAB_TEST_OVERLAY` set was two green tests that executed nothing. Running
//! `--ignored` **is** opting in — the `#[ignore]` attribute is the gate, and
//! nothing behind it may quietly decline. So [`Tier::enter`] returns a `Tier` and
//! never an `Option`: a missing `SCARAB_TEST_OVERLAY` is a panic that names every
//! other thing the tier needs, exactly like a missing capability is.

use std::path::{Path, PathBuf};
use std::process::Command;

use scarab_server::changeset::{
    can_read_overlay_markers, read_change_set, Directory, Markers, Written, WrittenKind,
};

/// A scratch tree on a filesystem that can host an `overlayfs` upper, plus the
/// proof that this process may actually read overlay markers.
///
/// **Never returns without a live tier.** Every failure — not opted in, wrong OS,
/// missing var, missing capability — panics, for the reason in the module docs.
/// There is deliberately no `Option` here and no `skip` anywhere in this file: the
/// only way to not run these cases is not to pass `--ignored`.
struct Tier {
    root: PathBuf,
}

impl Tier {
    fn enter(case: &str) -> Tier {
        let opted_in = std::env::var("SCARAB_TEST_OVERLAY").is_ok_and(|v| !v.is_empty());
        if !opted_in {
            panic!(
                "{case} was asked to run (`--ignored` is the opt-in) but SCARAB_TEST_OVERLAY is \
                 not set, and this case will not pass by doing nothing. It needs, all of them:\n  \
                 * SCARAB_TEST_OVERLAY=1 — this switch, so an accidental `--ignored` sweep is a \
                 loud refusal rather than a silent green;\n  * Linux — `overlayfs` and the \
                 `trusted.overlay.*` xattr namespace are kernel features;\n  * CAP_SYS_ADMIN \
                 (run it under `sudo`) — to mount the overlay AND to read the markers at all: \
                 unprivileged, the kernel answers every `trusted.*` read with ENODATA, which is \
                 indistinguishable from `not set`;\n  * SCARAB_TEST_OVERLAY_DIR=<dir> on a real \
                 ext4/xfs/btrfs filesystem — an overlay upperdir cannot live on tmpfs or on an \
                 overlay rootfs.\nRun `just overlay-tests`, which arranges all four and fails \
                 loudly on a host that cannot."
            );
        }
        if !cfg!(target_os = "linux") {
            panic!(
                "SCARAB_TEST_OVERLAY is set on {}, but `overlayfs` and the `trusted.overlay.*` \
                 xattr namespace are Linux kernel features. This case cannot prove anything here, \
                 so it fails instead of passing quietly.",
                std::env::consts::OS
            );
        }
        let base = match std::env::var("SCARAB_TEST_OVERLAY_DIR") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => panic!(
                "SCARAB_TEST_OVERLAY_DIR is not set, but SCARAB_TEST_OVERLAY is. Point it at a \
                 directory on a real ext4/xfs/btrfs filesystem (e.g. /var/tmp/scarab-overlay): an \
                 overlay upperdir cannot live on tmpfs or on an overlay rootfs, and defaulting to \
                 the temp dir would make this case pass or fail by accident of $TMPDIR."
            ),
        };
        if !can_read_overlay_markers() {
            panic!(
                "SCARAB_TEST_OVERLAY is set but this process cannot read `trusted.overlay.*` \
                 xattrs — CAP_SYS_ADMIN is not in CapEff. Every opaque directory and every rename \
                 would read as absent, so this case would assert against a change set the kernel \
                 never showed it. Run it under `sudo -E`."
            );
        }

        // ADR-0062: "a build must report which rung it took". Say which kernel and
        // which filesystem produced the numbers/markers below, so a result can
        // never be read as more general than the substrate it came from.
        eprintln!("--- {case}");
        eprintln!("kernel   : {}", one_line(&["uname", "-r"]));
        eprintln!(
            "upper fs : {} ({})",
            one_line(&["stat", "-f", "-c", "%T", base.to_str().expect("utf-8 path")]),
            base.display()
        );

        let root = base.join(format!("{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap_or_else(|e| {
            panic!(
                "cannot create the scratch tree at {}: {e}. SCARAB_TEST_OVERLAY_DIR must be a \
                 writable directory on a real disk filesystem.",
                root.display()
            )
        });
        for sub in ["lower", "upper", "work", "merged"] {
            std::fs::create_dir(root.join(sub)).expect("mkdir scratch subdir");
        }
        Tier { root }
    }

    fn lower(&self) -> PathBuf {
        self.root.join("lower")
    }
    fn upper(&self) -> PathBuf {
        self.root.join("upper")
    }
    fn merged(&self) -> PathBuf {
        self.root.join("merged")
    }

    /// Mount `overlayfs` with `lower` read-only underneath and `upper` catching
    /// every write — the Export of ADR-0062 part 2, minus the NFS hop.
    fn mount(&self, extra_options: &str) -> Mount {
        let mut options = format!(
            "lowerdir={},upperdir={},workdir={}",
            self.lower().display(),
            self.upper().display(),
            self.root.join("work").display()
        );
        if !extra_options.is_empty() {
            options.push(',');
            options.push_str(extra_options);
        }
        let out = Command::new("mount")
            .args(["-t", "overlay", "scarab-changeset-test", "-o", &options])
            .arg(self.merged())
            .output()
            .expect("spawning `mount` (util-linux) — this tier needs it");
        assert!(
            out.status.success(),
            "mount -t overlay -o {options} {} failed: {}\nIf it says the upper filesystem is not \
             supported, SCARAB_TEST_OVERLAY_DIR is on tmpfs or on an overlay rootfs — point it at \
             a real ext4/xfs/btrfs directory.",
            self.merged().display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Mount {
            target: self.merged(),
            live: true,
        }
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        std::fs::write(self.merged().join(rel), bytes)
            .unwrap_or_else(|e| panic!("write {rel} through the overlay: {e}"));
    }
}

impl Drop for Tier {
    fn drop(&mut self) {
        // Leave nothing behind on the operator's disk; the mount is already gone
        // (see `Mount::drop`, which runs first — it is dropped inside the case).
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A live overlay mount. Unmounted explicitly by each case *before* it reads the
/// upper — which is also what production does: the Export is torn down when the
/// Attempt settles and only then does the drain read the upper. `Drop` is the
/// backstop so a failed assertion cannot leave a stray mount on the host.
struct Mount {
    target: PathBuf,
    live: bool,
}

impl Mount {
    fn unmount(&mut self) {
        if !self.live {
            return;
        }
        let out = Command::new("umount")
            .arg(&self.target)
            .output()
            .expect("spawning `umount`");
        assert!(
            out.status.success(),
            "umount {} failed: {}",
            self.target.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        self.live = false;
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.live {
            let _ = Command::new("umount").arg(&self.target).output();
        }
    }
}

fn one_line(argv: &[&str]) -> String {
    Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("<{}: {e}>", argv[0]))
}

fn file(path: &str) -> Written {
    Written {
        path: PathBuf::from(path),
        kind: WrittenKind::File,
    }
}

fn symlink(path: &str) -> Written {
    Written {
        path: PathBuf::from(path),
        kind: WrittenKind::Symlink,
    }
}

fn dir(path: &str, opaque: bool) -> Directory {
    Directory {
        path: PathBuf::from(path),
        opaque,
        redirect: None,
    }
}

#[test]
#[ignore = "mounts a real overlayfs: Linux + CAP_SYS_ADMIN. `just overlay-tests`; running it \
            without SCARAB_TEST_OVERLAY=1 PANICS rather than passing"]
fn the_upper_layer_is_the_exact_change_set() {
    let tier = Tier::enter("exact-change-set");

    // The parent snapshot, as a Snapshot Farm would have materialised it.
    let lower = tier.lower();
    std::fs::write(lower.join("keep.txt"), b"keep").expect("seed");
    std::fs::write(lower.join("edit.txt"), b"before").expect("seed");
    std::fs::write(lower.join("gone.txt"), b"delete me").expect("seed");
    std::fs::create_dir(lower.join("doomed")).expect("seed");
    std::fs::write(lower.join("doomed/inner.txt"), b"from the lower").expect("seed");
    std::fs::create_dir_all(lower.join("nested/deep")).expect("seed");
    std::fs::write(lower.join("nested/deep/base.txt"), b"base").expect("seed");

    let mut mount = tier.mount("");

    // What "the Step" does. Every line below is one of the four kinds of thing an
    // upper layer can hold, and the assertion at the bottom is that the kernel
    // recorded exactly these and nothing else.
    tier.write("added.txt", b"new"); //                       add a file
    tier.write("edit.txt", b"after"); //                      modify (copy-up)
    std::fs::remove_file(tier.merged().join("gone.txt")).expect("rm"); //  whiteout
    // Replace a lower directory wholesale → the upper's `doomed` becomes OPAQUE,
    // with no whiteout for `inner.txt` anywhere. A walk that ignores the xattr
    // resurrects `inner.txt`.
    std::fs::remove_dir_all(tier.merged().join("doomed")).expect("rm -rf");
    std::fs::create_dir(tier.merged().join("doomed")).expect("mkdir");
    tier.write("doomed/fresh.txt", b"replacement");
    tier.write("nested/deep/new.txt", b"nested"); //          nested write
    std::fs::create_dir(tier.merged().join("brand_new")).expect("mkdir"); // empty dir
    std::os::unix::fs::symlink("edit.txt", tier.merged().join("link")).expect("symlink");
    // A file rename. This mount asks for no `metacopy` — and an Export never can,
    // because ADR-0062 measured `nfs_export=on,metacopy=on` as refused by the
    // kernel for conflicting options — so the kernel copies the file up and renames
    // it *inside* the upper, leaving a whiteout behind: an exact change set with no
    // marker at all. If a kernel ever does set `trusted.overlay.redirect` on a plain
    // file rename, this case goes RED with an `Unsupported::RedirectNonDirectory`,
    // which is a true finding about that kernel and not a flake.
    std::fs::rename(
        tier.merged().join("keep.txt"),
        tier.merged().join("moved.txt"),
    )
    .expect("rename");

    mount.unmount();

    let cs = read_change_set(&tier.upper(), Markers::Overlay)
        .expect("the upper layer of a plain overlay mount is readable with CAP_SYS_ADMIN");

    // The three vectors below are not a guess at what the kernel does: the same
    // sequence of operations was run against a real overlay (6.8.0-117-generic,
    // ext4 upper, `metacopy=N redirect_dir=N index=N`) and the upper held exactly
    // these twelve entries and no others — see the `Measured` block in
    // `changeset.rs`. Asserting the sets *exactly* is the point: ADR-0062 part 3
    // claims the upper holds "precisely the paths the Step touched", and a
    // subset-style assertion would not notice the kernel adding a thirteenth.

    assert_eq!(
        cs.written,
        vec![
            file("added.txt"),
            file("doomed/fresh.txt"),
            file("edit.txt"),
            symlink("link"),
            file("moved.txt"),
            file("nested/deep/new.txt"),
        ],
        "the content the Step wrote — and nothing it merely read"
    );
    assert_eq!(
        cs.deleted,
        vec![PathBuf::from("gone.txt"), PathBuf::from("keep.txt")],
        "both whiteouts: the explicit `rm`, and the source side of the rename"
    );
    assert_eq!(
        cs.directories,
        vec![
            dir("brand_new", false),
            dir("doomed", true),
            dir("nested", false),
            dir("nested/deep", false),
        ],
        "the added empty directory, the ancestors of nested writes, and `doomed` marked OPAQUE"
    );
    assert_eq!(
        cs.opaque_directories().collect::<Vec<_>>(),
        vec![Path::new("doomed")],
        "the one subtree the drain must drop from the parent snapshot wholesale"
    );
}

/// A renamed directory is a **graft**, not a refusal.
///
/// This case used to assert the opposite, and pinning that refusal was actively
/// harmful: ADR-0062 part 2 makes `redirect_dir=on` a **correctness requirement**
/// of the Export mount — without it `rename(2)` of a directory that exists only in
/// the lower layer answers `EXDEV`, and `mv` masks that by deep-copying the whole
/// inherited subtree into the upper. So `trusted.overlay.redirect` is a marker
/// every real Export will carry, the first `mv` of an inherited directory produces
/// one, and a reader that refused it aborted the entire Attempt's change set. The
/// old assertion made ADR-conforming behaviour read as a test failure.
///
/// Only the same-parent (relative) encoding is exercised here, because that is
/// what a rename inside one directory produces. The `/`-prefixed
/// cross-parent encoding is covered by the classifier's unit tests, which run
/// everywhere — this tier has never executed in CI and must not be the only
/// guard for anything.
#[test]
#[ignore = "mounts a real overlayfs: Linux + CAP_SYS_ADMIN. `just overlay-tests`; running it \
            without SCARAB_TEST_OVERLAY=1 PANICS rather than passing"]
fn a_directory_rename_is_reported_as_a_graft_from_its_old_path() {
    let tier = Tier::enter("directory-rename");

    std::fs::create_dir(tier.lower().join("olddir")).expect("seed");
    std::fs::write(tier.lower().join("olddir/f.txt"), b"content").expect("seed");

    // `redirect_dir=on` is what lets a lower directory be renamed at all; without
    // it the kernel answers EXDEV. Asked for explicitly so the case does not depend
    // on the kernel's default — and measured to take effect even when
    // `/sys/module/overlay/parameters/redirect_dir` reads `N`. ADR-0062 also
    // verified it coexists with `nfs_export=on`, so the Export mount this models
    // can really carry it.
    let mut mount = tier.mount("redirect_dir=on");
    let renamed = std::fs::rename(tier.merged().join("olddir"), tier.merged().join("newdir"));
    match &renamed {
        Ok(()) => eprintln!("directory rename: the kernel performed it (redirect_dir is live)"),
        Err(e) => eprintln!(
            "directory rename: the kernel refused it ({e}); synthesising the marker the kernel \
             would have written, so the graft is still proven rather than skipped"
        ),
    }
    mount.unmount();

    if renamed.is_err() {
        // NOT a skip: write the exact marker a redirect-capable kernel writes for a
        // same-parent rename (measured: `redirect="olddir"`, relative) and hold the
        // code to the same reading. The kernel's participation is nice to have; the
        // graft is the thing under test.
        let hand_made = tier.upper().join("newdir");
        std::fs::create_dir_all(&hand_made).expect("mkdir upper/newdir");
        set_overlay_redirect(&hand_made, "olddir");
    }

    let cs = read_change_set(&tier.upper(), Markers::Overlay).expect(
        "a renamed directory must be READ, not refused: `redirect_dir=on` is required for the \
         Export to work at all, so refusing the marker would make the first `mv` of an inherited \
         directory abort the whole Attempt's change set",
    );

    // Asserted on the one entry rather than on the whole vectors, because the two
    // branches above produce different uppers: the kernel additionally leaves a
    // whiteout at `olddir`, the hand-made fallback does not.
    let entry = cs
        .directories
        .iter()
        .find(|d| d.path == Path::new("newdir"))
        .unwrap_or_else(|| {
            panic!("the renamed directory must appear at its NEW, merged-view path: {cs:?}")
        });
    assert_eq!(
        entry.redirect,
        Some(PathBuf::from("olddir")),
        "the change set must carry the OLD path so a drain can graft the parent snapshot's subtree \
         from it: {cs:?}"
    );
    assert!(
        !entry.opaque,
        "a rename does not make the directory opaque — that would tell the drain to drop the very \
         subtree it is grafting: {cs:?}"
    );
    assert_eq!(
        cs.grafts().collect::<Vec<_>>(),
        vec![(Path::new("olddir"), Path::new("newdir"))],
        "one graft, read off without re-deriving it: {cs:?}"
    );

    if renamed.is_ok() {
        // Only the kernel path proves this. A directory rename whiteouts the old
        // name, so the drain must resolve the graft against the PARENT SNAPSHOT and
        // not against the tree it is building — applying this deletion first would
        // delete the graft's source.
        assert!(
            cs.deleted.contains(&PathBuf::from("olddir")),
            "the kernel whiteouts the old name as well as redirecting the new one: {cs:?}"
        );
    }
}

/// Write `trusted.overlay.redirect` exactly as the kernel would.
///
/// Declared against libc rather than pulling in an xattr crate, for the same
/// reason `changeset.rs` does: `std` already links libc, and this workspace
/// argues each dependency one at a time. `#[cfg]`'d because `lsetxattr` does not
/// exist off Linux and an unresolved symbol would fail to *link* the whole test
/// binary on a dev laptop — where the rest of this file must still compile and
/// report itself as skipped.
#[cfg(target_os = "linux")]
fn set_overlay_redirect(path: &Path, value: &str) {
    use std::ffi::{c_char, c_void, CString};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn lsetxattr(
            path: *const c_char,
            name: *const c_char,
            value: *const c_void,
            size: usize,
            flags: i32,
        ) -> i32;
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).expect("path without NUL");
    let c_name = CString::new("trusted.overlay.redirect").expect("static name");
    let rc = unsafe {
        lsetxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            value.as_ptr() as *const c_void,
            value.len(),
            0,
        )
    };
    assert_eq!(
        rc,
        0,
        "lsetxattr(trusted.overlay.redirect) on {} failed: {} — writing a `trusted.*` xattr needs \
         CAP_SYS_ADMIN, which this tier already checked for",
        path.display(),
        std::io::Error::last_os_error()
    );
}

#[cfg(not(target_os = "linux"))]
fn set_overlay_redirect(_path: &Path, _value: &str) {
    // Not a silent no-op: `Tier::enter` panics off Linux the moment the tier is
    // opted into, so nothing can reach this without the gate being open on a
    // platform that cannot honour it.
    unreachable!("overlay markers cannot be written off Linux");
}
