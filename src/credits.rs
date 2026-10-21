//! Reads your Copilot premium-request allowance from GitHub's internal
//! endpoint, <https://api.github.com/copilot_internal/user> - the same one the
//! editor extensions call to show "premium requests remaining". There is no
//! documented API for this, so the shape below is what the endpoint actually
//! returns, not a contract.
//!
//! It is read-only about credentials: it uses the token the GitHub CLI has
//! already stored (via `gh auth token`, which handles keyring and file storage
//! itself), or GH_TOKEN / GITHUB_TOKEN if either is set. It never writes,
//! refreshes or logs a token.

use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use wait_timeout::ChildExt;

use crate::http;
use crate::json::nullable;

/// GitHub's internal Copilot entitlement endpoint.
pub const DEFAULT_URL: &str = "https://api.github.com/copilot_internal/user";

const USER_AGENT: &str = "github-usage/1.0 (+https://github.com)";

/// The item the GitHub CLI makes when it stores credentials in the keyring
/// instead of in a file.
const KEYCHAIN_SERVICE: &str = "gh:github.com";

const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
const BODY_LIMIT: u64 = 1 << 20; // 1 MiB

/// Why a fetch gave no quota.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The GitHub CLI is installed but has no stored token to borrow. That is a
    /// setup problem, not a transient failure.
    NoToken,
    /// The gh binary is not in any of the places we look, so there is no
    /// stored token to read.
    NoCli,
    /// The account has no premium-request quota to report - an unlimited or
    /// quota-less plan, not a failure.
    NoQuota,
    /// Any other failure, with a description for the menu.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoToken => f.write_str("no GitHub token: run `gh auth login`, or set GH_TOKEN"),
            Error::NoCli => f.write_str("GitHub CLI not found: install gh, or set GH_TOKEN"),
            Error::NoQuota => f.write_str("no premium-request quota on this plan"),
            Error::Other(msg) => f.write_str(msg),
        }
    }
}

/// The premium-request allowance for the current billing period.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quota {
    /// 0-100, rounded. The menu bar shows this.
    pub percent_used: i64,
    /// The credits used so far (entitlement - remaining).
    pub used: i64,
    /// The credits left.
    #[allow(dead_code)] // parsed for parity, not shown
    pub remaining: i64,
    /// The credits in the period.
    pub entitlement: i64,
    /// The credits used beyond the entitlement.
    pub overage: i64,
    /// The start of the next period, if known.
    pub resets_at: Option<DateTime<Utc>>,
    /// For example "business", for the tooltip.
    pub plan: String,
}

