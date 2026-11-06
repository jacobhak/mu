//! Review facts: the git snapshots and diff of base..worktree, the changed files, the plan and
//! the request the lead reviewer is given. Shared by `/review` and `mu review`.
use mu_base::git::git;
use mu_base::message::{Message, text_of};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const MAX_FILES_LISTED: usize = 400;

const REQUEST: &str = include_str!("../prompts/review-request.md");

pub async fn repo_root(cwd: &Path) -> Result<PathBuf, String> {
    mu_base::git::repo_root(cwd).await.map_err(|e| format!("review needs a git repository: {e}"))
}

/// A tree sha of the whole working tree, untracked non-ignored files included. Uses a copy of the
/// index, so neither the real index nor the working tree is touched.
pub(crate) async fn snapshot_tree(cwd: &Path) -> Result<String, String> {
    let real = git(cwd, &["rev-parse", "--git-path", "index"], None).await?;
    let real = cwd.join(real.trim());
    let tmp = std::env::temp_dir().join(format!("mu-review-index-{}", uuid::Uuid::new_v4()));
    let res = async {
        if real.exists() {
            std::fs::copy(&real, &tmp).map_err(|e| format!("cannot copy index: {e}"))?;
            // Git's racy-entry check compares index entries with the index file's mtime; the copy must keep
            // the real index's mtime, or a same-tick rewrite of a tracked file looks unchanged.
            let mtime =
                std::fs::metadata(&real).and_then(|m| m.modified()).map_err(|e| format!("cannot stat index: {e}"))?;
            std::fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|f| f.set_modified(mtime))
                .map_err(|e| format!("cannot stamp index copy: {e}"))?;
        }
        git(cwd, &["add", "-A"], Some(&tmp)).await?;
        git(cwd, &["write-tree"], Some(&tmp)).await
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    res.map(|s| s.trim().to_string())
}

pub(crate) async fn head_tree(cwd: &Path) -> Result<String, String> {
    match git(cwd, &["rev-parse", "HEAD^{tree}"], None).await {
        Ok(s) => Ok(s.trim().to_string()),
        Err(_) => git(cwd, &["hash-object", "-t", "tree", "/dev/null"], None).await.map(|s| s.trim().to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    /// `None` for a binary file.
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

impl FileStat {
    fn changed(&self) -> u32 {
        self.added.unwrap_or(0) + self.removed.unwrap_or(0)
    }
}

pub(crate) async fn numstat(root: &Path, base: &str, cur: &str) -> Result<Vec<FileStat>, String> {
    git(root, &["diff", "--numstat", "-z", "--no-renames", base, cur], None).await.map(|s| parse_numstat(&s))
}

pub(crate) fn parse_numstat(raw: &str) -> Vec<FileStat> {
    raw.split('\0')
        .filter_map(|rec| {
            let mut it = rec.trim_start_matches('\n').splitn(3, '\t');
            let (a, r, path) = (it.next()?, it.next()?, it.next()?);
            Some(FileStat { path: path.into(), added: a.parse().ok(), removed: r.parse().ok() })
        })
        .collect()
}

pub(crate) fn excluded_reason(path: &str, binary: bool) -> Option<&'static str> {
    if binary {
        return Some("binary");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name.ends_with("-lock.yml")
    {
        Some("lock file")
    } else if name.ends_with(".min.js") || name.ends_with(".min.css") {
        Some("minified")
    } else if path.contains(".generated.") {
        Some("generated")
    } else if name.ends_with(".snap") {
        Some("snapshot")
    } else if name.ends_with(".map") {
        Some("source map")
    } else if ["dist/", "build/", "out/"].iter().any(|p| path.starts_with(p)) {
        Some("build output")
    } else if path.contains("node_modules/") || path.contains("vendor/") {
        Some("vendor")
    } else {
        None
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

/// What the user told the implementer since the plan was approved.
pub(crate) fn user_notes(messages: &[Message], since: i64) -> Vec<String> {
    let mut out: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content, timestamp, synthetic: false } if *timestamp >= since => {
                let t = text_of(content);
                let t = t.trim();
                let skip = t.is_empty()
                    || ["<system-notice>", "<system-reminder>", "I ran `", "Prior model work/tool state available."]
                        .iter()
                        .any(|p| t.starts_with(p));
                (!skip).then(|| truncate(t, 1500))
            }
            _ => None,
        })
        .collect();
    let drop = out.len().saturating_sub(15);
    out.drain(..drop);
    out
}

pub struct RequestInput<'a> {
    pub root: &'a Path,
    pub base: &'a str,
    pub cur: &'a str,
    pub base_label: &'a str,
    pub plan_path: Option<PathBuf>,
    pub files: &'a [FileStat],
    pub excluded: &'a [(FileStat, &'a str)],
    pub notes: &'a [String],
    pub focus: Option<&'a str>,
}

fn stat_cell(f: &FileStat) -> String {
    match (f.added, f.removed) {
        (Some(a), Some(r)) => format!("+{a}/-{r}"),
        _ => "binary".into(),
    }
}

