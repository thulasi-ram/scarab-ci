//! Fetching a library file from a git source (ADR-0076 §4).
//!
//! An org admin types the source, so the server fetching it is a request to
//! a URL someone else chose. The anonymous fetch is held to:
//!
//! - **https only**, to a **known forge**: `github.com` (read from
//!   `raw.githubusercontent.com`) or the base URL of a configured Forgejo
//!   ForgeConnection. The URL is built by Scarab from the parsed source; the
//!   admin never supplies one.
//! - **No redirects.** A forge serves a file at a commit without one.
//! - **Public addresses only.** Every name is resolved by [`PublicResolver`],
//!   which drops loopback, private, link-local, shared and other special
//!   addresses and fails when none is left; the connection uses only the
//!   addresses it returned, so a name cannot resolve one way when checked and
//!   another when used. A host written as an IP address is checked the same
//!   way. No proxy is used, since a proxy would resolve the name itself.
//! - **Size and time:** at most [`MAX_SOURCE_BYTES`], read in chunks and
//!   abandoned past the cap; 5 s to connect, 20 s in all.
//!
//! A private source is read through a ForgeConnection (`ForgePort::
//! read_file_at_ref`) by the caller, under the org rule in `libraries.rs`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use scarab_pipeline::MAX_SOURCE_BYTES;

/// A library's git source, pinned to a commit:
/// `<host>/<owner>/<repo>//<path>@<commit>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySource {
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// Repo-relative, e.g. `libs/clone.yaml`.
    pub path: String,
    /// A full commit id: 40 or 64 lowercase hex characters.
    pub commit: String,
}

impl std::fmt::Display for LibrarySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}/{}//{}@{}",
            self.host, self.owner, self.repo, self.path, self.commit
        )
    }
}

fn segment_ok(s: &str) -> bool {
    scarab_policy::library::valid_segment(s)
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn path_ok(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 512
        && (p.ends_with(".yaml") || p.ends_with(".yml"))
        && p.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

impl LibrarySource {
    /// Parses `[https://]<host>/<owner>/<repo>//<path>@<commit>`. The host is
    /// a lowercase DNS name with no port; the commit is a full commit id, so
    /// the content cannot change after the admin reviews it.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let shape = "a source is `<host>/<owner>/<repo>//<path>@<commit>`, for example \
                     `github.com/acme/scarab-libs//deploy-helm.yaml@<40-character commit>`";
        let text = raw.trim();
        let text = text.strip_prefix("https://").unwrap_or(text);
        if text.contains("://") {
            return Err(format!("{shape}; only https sources are fetched"));
        }
        let (location, commit) = text
            .rsplit_once('@')
            .ok_or_else(|| format!("{shape}: the commit is missing"))?;
        if !(commit.len() == 40 || commit.len() == 64)
            || !commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!(
                "{shape}: `{commit}` is not a full commit id (40 or 64 lowercase hex \
                 characters); a branch or tag can move after you review the library"
            ));
        }
        let (repo_part, path) = location
            .split_once("//")
            .ok_or_else(|| format!("{shape}: `//` before the file path is missing"))?;
        let mut parts = repo_part.split('/');
        let (Some(host), Some(owner), Some(repo), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!(
                "{shape}: `{repo_part}` is not `<host>/<owner>/<repo>`"
            ));
        };
        if !host_ok(host) {
            return Err(format!(
                "{shape}: `{host}` is not a host name (lowercase, no port)"
            ));
        }
        if !segment_ok(owner) || !segment_ok(repo) {
            return Err(format!(
                "{shape}: `{owner}/{repo}` is not a repository name"
            ));
        }
        if !path_ok(path) {
            return Err(format!(
                "{shape}: `{path}` is not a repo-relative `.yaml` path"
            ));
        }
        Ok(LibrarySource {
            host: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            path: path.to_string(),
            commit: commit.to_string(),
        })
    }

    /// The default `<name>`: the file name without its extension.
    pub fn default_name(&self) -> String {
        let file = self.path.rsplit('/').next().unwrap_or(&self.path);
        file.trim_end_matches(".yaml")
            .trim_end_matches(".yml")
            .to_string()
    }

    pub fn repo_ref(&self) -> scarab_forge::RepoRef {
        scarab_forge::RepoRef {
            owner: self.owner.clone(),
            name: self.repo.clone(),
        }
    }
}

/// The forges an anonymous fetch may reach: `github.com`, and the base URL
/// of each Forgejo ForgeConnection. Each `base` is an https URL.
#[derive(Clone, Debug, Default)]
pub struct KnownForges {
    pub forgejo_bases: Vec<String>,
}

/// The https host of `url`, lowercased, with no port; `None` for anything
/// else.
pub fn https_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "https" || parsed.port().is_some() {
        return None;
    }
    parsed.host_str().map(str::to_ascii_lowercase)
}

