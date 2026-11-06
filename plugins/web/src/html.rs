//! HTML helpers for fetch: Markdown conversion, the page's own markdown alternate, and the
//! quality gates that decide whether a conversion is good enough to return.

use html_to_markdown_rs::{
    ConversionOptions, HiddenContent, PreprocessingOptions, PreprocessingPreset, TierStrategy, WarningKind, convert,
};
use url::Url;

/// Alternate links are looked for only in the first 512 KiB, as omp does.
const ALTERNATE_SCAN_LIMIT: usize = 512 * 1024;

/// Converts with omp's reader settings: aggressive cleanup, reachable hidden content.
pub fn to_markdown(html: &str) -> Result<String, String> {
    let opts = ConversionOptions {
        hidden_content: HiddenContent::Reachable,
        preprocessing: PreprocessingOptions {
            enabled: true,
            preset: PreprocessingPreset::Aggressive,
            remove_navigation: true,
            remove_forms: true,
        },
        tier_strategy: TierStrategy::Tier2,
        ..Default::default()
    };
    let result = convert(html, Some(opts)).map_err(|e| format!("Conversion error: {e}"))?;
    if let Some(w) = result.warnings.iter().find(|w| w.kind == WarningKind::DepthLimitExceeded) {
        return Err(format!("Conversion error: {}", w.message));
    }
    Ok(result.content.unwrap_or_default())
}

/// A `<link rel="alternate" type="…markdown…">` that describes this page, resolved against it.
/// Site-wide feeds and index pages are skipped, and a root page never matches.
pub fn markdown_alternate(html: &str, page: &Url) -> Option<Url> {
    let window = prefix(html, ALTERNATE_SCAN_LIMIT);
    // ASCII lowercasing keeps byte offsets, so positions found in `lower` index `window`.
    let lower = window.to_ascii_lowercase();
    let page_path = page.path().strip_suffix('/').unwrap_or(page.path());
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<link") {
        let start = pos + i;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &window[start..end];
        pos = end;

        let rel = attr(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        if !rel.split_whitespace().any(|t| t == "alternate") {
            continue;
        }
        let Some(href) = attr(tag, "href") else { continue };
        let kind = attr(tag, "type").unwrap_or_default().to_ascii_lowercase();
        if !kind.contains("markdown") || is_site_feed(&href) {
            continue;
        }
        let Ok(alt) = page.join(&href) else { continue };
        let alt_path = alt.path();
        let page_specific = alt_path == page_path
            || (!page_path.is_empty()
                && (alt_path.starts_with(&format!("{page_path}/")) || alt_path.starts_with(&format!("{page_path}."))));
        if page_specific {
            return Some(alt);
        }
    }
    None
}

fn is_site_feed(href: &str) -> bool {
    ["RecentChanges", "Special:", "/feed/", "action=feed"].iter().any(|t| href.contains(t))
}

/// Reads one attribute from a tag, case-insensitively, on a word boundary. Quoted or bare values.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut from = 0;
    while let Some(off) = lower[from..].find(name) {
        let start = from + off;
        let mut i = start + name.len();
        from = i;
        if start > 0 && is_word(b[start - 1]) {
            continue;
        }
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if b.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        let rest = &tag[i..];
        let value = match rest.as_bytes().first() {
            Some(&q @ (b'"' | b'\'')) => {
                let inner = &rest[1..];
                &inner[..inner.find(q as char)?]
            }
            _ => {
                let end = rest
                    .find(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | '=' | '<' | '>'))
                    .unwrap_or(rest.len());
                &rest[..end]
            }
        };
        return Some(value.trim().to_string());
    }
    None
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// omp's `isLowQualityOutput`: a short JavaScript-required wall, or a page that is mostly
/// short navigation lines.
pub fn is_low_quality(md: &str) -> bool {
    const JS_GATED: [&str; 5] = [
        "enable javascript",
        "javascript required",
        "turn on javascript",
        "please enable javascript",
        "browser not supported",
    ];
    let lower = md.to_lowercase();
    if md.chars().count() < 1024 && JS_GATED.iter().any(|t| lower.contains(t)) {
        return true;
    }
    let lines: Vec<&str> = md.split('\n').filter(|l| !l.trim().is_empty()).collect();
    let short = lines.iter().filter(|l| l.trim().chars().count() < 40).count();
    lines.len() > 10 && short as f64 / lines.len() as f64 > 0.7
}

