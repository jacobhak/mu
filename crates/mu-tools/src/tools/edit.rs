use super::Tools;
use super::uri::Scheme;
use super::uri::physical;
use crate::permissions::Gate;
use async_trait::async_trait;
use mu_base::tool::{CHANGE_TEXT_MAX, FileChange, FileMeta, ToolMeta, ToolOutput};
use omp_edit::modes::hashline::input::{Patch, SplitOptions};
use omp_edit::modes::hashline::types::FileOp as HashlineOp;
use omp_edit::modes::sloppy::parse::split_sloppy_sections;
use omp_edit::{
    ApplyRequest, EditError, EditMode, EditResult, EditWriter, PathPolicy, Session, UrlResolution, WriteRequest,
    WriteResponse, engine::FileOp, session::SessionConfig,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct Args {
    input: String,
}

/// One file an edit payload writes, as omp-edit addresses it.
pub(crate) struct Target {
    pub authored: String,
    /// Where omp-edit writes it; `None` when it refuses the target.
    pub abs: Option<PathBuf>,
    /// An internal URL (`local://`), which stays inside mu's own pages.
    pub internal: bool,
}

impl Args {
    /// The files the payload names: the same splitting and resolving the editor does.
    pub(crate) fn targets(&self, t: &Tools) -> Result<Vec<Target>, String> {
        t.edit_targets(&self.input)
    }

    /// The paths as written; empty when the payload cannot be split.
    pub(crate) fn authored(&self, t: &Tools) -> Vec<String> {
        self.targets(t).map(|v| v.into_iter().map(|x| x.authored).collect()).unwrap_or_default()
    }
}

/// A header's words after `***`, lowercased, with whitespace runs collapsed to one space.
fn header_words(rest: &str) -> String {
    rest.split_whitespace().map(str::to_ascii_lowercase).collect::<Vec<_>>().join(" ")
}

/// `*** <action> Block: <selector>`, or a malformed block header, which the parser reports itself.
fn is_block_header(words: &str) -> bool {
    const ACTIONS: [&str; 9] = [
        "",
        "replace ",
        "delete ",
        "cut ",
        "insert ",
        "insert before ",
        "insert after ",
        "paste before ",
        "paste after ",
    ];
    words
        .split_once("block")
        .is_some_and(|(head, tail)| ACTIONS.contains(&head) && (tail.is_empty() || tail.starts_with([':', ' '])))
}

/// The first line that looks like a sloppy-format header but is not one. The parser reads such a line as
/// body text, so `*** Replace now` would be written into the file. Only unindented lines count: an indented
/// one is how a literal `*** …` line is written.
fn near_miss_header(input: &str) -> Option<(usize, &str)> {
    input.lines().enumerate().find_map(|(i, line)| {
        let rest = line.strip_prefix("***")?;
        let spaced = rest.starts_with([' ', '\t']);
        let words = header_words(rest);
        if spaced && is_block_header(&words) {
            return None;
        }
        let word = |w: &str| words.strip_prefix(w).filter(|t| t.is_empty() || t.starts_with(' '));
        let op = ["find", "replace", "insert before", "insert after", "cut", "paste before", "paste after"]
            .iter()
            .find_map(|w| word(w));
        let bad = match op {
            // An operator header takes no argument, except `all` after `Find`.
            Some(tail) => !spaced || !(tail.is_empty() || words == "find all"),
            None => {
                !spaced
                    && ["edit file", "find", "replace", "insert", "cut", "paste", "delete"]
                        .iter()
                        .any(|w| words.starts_with(w))
                    || spaced && ["all", "insert", "paste"].iter().any(|w| word(w).is_some())
            }
        };
        bad.then_some((i + 1, line.trim_end()))
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Header {
    Edit,
    Find,
    Replace,
    Insert {
        before: bool,
    },
    /// Cut, Paste and the Block actions: no anchor, no insert body.
    Other,
}

fn header_of(line: &str) -> Option<Header> {
    let rest = line.trim().strip_prefix("***")?;
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let words = header_words(rest);
    if is_block_header(&words) {
        return Some(Header::Other);
    }
    let w: Vec<&str> = words.split(' ').collect();
    match w.as_slice() {
        ["find"] | ["find", "all"] => Some(Header::Find),
        ["replace"] => Some(Header::Replace),
        ["insert", "before"] => Some(Header::Insert { before: true }),
        ["insert", "after"] => Some(Header::Insert { before: false }),
        ["cut"] | ["paste", "before" | "after"] => Some(Header::Other),
        ["edit", "file", ..] | ["edit", "file:", ..] => Some(Header::Edit),
        _ => None,
    }
}

fn trim_blank<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(start, |i| i + 1);
    &lines[start..end]
}

/// The first `Insert Before/After` whose body repeats the `Find` lines it is anchored on (the anchor goes in
/// `Find` only; the insert body holds just the new text). Returns the `*** Find` line number and the header.
/// Anchors under 8 non-blank characters (`}`, `end`) are skipped: new code legitimately repeats those.
fn repeated_anchor(input: &str) -> Option<(usize, &'static str)> {
    fn check(find: &[&str], body: &[&str], before: bool, at: usize) -> Option<(usize, &'static str)> {
        let (anchor, body) = (trim_blank(find), trim_blank(body));
        let chars: usize = anchor.iter().map(|l| l.trim().chars().count()).sum();
        let same =
            |a: &[&str], b: &[&str]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.trim_end() == y.trim_end());
        let hit = chars >= 8
            && body.len() >= anchor.len()
            && if before {
                same(&body[body.len() - anchor.len()..], anchor)
            } else {
                same(&body[..anchor.len()], anchor)
            };
        hit.then_some((at, if before { "*** Insert Before" } else { "*** Insert After" }))
    }
    let (mut find, mut body): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let (mut find_at, mut part) = (0, None::<Header>);
    for (i, line) in input.lines().enumerate() {
        let Some(h) = header_of(line) else {
            match part {
                Some(Header::Find) => find.push(line),
                Some(Header::Insert { .. }) => body.push(line),
                _ => {}
            }
            continue;
        };
        if let Some(Header::Insert { before }) = part
            && let Some(hit) = check(&find, &body, before, find_at)
        {
            return Some(hit);
        }
        match h {
            Header::Edit => find.clear(),
            Header::Find => {
                find.clear();
                find_at = i + 1;
            }
            Header::Insert { .. } => body.clear(),
            Header::Replace | Header::Other => {}
        }
        part = Some(h);
    }
    match part {
        Some(Header::Insert { before }) => check(&find, &body, before, find_at),
        _ => None,
    }
}

/// Sloppy payload mistakes the parser would accept silently: a header-looking line it reads as text, and an
/// insert body that repeats its anchor. Refused before anything is written.
fn refuse_malformed(mode: EditMode, input: &str) -> Option<ToolOutput> {
    if mode != EditMode::Sloppy {
        return None;
    }
    if let Some((n, line)) = near_miss_header(input) {
        return Some(ToolOutput::err(format!(
            "Line {n}, `{line}`, looks like an edit header but is not one, so it would be written into the file as \
             text. Headers are `*** Edit File: <path>` (with ` all` after the path to change every match), \
             `*** Find` or `*** Find all`, then one of `*** Replace`, `*** Insert Before`, `*** Insert After`, \
             `*** Cut`, `*** Paste Before` and `*** Paste After`, each alone on its line; a block action names its \
             opening line: `*** Replace Block: <prefix>`, `*** Delete Block: …`, `*** Cut Block: …`, \
             `*** Paste Before Block: …`, `*** Paste After Block: …`. If the line is meant as text, indent it. \
             Nothing was changed."
        )));
    }
    let (n, header) = repeated_anchor(input)?;
    Some(ToolOutput::err(format!(
        "The `{header}` body for the `*** Find` at line {n} repeats the Find lines, so they would appear twice. \
         The Find lines are only the anchor; put just the new text in the insert body. If the repeat is \
         intended, make the Find differ (add a neighbouring line to it). Nothing was changed."
    )))
}

#[cfg(test)]
impl Args {
    pub fn for_test(input: String) -> Self {
        Self { input }
    }
}

static MODE: std::sync::OnceLock<EditMode> = std::sync::OnceLock::new();

/// Set the edit format once at startup from `[edit] mode` (`MU_EDIT_MODE` overrides). Only the
/// formats that take a single `input` string are offered: `hashline` (default) and `sloppy`.
pub fn set_mode(configured: &str) -> Result<(), String> {
    let name = std::env::var("MU_EDIT_MODE").unwrap_or_else(|_| configured.to_string());
    let m = match name.trim() {
        "" | "hashline" => EditMode::Hashline,
        "sloppy" => EditMode::Sloppy,
        other => return Err(format!("unknown edit mode {other:?}; use \"hashline\" or \"sloppy\"")),
    };
    match MODE.set(m) {
        Ok(()) => Ok(()),
        Err(again) if MODE.get() == Some(&again) => Ok(()),
        Err(_) => Err(format!("the edit mode is already {:?}; restart mu to change it", mode())),
    }
}

pub fn mode() -> EditMode {
    *MODE.get().unwrap_or(&EditMode::Hashline)
}

pub fn is_hashline() -> bool {
    mode() == EditMode::Hashline
}

/// Writes the files an edit changes and keeps each file's text before and after. A file the
/// payload did not name (omp-edit rebinds a section whose path is missing onto another file with the
/// same tag) is written only where the gate would let a write of it run.
struct FsWriter {
    changes: std::sync::Mutex<Vec<FileChange>>,
    /// The physical paths the payload names, which the gate judged before the call ran.
    named: HashSet<PathBuf>,
    gate: Gate,
}

fn werr(e: impl std::fmt::Display) -> EditError {
    EditError::Writer(e.to_string())
}

/// A file's text: `Some(None)` when it does not exist, `None` when it cannot be kept (binary or large).
async fn snapshot(p: &std::path::Path) -> Option<Option<String>> {
    match tokio::fs::metadata(p).await {
        Err(_) => Some(None),
        Ok(m) if m.len() as usize > CHANGE_TEXT_MAX => None,
        Ok(_) => tokio::fs::read_to_string(p).await.ok().map(Some),
    }
}

impl FsWriter {
    fn new(named: HashSet<PathBuf>, gate: Gate) -> Self {
        Self { changes: Default::default(), named, gate }
    }

    /// Refuses a write to a file the payload did not name and the gate would ask about.
    fn check(&self, r: &WriteRequest) -> EditResult<()> {
        for path in std::iter::once(&r.absolute).chain(r.move_to.as_ref()) {
            let p = physical(path);
            if !(self.named.contains(&p) || self.gate.allows_write("edit", &p)) {
                return Err(werr(format!(
                    "{} resolved to {}, a file this edit did not name; it was not changed. Repeat the edit with that path.",
                    r.display,
                    p.display()
                )));
            }
        }
        Ok(())
    }

    fn record(&self, path: &std::path::Path, old: Option<Option<String>>, new: Option<&str>) {
        let Some(old) = old else { return };
        if new.is_some_and(|n| n.len() > CHANGE_TEXT_MAX) {
            return;
        }
        if let Ok(mut c) = self.changes.lock() {
            c.push(FileChange { path: path.to_path_buf(), old, new: new.map(String::from) });
        }
    }
}

#[async_trait]
impl EditWriter for FsWriter {
    async fn write(&self, r: WriteRequest) -> EditResult<WriteResponse> {
        if r.op != FileOp::Noop {
            self.check(&r)?;
        }
        match r.op {
            FileOp::Delete => {
                let old = snapshot(&r.absolute).await;
                tokio::fs::remove_file(&r.absolute).await.map_err(werr)?;
                self.record(&r.absolute, old, None);
                Ok(WriteResponse::default())
            }
            FileOp::Noop => Ok(WriteResponse::default()),
            FileOp::Create | FileOp::Update => {
                let content = r.content.ok_or_else(|| werr("edit request omitted content"))?;
                let dest = r.move_to.as_deref().unwrap_or(&r.absolute);
                let old = snapshot(&r.absolute).await;
                if let Some(p) = dest.parent() {
                    tokio::fs::create_dir_all(p).await.map_err(werr)?;
                }
                // Through a symlink: the rename must replace the target, not the link.
                let target = physical(dest);
                let bytes = content.clone().into_bytes();
                tokio::task::spawn_blocking(move || mu_common::fs::write_atomic(&target, &bytes))
                    .await
                    .map_err(werr)?
                    .map_err(werr)?;
                if r.move_to.is_some() {
                    if let Ok(m) = tokio::fs::metadata(&r.absolute).await {
                        let _ = tokio::fs::set_permissions(dest, m.permissions()).await;
                    }
                    tokio::fs::remove_file(&r.absolute).await.map_err(werr)?;
                }
                self.record(dest, old, Some(&content));
                Ok(WriteResponse { written: content, ..Default::default() })
            }
        }
    }
}

impl Tools {
    fn edit_policy(&self, plan: bool) -> PathPolicy {
        PathPolicy {
            cwd: self.cwd.clone(),
            home_dir: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
            url_schemes: vec!["local".into(), "artifact".into(), "skill".into()],
            url_alias_schemes: vec![],
            plan_writable_roots: vec![self.local_root()],
            plan_active: plan,
            block_auto_generated: true,
        }
    }

    fn url_resolution(&self, url: &str) -> UrlResolution {
        match self.resolve(url) {
            Ok(r) => match r.scheme {
                Some(Scheme::Local) => UrlResolution { absolute: Some(r.path), error: None, plan_writable: true },
                Some(sc) => UrlResolution {
                    absolute: None,
                    error: Some(format!("{}:// is read-only", sc.name())),
                    plan_writable: false,
                },
                None => UrlResolution { absolute: Some(r.path), error: None, plan_writable: false },
            },
            Err(e) => UrlResolution { absolute: None, error: Some(e), plan_writable: false },
        }
    }

    /// The files `input` writes: split by the active format's own splitter and resolved by the
    /// editor's own policy, so the gate reads a payload as the editor does.
    pub(crate) fn edit_targets(&self, input: &str) -> Result<Vec<Target>, String> {
        self.edit_targets_in(mode(), input)
    }

    fn edit_targets_in(&self, mode: EditMode, input: &str) -> Result<Vec<Target>, String> {
        let authored: Vec<String> = if mode == EditMode::Sloppy {
            split_sloppy_sections(input).map_err(|e| e.to_string())?.into_iter().map(|s| s.path).collect()
        } else {
            let patch =
                Patch::parse(input, &SplitOptions { cwd: Some(&self.cwd), path: None }).map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for s in &patch.sections {
                out.push(s.path.clone());
                if let Ok(Some(HashlineOp::Move { dest })) = s.file_op() {
                    out.push(dest.clone());
                }
            }
            out
        };
        let policy = self.edit_policy(false);
        Ok(authored
            .into_iter()
            .map(|a| {
                let mut urls = HashMap::new();
                if let Some(url) = policy.url_target(&a) {
                    urls.insert(url.to_string(), self.url_resolution(&url));
                }
                let abs = policy.resolve(&a, &urls).ok().map(|r| r.absolute);
                Target { internal: policy.is_internal_url(&a), abs, authored: a }
            })
            .collect())
    }

    pub async fn edit(&self, a: Args, plan: bool) -> ToolOutput {
        if let Some(refusal) = refuse_malformed(mode(), &a.input) {
            return refusal;
        }
        let policy = self.edit_policy(plan);
        let named: HashSet<PathBuf> = self
            .edit_targets(&a.input)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| t.abs)
            .map(|p| physical(&p))
            .collect();
        let cfg = SessionConfig {
            mode: mode(),
            policy,
            allow_fuzzy: true,
            fuzzy_threshold: 0.95,
            enforce_seen_lines: true,
            max_columns: 768,
            raw_input: false,
        };
        let mut s = Session::new(cfg, self.edit_store.clone());
        s.set_args_json(&json!({"input": a.input}).to_string());
        s.finish();
        for url in s.begin_apply_url_targets() {
            let r = self.url_resolution(&url);
            s.provide(url, r);
        }
        let writer = FsWriter::new(named, self.permissions.gate(&self.cwd));
        match s.apply(ApplyRequest::default(), &writer).await {
            Ok(o) => {
                let mut out = ToolOutput::ok(o.text);
                out.meta = o
                    .files
                    .first()
                    .map(|f| ToolMeta::File(FileMeta { path: f.display.clone(), tag: None, plan: false }));
                let diffs: Vec<&str> = o.files.iter().map(|f| f.diff.as_str()).filter(|d| !d.is_empty()).collect();
                if !diffs.is_empty() {
                    out.display = Some(diffs.join("\n"));
                }
                out.changes = writer.changes.into_inner().unwrap_or_default();
                out
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Permissions;
    use mu_base::config::MuPermissions;
    use mu_base::permissions::{PermissionMode, RuleSet};

    fn tools_in(dir: &std::path::Path) -> Tools {
        let mut t = Tools::for_test();
        t.cwd = dir.to_path_buf();
        t
    }

    #[test]
    fn targets_are_the_files_the_editor_resolves() {
        let t = tools_in(std::path::Path::new("/work"));
        let abs = |input: &str, mode| -> Vec<Option<PathBuf>> {
            t.edit_targets_in(mode, input).unwrap().into_iter().map(|x| x.abs).collect()
        };
        let some = |p: &str| Some(PathBuf::from(p));
        for header in ["file:///etc/zz#1A2B", "@/etc/zz", ":/etc/zz", "/etc/zz#1A2B"] {
            let input = format!("[{header}]\nPUT 1:\n+x");
            assert_eq!(abs(&input, EditMode::Hashline), [some("/etc/zz")], "{header}");
        }
        assert_eq!(
            abs("[a.ts#1A2B]\nMV ../moved.ts", EditMode::Hashline),
            [some("/work/a.ts"), some("/moved.ts")],
            "a move names its destination"
        );
        let sloppy = "***  Edit   File: /abs/x\n*** Find\na\n*** Replace\nb";
        let targets = t.edit_targets_in(EditMode::Sloppy, sloppy).unwrap();
        assert_eq!(targets.iter().map(|x| x.authored.as_str()).collect::<Vec<_>>(), ["/abs/x"]);
        assert!(t.edit_targets_in(EditMode::Hashline, "[local://plan.md#1A2B]\nPUT 1:\n+x").unwrap()[0].internal);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_file_the_edit_did_not_name_is_written_only_where_the_gate_allows() {
        let d = std::env::temp_dir().join(format!("mu-fswriter-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("other.txt");
        std::fs::write(&file, "old\n").unwrap();
        let request = || WriteRequest {
            absolute: file.clone(),
            display: "other.txt".into(),
            op: FileOp::Update,
            move_to: None,
            content: Some("new\n".into()),
            flush_lsp: false,
            lsp_batch_id: None,
        };
        let asking = Permissions::new(&MuPermissions {
            mode: PermissionMode::Ask,
            readonly_bash: true,
            rules: RuleSet::default(),
        });
        let writer = FsWriter::new(HashSet::new(), asking.gate(&d));
        let err = writer.write(request()).await.unwrap_err().to_string();
        assert!(err.contains("did not name"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");

        let named = HashSet::from([physical(&file)]);
        FsWriter::new(named, asking.gate(&d)).write(request()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");

        std::fs::write(&file, "old\n").unwrap();
        let yolo = Permissions::default();
        FsWriter::new(HashSet::new(), yolo.gate(&d)).write(request()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
        std::fs::remove_dir_all(&d).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_write_leaves_the_file_and_a_symlink_intact() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("mu-fswriter-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let locked = d.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let file = locked.join("a.txt");
        std::fs::write(&file, "old\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let request = |path: &std::path::Path| WriteRequest {
            absolute: path.to_path_buf(),
            display: "a.txt".into(),
            op: FileOp::Update,
            move_to: None,
            content: Some("new\n".into()),
            flush_lsp: false,
            lsp_batch_id: None,
        };
        let writer = FsWriter::new(HashSet::new(), Permissions::default().gate(&d));
        let probe = std::fs::File::create(locked.join("probe")).is_ok();
        let res = writer.write(request(&file)).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !probe {
            assert!(res.is_err(), "a write that cannot create its temp file must fail");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
        }

        let link = d.join("link.txt");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        writer.write(request(&link)).await.unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn near_miss_headers_are_found_and_real_ones_are_not() {
        let ok = "*** Edit File: a.rs\n*** Find\nx\n*** Replace\ny\n*** Edit File: b.rs all\n*** Insert Before\nz\n\
                  *** Insert After\n***bold*** text\n*** Finding things\n*** End Patch\n    *** Insert here\n";
        let new =
            ["Find all", "Cut", "Paste  Before", "Paste After", "Replace Block: if (x) {", "Delete Block: fn old("];
        let blocks = ["Cut Block: fn a(", "Paste After Block: fn b(", "Edit File: blocks/a.rs"];
        for h in new.iter().chain(&blocks) {
            assert_eq!(near_miss_header(&format!("{ok}*** {h}\n")), None, "{h}");
        }
        assert_eq!(near_miss_header(ok), None);
        for (bad, line) in [
            ("*** Find\nx\n*** Paste Before here\n", 3),
            ("*** Find everything\n", 1),
            ("*** all\n", 1),
            ("*** Paste\n", 1),
            ("*** Cut here\n", 1),
            ("*** Replace blocking\n", 1),
            ("***Cut Block: fn a(\n", 1),
            ("***Find\n", 1),
            ("*** Replace now\n", 1),
            ("*** Insert\n", 1),
            ("***Edit File: a.rs\n", 1),
            ("*** Insert After here\n", 1),
        ] {
            assert_eq!(near_miss_header(bad).map(|(n, _)| n), Some(line), "{bad:?}");
        }
    }

    #[test]
    fn a_near_miss_is_refused_in_sloppy_mode_only() {
        let input = "*** Edit File: x.txt\n*** Find\na\n*** Replace\nb\n*** Paste Before here\n";
        let r = refuse_malformed(EditMode::Sloppy, input).unwrap();
        assert!(r.is_error && r.text.contains("Line 6") && r.text.contains("indent it"), "{}", r.text);
        assert!(refuse_malformed(EditMode::Hashline, input).is_none());
    }

    #[test]
    fn an_insert_body_that_repeats_its_anchor_is_found() {
        let before = "*** Edit File: a.rs\n*** Find\n#[cfg(test)]\nimpl Args {\n*** Insert Before\nfn new() {}\n\n#[cfg(test)]\nimpl Args {\n";
        assert_eq!(repeated_anchor(before), Some((2, "*** Insert Before")));
        let after = "*** Edit File: a.rs\n*** Find\nlet total = compute();\n*** Insert After\nlet total = compute();\nuse(total);\n";
        assert_eq!(repeated_anchor(after), Some((2, "*** Insert After")));
        let r = refuse_malformed(EditMode::Sloppy, after).unwrap();
        assert!(r.is_error && r.text.contains("line 2") && r.text.contains("only the anchor"), "{}", r.text);
        let all = after.replace("*** Find\n", "*** Find all\n");
        assert_eq!(repeated_anchor(&all), Some((2, "*** Insert After")));
    }

    #[test]
    fn correct_inserts_and_short_anchors_pass() {
        let ok = "*** Edit File: a.rs\n*** Find\nlet total = compute();\n*** Insert After\nuse(total);\n\
                  *** Find\nimpl Args {\n*** Insert Before\n#[cfg(test)]\n\
                  *** Find\n}\n*** Insert Before\nfn f() {\n}\n\
                  *** Find\nlet x = 1;\n*** Replace\nlet x = 1;\nlet y = 2;\n";
        // A block body is not part of the insert before it, even when it ends with that insert's anchor.
        let block = format!(
            "{ok}{}",
            "*** Find all\nlet total = compute();\n*** Insert Before\nuse(total);\n\
             *** Replace Block: fn run(\nfn run() {\nlet total = compute();\n"
        );
        assert_eq!(repeated_anchor(&block), None);
        assert_eq!(repeated_anchor(ok), None);
        assert!(refuse_malformed(EditMode::Sloppy, ok).is_none());
    }
}
