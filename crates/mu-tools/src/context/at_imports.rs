//! `@path` imports in AGENTS.md-style files (port of omp's `discovery/at-imports.ts`).
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_DEPTH: usize = 5;
/// The most an imported file may hold; a larger one is skipped, not truncated.
pub const MAX_IMPORT_BYTES: u64 = 256 * 1024;

type Warn<'a> = &'a mut dyn FnMut(String);

fn key(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// What an import may resolve against: `~` and the canonical directories it may read from.
struct Scope<'a> {
    home: &'a Path,
    roots: Vec<PathBuf>,
}

enum Fail {
    Read(String),
    Refused(String),
}

/// Reads a canonical path through the opened file, never a FIFO or device: `O_NONBLOCK` keeps the
/// open from waiting on a FIFO, and the type and size are checked on the handle that is then read.
fn read_import(path: &Path) -> Result<String, Fail> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NONBLOCK);
    let f = o.open(path).map_err(|e| Fail::Read(e.to_string()))?;
    let md = f.metadata().map_err(|e| Fail::Read(e.to_string()))?;
    if !md.is_file() {
        return Err(Fail::Refused("not a regular file".into()));
    }
    let too_big = || Fail::Refused(format!("larger than {} KiB", MAX_IMPORT_BYTES / 1024));
    if md.len() > MAX_IMPORT_BYTES {
        return Err(too_big());
    }
    let mut buf = Vec::new();
    f.take(MAX_IMPORT_BYTES + 1).read_to_end(&mut buf).map_err(|e| Fail::Read(e.to_string()))?;
    if buf.len() as u64 > MAX_IMPORT_BYTES {
        return Err(too_big());
    }
    String::from_utf8(buf).map_err(|_| Fail::Read("stream did not contain valid UTF-8".into()))
}

/// Expands `@path` imports in `content`, which was read from `file`. Only files under `roots` or
/// `file`'s own directory are read; any other import is replaced by a notice saying it was skipped.
pub fn expand(content: &str, file: &Path, home: &Path, roots: &[PathBuf], warn: &mut dyn FnMut(String)) -> String {
    let mut visited = HashSet::from([key(file)]);
    let base = file.parent().unwrap_or(Path::new("."));
    let mut roots: Vec<PathBuf> = roots.iter().chain(std::iter::once(&base.to_path_buf())).map(|r| key(r)).collect();
    roots.dedup();
    let scope = Scope { home, roots };
    expand_at(content, base, 0, file, &scope, &mut visited, warn)
}