// The part of copilot_internal/user that we use. Every field is optional and
// may be null, as with Go's encoding/json.

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiResponse {
    #[serde(deserialize_with = "nullable")]
    copilot_plan: String,
    #[serde(deserialize_with = "nullable")]
    quota_reset_date_utc: String,
    #[serde(deserialize_with = "nullable")]
    quota_reset_date: String,
    #[serde(deserialize_with = "nullable")]
    quota_snapshots: Snapshots,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Snapshots {
    #[serde(rename = "premium_interactions", deserialize_with = "nullable")]
    premium: Premium,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Premium {
    #[serde(deserialize_with = "nullable")]
    percent_remaining: f64,
    #[serde(rename = "quota_remaining", deserialize_with = "nullable")]
    remaining: f64,
    #[serde(deserialize_with = "nullable")]
    entitlement: f64,
    #[serde(rename = "overage_count", deserialize_with = "nullable")]
    overage: f64,
    #[serde(deserialize_with = "nullable")]
    unlimited: bool,
    #[serde(deserialize_with = "nullable")]
    has_quota: bool,
}

/// Reads the token and queries the endpoint. `url` can override the endpoint
/// for offline tests: empty means DEFAULT_URL, and a file:// URL is read
/// straight off disk.
pub fn fetch(url: &str, deadline: Instant) -> Result<Quota, Error> {
    let url = if url.is_empty() { DEFAULT_URL } else { url };
    let body = get(url, deadline)?;
    parse(&body)
}

fn get(url: &str, deadline: Instant) -> Result<Vec<u8>, Error> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).map_err(|e| Error::Other(http::go_open_error(path, &e)));
    }

    let token = token(deadline)?;

    let mut resp = http::agent()
        .get(url)
        .header("Authorization", &format!("token {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .config()
        .timeout_global(Some(http::timeout(CLIENT_TIMEOUT, deadline)))
        .build()
        .call()
        .map_err(|e| Error::Other(format!("Get {url:?}: {e}")))?;

    match resp.status().as_u16() {
        200 => {
            http::read_limited(resp.body_mut(), BODY_LIMIT).map_err(|e| Error::Other(e.to_string()))
        }
        // Deliberately says nothing about the token itself.
        code @ (401 | 403) => Err(Error::Other(format!(
            "GitHub rejected the token (HTTP {code}) — try `gh auth login`"
        ))),
        404 => Err(Error::NoQuota),
        code => Err(Error::Other(format!(
            "copilot endpoint returned HTTP {code}"
        ))),
    }
}

fn parse(body: &[u8]) -> Result<Quota, Error> {
    let r: ApiResponse = serde_json::from_slice(body)
        .map_err(|e| Error::Other(format!("parse copilot response: {e}")))?;

    let p = &r.quota_snapshots.premium;
    if p.unlimited || !p.has_quota || p.entitlement <= 0.0 {
        return Err(Error::NoQuota);
    }

    // percent_remaining is the number that GitHub's own UI shows, so derive
    // from it instead of recomputing, and report it as claude-usage does: used.
    // `as i64` truncates towards zero, as Go's int() conversion does.
    let used = (100 - (p.percent_remaining + 0.5) as i64).clamp(0, 100);

    Ok(Quota {
        percent_used: used,
        used: (p.entitlement - p.remaining + 0.5) as i64,
        remaining: (p.remaining + 0.5) as i64,
        entitlement: (p.entitlement + 0.5) as i64,
        overage: (p.overage + 0.5) as i64,
        resets_at: reset_time(&r),
        plan: r.copilot_plan.clone(),
    })
}

/// Uses the full UTC timestamp if it parses, and the date-only field if not.
fn reset_time(r: &ApiResponse) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(r.quota_reset_date_utc.trim()) {
        return Some(t.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(r.quota_reset_date.trim(), "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc())
}

/// Returns the GitHub token to authenticate with: GH_TOKEN or GITHUB_TOKEN if
/// set, otherwise what the GitHub CLI has stored. The value is never logged or
/// shown in the UI.
///
/// The CLI's own store is read three ways, because a menu-bar app started by
/// Finder or at login gets a minimal PATH - /usr/bin:/bin and similar - without
/// Homebrew, where gh usually is. Asking for "gh" by name therefore works in a
/// terminal and fails in the built app.
fn token(deadline: Instant) -> Result<String, Error> {
    for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Ok(v.to_string());
            }
        }
    }
    // Ask the CLI first: it knows where it put the token, and reading it
    // through gh raises no keychain prompt, because gh owns that item.
    if let Some(t) = token_from_cli(deadline) {
        return Ok(t);
    }
    // Then the stores that gh writes to, for when the binary is not found.
    if let Some(t) = token_from_keychain(deadline) {
        return Ok(t);
    }
    if let Some(t) = token_from_hosts_file() {
        return Ok(t);
    }
    // Say which of the two situations this is: "run gh auth login" is no help
    // to someone who has already done exactly that.
    if gh_path().is_none() {
        return Err(Error::NoCli);
    }
    Err(Error::NoToken)
}

/// Runs `gh auth token`, which reads the keyring or the hosts file as
/// necessary, so we do not have to guess which.
fn token_from_cli(deadline: Instant) -> Option<String> {
    let gh = gh_path()?;
    let mut cmd = Command::new(gh);
    cmd.args(["auth", "token"]);
    non_empty(output_by(cmd, deadline)?)
}

