//! Syntax highlighting for fenced code via tree-sitter. Each language is a Cargo feature
//! (`lang-*`); without any, `highlight` returns `None`. Captures map to omp's 11 categories,
//! coloured with the active theme's `syntax*` keys. Configs are built on first use per language.

use super::line::Line;

/// Larger blocks are not highlighted; callers fall back to plain text.
const MAX_LINES: usize = 2000;
/// Measured in bytes, which bounds chars as well.
const MAX_LINE_BYTES: usize = 2000;

/// Highlights `code` as `lang` (name, alias or extension). One `Line` per source line.
/// `None` when the language isn't compiled in, the input is over the caps, or highlighting fails.
pub fn highlight(code: &str, lang: &str) -> Option<Vec<Line>> {
    if lang.is_empty() || code.lines().count() > MAX_LINES || code.lines().any(|l| l.len() > MAX_LINE_BYTES) {
        return None;
    }
    #[cfg(feature = "highlight")]
    {
        let canon = canonical(&lang.to_ascii_lowercase())?;
        return std::panic::catch_unwind(|| ts::run(code, canon)).ok().flatten();
    }
    #[cfg(not(feature = "highlight"))]
    {
        None
    }
}

/// Canonical language for an info-string token, only for languages compiled in.
#[cfg(feature = "highlight")]
fn canonical(l: &str) -> Option<&'static str> {
    Some(match l {
        #[cfg(feature = "lang-diff")]
        "diff" | "patch" => "diff",
        #[cfg(feature = "lang-markdown")]
        "markdown" | "md" => "markdown",
        #[cfg(feature = "lang-rust")]
        "rust" | "rs" => "rust",
        #[cfg(feature = "lang-json")]
        "json" | "jsonc" | "json5" => "json",
        #[cfg(feature = "lang-bash")]
        "bash" | "sh" | "shell" | "zsh" | "shellscript" => "bash",
        #[cfg(feature = "lang-toml")]
        "toml" => "toml",
        #[cfg(feature = "lang-yaml")]
        "yaml" | "yml" => "yaml",
        #[cfg(feature = "lang-python")]
        "python" | "py" | "python3" => "python",
        #[cfg(feature = "lang-javascript")]
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => "javascript",
        #[cfg(feature = "lang-typescript")]
        "typescript" | "ts" | "mts" | "cts" => "typescript",
        #[cfg(feature = "lang-typescript")]
        "tsx" => "tsx",
        _ => return None,
    })
}

#[cfg(feature = "highlight")]
mod ts {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use crossterm::style::{Attribute, ContentStyle};
    use tree_sitter::Language;
    use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

    use crate::tui::line::{Line, span};
    use crate::tui::theme::{self, Role};

    #[derive(Clone, Copy)]
    enum Cat {
        Comment,
        Keyword,
        Function,
        Variable,
        String,
        Number,
        Type,
        Operator,
        Punctuation,
        Inserted,
        Deleted,
        Heading,
        Code,
        Url,
        Link,
        Emph,
        Strong,
    }

    impl Cat {
        fn key(self) -> Option<Role> {
            Some(match self {
                Cat::Comment => Role::SynComment,
                Cat::Keyword => Role::SynKeyword,
                Cat::Function => Role::SynFunction,
                Cat::String => Role::SynString,
                Cat::Number => Role::SynNumber,
                Cat::Type => Role::SynType,
                Cat::Punctuation => Role::SynPunct,
                Cat::Inserted => Role::Ok,
                Cat::Deleted => Role::Err,
                Cat::Heading => Role::Strong,
                Cat::Code => Role::Literal,
                Cat::Url => Role::Faint,
                Cat::Link => Role::Accent,
                Cat::Variable | Cat::Operator | Cat::Emph | Cat::Strong => return None,
            })
        }

        fn style(self, t: &theme::Theme, plain: ContentStyle) -> ContentStyle {
            let mut s = ContentStyle { foreground_color: self.key().and_then(|k| t.color(k)).or(plain.foreground_color), ..plain };
            match self {
                Cat::Heading | Cat::Strong => s.attributes.set(Attribute::Bold),
                Cat::Comment => s.attributes.set(Attribute::Italic),
                Cat::Emph => s.attributes.set(Attribute::Italic),
                _ => {}
            }
            s
        }
    }

