use super::{ToolOutput, Tools};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use pi_walker::{WalkFilter, WalkOptions, WalkRequest};
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

pub fn walk_options() -> WalkOptions {
    WalkOptions { include_hidden: false, use_gitignore: true, skip_git: true, ..Default::default() }
}

impl Tools {
    pub fn grep(&self, a: Args) -> ToolOutput {
        let matcher = match RegexMatcherBuilder::new().case_insensitive(!a.case.unwrap_or(true)).build(&a.pattern) {
            Ok(m) => m,
            Err(e) => return ToolOutput::err(format!("Invalid pattern: {e}")),
        };
        let root = self.abs(a.path.as_deref().unwrap_or("."));
        if !root.exists() {
            return ToolOutput::err(format!("File not found: {}", self.display(&root)));
        }
        let files: Vec<(String, PathBuf)> = if root.is_file() {
            vec![(self.display(&root), root.clone())]
        } else {
            let req = WalkRequest::from_options(&root, walk_options())
                .filter(WalkFilter::files_only().max_file_size(4 << 20));
            let mut v = match req.collect_files() {
                Ok(v) => v,
                Err(e) => return ToolOutput::err(format!("Walk failed: {e:?}")),
            };
            v.sort_by(|a, b| a.path.cmp(&b.path));
            v.into_iter()
                .map(|e| {
                    let abs = root.join(&e.path);
                    (self.display(&abs), abs)
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
                    Ok(true)
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
}