/// Finds the gh binary without trusting PATH. The usual Homebrew prefixes come
/// first, because that is how gh is nearly always installed on macOS.
fn gh_path() -> Option<PathBuf> {
    gh_path_with(
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

fn gh_path_with(
    home: Option<&std::ffi::OsStr>,
    path_env: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = [
        "/opt/homebrew/bin/gh", // Homebrew on Apple silicon
        "/usr/local/bin/gh",    // Homebrew on Intel, or a manual install
        "/usr/bin/gh",
        "/opt/local/bin/gh", // MacPorts
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = home {
        candidates.push(Path::new(home).join(".local").join("bin").join("gh"));
    }
    if let Some(p) = candidates.into_iter().find(|p| is_executable(p)) {
        return Some(p);
    }
    // The last resort is a PATH lookup, which is what works when the app is
    // started from a shell, not by Finder. As with Go's exec.LookPath, a
    // relative directory in PATH is not used.
    std::env::split_paths(path_env?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("gh"))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
}

/// Reads the item that gh makes when it stores credentials in the keyring.
/// /usr/bin/security has an absolute path for the same PATH reason.
fn token_from_keychain(deadline: Instant) -> Option<String> {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"]);
    non_empty(output_by(cmd, deadline)?)
}

/// Reads the gh configuration, for when gh stores the token in a file instead
/// of the keyring. The file is small and its shape is stable, so a line scan
/// avoids a YAML dependency for one field.
fn token_from_hosts_file() -> Option<String> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let path = Path::new(&home)
        .join(".config")
        .join("gh")
        .join("hosts.yml");
    let data = std::fs::read(path).ok()?;
    let token = oauth_token_from_yaml(&String::from_utf8_lossy(&data));
    (!token.is_empty()).then_some(token)
}

fn oauth_token_from_yaml(s: &str) -> String {
    for line in s.split('\n') {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        if key.trim() != "oauth_token" {
            continue;
        }
        return value.trim().trim_matches(['"', '\'']).to_string();
    }
    String::new()
}

fn non_empty(out: Vec<u8>) -> Option<String> {
    let s = String::from_utf8_lossy(&out).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Runs `cmd` and returns its stdout when it exits with success before
/// `deadline`. It is stopped at the deadline. Stdout is read on a second thread
/// while the process runs, as Go's `Output()` does: a process with more output
/// than the pipe holds otherwise blocks on its write, and never exits.
fn output_by(mut cmd: Command, deadline: Instant) -> Option<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).map(|_| out)
    });

    let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join(); // the pipe closes when the process dies
            return None;
        }
    };
    let out = reader.join().ok()?.ok()?;
    status.success().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Mirrors what copilot_internal/user actually returns, including the
    /// fields we ignore, so a stray key or an unlimited sibling quota cannot
    /// confuse the parser.
    const REAL_SHAPE: &str = r#"{
  "login": "octocat",
  "copilot_plan": "business",
  "quota_reset_date": "2026-09-01",
  "quota_reset_date_utc": "2026-09-01T00:00:00.000Z",
  "token_based_billing": true,
  "quota_snapshots": {
    "chat": {"percent_remaining": 100.0, "unlimited": true, "has_quota": true, "entitlement": 0},
    "completions": {"percent_remaining": 100.0, "unlimited": true, "has_quota": true, "entitlement": 0},
    "premium_interactions": {
      "overage_count": 0, "overage_permitted": true, "percent_remaining": 93.6,
      "quota_id": "premium_interactions", "quota_remaining": 12645.2, "unlimited": false,
      "has_quota": true, "credits_used": 299, "remaining": 12645, "entitlement": 13500
    }
  }
}"#;

    fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn parse_real_shape() {
        let q = parse(REAL_SHAPE.as_bytes()).expect("parse");
        assert_eq!(q.percent_used, 6); // 100 - 93.6, rounded
        assert_eq!(q.entitlement, 13500);
        assert_eq!(q.remaining, 12645);
        assert_eq!(q.used, 855); // entitlement - remaining, not credits_used
        assert_eq!(q.plan, "business");
        assert_eq!(q.resets_at, Some(utc(2026, 9, 1)));
    }

    #[test]
    fn parse_no_quota() {
        let cases = [
            (
                "unlimited plan",
                r#"{"quota_snapshots":{"premium_interactions":{"unlimited":true,"has_quota":true,"entitlement":300}}}"#,
            ),
            (
                "has_quota false",
                r#"{"quota_snapshots":{"premium_interactions":{"has_quota":false,"entitlement":300}}}"#,
            ),
            (
                "zero entitlement",
                r#"{"quota_snapshots":{"premium_interactions":{"has_quota":true,"entitlement":0}}}"#,
            ),
            ("no snapshots at all", r#"{"login":"octocat"}"#),
        ];
        for (name, body) in cases {
            assert_eq!(parse(body.as_bytes()), Err(Error::NoQuota), "{name}");
        }
    }

    #[test]
    fn parse_garbage() {
        let err = parse(b"<html>not json</html>").unwrap_err();
        assert!(
            err.to_string().starts_with("parse copilot response: "),
            "{err}"
        );
    }

    /// Guards the menu-bar string: whatever the endpoint says, the percentage
    /// stays in 0-100.
    #[test]
    fn parse_clamps_percent() {
        let cases = [
            (100.0, 0),
            (0.0, 100),
            (50.0, 50),
            (93.6, 6),
            (0.4, 100),
            (120.0, 0),
            (-20.0, 100),
        ];
        for (remaining, want) in cases {
            let body = format!(
                r#"{{"quota_snapshots":{{"premium_interactions":{{"has_quota":true,"entitlement":100,"quota_remaining":1,"percent_remaining":{remaining}}}}}}}"#
            );
            let q = parse(body.as_bytes()).expect("parse");
            assert_eq!(q.percent_used, want, "percent_remaining {remaining}");
        }
    }

    #[test]
    fn reset_time_falls_back_to_date_only() {
        let body = r#"{"quota_reset_date":"2026-09-01","quota_snapshots":{"premium_interactions":
            {"has_quota":true,"entitlement":100,"quota_remaining":50,"percent_remaining":50}}}"#;
        let q = parse(body.as_bytes()).expect("parse");
        assert_eq!(q.resets_at, Some(utc(2026, 9, 1)));
    }

    /// The regression test for the bug that made the built app report "no
    /// GitHub token" while `gh auth token` worked in a terminal: an app started
    /// by Finder gets a minimal PATH without Homebrew, so a lookup of gh by
    /// name alone fails.
    #[test]
    fn gh_path_ignores_path() {
        let Some(got) = gh_path_with(None, Some(OsStr::new("/nonexistent"))) else {
            eprintln!("gh is not installed at any known location on this machine");
            return;
        };
        assert!(got.exists(), "{got:?} does not exist");
        assert!(got.is_absolute(), "{got:?} is not absolute");
    }

    #[test]
    fn gh_path_skips_relative_path_entries() {
        let dir = std::env::temp_dir().join(format!("githubusage-gh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("gh");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Only reachable through PATH when no well-known location has gh.
        if gh_path_with(None, None).is_none() {
            assert_eq!(
                gh_path_with(None, Some(dir.as_os_str())),
                Some(fake.clone())
            );
            assert_eq!(gh_path_with(None, Some(OsStr::new("relative/dir"))), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oauth_token_from_yaml_shapes() {
        let cases = [
            (
                "gh's file layout",
                "github.com:\n    users:\n        octocat:\n            oauth_token: gho_EXAMPLE\n    git_protocol: ssh\n    user: octocat\n    oauth_token: gho_EXAMPLE\n",
                "gho_EXAMPLE",
            ),
            (
                "quoted",
                "github.com:\n    oauth_token: \"gho_QUOTED\"\n",
                "gho_QUOTED",
            ),
            (
                "single quoted",
                "github.com:\n    oauth_token: 'gho_SINGLE'\n",
                "gho_SINGLE",
            ),
            (
                "keyring storage, no token in the file",
                "github.com:\n    git_protocol: ssh\n    user: octocat\n",
                "",
            ),
            ("empty", "", ""),
            (
                "lookalike key is not matched",
                "github.com:\n    not_oauth_token: nope\n",
                "",
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(oauth_token_from_yaml(input), want, "{name}");
        }
    }

    #[test]
    fn file_url_errors_read_like_go() {
        let err = fetch(
            "file:///nonexistent/copilot.json",
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "open /nonexistent/copilot.json: no such file or directory"
        );
    }

    /// More output than a pipe holds (64 KiB) must not block the process until
    /// the deadline.
    #[test]
    fn output_by_drains_large_output() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "head -c 300000 /dev/zero"]);
        let start = Instant::now();
        let out = output_by(cmd, start + Duration::from_secs(5)).expect("output");
        assert_eq!(out.len(), 300_000);
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "it waited for the deadline"
        );
    }

    #[test]
    fn output_by_stops_at_the_deadline() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("10");
        let start = Instant::now();
        assert!(output_by(cmd, start + Duration::from_millis(300)).is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_by_rejects_failure() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo partial; exit 44"]);
        assert!(output_by(cmd, Instant::now() + Duration::from_secs(5)).is_none());
    }

    /// Diagnostic: reads the token and the live endpoint, and prints the quota.
    /// The token is never printed. Opt-in, because it needs gh credentials:
    ///
    ///     cargo test live_credits -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_credits() {
        let q = fetch(DEFAULT_URL, Instant::now() + Duration::from_secs(20)).expect("fetch");
        println!(
            "{}% used, {} of {} (+{} over), resets {:?}, plan {:?}",
            q.percent_used, q.used, q.entitlement, q.overage, q.resets_at, q.plan
        );
    }
}
