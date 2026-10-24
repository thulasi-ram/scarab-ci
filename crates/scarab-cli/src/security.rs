//! `scarab security …` (ADR-0073 §10) and the CLI's handling of step-up
//! refusals.
//!
//! - `security setup` makes sure the human this CLI acts for holds a passkey:
//!   if not, it opens a FRESH browser login that lands on passkey enrollment
//!   (a first passkey needs a login from the last 5 minutes) and waits until
//!   one exists. Running it again is a no-op.
//! - `security status` shows your passkeys and, for each Org you own, the
//!   admins who have none (every crown jewel is refused for them) and the
//!   recent refusals.
//! - [`print_failure`] is how every command reports a non-2xx answer: a
//!   `403 step_up_required` becomes "confirm it in a browser at <url>", a
//!   `403 fresh_login_required` becomes "log in again at <url>".
//!
//! A CLI caller (an API token, or a session presented as `Bearer`) can never
//! confirm a step-up itself: only its human can, in a browser, with a passkey.

use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde_json::Value;

/// Where `security setup` sends the browser: a fresh login that lands on the
/// personal security page with the enrollment dialog open. The provider the
/// human signs in with is added by [`setup_login_path`].
pub const SETUP_LOGIN_PATH: &str = "/v1/auth/login?next=%2Faccount%2Fsecurity%3Fenroll%3D1";

/// The login provider `subject` signs in again through (git-bug 584bd03) —
/// the server's `AppState::login_provider_for`, from what a CLI can see:
/// `providers` is `GET /v1/auth/providers`, `pinned` the providers of
/// `GET /v1/me`'s `identities`. Among the configured providers: the pinned
/// one the id is prefixed with, else the first pinned; else the id's
/// `provider:` prefix (ADR-0069 §2 mints `provider:handle`); else the
/// default. An unnamed login answers 400 with several providers and no
/// default, and with a default would sign a non-default user in as somebody
/// else.
pub fn login_provider(subject: &str, providers: &Value, pinned: &[String]) -> Option<String> {
    let listed: Vec<(&str, bool)> = providers
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p["id"].as_str()?, p["default"] == true)))
                .collect()
        })
        .unwrap_or_default();
    let configured = |id: &str| listed.iter().any(|(i, _)| *i == id);
    let prefix = subject
        .split_once(':')
        .map(|(p, _)| p)
        .filter(|p| configured(p));
    let pinned: Vec<&str> = pinned
        .iter()
        .map(String::as_str)
        .filter(|p| configured(p))
        .collect();
    if let Some(p) = prefix.filter(|p| pinned.contains(p)) {
        return Some(p.to_string());
    }
    if let Some(p) = pinned.first() {
        return Some(p.to_string());
    }
    if let Some(p) = prefix {
        return Some(p.to_string());
    }
    listed
        .iter()
        .find(|(_, default)| *default)
        .map(|(id, _)| id.to_string())
}

/// [`SETUP_LOGIN_PATH`] naming `provider`, when there is one.
pub fn setup_login_path(provider: Option<&str>) -> String {
    match provider {
        Some(p) => format!("{SETUP_LOGIN_PATH}&provider={}", encode_component(p)),
        None => SETUP_LOGIN_PATH.to_string(),
    }
}

/// Percent-encode a query value (RFC 3986: everything but unreserved).
fn encode_component(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// How often `security setup` asks whether the passkey exists yet.
const POLL_EVERY: Duration = Duration::from_secs(2);

/// The longest any one API request may take: a hung server must not hang
/// the CLI (or `security setup`'s wait) forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// Make sure you hold a passkey: if not, log in in a browser, enroll one,
    /// and wait until it is there. Safe to re-run.
    Setup(SetupArgs),
    /// Your passkeys and, for Orgs you own, admins without a passkey and
    /// recent step-up refusals.
    Status(Conn),
}

/// The server and the credential the CLI acts with.
#[derive(Debug, Args)]
pub struct Conn {
    /// Base URL of the Scarab server.
    #[arg(long, env = "SCARAB_SERVER", default_value = "http://localhost:8080")]
    pub server: String,
    /// An API token (`scarab_pat_…`) or a session id, sent as `Bearer`.
    #[arg(long, env = "SCARAB_TOKEN")]
    pub token: Option<String>,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    #[command(flatten)]
    pub conn: Conn,
    /// Print the login URL but do not try to open a browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Give up after this many seconds without a passkey.
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
}

