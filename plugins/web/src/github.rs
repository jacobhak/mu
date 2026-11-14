//! GitHub URLs answered from the REST API and the raw file host (a port of omp's
//! `web/scrapers/github.ts`). Any failure returns `None`, so the generic fetch path runs.

use crate::fetch::Rendered;
use crate::net::{Guard, LoadOpts, load_page};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

const API_ACCEPT: &str = "application/vnd.github.v3+json";
const API_NOTE: &str = "Fetched via GitHub API";
/// Hosts that may receive the token. A redirect elsewhere drops it inside `load_page`.
const TRUSTED_HOSTS: [&str; 2] = ["api.github.com", "raw.githubusercontent.com"];

/// Where API and raw requests go. Tests point both at a loopback server.
pub struct Endpoints {
    pub api: String,
    pub raw: String,
    pub token: Option<String>,
}

impl Endpoints {
    pub fn production() -> Self {
        let token =
            ["GITHUB_TOKEN", "GH_TOKEN"].into_iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        Self { api: "https://api.github.com".into(), raw: "https://raw.githubusercontent.com".into(), token }
    }

    fn opts(&self, url: &str, api: bool) -> LoadOpts {
        let mut headers = Vec::new();
        if api {
            headers.push(("User-Agent".to_string(), "mu-web".to_string()));
        }
        let trusted = Url::parse(url).ok().and_then(|u| u.host_str().map(|h| TRUSTED_HOSTS.contains(&h)));
        if let (Some(token), Some(true)) = (&self.token, trusted) {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        LoadOpts { accept: api.then_some(API_ACCEPT), headers, ..Default::default() }
    }
}

#[derive(Debug, PartialEq)]
pub struct Target {
    pub owner: String,
    pub repo: String,
    pub kind: Kind,
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    Blob { git_ref: String, path: String },
    Tree { git_ref: Option<String>, path: String },
    Repo,
    Commit { sha: String },
    Issue { number: u64, pull: bool },
    Issues,
}

/// Parses `github.com` URLs the handlers cover. Other sections (pull lists, discussions,
/// Actions) return `None`.
pub fn parse(url: &str) -> Option<Target> {
    let u = Url::parse(url).ok()?;
    if u.host_str() != Some("github.com") {
        return None;
    }
    let parts: Vec<&str> = u.path_segments()?.filter(|s| !s.is_empty()).collect();
    let [owner, repo, rest @ ..] = parts.as_slice() else { return None };
    let kind = match rest {
        [] => Kind::Repo,
        ["blob", git_ref, path @ ..] => Kind::Blob { git_ref: (*git_ref).into(), path: path.join("/") },
        ["tree"] => Kind::Tree { git_ref: None, path: String::new() },
        ["tree", git_ref, path @ ..] => Kind::Tree { git_ref: Some((*git_ref).into()), path: path.join("/") },
        ["commit", sha, ..] => Kind::Commit { sha: (*sha).into() },
        ["issues"] => Kind::Issues,
        ["issues", n, ..] => match digits(n) {
            Some(number) => Kind::Issue { number, pull: false },
            None => Kind::Issues,
        },
        ["pull", n, ..] => Kind::Issue { number: digits(n)?, pull: true },
        _ => return None,
    };
    Some(Target { owner: (*owner).into(), repo: (*repo).into(), kind })
}

fn digits(s: &str) -> Option<u64> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) { s.parse().ok() } else { None }
}

