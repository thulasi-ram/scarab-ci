//! Plugin-owned storage (ADR-0074 §7): a PluginInstallation may name a
//! PersistentVolumeClaim that an operator created in the plugin namespace,
//! and the plugin runtime mounts it into the Plugin's Pod.
//!
//! Scarab never creates, migrates, backs up or reads that storage, and it is
//! outside Scarab's durability claims. What this module decides is only the
//! shape: a claim name Kubernetes accepts, and a mount path that cannot
//! cover anything the runtime itself mounts.
//!
//! The plugin namespace is shared by every Org, so naming a claim is not
//! enough to mount it: the claim must carry
//! [`PLUGIN_STORAGE_OWNER_ANNOTATION`] set to the installation's
//! [`storage_owner`] (`<org>/<name>`). Only an operator can set it, and an
//! installation name is never reused in its Org, so one Org's admin cannot
//! mount a claim made for another Org's plugin.

use crate::PluginError;

/// Where the claim is mounted when the installation does not say.
pub const DEFAULT_PLUGIN_STORAGE_MOUNT_PATH: &str = "/data";

/// The annotation an operator puts on a claim to say which installation it
/// is for; its value is [`storage_owner`].
pub const PLUGIN_STORAGE_OWNER_ANNOTATION: &str = "scarab.io/plugin-installation";

/// The longest claim name: a DNS-1123 subdomain, as Kubernetes names a
/// PersistentVolumeClaim.
pub const MAX_STORAGE_CLAIM_CHARS: usize = 253;

/// The longest mount path.
pub const MAX_STORAGE_MOUNT_PATH_CHARS: usize = 256;

/// Paths a storage mount may not be, sit under, or contain: what the plugin
/// runtime mounts itself (the writable `/tmp`, the broker token under
/// `/var/run/secrets`), the kubelet's own files, and the kernel's
/// filesystems. A mount that contained one of them would put the runtime's
/// mount inside the claim.
pub const RESERVED_PLUGIN_MOUNT_PATHS: [&str; 9] = [
    "/tmp",
    "/var/run/secrets",
    "/run/secrets",
    "/etc/hosts",
    "/etc/hostname",
    "/etc/resolv.conf",
    "/dev",
    "/proc",
    "/sys",
];

/// A PersistentVolumeClaim an installation's Pod mounts read-write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStorage {
    /// The claim's name in the plugin namespace.
    pub claim: String,
    /// An absolute path in the Plugin's container.
    pub mount_path: String,
}

impl PluginStorage {
    /// Build from what an admin sent: `mount_path` defaults to
    /// [`DEFAULT_PLUGIN_STORAGE_MOUNT_PATH`], and a trailing `/` is dropped.
    pub fn new(claim: &str, mount_path: Option<&str>) -> Result<Self, PluginError> {
        let mount_path = match mount_path.map(str::trim).filter(|p| !p.is_empty()) {
            None => DEFAULT_PLUGIN_STORAGE_MOUNT_PATH.to_string(),
            Some(p) if p.len() > 1 => p.trim_end_matches('/').to_string(),
            Some(p) => p.to_string(),
        };
        let storage = Self {
            claim: claim.trim().to_string(),
            mount_path,
        };
        storage.validate()?;
        Ok(storage)
    }

    pub fn validate(&self) -> Result<(), PluginError> {
        validate_storage_claim(&self.claim)?;
        validate_storage_mount_path(&self.mount_path)
    }
}

/// What [`PLUGIN_STORAGE_OWNER_ANNOTATION`] must say on a claim for the
/// installation `name` in `org` to mount it.
pub fn storage_owner(org: &str, name: &str) -> String {
    format!("{org}/{name}")
}

/// A claim name: a DNS-1123 subdomain (lowercase letters, digits, `-` and
/// `.`; each `.`-separated part starts and ends with a letter or digit).
pub fn validate_storage_claim(claim: &str) -> Result<(), PluginError> {
    let label_ok = |l: &str| {
        let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        !l.is_empty()
            && l.chars().all(|c| alnum(c) || c == '-')
            && l.starts_with(alnum)
            && l.ends_with(alnum)
    };
    if !claim.is_empty() && claim.len() <= MAX_STORAGE_CLAIM_CHARS && claim.split('.').all(label_ok)
    {
        Ok(())
    } else {
        Err(PluginError::Invalid(format!(
            "storage claim `{}` must be a PersistentVolumeClaim name: 1-{MAX_STORAGE_CLAIM_CHARS} \
             lowercase letters, digits, `-` or `.`, starting and ending with a letter or digit",
            claim.escape_debug()
        )))
    }
}