fn fence_of(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches([' ', '\t']);
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

fn expand_at(
    content: &str,
    base: &Path,
    depth: usize,
    importer: &Path,
    scope: &Scope,
    visited: &mut HashSet<PathBuf>,
    warn: Warn,
) -> String {
    if depth >= MAX_DEPTH {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    let mut fence: Option<(char, usize)> = None;
    for line in content.split_inclusive('\n') {
        match (fence, fence_of(line)) {
            (None, Some(f)) => {
                fence = Some(f);
                out.push_str(line);
            }
            (Some((c, n)), Some((c2, n2))) if c == c2 && n2 >= n => {
                fence = None;
                out.push_str(line);
            }
            (Some(_), _) => out.push_str(line),
            (None, None) => out.push_str(&expand_line(line, base, depth, importer, scope, visited, warn)),
        }
    }
    out
}

fn in_inline_code(prefix: &str) -> bool {
    let mut open = false;
    let mut prev_tick = false;
    for c in prefix.chars() {
        let tick = c == '`';
        if tick && !prev_tick {
            open = !open;
        }
        prev_tick = tick;
    }
    open
}

fn expand_line(
    line: &str,
    base: &Path,
    depth: usize,
    importer: &Path,
    scope: &Scope,
    visited: &mut HashSet<PathBuf>,
    warn: Warn,
) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some(off) = line[i..].find('@') {
        let p = i + off;
        let start_ok = p == 0 || matches!(line.as_bytes()[p - 1], b' ' | b'\t');
        let first_ok = line[p + 1..]
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '.' | '/' | '~' | '_' | '-') || c.is_ascii_alphanumeric());
        if !start_ok || !first_ok || in_inline_code(&line[..p]) {
            out.push_str(&line[i..=p]);
            i = p + 1;
            continue;
        }
        let rest = &line[p + 1..];
        let mut end = 0;
        let mut it = rest.char_indices().peekable();
        while let Some((j, c)) = it.next() {
            if c == '\\' && it.peek().is_some_and(|(_, n)| *n == ' ') {
                it.next();
                end = j + 2;
            } else if c.is_whitespace() {
                break;
            } else {
                end = j + c.len_utf8();
            }
        }
        let token = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
        out.push_str(&line[i..p]);
        let span_end = p + 1 + token.len();
        if token.is_empty() {
            out.push('@');
            i = p + 1;
            continue;
        }
        let raw = token.replace("\\ ", " ");
        let resolved = if raw == "~" {
            scope.home.to_path_buf()
        } else if let Some(r) = raw.strip_prefix("~/") {
            scope.home.join(r)
        } else if Path::new(&raw).is_absolute() {
            PathBuf::from(&raw)
        } else {
            base.join(&raw)
        };
        let original = &line[p..span_end];
        let canon = std::fs::canonicalize(&resolved);
        if canon.as_ref().is_ok_and(|c| visited.contains(c)) {
            out.push_str(original);
        } else {
            let result = match &canon {
                Err(e) => Err(Fail::Read(e.to_string())),
                Ok(c) if scope.roots.iter().any(|r| c.starts_with(r)) => read_import(c),
                Ok(_) => Err(Fail::Refused("outside the project and mu's directories".into())),
            };
            let canon = canon.unwrap_or_else(|_| resolved.clone());
            match result {
                Err(Fail::Read(e)) => {
                    if ["./", "../", "/", "~"].iter().any(|s| raw.starts_with(s)) {
                        warn(format!("{}: cannot read @-import {}: {e}", importer.display(), resolved.display()));
                    }
                    out.push_str(original);
                }
                Err(Fail::Refused(why)) => {
                    warn(format!("{}: skipped @-import {}: {why}", importer.display(), canon.display()));
                    out.push_str(&format!("[@-import {} skipped: {why}]", canon.display()));
                }
                Ok(text) => {
                    visited.insert(canon.clone());
                    let dir = canon.parent().unwrap_or(Path::new(".")).to_path_buf();
                    let inner = expand_at(&text, &dir, depth + 1, &canon, scope, visited, warn);
                    out.push_str(inner.trim_end_matches(['\n', '\r']));
                }
            }
        }
        i = span_end;
    }
    out.push_str(&line[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::tmp;

    fn w(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }
    fn run(dir: &Path, name: &str, input: &str, home: &Path) -> (String, Vec<String>) {
        let mut warns = Vec::new();
        let out = expand(input, &dir.join(name), home, &[home.to_path_buf()], &mut |x| warns.push(x));
        (out, warns)
    }

    fn confined(project: &Path, input: &str) -> (String, Vec<String>) {
        let mut warns = Vec::new();
        let out = expand(input, &project.join("AGENTS.md"), project, &[], &mut |x| warns.push(x));
        (out, warns)
    }

    fn skipped(o: &str, warns: &[String], secret: &str, why: &str) {
        assert!(!o.contains(secret), "{o}");
        assert!(o.contains("skipped") && o.contains(why), "{o}");
        assert!(warns.len() == 1 && warns[0].contains(why), "{warns:?}");
    }

    #[test]
    fn absolute_path_outside_is_skipped() {
        let d = tmp();
        w(&d.join("out/key"), "SECRET-KEY");
        let input = format!("@{}\n", d.join("out/key").display());
        let (o, warns) = confined(&d.join("proj"), &input);
        skipped(&o, &warns, "SECRET-KEY", "outside");
    }

    #[test]
    fn dotdot_escape_is_skipped() {
        let d = tmp();
        w(&d.join("out/key"), "SECRET-KEY");
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let (o, warns) = confined(&d.join("proj"), "@../out/key\n");
        skipped(&o, &warns, "SECRET-KEY", "outside");
    }

    #[test]
    fn home_import_outside_roots_is_skipped() {
        let d = tmp();
        w(&d.join("home/.ssh/id"), "SECRET-KEY");
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let mut warns = Vec::new();
        let o = expand("@~/.ssh/id\n", &d.join("proj/AGENTS.md"), &d.join("home"), &[d.join("home/.omp")], &mut |x| {
            warns.push(x)
        });
        skipped(&o, &warns, "SECRET-KEY", "outside");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_skipped() {
        let d = tmp();
        w(&d.join("out/key"), "SECRET-KEY");
        std::fs::create_dir_all(d.join("proj")).unwrap();
        std::os::unix::fs::symlink(d.join("out/key"), d.join("proj/link.md")).unwrap();
        let (o, warns) = confined(&d.join("proj"), "@./link.md\n");
        skipped(&o, &warns, "SECRET-KEY", "outside");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_skipped_without_blocking() {
        let d = tmp();
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let fifo = std::ffi::CString::new(d.join("proj/pipe.md").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let (o, warns) = confined(&d.join("proj"), "@./pipe.md\n");
        skipped(&o, &warns, "@./pipe.md\n", "not a regular file");
    }

    #[test]
    fn oversize_file_is_skipped() {
        let d = tmp();
        w(&d.join("proj/big.md"), &"x".repeat(MAX_IMPORT_BYTES as usize + 1));
        let (o, warns) = confined(&d.join("proj"), "@./big.md\n");
        skipped(&o, &warns, "xxxx", "larger than");
        w(&d.join("proj/ok.md"), &"y".repeat(MAX_IMPORT_BYTES as usize));
        let (o, warns) = confined(&d.join("proj"), "@./ok.md\n");
        assert!(o.starts_with("yyyy") && warns.is_empty());
    }

    #[test]
    fn in_project_import_still_works() {
        let d = tmp();
        w(&d.join("proj/rules/a.md"), "RULE-A @../b.md");
        w(&d.join("proj/b.md"), "RULE-B");
        let (o, warns) = confined(&d.join("proj"), "@./rules/a.md\n");
        assert_eq!(o, "RULE-A RULE-B\n");
        assert!(warns.is_empty());
    }

    #[test]
    fn whole_line_import() {
        let d = tmp();
        w(&d.join("AGENTS.md"), "ALWAYS use uppercase letters.");
        let (o, _) = run(&d, "CLAUDE.md", "@AGENTS.md", &d);
        assert_eq!(o.trim(), "ALWAYS use uppercase letters.");
    }

    #[test]
    fn relative_to_importer() {
        let d = tmp();
        w(&d.join("rules/no-push.md"), "NEVER push.");
        let (o, _) = run(&d.join("rules"), "AGENTS.md", "Rule: @./no-push.md\n", &d);
        assert!(o.contains("Rule: NEVER push."));
        assert!(!o.contains("@./no-push.md"));
    }

    #[test]
    fn home_import() {
        let d = tmp();
        let home = d.join("home");
        w(&home.join("prefs.md"), "use 2 spaces");
        let (o, _) = run(&d, "AGENTS.md", "See @~/prefs.md.\n", &home);
        assert!(o.contains("See use 2 spaces."), "{o}");
    }

    #[test]
    fn escaped_spaces() {
        let d = tmp();
        w(&d.join("rules/plain.md"), "Marker: PLAIN-OK\n");
        w(&d.join("rules/my rule.md"), "Marker: SPACED-OK\n");
        w(&d.join("rules/other.md"), "Marker: OTHER-OK\n");
        let (o, _) = run(&d, "AGENTS.md", "Rules: @rules/plain.md and @rules/my\\ rule.md then @rules/other.md\n", &d);
        assert_eq!(o, "Rules: Marker: PLAIN-OK and Marker: SPACED-OK then Marker: OTHER-OK\n");
    }

    #[test]
    fn recursion() {
        let d = tmp();
        w(&d.join("b.md"), "B then @c.md");
        w(&d.join("c.md"), "LEAF");
        let (o, _) = run(&d, "a.md", "A then @b.md", &d);
        assert_eq!(o, "A then B then LEAF");
    }

    #[test]
    fn depth_cap() {
        let d = tmp();
        for i in 0..6 {
            w(&d.join(format!("step{i}.md")), &format!("step-{i} -> @step{}.md", i + 1));
        }
        w(&d.join("step6.md"), "TERMINAL");
        let (o, _) = run(&d, "step0.md", "step-0 -> @step1.md", &d);
        assert!(o.contains("@step6.md"), "{o}");
        assert!(!o.contains("TERMINAL"));
    }

    #[test]
    fn cycle() {
        let d = tmp();
        w(&d.join("loop-a.md"), "A: @loop-b.md");
        w(&d.join("loop-b.md"), "B: @loop-a.md");
        let (o, warns) = run(&d, "root.md", "@loop-a.md", &d);
        assert!(o.contains("A: B:") && o.contains("@loop-a.md"), "{o}");
        assert!(warns.is_empty());
    }

    #[test]
    fn missing_file_warns() {
        let d = tmp();
        let input = "See @./does-not-exist.md and @./does-not\\ exist.md\n";
        let (o, warns) = run(&d, "AGENTS.md", input, &d);
        assert_eq!(o, input);
        assert_eq!(warns.len(), 2);
        assert!(warns.iter().all(|x| x.contains("cannot read @-import")));
    }

    #[test]
    fn fenced_block_literal() {
        let d = tmp();
        w(&d.join("guide.md"), "INLINED");
        let input = "```bash\necho @./guide.md\n```\nAlso see @./guide.md.\n";
        let (o, _) = run(&d, "AGENTS.md", input, &d);
        assert_eq!(o, "```bash\necho @./guide.md\n```\nAlso see INLINED.\n");
    }

    #[test]
    fn inline_code_literal() {
        let d = tmp();
        w(&d.join("guide.md"), "INLINED");
        let (o, _) = run(&d, "AGENTS.md", "`npm i @./guide.md` then @./guide.md.\n", &d);
        assert_eq!(o, "`npm i @./guide.md` then INLINED.\n");
    }

    #[test]
    fn emails_and_ssh() {
        let d = tmp();
        let input = "me@example.com and git@github.com:foo/bar.git\n";
        let (o, warns) = run(&d, "AGENTS.md", input, &d);
        assert_eq!(o, input);
        assert!(warns.is_empty());
    }

    #[test]
    fn bare_missing_silent() {
        let d = tmp();
        let (o, warns) = run(&d, "AGENTS.md", "ping @someone\n", &d);
        assert_eq!(o, "ping @someone\n");
        assert!(warns.is_empty());
    }
}