/// The URL Scarab reads `src` from anonymously, or why it will not.
pub fn anonymous_url(src: &LibrarySource, known: &KnownForges) -> Result<String, String> {
    if src.host == "github.com" {
        return Ok(format!(
            "https://raw.githubusercontent.com/{}/{}/{}/{}",
            src.owner, src.repo, src.commit, src.path
        ));
    }
    for base in &known.forgejo_bases {
        if https_host(base).as_deref() == Some(src.host.as_str()) {
            return Ok(format!(
                "{}/{}/{}/raw/commit/{}/{}",
                base.trim_end_matches('/'),
                src.owner,
                src.repo,
                src.commit,
                src.path
            ));
        }
    }
    Err(format!(
        "`{}` is not a forge this Scarab knows: an anonymous install reads from github.com or \
         from the https base URL of a Forgejo connection; to read from elsewhere, pick a \
         connection that can reach it",
        src.host
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Failed(String),
}

/// Reads a URL anonymously. The real one is [`HttpsFetcher`]; tests stand in
/// for the forge.
#[async_trait]
pub trait AnonymousFetcher: Send + Sync {
    async fn get(&self, url: &str) -> Result<Vec<u8>, FetchError>;
}

/// Is `ip` an address an anonymous fetch may connect to?
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            is_public_v6(v6)
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        // Shared address space (RFC 6598), used by carrier NAT and some clusters.
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments (RFC 6890).
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking (RFC 2544).
        || (a == 198 && (b == 18 || b == 19))
        // Reserved, 240.0.0.0/4.
        || a >= 240)
}

/// Only global unicast, 2000::/3, minus the special-purpose blocks inside
/// it (IANA IPv6 Special-Purpose Address Registry). Everything outside
/// 2000::/3 is refused, which covers `::/96` (IPv4-compatible, so
/// `::127.0.0.1`), `::1`, `100::/64` (discard), `64:ff9b::/96` and
/// `64:ff9b:1::/48` (NAT64), `fc00::/7`, `fe80::/10`, `fec0::/10`
/// (site-local) and `ff00::/8` (multicast). `::ffff:a.b.c.d` is unwrapped
/// and checked as IPv4 before this.
fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xe000) == 0x2000
        // IETF protocol assignments, 2001::/23: Teredo 2001::/32 carries
        // an IPv4 address, plus benchmarking and ORCHID.
        && !(s[0] == 0x2001 && s[1] < 0x0200)
        // Documentation, 2001:db8::/32.
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        // 6to4, 2002::/16, carries an IPv4 address that may be private.
        && s[0] != 0x2002
        // Documentation, 3fff::/20.
        && !(s[0] == 0x3fff && s[1] < 0x1000)
}

/// Resolves names for [`HttpsFetcher`], keeping public addresses only.
pub struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs
                .iter()
                .copied()
                .filter(|a| is_public_ip(a.ip()))
                .collect();
            if public.is_empty() {
                return Err(format!(
                    "`{host}` resolves to no public address; Scarab does not fetch from \
                     loopback, private or link-local addresses"
                )
                .into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The real anonymous fetcher.
pub struct HttpsFetcher {
    client: reqwest::Client,
}

impl HttpsFetcher {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(PublicResolver))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(HttpsFetcher { client })
    }
}