/// Whether the body starts like an HTML document.
pub fn looks_like_html(s: &str) -> bool {
    let head: String = s.trim_start().chars().take(16).collect::<String>().to_ascii_lowercase();
    ["<!doctype", "<html", "<head", "<body"].iter().any(|t| head.starts_with(t))
}

/// Drops `![alt](data:…)` images and keeps the alt text as `![alt]`. Inline payloads are
/// unreadable and often larger than the article.
pub fn strip_data_uri_images(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find("![") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match data_image_end(after) {
            Some((alt, consumed)) => {
                if !alt.trim().is_empty() {
                    out.push_str("![");
                    out.push_str(alt);
                    out.push(']');
                }
                rest = &after[consumed..];
            }
            None => {
                out.push_str("![");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// For `alt](data:…)` at the start of `s`: the alt text and the length through the closing `)`.
/// A title containing `)` is not handled.
fn data_image_end(s: &str) -> Option<(&str, usize)> {
    let b = s.as_bytes();
    let mut j = 0;
    while j < b.len() && b[j] != b']' {
        j += if b[j] == b'\\' { 2 } else { 1 };
    }
    if b.get(j) != Some(&b']') || b.get(j + 1) != Some(&b'(') {
        return None;
    }
    let mut k = j + 2;
    while b.get(k).is_some_and(u8::is_ascii_whitespace) {
        k += 1;
    }
    let tail = &s[k..];
    let angle = tail.starts_with('<');
    let scheme_at = usize::from(angle);
    if !tail.as_bytes().get(scheme_at..scheme_at + 5).is_some_and(|p| p.eq_ignore_ascii_case(b"data:")) {
        return None;
    }
    let from = if angle { tail.find('>')? + 1 } else { 0 };
    let close = k + from + tail[from..].find(')')?;
    Some((&s[..j], close + 1))
}

fn prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn converts_docs_page() {
        let html = "<html><body><nav><a href='/'>Home</a></nav><h1>Getting started</h1>\
                    <p>Install the crate with cargo and read the guide.</p>\
                    <pre><code>cargo add foo</code></pre></body></html>";
        let md = to_markdown(html).unwrap();
        assert!(md.contains("# Getting started"), "{md}");
        assert!(md.contains("cargo add foo"), "{md}");
    }

    #[test]
    fn markdown_alternate_must_describe_the_page() {
        let html = r#"<head><LINK REL="alternate" type="text/markdown" href="/docs/intro.md"></head>"#;
        assert_eq!(
            markdown_alternate(html, &page("https://x.test/docs/intro")),
            Some(page("https://x.test/docs/intro.md"))
        );
        let other = r#"<link rel=alternate type="text/markdown" href=/other/page.md>"#;
        assert_eq!(markdown_alternate(other, &page("https://x.test/docs/intro")), None);
        let feed = r#"<link rel="alternate" type="text/markdown" href="/feed/">"#;
        assert_eq!(markdown_alternate(feed, &page("https://x.test/feed")), None);
        let root = r#"<link rel="alternate" type="text/markdown" href="/x.md">"#;
        assert_eq!(markdown_alternate(root, &page("https://x.test/")), None);
    }

    #[test]
    fn attribute_values() {
        assert_eq!(attr("<link data-rel='x' rel='alternate' HREF=/x.md>", "href"), Some("/x.md".into()));
        assert_eq!(attr("<link rel=\"a\">", "href"), None);
    }

    #[test]
    fn low_quality_detection() {
        assert!(is_low_quality("# App\n\nPlease enable JavaScript to use this app."));
        assert!(!is_low_quality(&"A normal paragraph of prose that reads as an article. ".repeat(5)));
        let nav = "Home\nAbout\nBlog\nDocs\nPricing\nLogin\nSignup\nTeam\nCareers\nPress\nHelp\nContact";
        assert!(is_low_quality(nav));
    }

    #[test]
    fn html_detection() {
        assert!(looks_like_html("  <!DOCTYPE html><html>"));
        assert!(looks_like_html("<BODY>x"));
        assert!(!looks_like_html("# Heading <html>"));
    }

    #[test]
    fn data_uri_images_are_stripped() {
        let md = "![logo](data:image/png;base64,AAAA) text ![](data:image/svg+xml,<svg>) ![ok](https://x/y.png)";
        let out = strip_data_uri_images(md);
        assert_eq!(out, "![logo] text  ![ok](https://x/y.png)");
    }
}