/// A mount path: absolute, plain (`A-Z a-z 0-9 . _ -` between single `/`s,
/// no `.` or `..` part), not `/`, and clear of every
/// [`RESERVED_PLUGIN_MOUNT_PATHS`] entry — neither one of them, under one,
/// nor above one.
pub fn validate_storage_mount_path(path: &str) -> Result<(), PluginError> {
    let invalid = |why: &str| {
        Err(PluginError::Invalid(format!(
            "storage mount_path `{}` {why}",
            path.escape_debug()
        )))
    };
    if path.len() > MAX_STORAGE_MOUNT_PATH_CHARS {
        return invalid(&format!(
            "is longer than {MAX_STORAGE_MOUNT_PATH_CHARS} characters"
        ));
    }
    let Some(rest) = path.strip_prefix('/') else {
        return invalid("must be an absolute path");
    };
    if rest.is_empty() {
        return invalid("must not be `/`");
    }
    for part in rest.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return invalid("must not hold an empty, `.` or `..` part");
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return invalid("may hold only letters, digits, `.`, `_`, `-` and `/`");
        }
    }
    let within = |inner: &str, outer: &str| {
        inner == outer
            || inner
                .strip_prefix(outer)
                .is_some_and(|tail| tail.starts_with('/'))
    };
    for reserved in RESERVED_PLUGIN_MOUNT_PATHS {
        if within(path, reserved) || within(reserved, path) {
            return invalid(&format!(
                "overlaps `{reserved}`, which the plugin runtime or the kubelet mounts"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mount_path_defaults_to_data_and_loses_a_trailing_slash() {
        let s = PluginStorage::new("argocd-state", None).unwrap();
        assert_eq!(s.mount_path, "/data");
        assert_eq!(
            PluginStorage::new("argocd-state", Some("  "))
                .unwrap()
                .mount_path,
            "/data"
        );
        assert_eq!(
            PluginStorage::new("argocd-state", Some("/var/lib/plugin/"))
                .unwrap()
                .mount_path,
            "/var/lib/plugin"
        );
    }

    #[test]
    fn a_claim_is_a_dns_1123_subdomain() {
        for ok in ["a", "argocd-state", "state.v2", "0data"] {
            assert_eq!(validate_storage_claim(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "Data",
            "-data",
            "data-",
            "data..v2",
            ".data",
            "data_v2",
            "data/v2",
            "da ta",
            &"a".repeat(254),
        ] {
            assert!(
                validate_storage_claim(bad).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_mount_path_is_absolute_plain_and_not_the_root() {
        for ok in ["/data", "/var/lib/argocd", "/srv/plugin-state_1.0", "/temp"] {
            assert_eq!(validate_storage_mount_path(ok), Ok(()), "{ok}");
        }
        for bad in [
            "data",
            "/",
            "//data",
            "/data//x",
            "/data/./x",
            "/data/../tmp",
            "/da ta",
            "/data:ro",
            "/dätä",
        ] {
            assert!(
                validate_storage_mount_path(bad).is_err(),
                "{bad} must be refused"
            );
        }
    }

    /// Nothing the runtime mounts can end up inside, over or under the claim.
    #[test]
    fn a_mount_path_never_overlaps_what_the_runtime_mounts() {
        for bad in [
            "/tmp",
            "/tmp/state",
            "/var/run/secrets",
            "/var/run/secrets/scarab/broker",
            "/var/run",
            "/var",
            "/run",
            "/etc",
            "/etc/hosts",
            "/dev",
            "/proc/self",
            "/sys",
        ] {
            let err = validate_storage_mount_path(bad).unwrap_err();
            assert!(err.to_string().contains("overlaps"), "{bad}: {err}");
        }
        // A sibling that merely shares a prefix is fine.
        assert_eq!(validate_storage_mount_path("/tmpdata"), Ok(()));
        assert_eq!(validate_storage_mount_path("/var/lib"), Ok(()));
    }

    #[test]
    fn the_owner_is_org_slash_installation_name() {
        assert_eq!(storage_owner("acme", "argocd-prod"), "acme/argocd-prod");
    }
}