    /// Capture names handed to tree-sitter (longest dotted prefix wins) and their category.
    const CAPTURES: &[(&str, Cat)] = &[
        ("comment", Cat::Comment),
        ("keyword", Cat::Keyword),
        ("conditional", Cat::Keyword),
        ("repeat", Cat::Keyword),
        ("include", Cat::Keyword),
        ("tag", Cat::Keyword),
        ("function", Cat::Function),
        ("constructor", Cat::Function),
        ("variable", Cat::Variable),
        ("property", Cat::Variable),
        ("parameter", Cat::Variable),
        ("label", Cat::Variable),
        ("string", Cat::String),
        ("escape", Cat::String),
        ("number", Cat::Number),
        ("constant", Cat::Number),
        ("boolean", Cat::Number),
        ("type", Cat::Type),
        ("attribute", Cat::Type),
        ("module", Cat::Type),
        ("operator", Cat::Operator),
        ("punctuation", Cat::Punctuation),
        ("diff.plus", Cat::Inserted),
        ("diff.minus", Cat::Deleted),
        ("text.title", Cat::Heading),
        ("text.literal", Cat::Code),
        ("text.uri", Cat::Url),
        ("text.reference", Cat::Link),
        ("text.emphasis", Cat::Emph),
        ("text.strong", Cat::Strong),
    ];

    thread_local! {
        static CONFIGS: RefCell<HashMap<&'static str, Option<&'static HighlightConfiguration>>> = RefCell::default();
    }

    fn config(lang: &'static str) -> Option<&'static HighlightConfiguration> {
        CONFIGS.with(|c| {
            *c.borrow_mut().entry(lang).or_insert_with(|| {
                // Leaked: one config per language for the process lifetime, so injection callbacks can return `'static` refs.
                build(lang).map(|b| &*Box::leak(Box::new(b)))
            })
        })
    }

    fn build(lang: &'static str) -> Option<HighlightConfiguration> {
        let (language, query, injections): (Language, String, &str) = match lang {
            #[cfg(feature = "lang-rust")]
            "rust" => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
                tree_sitter_rust::INJECTIONS_QUERY,
            ),
            #[cfg(feature = "lang-json")]
            "json" => (tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY.into(), ""),
            #[cfg(feature = "lang-bash")]
            "bash" => (tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY.into(), ""),
            #[cfg(feature = "lang-toml")]
            "toml" => (tree_sitter_toml_ng::LANGUAGE.into(), tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(), ""),
            #[cfg(feature = "lang-yaml")]
            "yaml" => (tree_sitter_yaml::LANGUAGE.into(), tree_sitter_yaml::HIGHLIGHTS_QUERY.into(), ""),
            #[cfg(feature = "lang-python")]
            "python" => (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY.into(), ""),
            #[cfg(feature = "lang-javascript")]
            "javascript" => (
                tree_sitter_javascript::LANGUAGE.into(),
                format!("{}\n{}", tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::JSX_HIGHLIGHT_QUERY),
                tree_sitter_javascript::INJECTIONS_QUERY,
            ),
            // The TypeScript query only adds to the JavaScript one.
            #[cfg(feature = "lang-typescript")]
            "typescript" | "tsx" => (
                if lang == "tsx" {
                    tree_sitter_typescript::LANGUAGE_TSX.into()
                } else {
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
                },
                format!("{}\n{}", tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_typescript::HIGHLIGHTS_QUERY),
                "",
            ),
            #[cfg(feature = "lang-diff")]
            "diff" => (
                tree_sitter_diff::LANGUAGE.into(),
                "(addition) @diff.plus\n(deletion) @diff.minus\n[(new_file) (old_file)] @keyword\n(location) @keyword\n(commit) @constant\n(command) @keyword\n".into(),
                "",
            ),
            #[cfg(feature = "lang-markdown")]
            "markdown" => (
                tree_sitter_md::LANGUAGE.into(),
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
                tree_sitter_md::INJECTION_QUERY_BLOCK,
            ),
            #[cfg(feature = "lang-markdown")]
            "markdown_inline" => (
                tree_sitter_md::INLINE_LANGUAGE.into(),
                tree_sitter_md::HIGHLIGHT_QUERY_INLINE.into(),
                tree_sitter_md::INJECTION_QUERY_INLINE,
            ),
            _ => return None,
        };
        let mut cfg = HighlightConfiguration::new(language, lang, &query, injections, "").ok()?;
        let names: Vec<&str> = CAPTURES.iter().map(|(n, _)| *n).collect();
        cfg.configure(&names);
        Some(cfg)
    }

