//! ADR-0073 §10: the passkey break-glass is a SERVER-binary command
//! (`scarab-server admin reset-passkeys`, run against the database). The
//! `scarab` CLI — which talks to the API with a token or session — has no
//! way to reach it: no `admin` command at all.

use std::process::Command;

#[test]
fn the_scarab_cli_has_no_admin_command() {
    for args in [
        &["admin", "reset-passkeys", "--principal", "github:42"][..],
        &["admin", "--help"][..],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_scarab"))
            .args(args)
            .output()
            .expect("run scarab");
        assert!(
            !out.status.success(),
            "`scarab {}` must not exist",
            args.join(" ")
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("unrecognized subcommand"),
            "`scarab {}`: {stderr}",
            args.join(" ")
        );
    }
    // And nothing in its help offers one.
    let help = Command::new(env!("CARGO_BIN_EXE_scarab"))
        .arg("--help")
        .output()
        .expect("run scarab --help");
    let text = String::from_utf8_lossy(&help.stdout).to_lowercase();
    assert!(
        !text
            .lines()
            .any(|l| l.split_whitespace().next() == Some("admin")),
        "{text}"
    );
    assert!(!text.contains("reset-passkeys"), "{text}");
}
