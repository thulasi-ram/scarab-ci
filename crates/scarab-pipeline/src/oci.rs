//! OCI image references (the grammar of the distribution `reference`
//! package): `[host[:port]/]path[:tag][@algorithm:digest]`.
//!
//! A Step's image is checked once it is rendered, before it launches
//! (ADR-0076): a rendered value that is not a reference (whitespace, a
//! leading `-`, quotes, an empty path component) is refused instead of being
//! handed to the container runtime. This checks the form only: whether the
//! image exists, or a digest has the length its algorithm gives, is the
//! registry's to say.

/// The longest image name (host and path) a registry accepts.
const MAX_NAME: usize = 255;

/// Whether `image` is an OCI image reference.
pub fn is_image_reference(image: &str) -> bool {
    let (rest, digest) = match image.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest)),
        None => (image, None),
    };
    if digest.is_some_and(|d| !is_digest(d)) {
        return false;
    }
    // A tag follows the last `:` after the last `/`; a `:` before that is a
    // host's port.
    let last_slash = rest.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = match rest[last_slash..].rfind(':') {
        Some(i) => (&rest[..last_slash + i], Some(&rest[last_slash + i + 1..])),
        None => (rest, None),
    };
    if tag.is_some_and(|t| !is_tag(t)) {
        return false;
    }
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    let mut parts: Vec<&str> = name.split('/').collect();
    // The first part is a host when it has a `.` or a `:` or is
    // `localhost`, and there is more after it.
    if parts.len() > 1 {
        let first = parts[0];
        if first.contains(['.', ':'])
            || first == "localhost"
            || first.chars().any(|c| c.is_ascii_uppercase())
        {
            if !is_host(first) {
                return false;
            }
            parts.remove(0);
        }
    }
    parts.iter().all(|p| is_path_component(p))
}

/// `[a-z0-9]+` joined by `.`, `_`, `__` or any run of `-`.
fn is_path_component(p: &str) -> bool {
    let b = p.as_bytes();
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if alnum(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && !alnum(b[i]) {
            i += 1;
        }
        let sep = &p[start..i];
        let ok = sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|c| c == b'-');
        if !ok {
            return false;
        }
    }
    true
}

/// A DNS host (labels of letters, digits and inner `-`, joined by `.`) or a
/// bracketed IPv6 address, with an optional numeric port.
fn is_host(h: &str) -> bool {
    let (host, port) = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, after)) => {
                if addr.is_empty() || !addr.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
                    return false;
                }
                match after {
                    "" => return true,
                    p => match p.strip_prefix(':') {
                        Some(port) => return is_port(port),
                        None => return false,
                    },
                }
            }
            None => return false,
        }
    } else {
        match h.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (h, None),
        }
    };
    if port.is_some_and(|p| !is_port(p)) {
        return false;
    }
    !host.is_empty()
        && host.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_alphanumeric()
                && b[b.len() - 1].is_ascii_alphanumeric()
                && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
        })
}

fn is_port(p: &str) -> bool {
    !p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit())
}

/// `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
fn is_tag(t: &str) -> bool {
    let b = t.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    !b.is_empty()
        && b.len() <= 128
        && word(b[0])
        && b.iter().all(|c| word(*c) || *c == b'.' || *c == b'-')
}

/// `algorithm:encoded`: the algorithm is `[a-z0-9]+` parts joined by one of
/// `+._-`; the encoded part is `[A-Za-z0-9=_-]+`.
fn is_digest(d: &str) -> bool {
    let Some((algorithm, encoded)) = d.split_once(':') else {
        return false;
    };
    let algo_ok = algorithm.split(['+', '.', '_', '-']).all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    algo_ok
        && !encoded.is_empty()
        && encoded
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'=' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::is_image_reference;

    #[test]
    fn references_are_accepted_and_anything_else_is_refused() {
        for ok in [
            "busybox",
            "busybox:1.36",
            "library/busybox:latest",
            "ghcr.io/thulasi-ram/scarab-clone:v0.2.2",
            "localhost:5000/app",
            "localhost/app:dev",
            "registry.example.com:443/team/app__x/y-z.w@sha256:0123abcdef",
            "app@sha256:6f1b0a1e",
            "[::1]:5000/app",
            "Registry.Example.com/app",
            "a/b/c/d:tag_1.2-rc",
        ] {
            assert!(is_image_reference(ok), "{ok}");
        }
        for bad in [
            "",
            " busybox",
            "busybox ",
            "busy box",
            "-busybox",
            "--privileged",
            "Busybox",
            "busybox:",
            "busybox:-x",
            "busybox@",
            "busybox@sha256",
            "busybox@SHA256:abc",
            "busybox@sha256:a b",
            "a//b",
            "/busybox",
            "busybox/",
            "a..b",
            "a.-b",
            "${{ inputs.image }}",
            "busybox;rm -rf /",
            "busybox\n",
            "\"busybox\"",
            "host:99999999/app",
            "[zz]/app",
            "app:x@sha256:1@sha256:2",
        ] {
            assert!(!is_image_reference(bad), "{bad:?}");
        }
        assert!(!is_image_reference(&"a".repeat(256)));
        assert!(!is_image_reference(&format!("app:{}", "t".repeat(129))));
    }
}