pub async fn run(cmd: SecurityCommand) -> i32 {
    match cmd {
        SecurityCommand::Setup(args) => setup(args).await,
        SecurityCommand::Status(conn) => status(conn).await,
    }
}

// ---------------------------------------------------------------------------
// The HTTP layer
// ---------------------------------------------------------------------------

/// A minimal client over the API: base URL + optional Bearer credential.
pub struct Api {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Api {
    pub fn new(server: &str, token: &Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base: server.trim_end_matches('/').to_string(),
            token: token.clone(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// `path` (starting with `/`) on this server.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// GET `path` and parse the JSON answer. Any failure is printed (see
    /// [`print_failure`]) and comes back as the exit code to use.
    pub async fn get_json(&self, path: &str) -> Result<Value, i32> {
        self.try_get_json(path).await.map_err(|f| {
            f.print(path, &self.base);
            1
        })
    }

    /// GET `path` and parse the JSON answer; a failure comes back unprinted,
    /// so a caller that retries can decide what is worth giving up on.
    async fn try_get_json(&self, path: &str) -> Result<Value, GetFailure> {
        let url = self.url(path);
        let mut req = self.client.get(&url);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| GetFailure::Transport(format!("request to {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(GetFailure::Status(status, text));
        }
        serde_json::from_str(&text)
            .map_err(|e| GetFailure::Transport(format!("GET {path}: not JSON: {e}")))
    }
}

/// Why a GET did not produce JSON.
enum GetFailure {
    /// No answer, or an unreadable one.
    Transport(String),
    /// A non-2xx answer: its status and body.
    Status(u16, String),
}

impl GetFailure {
    fn print(&self, path: &str, server: &str) {
        match self {
            GetFailure::Transport(why) => eprintln!("error: {}", sanitize(why)),
            GetFailure::Status(status, body) => {
                print_failure(&format!("GET {path}"), server, *status, body)
            }
        }
    }

    /// Only "you are not (or no longer) allowed" ends a wait early; a
    /// network blip or a 5xx is worth asking again.
    fn is_final(&self) -> bool {
        matches!(self, GetFailure::Status(401 | 403, _))
    }
}

/// `s` with every control, bidi and invisible character written as
/// `\u{XX}`: a server string (another member's label, an Org name, a
/// refusal's target) printed raw could carry terminal escape sequences, or
/// reorder or hide what the human reads.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_invisible(c) {
            out.push_str(&format!("\\u{{{:02X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Does `c` render as nothing, or rearrange or break the text around it?
/// Every control (Cc), format (Cf — bidi controls included), line- and
/// paragraph-separator (Zl, Zp) character, plus the other default-ignorable
/// code points. The same set as the server's
/// `scarab_server::step_up::ops::is_invisible` (the CLI does not link the
/// server), enumerated by hand (Unicode 15.1).
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF0}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

/// Report a non-2xx API answer on stderr. Step-up refusals get guidance a
/// human can act on (ADR-0073 §6); anything else prints status and body.
pub fn print_failure(what: &str, server: &str, status: u16, body: &str) {
    match refusal_guidance(server, status, body) {
        Some(guidance) => eprintln!("{what} refused: {guidance}"),
        None if status == 401 => eprintln!(
            "{what} failed (401): not signed in — pass --token (or set SCARAB_TOKEN) to an API \
             token or a session id"
        ),
        None => eprintln!("{what} failed ({status}): {}", sanitize(body.trim())),
    }
}

/// The human-facing text for a `403 step_up_required` or
/// `403 fresh_login_required` body; `None` for any other answer.
pub fn refusal_guidance(server: &str, status: u16, body: &str) -> Option<String> {
    if status != 403 {
        return None;
    }
    let v: Value = serde_json::from_str(body).ok()?;
    let message = &sanitize(v["message"].as_str().unwrap_or_default());
    match v["code"].as_str()? {
        "step_up_required" => {
            let what = match (
                v["summary"]["operation"].as_str(),
                v["summary"]["target"].as_str(),
            ) {
                (Some(op), Some(target)) if !target.is_empty() => {
                    format!("{} ({})", sanitize(op), sanitize(target))
                }
                (Some(op), _) => sanitize(op),
                _ => sanitize(v["op"].as_str().unwrap_or("this operation")),
            };
            let mut out = format!("{what} needs a step-up confirmation.\n");
            if let Some(url) = v["confirmation_url"].as_str() {
                out.push_str(&format!(
                    "This needs your confirmation in a browser: {}",
                    sanitize(&absolute(server, url))
                ));
                if let Some(at) = v["expires_at"].as_i64() {
                    out.push_str(&format!(" (expires {})", fmt_ms(at)));
                }
                out.push_str(
                    "\nOpen it signed in as the human this credential acts for and confirm \
                     with your passkey; the operation then runs once, exactly as sent.",
                );
            } else if v["mode"] == "enroll" {
                out.push_str(message);
                out.push_str("\nRun `scarab security setup` to enroll a passkey.");
            } else {
                out.push_str(message);
            }
            Some(out)
        }
        "fresh_login_required" => {
            let mut out = message.to_string();
            if let Some(url) = v["login_url"].as_str() {
                out.push_str(&format!(
                    "\nLog in again at: {}",
                    sanitize(&absolute(server, url))
                ));
            }
            Some(out)
        }
        _ => None,
    }
}

/// A server-relative URL (`/v1/auth/login…`) made absolute against
/// `server`; an absolute one is kept.
fn absolute(server: &str, url: &str) -> String {
    if url.starts_with('/') {
        format!("{}{url}", server.trim_end_matches('/'))
    } else {
        url.to_string()
    }
}

/// Unix ms as `YYYY-MM-DD HH:MM:SS UTC` (no date crate in the CLI).
pub fn fmt_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

fn passkey_count(me: &Value) -> usize {
    // An API token is not shown the list, only the count.
    if let Some(n) = me["usable_passkey_count"].as_u64() {
        return n as usize;
    }
    me["passkeys"]
        .as_array()
        .map(|ps| ps.iter().filter(|p| p["clone_suspected"] != true).count())
        .unwrap_or(0)
}

fn step_up_off(me: &Value) -> bool {
    me["step_up"] == "off"
}

// ---------------------------------------------------------------------------
// security setup
// ---------------------------------------------------------------------------

async fn setup(args: SetupArgs) -> i32 {
    let api = Api::new(&args.conn.server, &args.conn.token);
    let me = match api.get_json("/v1/me/security").await {
        Ok(v) => v,
        Err(code) => return code,
    };
    if step_up_off(&me) {
        println!(
            "step-up is off on this server (dev-insecure): nothing is enforced and there is \
             nothing to set up."
        );
        return 0;
    }
    if me["has_passkey"] == true {
        println!("already enrolled: {} passkey(s)", passkey_count(&me));
        return 0;
    }

    let raw_subject = me["subject"].as_str().unwrap_or("");
    let subject = &sanitize(if raw_subject.is_empty() {
        "?"
    } else {
        raw_subject
    });
    // Best-effort: a server that cannot say leaves the login unnamed, which
    // is right for one provider or a default one.
    let providers = api
        .try_get_json("/v1/auth/providers")
        .await
        .unwrap_or(Value::Null);
    let pinned: Vec<String> = match api.try_get_json("/v1/me").await {
        Ok(v) => v["identities"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i["provider"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let provider = login_provider(raw_subject, &providers, &pinned);
    let url = api.url(&setup_login_path(provider.as_deref()));
    println!("You have no passkey yet. This CLI acts for `{subject}`: sign in as that account.");
    println!("Log in and enroll a passkey at:");
    println!("{url}");
    if !args.no_browser && !open_browser(&url) {
        println!("(could not open a browser — open the URL above yourself)");
    }
    println!("Waiting for the passkey (up to {}s)…", args.timeout);

    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            eprintln!(
                "timed out after {}s with no passkey enrolled for `{subject}`. If you signed \
                 in as someone else, sign in as `{subject}` and run this again.",
                args.timeout
            );
            return 1;
        }
        tokio::time::sleep(left.min(POLL_EVERY)).await;
        let me = match api.try_get_json("/v1/me/security").await {
            Ok(v) => v,
            Err(f) if f.is_final() => {
                f.print("/v1/me/security", api.base());
                return 1;
            }
            // A network blip or a 5xx: keep waiting until the deadline.
            Err(_) => continue,
        };
        if me["has_passkey"] == true {
            println!("passkey enrolled: {} passkey(s)", passkey_count(&me));
            return 0;
        }
    }
}

/// Open `url` in the default browser. `false` when that could not be done.
fn open_browser(url: &str) -> bool {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---------------------------------------------------------------------------
// security status
// ---------------------------------------------------------------------------

async fn status(conn: Conn) -> i32 {
    let api = Api::new(&conn.server, &conn.token);
    let me = match api.get_json("/v1/me/security").await {
        Ok(v) => v,
        Err(code) => return code,
    };
    println!(
        "subject: {}",
        sanitize(me["subject"].as_str().unwrap_or("?"))
    );
    if step_up_off(&me) {
        println!("step-up: off on this server (dev-insecure) — nothing is enforced");
        return 0;
    }
    println!("step-up: enforced");
    let passkeys = me["passkeys"].as_array().cloned().unwrap_or_default();
    if me["passkeys_hidden"] == true {
        println!(
            "passkeys: {} usable (an API token is not shown the list — use a session to see it)",
            passkey_count(&me)
        );
    } else if passkeys.is_empty() {
        println!("passkeys: none — run `scarab security setup` to enroll one");
    } else {
        println!("passkeys ({}):", passkeys.len());
        for p in &passkeys {
            let last = p["last_used_at"]
                .as_i64()
                .map_or_else(|| "never".to_string(), fmt_ms);
            let mut line = format!(
                "  - {}  (id {}, added {}, last used {})",
                sanitize(p["label"].as_str().unwrap_or("?")),
                sanitize(p["id"].as_str().unwrap_or("?")),
                p["created_at"].as_i64().map_or_else(|| "?".into(), fmt_ms),
                last,
            );
            if p["clone_suspected"] == true {
                line.push_str("  CLONE SUSPECTED — never accepted again");
            }
            println!("{line}");
        }
    }
    if let Some(until) = me["window_expires_at"].as_i64() {
        println!("step-up window: open until {}", fmt_ms(until));
    }

    // The Orgs the caller may administer; the posture is shown for Owners.
    let orgs = match api.get_json("/v1/me").await {
        Ok(v) => v["admin_orgs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Err(code) => return code,
    };
    for org in orgs {
        let summary = match api
            .get_json(&format!("/v1/orgs/{org}/security/summary"))
            .await
        {
            Ok(v) => v,
            Err(code) => return code,
        };
        if summary["caller_role"] != "owner" {
            continue;
        }
        print_org(&org, &summary);
    }
    0
}

fn print_org(org: &str, s: &Value) {
    let without: Vec<&str> = s["admins_without_passkey"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    println!();
    let org = sanitize(org);
    println!(
        "org {org} (you are owner): {} admin(s), {} without a passkey",
        s["admins_total"].as_u64().unwrap_or(0),
        without.len()
    );
    if !without.is_empty() {
        println!("  admins without a passkey (every crown jewel is refused for them):");
        for a in &without {
            println!("    - {}", sanitize(a));
        }
    }
    let refusals = s["recent_refusals"].as_array().cloned().unwrap_or_default();
    if refusals.is_empty() {
        println!("  recent refusals (24h): none");
    } else {
        println!("  recent refusals (24h):");
        for r in &refusals {
            println!(
                "    - {}  {}  {}  {}  via {}",
                r["at"].as_i64().map_or_else(|| "?".into(), fmt_ms),
                sanitize(r["subject"].as_str().unwrap_or("?")),
                sanitize(r["op"].as_str().unwrap_or("?")),
                sanitize(r["target"].as_str().unwrap_or("")),
                sanitize(r["caller"].as_str().unwrap_or("?")),
            );
        }
    }
    if let Some(n) = s["pending_requests"].as_u64().filter(|n| *n > 0) {
        println!("  confirmation requests waiting for a human: {n}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An API token is shown no passkey list, only the count (git-bug
    /// fe777ed): `security setup` still reports what is held.
    #[test]
    fn the_passkey_count_comes_from_the_server_when_the_list_is_hidden() {
        let token_view = serde_json::json!({
            "has_passkey": true, "usable_passkey_count": 2, "passkeys": [], "passkeys_hidden": true
        });
        assert_eq!(passkey_count(&token_view), 2);
        let older_server = serde_json::json!({
            "passkeys": [{"clone_suspected": false}, {"clone_suspected": true}]
        });
        assert_eq!(passkey_count(&older_server), 1);
    }

    /// git-bug 584bd03: the provider `security setup` logs in through.
    #[test]
    fn the_setup_login_names_the_subjects_provider() {
        let none_default = serde_json::json!([
            {"id": "gitea", "name": "Gitea", "default": false},
            {"id": "okta", "name": "Okta", "default": false},
        ]);
        // By the id's prefix.
        assert_eq!(
            login_provider("okta:amy", &none_default, &[]).as_deref(),
            Some("okta")
        );
        // A pinned identity wins over the spelling; a prefix it holds wins
        // over the other pinned ones.
        let pinned = vec!["gitea".to_string()];
        assert_eq!(
            login_provider("okta:amy", &none_default, &pinned).as_deref(),
            Some("gitea")
        );
        let both = vec!["gitea".to_string(), "okta".to_string()];
        assert_eq!(
            login_provider("okta:amy", &none_default, &both).as_deref(),
            Some("okta")
        );
        // Nothing to go on and no default: unnamed (the server says so).
        assert_eq!(login_provider("amy", &none_default, &[]), None);
        // An unknown prefix is not a provider.
        assert_eq!(login_provider("ldap:amy", &none_default, &[]), None);
        let with_default = serde_json::json!([
            {"id": "default", "name": "GitHub", "default": true},
            {"id": "okta", "name": "Okta", "default": false},
        ]);
        assert_eq!(
            login_provider("amy", &with_default, &[]).as_deref(),
            Some("default")
        );
        assert_eq!(
            login_provider("okta:amy", &with_default, &[]).as_deref(),
            Some("okta")
        );
        // A server that could not list its providers.
        assert_eq!(login_provider("okta:amy", &Value::Null, &[]), None);

        assert_eq!(setup_login_path(None), SETUP_LOGIN_PATH);
        assert_eq!(
            setup_login_path(Some("ok ta")),
            "/v1/auth/login?next=%2Faccount%2Fsecurity%3Fenroll%3D1&provider=ok%20ta"
        );
    }

    #[test]
    fn fmt_ms_is_utc_civil_time() {
        assert_eq!(fmt_ms(0), "1970-01-01 00:00:00 UTC");
        // 2026-10-24 12:34:56 UTC
        assert_eq!(fmt_ms(1_792_845_296_000), "2026-10-24 12:34:56 UTC");
        // A leap day.
        assert_eq!(fmt_ms(951_782_400_000), "2000-02-29 00:00:00 UTC");
    }

    #[test]
    fn a_fresh_login_refusal_points_at_an_absolute_login_url() {
        // The server's body (lib.rs `ApiError::FreshLoginRequired`).
        let body = serde_json::json!({
            "code": "fresh_login_required",
            "message": "Log in again to enroll your first passkey.",
            "login_url": "/v1/auth/login?next=/account/security%3Fenroll%3D1",
        })
        .to_string();
        let g = refusal_guidance("https://ci.example.com/", 403, &body).unwrap();
        assert!(g.contains("Log in again to enroll"), "{g}");
        assert!(
            g.contains("https://ci.example.com/v1/auth/login?next=/account/security%3Fenroll%3D1"),
            "{g}"
        );
    }

    #[test]
    fn sanitize_defuses_terminal_escapes() {
        let s = sanitize("a\x1b]52;x\x07b");
        assert!(!s.contains('\x1b') && !s.contains('\x07'), "{s}");
        assert_eq!(s, "a\\u{1B}]52;x\\u{07}b");
        let body = serde_json::json!({
            "code": "step_up_required",
            "message": "m",
            "summary": {"operation": "Write a secret", "target": "evil\x1b[2Jorg"},
        })
        .to_string();
        let g = refusal_guidance("http://s", 403, &body).unwrap();
        assert!(!g.contains('\x1b'), "{g}");
    }

    #[test]
    fn sanitize_escapes_bidi_and_invisible_characters() {
        for c in [
            '\u{202E}',
            '\u{2066}',
            '\u{061C}',
            '\u{200B}',
            '\u{2028}',
            '\u{2029}',
            '\u{00AD}',
            '\u{180E}',
            '\u{FEFF}',
            '\u{E0041}',
        ] {
            let s = sanitize(&format!("Laptop{c}key"));
            assert!(!s.contains(c), "U+{:04X} printed raw: {s:?}", c as u32);
        }
        assert_eq!(sanitize("evil\u{202E}txt"), "evil\\u{202E}txt");
        assert_eq!(sanitize("plain é label"), "plain é label");
    }

    #[test]
    fn other_answers_get_no_step_up_guidance() {
        assert!(refusal_guidance("http://s", 403, "insufficient role").is_none());
        assert!(refusal_guidance("http://s", 400, r#"{"code":"step_up_required"}"#).is_none());
        assert!(refusal_guidance("http://s", 403, r#"{"code":"other"}"#).is_none());
    }
}