/// Refuses a URL whose host is written as an address that is not public:
/// the resolver never sees an address literal.
pub fn check_literal_host(url: &str) -> Result<(), FetchError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|e| FetchError::Refused(format!("{url}: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(FetchError::Refused(format!("{url}: only https is fetched")));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::Refused(format!("{url}: no host")))?;
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok();
    if let Some(ip) = ip.filter(|ip| !is_public_ip(*ip)) {
        return Err(FetchError::Refused(format!(
            "{url}: {ip} is not a public address"
        )));
    }
    Ok(())
}

/// An error with its causes, so "error sending request" says why.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

/// Appends `chunk` to `body`, or fails once the body passes `cap` bytes.
pub fn append_capped(body: &mut Vec<u8>, chunk: &[u8], cap: usize) -> Result<(), FetchError> {
    if body.len() + chunk.len() > cap {
        return Err(FetchError::Refused(format!(
            "the file is larger than {cap} bytes, the limit for a library"
        )));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

#[async_trait]
impl AnonymousFetcher for HttpsFetcher {
    async fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        check_literal_host(url)?;
        let mut resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| FetchError::Failed(format!("fetching {url}: {}", chain(&e))))?;
        let status = resp.status();
        if status.is_redirection() {
            return Err(FetchError::Refused(format!(
                "{url} answered with a redirect ({status}); Scarab does not follow redirects \
                 when it fetches a library"
            )));
        }
        if !status.is_success() {
            return Err(FetchError::Failed(format!("{url} answered {status}")));
        }
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_SOURCE_BYTES as u64)
        {
            return Err(FetchError::Refused(format!(
                "the file is larger than {MAX_SOURCE_BYTES} bytes, the limit for a library"
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| FetchError::Failed(format!("reading {url}: {}", chain(&e))))?
        {
            append_capped(&mut body, &chunk, MAX_SOURCE_BYTES)?;
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C40: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_source_parses_and_prints_back() {
        let raw = format!("github.com/acme/scarab-libs//deploy/helm.yaml@{C40}");
        let src = LibrarySource::parse(&raw).unwrap();
        assert_eq!(src.owner, "acme");
        assert_eq!(src.path, "deploy/helm.yaml");
        assert_eq!(src.default_name(), "helm");
        assert_eq!(src.to_string(), raw);
        assert_eq!(
            LibrarySource::parse(&format!("https://{raw}")).unwrap(),
            src
        );
    }

    #[test]
    fn a_source_that_could_move_or_escape_is_refused() {
        for bad in [
            "github.com/acme/libs//x.yaml@main".to_string(),
            "github.com/acme/libs//x.yaml".to_string(),
            format!("http://github.com/acme/libs//x.yaml@{C40}"),
            format!("github.com:8080/acme/libs//x.yaml@{C40}"),
            format!("github.com/acme/libs//../x.yaml@{C40}"),
            format!("github.com/acme/libs//x.sh@{C40}"),
            format!("github.com/acme//x.yaml@{C40}"),
            format!("GitHub.com/acme/libs//x.yaml@{C40}"),
            format!("localhost/acme/libs//x.yaml@{C40}"),
            format!("github.com/acme/libs//x.yaml@{}", C40.to_uppercase()),
        ] {
            assert!(LibrarySource::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_known_forges_are_read_anonymously() {
        let gh = LibrarySource::parse(&format!("github.com/acme/libs//x.yaml@{C40}")).unwrap();
        let known = KnownForges {
            forgejo_bases: vec![
                "https://git.example.com/".into(),
                "http://plain.example.com".into(),
            ],
        };
        assert_eq!(
            anonymous_url(&gh, &known).unwrap(),
            format!("https://raw.githubusercontent.com/acme/libs/{C40}/x.yaml")
        );
        let fj = LibrarySource::parse(&format!("git.example.com/acme/libs//x.yaml@{C40}")).unwrap();
        assert_eq!(
            anonymous_url(&fj, &known).unwrap(),
            format!("https://git.example.com/acme/libs/raw/commit/{C40}/x.yaml")
        );
        // A connection over plain http is not a host for an anonymous fetch.
        let plain =
            LibrarySource::parse(&format!("plain.example.com/acme/libs//x.yaml@{C40}")).unwrap();
        assert!(anonymous_url(&plain, &known).is_err());
        let other =
            LibrarySource::parse(&format!("metadata.google.internal/a/b//x.yaml@{C40}")).unwrap();
        assert!(anonymous_url(&other, &known).is_err());
    }

    #[test]
    fn special_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "::127.0.0.1",
            "fec0::1",
            "ff02::1",
            "100::1",
            "2001::a00:1",
            "2001:2::1",
            "2001:db8::1",
            "2002:a00:1::1",
            "3fff::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["140.82.112.3", "185.199.108.133", "2606:50c0:8000::154"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn an_address_literal_host_is_checked_before_any_connection() {
        assert!(check_literal_host("https://169.254.169.254/latest/meta-data").is_err());
        assert!(check_literal_host("https://[::1]/x.yaml").is_err());
        assert!(check_literal_host("http://github.com/x.yaml").is_err());
        assert!(check_literal_host("https://raw.githubusercontent.com/a/b/c/x.yaml").is_ok());
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused() {
        use reqwest::dns::Resolve;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let err = match PublicResolver.resolve(name).await {
            Ok(_) => panic!("localhost resolved to a public address"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no public address"), "{err}");
    }

    #[tokio::test]
    async fn the_real_fetcher_refuses_a_loopback_url_without_connecting() {
        let f = HttpsFetcher::new().unwrap();
        let err = f.get("https://127.0.0.1:1/x.yaml").await.unwrap_err();
        assert!(matches!(err, FetchError::Refused(_)), "{err:?}");
        let err = f.get("https://localhost:1/x.yaml").await.unwrap_err();
        assert!(err.to_string().contains("no public address"), "{err}");
    }

    #[test]
    fn a_body_past_the_cap_is_abandoned() {
        let mut body = Vec::new();
        append_capped(&mut body, &[0; 6], 10).unwrap();
        assert!(append_capped(&mut body, &[0; 5], 10).is_err());
        assert_eq!(body.len(), 6);
    }
}