/// Renders a GitHub URL, or returns `None` to let the generic path run.
pub async fn handle(g: &Guard, ep: &Endpoints, url: &str) -> Option<Rendered> {
    let Target { owner, repo, kind } = parse(url)?;
    let base = format!("/repos/{owner}/{repo}");
    match kind {
        Kind::Blob { git_ref, path } => {
            let raw = format!("{}/{owner}/{repo}/{git_ref}/{path}", ep.raw);
            let page = load_page(g, &raw, ep.opts(&raw, false)).await;
            if !page.ok {
                return None;
            }
            let notes = vec![format!("Fetched raw: {raw}")];
            if page.looks_binary() {
                let notice = format!("Binary content ({} bytes) not shown.", page.body.len());
                return Some(Rendered::new(&raw, &page.content_type, "binary", notes, notice));
            }
            Some(Rendered::new(&raw, "text/plain", "github-raw", notes, page.text()))
        }
        Kind::Tree { git_ref, path } => {
            let info: RepoInfo = api_json(g, ep, &base).await?;
            let git_ref = git_ref.unwrap_or(info.default_branch);
            let shown = if path.is_empty() { "(root)" } else { path.as_str() };
            let mut md = format!("# {}/{shown}\n\n**Branch:** {git_ref}\n\n", info.full_name);
            if let Some(mut items) =
                api_json::<Vec<ContentItem>>(g, ep, &format!("{base}/contents/{path}?ref={git_ref}")).await
            {
                items.sort_by(|a, b| {
                    (a.kind != "dir")
                        .cmp(&(b.kind != "dir"))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                md += "## Contents\n\n```\n";
                for item in &items {
                    let prefix = if item.kind == "dir" { "[dir] " } else { "      " };
                    let size = match item.size {
                        Some(s) if s > 0 && item.kind == "file" => format!(" ({s} bytes)"),
                        _ => String::new(),
                    };
                    md += &format!("{prefix}{}{size}\n", item.name);
                }
                md += "```\n\n";
                if let Some(readme) =
                    items.iter().find(|it| it.kind == "file" && it.name.eq_ignore_ascii_case("readme.md"))
                {
                    let readme_path =
                        if path.is_empty() { readme.name.clone() } else { format!("{path}/{}", readme.name) };
                    let raw = format!("{}/{owner}/{repo}/{git_ref}/{readme_path}", ep.raw);
                    let page = load_page(g, &raw, ep.opts(&raw, false)).await;
                    if page.ok {
                        md += &format!("---\n\n## README\n\n{}", page.text());
                    }
                }
            }
            Some(Rendered::new(url, "text/markdown", "github-tree", vec![API_NOTE.into()], md))
        }
        Kind::Repo => {
            let tree_path = format!("{base}/git/trees/HEAD?recursive=1");
            let readme_path = format!("{base}/readme");
            let (info, tree, readme) = tokio::join!(
                api_json::<RepoInfo>(g, ep, &base),
                api_json::<TreeResp>(g, ep, &tree_path),
                api_json::<Readme>(g, ep, &readme_path),
            );
            let info = info?;
            let mut md = format!("# {}\n\n", info.full_name);
            if let Some(d) = info.description.as_deref().filter(|d| !d.is_empty()) {
                md += &format!("{d}\n\n");
            }
            md += &format!(
                "Stars: {} · Forks: {} · Issues: {}\n",
                info.stargazers_count, info.forks_count, info.open_issues_count
            );
            if let Some(lang) = info.language.as_deref().filter(|l| !l.is_empty()) {
                md += &format!("Language: {lang}\n");
            }
            if let Some(license) = &info.license {
                md += &format!("License: {}\n", license.name);
            }
            md += "\n---\n\n";
            if let Some(tree) = tree {
                md += "## Files\n\n```\n";
                for item in tree.tree.iter().take(100) {
                    let prefix = if item.kind == "tree" { "[dir] " } else { "      " };
                    md += &format!("{prefix}{}\n", item.path);
                }
                if tree.tree.len() > 100 {
                    md += &format!("[…{} files elided…]\n", tree.tree.len() - 100);
                }
                md += "```\n\n";
            }
            if let Some(readme) = readme.filter(|r| r.encoding == "base64") {
                let compact: String = readme.content.split_whitespace().collect();
                if let Ok(bytes) = STANDARD.decode(compact) {
                    md += &format!("## README\n\n{}", String::from_utf8_lossy(&bytes));
                }
            }
            Some(Rendered::new(url, "text/markdown", "github-repo", vec![API_NOTE.into()], md))
        }
        Kind::Commit { sha } => {
            let c: CommitResp = api_json(g, ep, &format!("{base}/commits/{sha}")).await?;
            let message = c.commit.message;
            let (subject, body) = match message.split_once('\n') {
                Some((s, b)) => (s, b.trim()),
                None => (message.as_str(), ""),
            };
            let author = match &c.author {
                Some(u) => format!("@{}", u.login),
                None => c.commit.author.as_ref().and_then(|a| a.name.clone()).unwrap_or_else(|| "unknown".into()),
            };
            let authored_at = c.commit.author.as_ref().and_then(|a| a.date.clone()).unwrap_or_default();
            let mut md = format!("# {}\n\n", if subject.is_empty() { short(&c.sha, 7) } else { subject });
            md += &format!("**{}** · authored by {author}", short(&c.sha, 12));
            if !authored_at.is_empty() {
                md += &format!(" · {authored_at}");
            }
            md += "\n";
            if let Some(s) = &c.stats {
                let n = c.files.len();
                let plural = if n == 1 { "" } else { "s" };
                md += &format!("{n} file{plural} changed · +{} −{}\n", s.additions, s.deletions);
            }
            if !c.parents.is_empty() {
                let parents: Vec<&str> = c.parents.iter().map(|p| short(&p.sha, 12)).collect();
                md += &format!("Parents: {}\n", parents.join(", "));
            }
            if !body.is_empty() {
                md += &format!("\n{body}\n");
            }
            if !c.files.is_empty() {
                md += &format!("\n---\n\n## Files ({})\n\n", c.files.len());
                for f in &c.files {
                    let name = match &f.previous_filename {
                        Some(prev) => format!("{prev} → {}", f.filename),
                        None => f.filename.clone(),
                    };
                    md += &format!("### {name}\n\n{} · +{} −{}\n\n", f.status, f.additions, f.deletions);
                    match f.patch.as_deref().filter(|p| !p.is_empty()) {
                        Some(p) => md += &format!("```diff\n{p}\n```\n\n"),
                        None => md += "*No textual diff (binary or too large).*\n\n",
                    }
                }
            }
            Some(Rendered::new(url, "text/markdown", "github-commit", vec![API_NOTE.into()], md))
        }
        Kind::Issue { number, pull } => {
            let endpoint = if pull { format!("{base}/pulls/{number}") } else { format!("{base}/issues/{number}") };
            let issue: IssueResp = api_json(g, ep, &endpoint).await?;
            let mut md = format!("# {}\n\n", issue.title);
            md += &format!("**#{}** · {} · opened by @{}\n", issue.number, issue.state, login(&issue.user));
            md += &format!("Created: {} · Updated: {}\n", issue.created_at, issue.updated_at);
            if !issue.labels.is_empty() {
                let names: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
                md += &format!("Labels: {}\n", names.join(", "));
            }
            md += "\n---\n\n";
            md += issue.body.as_deref().filter(|b| !b.is_empty()).unwrap_or("*No description provided.*");
            md += "\n\n---\n\n";
            if issue.comments > 0 {
                let comments = issue_comments(g, ep, &base, number, issue.comments).await;
                if !comments.is_empty() {
                    let count = if issue.comments > comments.len() as u64 {
                        format!("{} of {}", comments.len(), issue.comments)
                    } else {
                        comments.len().to_string()
                    };
                    md += &format!("## Comments ({count})\n\n");
                    for c in comments {
                        md += &format!(
                            "### @{} · {}\n\n{}\n\n---\n\n",
                            login(&c.user),
                            c.created_at,
                            c.body.unwrap_or_default()
                        );
                    }
                }
            }
            let method = if pull { "github-pr" } else { "github-issue" };
            Some(Rendered::new(url, "text/markdown", method, vec![API_NOTE.into()], md))
        }
        Kind::Issues => {
            let list: Vec<IssueListItem> = api_json(g, ep, &format!("{base}/issues?state=open&per_page=30")).await?;
            let mut md = format!("# {owner}/{repo} - Open Issues\n\n");
            for issue in list.iter().filter(|i| i.pull_request.is_none()) {
                let labels = if issue.labels.is_empty() {
                    String::new()
                } else {
                    let names: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
                    format!(" [{}]", names.join(", "))
                };
                md += &format!("- **#{}** {}{labels}\n", issue.number, issue.title);
                md += &format!("  by @{} · {} comments · {}\n\n", login(&issue.user), issue.comments, issue.created_at);
            }
            Some(Rendered::new(url, "text/markdown", "github-issues", vec![API_NOTE.into()], md))
        }
    }
}

/// GET `ep.api` + `path`, parsed as JSON. `None` on any failure.
async fn api_json<T: DeserializeOwned>(g: &Guard, ep: &Endpoints, path: &str) -> Option<T> {
    let url = format!("{}{path}", ep.api);
    let page = load_page(g, &url, ep.opts(&url, true)).await;
    if !page.ok {
        return None;
    }
    serde_json::from_slice(&page.body).ok()
}

/// Pages through issue comments until `expected` are collected or a page comes back short.
async fn issue_comments(g: &Guard, ep: &Endpoints, base: &str, number: u64, expected: u64) -> Vec<Comment> {
    const PER_PAGE: usize = 100;
    let mut out = Vec::new();
    let mut page = 1;
    while (out.len() as u64) < expected {
        let path = format!("{base}/issues/{number}/comments?per_page={PER_PAGE}&page={page}");
        let Some(batch) = api_json::<Vec<Comment>>(g, ep, &path).await else { break };
        if batch.is_empty() {
            break;
        }
        let full = batch.len() == PER_PAGE;
        out.extend(batch);
        if !full {
            break;
        }
        page += 1;
    }
    out
}

fn short(s: &str, n: usize) -> &str {
    s.get(..n).unwrap_or(s)
}

fn login(user: &Option<User>) -> &str {
    user.as_ref().map_or("ghost", |u| u.login.as_str())
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct RepoInfo {
    full_name: String,
    #[serde(default)]
    default_branch: String,
    description: Option<String>,
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    forks_count: u64,
    #[serde(default)]
    open_issues_count: u64,
    language: Option<String>,
    license: Option<Named>,
}

#[derive(Deserialize)]
struct ContentItem {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct TreeResp {
    tree: Vec<TreeItem>,
}

#[derive(Deserialize)]
struct TreeItem {
    path: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Readme {
    content: String,
    encoding: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct IssueResp {
    number: u64,
    title: String,
    #[serde(default)]
    state: String,
    user: Option<User>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    comments: u64,
}

#[derive(Deserialize)]
struct IssueListItem {
    number: u64,
    title: String,
    user: Option<User>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    comments: u64,
    #[serde(default)]
    labels: Vec<Label>,
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Comment {
    user: Option<User>,
    #[serde(default)]
    created_at: String,
    body: Option<String>,
}

#[derive(Deserialize)]
struct GitPerson {
    name: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
struct CommitInner {
    #[serde(default)]
    message: String,
    author: Option<GitPerson>,
}

#[derive(Deserialize)]
struct Parent {
    sha: String,
}

#[derive(Deserialize)]
struct Stats {
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
}

#[derive(Deserialize)]
struct CommitFile {
    filename: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    patch: Option<String>,
    previous_filename: Option<String>,
}

#[derive(Deserialize)]
struct CommitResp {
    #[serde(default)]
    sha: String,
    commit: CommitInner,
    author: Option<User>,
    #[serde(default)]
    parents: Vec<Parent>,
    stats: Option<Stats>,
    #[serde(default)]
    files: Vec<CommitFile>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::test_server::{Resp, serve};

    fn endpoints(port: u16) -> Endpoints {
        let base = format!("http://127.0.0.1:{port}");
        Endpoints { api: base.clone(), raw: base, token: None }
    }

    #[test]
    fn parses_the_handled_sections() {
        let blob = parse("https://github.com/o/r/blob/main/docs/a.md").unwrap();
        assert_eq!(blob.kind, Kind::Blob { git_ref: "main".into(), path: "docs/a.md".into() });
        assert_eq!(
            parse("https://github.com/o/r/tree/dev").unwrap().kind,
            Kind::Tree { git_ref: Some("dev".into()), path: String::new() }
        );
        assert_eq!(parse("https://github.com/o/r/pull/3").unwrap().kind, Kind::Issue { number: 3, pull: true });
        assert_eq!(parse("https://github.com/o/r/issues/x").unwrap().kind, Kind::Issues);
        assert_eq!(parse("https://github.com/o/r").unwrap().kind, Kind::Repo);
        assert!(parse("https://github.com/o/r/pulls").is_none());
        assert!(parse("https://github.com/o/r/discussions/1").is_none());
        assert!(parse("https://example.com/o/r").is_none());
    }

    #[tokio::test]
    async fn blob_reads_the_raw_file() {
        let port = serve(|req| match req.path.as_str() {
            "/o/r/main/docs/a.md" => Resp::new(200, "text/plain", "# Hello"),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let url = "https://github.com/o/r/blob/main/docs/a.md";
        let out = handle(&Guard::allow_all(), &endpoints(port), url).await.unwrap();
        assert_eq!(out.method, "github-raw");
        assert_eq!(out.content, "# Hello");
        assert_eq!(out.final_url, format!("http://127.0.0.1:{port}/o/r/main/docs/a.md"));
    }

    #[tokio::test]
    async fn binary_blob_is_described_not_decoded() {
        let port = serve(|_| Resp::new(200, "application/octet-stream", vec![0u8, 159, 146, 150])).await;
        let url = "https://github.com/o/r/blob/main/logo.png";
        let out = handle(&Guard::allow_all(), &endpoints(port), url).await.unwrap();
        assert_eq!(out.method, "binary");
        assert_eq!(out.content, "Binary content (4 bytes) not shown.");
    }

    #[tokio::test]
    async fn issue_with_comments() {
        let port = serve(|req| match req.path.as_str() {
            "/repos/o/r/issues/7" => Resp::new(
                200,
                "application/json",
                r#"{"number":7,"title":"Crash on start","state":"open","user":{"login":"bob"},"created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-03T00:00:00Z","body":"It panics.","labels":[{"name":"bug"}],"comments":1}"#,
            ),
            "/repos/o/r/issues/7/comments?per_page=100&page=1" => Resp::new(
                200,
                "application/json",
                r#"[{"user":{"login":"alice"},"created_at":"2026-01-02T01:00:00Z","body":"Same here."}]"#,
            ),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = handle(&Guard::allow_all(), &endpoints(port), "https://github.com/o/r/issues/7").await.unwrap();
        assert_eq!(out.method, "github-issue");
        for s in [
            "# Crash on start",
            "**#7** · open · opened by @bob",
            "Labels: bug",
            "It panics.",
            "## Comments (1)",
            "### @alice",
            "Same here.",
        ] {
            assert!(out.content.contains(s), "missing {s:?} in:\n{}", out.content);
        }
    }

    #[tokio::test]
    async fn repo_with_tree_and_readme() {
        let port = serve(|req| match req.path.as_str() {
            "/repos/o/r" => Resp::new(
                200,
                "application/json",
                r#"{"full_name":"o/r","description":"Demo","default_branch":"main","stargazers_count":3,"forks_count":1,"open_issues_count":0,"language":"Rust","license":null}"#,
            ),
            "/repos/o/r/git/trees/HEAD?recursive=1" => {
                Resp::new(200, "application/json", r#"{"tree":[{"path":"src/lib.rs","type":"blob"}]}"#)
            }
            "/repos/o/r/readme" => Resp::new(200, "application/json", r#"{"content":"IyBIZWxsbwo=\n","encoding":"base64"}"#),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = handle(&Guard::allow_all(), &endpoints(port), "https://github.com/o/r").await.unwrap();
        assert_eq!(out.method, "github-repo");
        for s in ["# o/r", "Demo", "Language: Rust", "src/lib.rs", "## README", "# Hello"] {
            assert!(out.content.contains(s), "missing {s:?} in:\n{}", out.content);
        }
    }

    #[tokio::test]
    async fn failed_api_call_falls_back_to_generic_path() {
        let port = serve(|_| Resp::new(404, "application/json", "{}")).await;
        assert!(handle(&Guard::allow_all(), &endpoints(port), "https://github.com/o/r/issues/9").await.is_none());
        assert!(handle(&Guard::allow_all(), &endpoints(1), "https://example.com/x").await.is_none());
    }
}
