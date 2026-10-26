//! `mu update`: release check and self-update through the public GitHub API (via `curl`), falling back to
//! an authenticated `gh` when the repo is private.
//! Releases are tagged `mu-YYYYMMDD-<sha7>` by the maintenance bot; the installed build knows its own sha.

use anyhow::{Result, bail};
use mu_common::paths;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = "jacobhak/mu";
const CACHE_TTL_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    UpToDate {
        tag: String,
    },
    Available {
        tag: String,
        ahead: u64,
    },
    /// The installed build is dirty, unknown, or not on the release lineage.
    Dev {
        sha: String,
    },
    NoReleases,
    /// curl missing, offline, or rate limited.
    Unknown(String),
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Status::UpToDate { tag } => format!("mu is up to date (latest release {tag})"),
            Status::Available { tag, ahead } => {
                format!(
                    "update available: {tag} ({ahead} commit{} ahead of this build). Run `mu update`.",
                    if *ahead == 1 { "" } else { "s" }
                )
            }
            Status::Dev { sha } => {
                format!("this is a dev build ({sha}); not comparable with releases. `mu update --force` replaces it.")
            }
            Status::NoReleases => "no releases published yet".into(),
            Status::Unknown(why) => format!("could not check for updates: {why}"),
        }
    }
}

/// Short sha of the running build, from the compile-time `MU_GIT_SHA`.
fn installed_sha() -> &'static str {
    env!("MU_GIT_SHA")
}

fn is_dev_sha(sha: &str) -> bool {
    sha == "unknown" || sha.ends_with("-dirty") || sha.is_empty()
}

/// Last `-`-separated part of `mu-YYYYMMDD-<sha7>`.
fn tag_sha(tag: &str) -> Option<&str> {
    let mut parts = tag.split('-');
    let (prefix, date, s) = (parts.next()?, parts.next()?, parts.next()?);
    if prefix != "mu" || date.len() != 8 || parts.next().is_some() {
        return None;
    }
    (s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit())).then_some(s)
}

/// Maps the compare API's `status` and `ahead_by` (installed...release) to a result.
fn classify(tag: &str, status: &str, ahead: u64, sha: &str) -> Status {
    match status {
        "ahead" => Status::Available { tag: tag.into(), ahead },
        "identical" | "behind" => Status::UpToDate { tag: tag.into() },
        _ => Status::Dev { sha: sha.into() },
    }
}

enum Api {
    Ok(serde_json::Value),
    NotFound,
    Err(String),
}

/// Runs `curl` and returns (HTTP status, body). HTTP errors are returned, not raised; network failures are errors.
fn curl(url: &str, accept: &str) -> Result<(u16, String)> {
    let o = Command::new("curl")
        .args([
            "-sSL",
            "--connect-timeout",
            "5",
            "--max-time",
            "20",
            "-H",
            accept,
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "-w",
            "\n%{http_code}",
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("curl unavailable: {e}"))?;
    let out = String::from_utf8_lossy(&o.stdout);
    let (body, code) = out.rsplit_once('\n').unwrap_or(("", &out));
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        bail!("{}", err.lines().next().unwrap_or("curl failed"));
    }
    Ok((code.trim().parse().unwrap_or(0), body.to_string()))
}

/// Public API first; on a 404 (which is also what a private repo looks like), retries through an
/// authenticated `gh`. Without a working `gh` the 404 stands.
fn api(path: &str) -> Api {
    match curl(&format!("https://api.github.com/{path}"), "Accept: application/vnd.github+json") {
        Ok((200, body)) => parse(&body),
        Ok((404, _)) => gh(&["api", path]).map_or(Api::NotFound, |body| parse(&body)),
        Ok((403 | 429, _)) => Api::Err("GitHub API rate limit reached".into()),
        Ok((code, _)) => Api::Err(format!("GitHub API returned HTTP {code}")),
        Err(e) => Api::Err(e.to_string()),
    }
}

fn parse(body: &str) -> Api {
    match serde_json::from_str(body) {
        Ok(v) => Api::Ok(v),
        Err(e) => Api::Err(format!("bad response from GitHub: {e}")),
    }
}

