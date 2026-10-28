//! `@path` imports in AGENTS.md-style files (port of omp's `discovery/at-imports.ts`).
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const MAX_DEPTH: usize = 5;

type Warn<'a> = &'a mut dyn FnMut(String);

fn key(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Expands `@path` imports in `content`, which was read from `file`.
pub fn expand(content: &str, file: &Path, home: &Path, warn: &mut dyn FnMut(String)) -> String {
    let mut visited = HashSet::from([key(file)]);
    let base = file.parent().unwrap_or(Path::new("."));
    expand_at(content, base, 0, file, home, &mut visited, warn)
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
    home: &Path,
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
            (None, None) => out.push_str(&expand_line(line, base, depth, importer, home, visited, warn)),
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
    home: &Path,
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
            home.to_path_buf()
        } else if let Some(r) = raw.strip_prefix("~/") {
            home.join(r)
        } else if Path::new(&raw).is_absolute() {
            PathBuf::from(&raw)
        } else {
            base.join(&raw)
        };
        let original = &line[p..span_end];
        if visited.contains(&key(&resolved)) {
            out.push_str(original);
        } else {
            match std::fs::read_to_string(&resolved) {
                Err(e) => {
                    if ["./", "../", "/", "~"].iter().any(|s| raw.starts_with(s)) {
                        warn(format!("{}: cannot read @-import {}: {e}", importer.display(), resolved.display()));
                    }
                    out.push_str(original);
                }
                Ok(text) => {
                    visited.insert(key(&resolved));
                    let dir = resolved.parent().unwrap_or(Path::new(".")).to_path_buf();
                    let inner = expand_at(&text, &dir, depth + 1, &resolved, home, visited, warn);
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

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-at-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn w(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }
    fn run(dir: &Path, name: &str, input: &str, home: &Path) -> (String, Vec<String>) {
        let mut warns = Vec::new();
        let out = expand(input, &dir.join(name), home, &mut |x| warns.push(x));
        (out, warns)
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
