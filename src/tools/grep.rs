use super::Tools;
use super::output::ToolOutput;
use crate::walk::{block_on_walk, walk_options};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use pi_walker::{Detail, FileType, Fs, walk};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct Args {
    pattern: String,
    path: Option<String>,
    case: Option<bool>,
}

const PER_FILE: usize = 20;
const TOTAL: usize = 200;
const MAX_COL: usize = 512;

impl Tools {
    /// The filesystem walk and search run on a blocking thread so the event loop keeps turning.
    pub async fn grep(&self, a: Args) -> ToolOutput {
        let root = self.abs(a.path.as_deref().unwrap_or("."));
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || grep_blocking(&cwd, root, a))
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("grep failed: {e}")))
    }
}

fn grep_blocking(cwd: &std::path::Path, root: PathBuf, a: Args) -> ToolOutput {
    let display = |abs: &std::path::Path| super::display_in(cwd, abs);
    let matcher = match RegexMatcherBuilder::new().case_insensitive(!a.case.unwrap_or(true)).build(&a.pattern) {
        Ok(m) => m,
        Err(e) => return ToolOutput::err(format!("Invalid pattern: {e}")),
    };
    if !root.exists() {
        return ToolOutput::err(format!("File not found: {}", display(&root)));
    }
    let files: Vec<(String, PathBuf)> = if root.is_file() {
        vec![(display(&root), root.clone())]
    } else {
        // Full detail so sizes are known for the 4 MiB cap.
        let opts = walk::Options { detail: Detail::Full, ..walk_options() };
        let found = block_on_walk(walk::collect(&Fs::native(), &root, &opts, true, |_, e| {
            if e.file_type == FileType::File && e.size.is_none_or(|s| s <= 4 << 20) {
                walk::Action::Continue
            } else {
                walk::Action::Skip
            }
        }));
        let found = match found {
            Ok(c) => c,
            Err(e) => return ToolOutput::err(format!("Walk failed: {e}")),
        };
        found
            .iter()
            .map(|e| {
                let abs = root.join(e.rel_path());
                (display(&abs), abs)
            })
            .collect()
    };
    let mut searcher = SearcherBuilder::new().line_number(true).binary_detection(BinaryDetection::quit(0)).build();
    let mut out = Vec::new();
    let mut total = 0;
    let mut notices = Vec::new();
    'files: for (rel, abs) in files {
        let mut n = 0;
        let mut capped = false;
        let _ = searcher.search_path(
            &matcher,
            &abs,
            UTF8(|ln, line| {
                if n >= PER_FILE {
                    capped = true;
                    return Ok(false);
                }
                n += 1;
                let line = line.trim_end_matches(['\n', '\r']);
                let line: String = if line.chars().count() > MAX_COL {
                    line.chars().take(MAX_COL).chain(['…']).collect()
                } else {
                    line.into()
                };
                out.push(format!("{rel}:{ln}:{line}"));
                // The overall limit is enforced here, per match, not after the file is finished.
                Ok(total + n < TOTAL)
            }),
        );
        total += n;
        if capped {
            notices.push(format!("More than {PER_FILE} matches in {rel}; narrow the pattern or grep that file alone."));
        }
        if total >= TOTAL {
            notices.push(format!("Stopped at {TOTAL} matches; narrow the pattern or path."));
            break 'files;
        }
    }
    if out.is_empty() {
        return ToolOutput::ok("No matches found");
    }
    if !notices.is_empty() {
        out.push(String::new());
        out.extend(notices);
    }
    ToolOutput::ok(out.join("\n"))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A git-like tree: `a.rs`, `sub/b.rs`, gitignored `sub/gen.rs`, hidden `.h/c.rs`, a 5 MiB `big.rs`.
    pub fn tree() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-walk-{}", uuid::Uuid::new_v4()));
        for dir in [".git", "sub", ".h"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        std::fs::write(d.join(".gitignore"), "sub/gen.rs\n").unwrap();
        for f in ["a.rs", "sub/b.rs", "sub/gen.rs", ".h/c.rs"] {
            std::fs::write(d.join(f), "fn needle() {}\n").unwrap();
        }
        std::fs::write(d.join("big.rs"), format!("fn needle() {{}}\n{}", "x".repeat(5 << 20))).unwrap();
        d
    }

    #[tokio::test(flavor = "current_thread")]
    async fn walks_honour_gitignore_hidden_and_size_cap() {
        let d = tree();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.grep(Args { pattern: "needle".into(), path: None, case: None }).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(o.text, "a.rs:1:fn needle() {}\nsub/b.rs:1:fn needle() {}");
        let o = t.grep(Args { pattern: "needle".into(), path: Some("big.rs".into()), case: None }).await;
        assert_eq!(o.text, "big.rs:1:fn needle() {}", "an explicit file is searched regardless of size");
    }
}