/// Stdout of a successful `gh` run; `None` when gh is missing, unauthenticated, or the request fails.
fn gh(args: &[&str]) -> Option<String> {
    let o = Command::new("gh").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Queries GitHub. Blocking; call from a thread or before the runtime starts.
pub fn check() -> Status {
    let sha = installed_sha();
    if is_dev_sha(sha) {
        return Status::Dev { sha: sha.into() };
    }
    let tag = match api(&format!("repos/{REPO}/releases/latest")) {
        Api::Ok(v) => v["tag_name"].as_str().unwrap_or_default().to_string(),
        // A repo with no releases and a private repo both answer 404; the repo itself tells them apart.
        Api::NotFound => {
            return match api(&format!("repos/{REPO}")) {
                Api::Ok(_) => Status::NoReleases,
                Api::NotFound => {
                    Status::Unknown("repo is private or missing; authenticate gh (`gh auth login`)".into())
                }
                Api::Err(e) => Status::Unknown(e),
            };
        }
        Api::Err(e) => return Status::Unknown(e),
    };
    if tag.is_empty() {
        return Status::NoReleases;
    }
    let Some(rsha) = tag_sha(&tag) else {
        return Status::Unknown(format!("unrecognised release tag {tag}"));
    };
    match api(&format!("repos/{REPO}/compare/{sha}...{rsha}?per_page=1")) {
        Api::Ok(v) => classify(&tag, v["status"].as_str().unwrap_or(""), v["ahead_by"].as_u64().unwrap_or(0), sha),
        // An unknown sha (a local build never pushed) or unrelated history is a 404, not a network error.
        Api::NotFound => Status::Dev { sha: sha.into() },
        Api::Err(e) => Status::Unknown(e),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cache_path() -> std::path::PathBuf {
    paths::mu_home().join("update-check")
}

/// Cache line: `<unix>\t<sha of running build>\t<tag or ->\t<ahead>`; only available updates are stored.
fn parse_cache(s: &str, sha: &str, at: u64) -> Option<Option<(String, u64)>> {
    let mut f = s.trim().split('\t');
    let ts: u64 = f.next()?.parse().ok()?;
    if f.next()? != sha || at.saturating_sub(ts) > CACHE_TTL_SECS {
        return None;
    }
    let tag = f.next()?;
    let ahead: u64 = f.next()?.parse().ok()?;
    Some((tag != "-").then(|| (tag.to_string(), ahead)))
}

/// The cached or freshly checked notice text, or `None` when up to date, unreachable, or a dev build.
/// Blocking.
pub fn startup_notice() -> Option<String> {
    let sha = installed_sha();
    if is_dev_sha(sha) {
        return None;
    }
    let cached = std::fs::read_to_string(cache_path()).ok().and_then(|s| parse_cache(&s, sha, now()));
    let avail = match cached {
        Some(a) => a,
        None => match check() {
            Status::Available { tag, ahead } => Some((tag, ahead)),
            Status::UpToDate { .. } | Status::NoReleases => None,
            Status::Dev { .. } => return None,
            Status::Unknown(_) => {
                // Back off for an hour instead of retrying on every start.
                let _ = std::fs::create_dir_all(paths::mu_home());
                let ts = now().saturating_sub(CACHE_TTL_SECS.saturating_sub(3600));
                let _ = std::fs::write(cache_path(), format!("{ts}\t{sha}\t-\t0\n"));
                return None;
            }
        },
    };
    let (tag, ahead) = avail.clone().unwrap_or(("-".into(), 0));
    let _ = std::fs::create_dir_all(paths::mu_home());
    let _ = std::fs::write(cache_path(), format!("{}\t{sha}\t{tag}\t{ahead}\n", now()));
    avail.map(|(tag, ahead)| Status::Available { tag, ahead }.describe())
}

/// `mu update [--check] [--force] [--tag T]`.
pub fn run(check_only: bool, force: bool, tag: Option<String>) -> Result<i32> {
    let st = check();
    println!("{}", st.describe());
    if check_only {
        return Ok(i32::from(matches!(st, Status::Unknown(_))));
    }
    match (&st, &tag) {
        (Status::UpToDate { .. } | Status::NoReleases, None) => return Ok(0),
        (Status::Unknown(_), _) => bail!("not updating: cannot reach the release repo"),
        (Status::Dev { .. }, _) if !force => bail!("not updating a dev build; pass --force to replace it"),
        _ => {}
    }
    // The installer comes from the release tag being installed, never from `main`: it matches
    // that release's layout and checksums, and nothing unreleased can run.
    let target = match (&tag, &st) {
        (Some(t), _) => t.clone(),
        (None, Status::Available { tag, .. }) => tag.clone(),
        _ => bail!("not updating: no release tag to install"),
    };
    let url = format!("https://raw.githubusercontent.com/{REPO}/{target}/scripts/install.sh");
    let script = match curl(&url, "Accept: text/plain")? {
        (200, body) if !body.is_empty() => body,
        (404, _) => gh(&[
            "api",
            &format!("repos/{REPO}/contents/scripts/install.sh?ref={target}"),
            "-H",
            "Accept: application/vnd.github.raw",
        ])
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("could not fetch install.sh: HTTP 404 (a private repo needs an authenticated gh)")
        })?,
        (code, _) => bail!("could not fetch install.sh: HTTP {code}"),
    };
    let mut cmd = Command::new("bash");
    cmd.args(["-s", "--"]);
    cmd.args(["--tag", &target]);
    let mut child = cmd.stdin(Stdio::piped()).spawn()?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(script.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let status = child.wait()?;
    let _ = std::fs::remove_file(cache_path());
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_sha_parses_release_tags() {
        assert_eq!(tag_sha("mu-20261026-799ca34"), Some("799ca34"));
        assert_eq!(tag_sha("mu-20261026"), None);
        assert_eq!(tag_sha("v1"), None);
    }

    #[test]
    fn classify_maps_compare_status() {
        let t = "mu-20261026-799ca34";
        assert_eq!(classify(t, "ahead", 3, "abc"), Status::Available { tag: t.into(), ahead: 3 });
        assert_eq!(classify(t, "identical", 0, "abc"), Status::UpToDate { tag: t.into() });
        assert_eq!(classify(t, "behind", 2, "abc"), Status::UpToDate { tag: t.into() });
        assert_eq!(classify(t, "diverged", 1, "abc"), Status::Dev { sha: "abc".into() });
    }

    #[test]
    fn dev_shas() {
        assert!(is_dev_sha("unknown"));
        assert!(is_dev_sha("abc123-dirty"));
        assert!(!is_dev_sha("799ca34873a1"));
    }

    #[test]
    fn cache_respects_ttl_and_sha() {
        let line = "1000\tabc\tmu-1-abcdef0\t2\n";
        assert_eq!(parse_cache(line, "abc", 1500), Some(Some(("mu-1-abcdef0".into(), 2))));
        assert_eq!(parse_cache(line, "other", 1500), None);
        assert_eq!(parse_cache(line, "abc", 1000 + CACHE_TTL_SECS + 1), None);
        assert_eq!(parse_cache("1000\tabc\t-\t0\n", "abc", 1500), Some(None));
        assert_eq!(parse_cache("junk", "abc", 1500), None);
    }

    #[test]
    fn describe_pluralises() {
        let s = Status::Available { tag: "t".into(), ahead: 1 }.describe();
        assert!(s.contains("1 commit ahead"));
    }

    #[test]
    fn failure_backoff_entry_is_fresh_for_one_hour_only() {
        let sha = "abcdef1";
        let at = 1_000_000;
        let ts = at - (CACHE_TTL_SECS - 3600);
        let line = format!("{ts}\t{sha}\t-\t0\n");
        assert_eq!(parse_cache(&line, sha, at), Some(None));
        assert_eq!(parse_cache(&line, sha, at + 3601), None);
    }
}