pub fn request(r: &RequestInput) -> String {
    let mut sorted: Vec<&FileStat> = r.files.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.changed()));
    let mut files = String::from("| path | +a/-r |\n|---|---|\n");
    for f in sorted.iter().take(MAX_FILES_LISTED) {
        let _ = writeln!(files, "| {} | {} |", f.path, stat_cell(f));
    }
    if sorted.len() > MAX_FILES_LISTED {
        let _ = writeln!(
            files,
            "… {} more files: run `git diff --stat {} {}`",
            sorted.len() - MAX_FILES_LISTED,
            r.base,
            r.cur
        );
    }
    let excluded = if r.excluded.is_empty() {
        "none".to_string()
    } else {
        r.excluded.iter().map(|(f, why)| format!("- {} ({why})", f.path)).collect::<Vec<_>>().join("\n")
    };
    let (a, d) = r.files.iter().fold((0, 0), |(a, d), f| (a + f.added.unwrap_or(0), d + f.removed.unwrap_or(0)));
    let totals = format!("{} reviewable files, +{a}/-{d} lines", r.files.len());
    let plan = match &r.plan_path {
        Some(p) => format!("The approved plan is at `{}`. Read it first.", p.display()),
        None => "No plan: review for correctness and quality only.".into(),
    };
    let notes = if r.notes.is_empty() {
        "none".to_string()
    } else {
        r.notes.iter().enumerate().map(|(i, n)| format!("{}. {n}", i + 1)).collect::<Vec<_>>().join("\n")
    };
    REQUEST
        .replace("{{root}}", &r.root.display().to_string())
        .replace("{{base_label}}", r.base_label)
        .replace("{{base}}", r.base)
        .replace("{{cur}}", r.cur)
        .replace("{{totals}}", &totals)
        .replace("{{plan}}", &plan)
        .replace("{{files}}", files.trim_end())
        .replace("{{excluded}}", &excluded)
        .replace("{{notes}}", &notes)
        .replace("{{focus}}", r.focus.unwrap_or("none"))
}

/// Splits changed files into the reviewable ones and `(file, reason)` exclusions.
pub(crate) fn split_excluded(stats: Vec<FileStat>) -> (Vec<FileStat>, Vec<(FileStat, &'static str)>) {
    let (mut files, mut excluded) = (Vec::new(), Vec::new());
    for f in stats {
        match excluded_reason(&f.path, f.added.is_none()) {
            Some(why) => excluded.push((f, why)),
            None => files.push(f),
        }
    }
    (files, excluded)
}

/// The tree of a git ref.
pub(crate) async fn rev_tree(root: &Path, rev: &str) -> Result<String, String> {
    git(root, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{tree}}")], None)
        .await
        .map(|t| t.trim().to_string())
        .map_err(|_| format!("cannot resolve --base `{rev}` to a tree"))
}

/// The base a review diffs against.
pub enum Base<'a> {
    /// A tree already known, e.g. the plan approval snapshot. `label` names it in the request.
    Tree { tree: String, label: &'a str },
    /// HEAD's tree, or the empty tree before the first commit. `label` names it in the request.
    Head { label: &'a str },
    /// A git ref, resolved to its tree. The request calls it `--base <ref>`.
    Rev(&'a str),
}

/// A repository's working tree, snapshotted as a tree.
pub struct Worktree {
    pub root: PathBuf,
    pub cur: String,
}

impl Worktree {
    /// Snapshots the working tree of the repository at `root` (see `repo_root`).
    pub async fn snapshot(root: PathBuf) -> Result<Self, String> {
        let cur = snapshot_tree(&root).await?;
        Ok(Self { root, cur })
    }

    /// Gathers the changes from `base` to the snapshot. Errors are bare messages for the caller to show.
    pub async fn gather(self, base: Base<'_>) -> Result<Facts, String> {
        let Worktree { root, cur } = self;
        let (base, base_label) = match base {
            Base::Tree { tree, label } => (tree, label.to_string()),
            Base::Head { label } => (head_tree(&root).await?, label.to_string()),
            Base::Rev(r) => (rev_tree(&root, r).await?, format!("--base {r}")),
        };
        let stats = numstat(&root, &base, &cur).await?;
        let (files, excluded) = split_excluded(stats);
        let (added, removed) =
            files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
        Ok(Facts { root, base, base_label, cur, files, excluded, added, removed })
    }
}

/// What a review runs on: the snapshots compared and the changed files.
pub struct Facts {
    pub root: PathBuf,
    pub base: String,
    base_label: String,
    pub cur: String,
    /// Reviewable files. Excluded ones are listed apart and not counted in `added`/`removed`.
    pub files: Vec<FileStat>,
    pub excluded: Vec<(FileStat, &'static str)>,
    pub added: u32,
    pub removed: u32,
}

impl Facts {
    /// Whether any file changed, excluded ones included.
    pub fn changed(&self) -> bool {
        !self.files.is_empty() || !self.excluded.is_empty()
    }

    /// The lead reviewer's request. `plan` is the plan file, already checked to exist.
    pub fn request(&self, plan: Option<PathBuf>, notes: &[String], focus: Option<&str>) -> String {
        request(&RequestInput {
            root: &self.root,
            base: &self.base,
            cur: &self.cur,
            base_label: &self.base_label,
            plan_path: plan,
            files: &self.files,
            excluded: &self.excluded,
            notes,
            focus,
        })
    }
}