    pub fn run(code: &str, lang: &'static str) -> Option<Vec<Line>> {
        let cfg = config(lang)?;
        let t = theme::get();
        let plain = ContentStyle::default();
        let styles: Vec<ContentStyle> = CAPTURES.iter().map(|(_, c)| c.style(t, plain)).collect();
        let mut hl = Highlighter::new();
        let events = hl
            .highlight(cfg, code.as_bytes(), None, |name| {
                config(if name == "markdown_inline" { "markdown_inline" } else { super::canonical(&name.to_ascii_lowercase())? })
            })
            .ok()?;
        let mut stack: Vec<usize> = Vec::new();
        let mut out: Vec<Line> = vec![Vec::new()];
        for ev in events {
            match ev.ok()? {
                HighlightEvent::HighlightStart(h) => stack.push(h.0),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let style = stack.last().map_or(plain, |&i| styles[i]);
                    let text = code.get(start..end)?;
                    for (i, part) in text.split('\n').enumerate() {
                        if i > 0 {
                            out.push(Vec::new());
                        }
                        let part = part.strip_suffix('\r').unwrap_or(part);
                        if part.is_empty() {
                            continue;
                        }
                        let line = out.last_mut()?;
                        match line.last_mut() {
                            Some(s) if s.style == style => s.text.push_str(part),
                            _ => line.push(span(part, style)),
                        }
                    }
                }
            }
        }
        // `lines()` semantics: no trailing empty line for a final newline.
        if code.ends_with('\n') && out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        Some(out)
    }
}

#[cfg(all(test, feature = "highlight"))]
mod tests {
    use super::*;
    use crate::tui::line::line_text;
    use crate::tui::theme::{self, Role};
    use crossterm::style::Color;

    fn fg_of(lines: &[Line], text: &str) -> Option<Color> {
        lines.iter().flatten().find(|s| s.text.contains(text)).and_then(|s| s.style.foreground_color)
    }

    #[test]
    fn rust_keyword_string_comment_get_theme_colours() {
        let t = theme::get();
        let src = "fn main() {\n    let s = \"hi\"; // note\n}\n";
        let ls = highlight(src, "rust").expect("rust is known");
        assert_eq!(ls.len(), 3);
        assert_eq!(line_text(&ls[1]), "    let s = \"hi\"; // note");
        let (kw, st, cm) = (fg_of(&ls, "fn"), fg_of(&ls, "hi"), fg_of(&ls, "note"));
        assert_eq!(kw, t.color(Role::SynKeyword));
        assert_eq!(st, t.color(Role::SynString));
        assert_eq!(cm, t.color(Role::SynComment));
        assert!(kw.is_some() && st.is_some() && cm.is_some());
        assert_ne!(kw, st);
        assert_ne!(st, cm);
    }

    #[test]
    fn aliases_resolve() {
        for lang in ["rs", "py", "sh", "json", "yaml", "ts", "toml", "js", "diff", "md"] {
            assert!(highlight("x\n", lang).is_some(), "{lang}");
        }
    }

    #[cfg(feature = "lang-diff")]
    #[test]
    fn diff_lines_use_diff_colours() {
        let t = theme::get();
        let ls = highlight("@@ -1 +1 @@\n-a\n+b\n", "diff").expect("diff is known");
        assert_eq!(fg_of(&ls, "-a"), t.color(Role::Err));
        assert_eq!(fg_of(&ls, "+b"), t.color(Role::Ok));
        assert!(t.color(Role::Ok).is_some());
    }

    #[cfg(all(feature = "lang-markdown", feature = "lang-rust"))]
    #[test]
    fn markdown_heading_and_injected_rust() {
        let t = theme::get();
        let ls = highlight("# Title\n\n```rust\nfn x() {}\n```\n", "markdown").expect("markdown is known");
        assert_eq!(fg_of(&ls, "Title"), t.color(Role::Strong));
        assert_eq!(fg_of(&ls, "fn"), t.color(Role::SynKeyword));
    }

    #[test]
    fn unknown_or_empty_language_is_none() {
        assert!(highlight("x = 1\n", "nosuchlang").is_none());
        assert!(highlight("x = 1\n", "").is_none());
    }

    #[test]
    fn huge_input_is_capped() {
        let many = "x\n".repeat(MAX_LINES);
        assert_eq!(highlight(&many, "rust").map(|l| l.len()), Some(MAX_LINES));
        assert!(highlight(&"x\n".repeat(MAX_LINES + 1), "rust").is_none());
        assert!(highlight(&"a".repeat(MAX_LINE_BYTES + 1), "rust").is_none());
    }

    #[test]
    fn arbitrary_input_does_not_panic() {
        let junk = "\u{0}\u{7f}é日本\t\r\n\"'/*`${}\\\u{1F600}\n\n";
        for lang in ["rust", "python", "toml", "json"] {
            let _ = highlight(junk, lang);
        }
    }
}
