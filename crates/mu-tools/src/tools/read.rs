use super::Tools;
use mu_base::tool::{FileMeta, ToolMeta, ToolOutput};
use pi_edit::{path_policy::canonical_key, text};
use serde::Deserialize;
use std::fmt::Write;

#[derive(Deserialize)]
pub struct Args {
    path: String,
}

const DEFAULT_LINES: usize = 300;
const MAX_LINES: usize = 3000;
const MAX_BYTES: usize = 50 * 1024;
const MAX_COL: usize = 768;

/// 1-based inclusive range over `total` lines.
fn select(sel: Option<&str>, total: usize) -> Option<(usize, usize)> {
    let Some(sel) = sel else { return Some((1, DEFAULT_LINES.min(total))) };
    if let Some(k) = sel.strip_prefix('-') {
        let k: usize = k.parse().ok().filter(|&k| k > 0)?;
        return Some((total.saturating_sub(k) + 1, total));
    }
    if let Some((n, m)) = sel.split_once('-') {
        let (n, m): (usize, usize) = (n.parse().ok().filter(|&n| n > 0)?, m.parse().ok().filter(|&m| m > 0)?);
        return Some((n.saturating_sub(1).max(1), m.saturating_add(3).min(total)));
    }
    if let Some((n, k)) = sel.split_once('+') {
        let (n, k): (usize, usize) = (n.parse().ok().filter(|&n| n > 0)?, k.parse().ok()?);
        return Some((n, n.saturating_add(k).saturating_sub(1).min(total)));
    }
    let n: usize = sel.parse().ok().filter(|&n| n > 0)?;
    Some((n, n.saturating_add(DEFAULT_LINES - 1).min(total)))
}

/// A comma-separated list of `N` / `N-M` ranges, as the edit tool's unseen-lines message emits
/// them. Unlike a lone `N` (300 lines from N), `N` here means exactly line N, and no context
/// lines are added. Ranges are sorted and merged.
fn parse_list(sel: &str, total: usize) -> Option<Vec<(usize, usize)>> {
    sel.split(',')
        .map(|p| {
            let (n, m) = match p.split_once('-') {
                Some((n, m)) => (n.parse::<usize>().ok()?, m.parse::<usize>().ok()?),
                None => {
                    let n = p.parse::<usize>().ok()?;
                    (n, n)
                }
            };
            (n > 0 && m >= n).then_some((n, m.min(total)))
        })
        .collect::<Option<Vec<_>>>()
        .map(|mut v| {
            v.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(v.len());
            for (s, e) in v {
                match merged.last_mut() {
                    Some(l) if s <= l.1.saturating_add(1) => l.1 = l.1.max(e),
                    _ => merged.push((s, e)),
                }
            }
            merged
        })
}

/// Sorted line numbers as a selector list, runs collapsed to `N-M`, at most 20 items.
fn runs(lines: &[usize]) -> String {
    let mut items: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut j = i;
        while j + 1 < lines.len() && lines[j + 1] == lines[j] + 1 {
            j += 1;
        }
        items.push(if j > i { format!("{}-{}", lines[i], lines[j]) } else { lines[i].to_string() });
        i = j + 1;
    }
    let more = items.len() > 20;
    items.truncate(20);
    let mut s = items.join(",");
    if more {
        s.push_str(",…");
    }
    s
}

impl Tools {
    pub async fn read(&self, a: Args) -> ToolOutput {
        let super::uri::ReadTarget { path: authored, sel, raw: raw_mode } =
            super::uri::read_target(&a.path, |p| self.resolve(p).is_ok_and(|r| r.path.exists()));
        let res = match self.resolve(&authored) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let abs = res.path;
        let disp = if res.scheme.is_some() { authored.to_string() } else { self.display(&abs) };
        if !abs.exists() {
            return ToolOutput::err(match res.scheme {
                Some(super::uri::Scheme::Artifact) => format!("{authored} not found"),
                _ => format!("File not found: {disp}"),
            });
        }
        // Directory listing and file reading run on a blocking thread.
        let path = abs.clone();
        let source = tokio::task::spawn_blocking(move || {
            if path.is_dir() { Source::Dir(list_dir(&path)) } else { Source::File(std::fs::read(&path)) }
        })
        .await;
        let raw = match source {
            Ok(Source::Dir(out)) => return out,
            Ok(Source::File(Ok(raw))) => raw,
            Ok(Source::File(Err(e))) => return ToolOutput::err(format!("Cannot read {disp}: {e}")),
            Err(e) => return ToolOutput::err(format!("read failed: {e}")),
        };
        // Images go to the model as image blocks; the text names what it is looking at.
        if let Some(mime) = mu_base::image::sniff(&raw) {
            let n = raw.len();
            return match mu_base::image::from_bytes(raw) {
                Ok(img) => {
                    ToolOutput { images: vec![img], ..ToolOutput::ok(format!("Image: {disp} ({mime}, {n} bytes)")) }
                }
                Err(e) => ToolOutput::err(format!("{disp}: {e}")),
            };
        }
        let head = &raw[..raw.len().min(8192)];
        let s = match std::str::from_utf8(&raw) {
            Ok(s) if !head.contains(&0) => s,
            _ => return ToolOutput::ok(format!("Binary file ({} bytes); not displayed", raw.len())),
        };
        // Borrowed all the way unless the file has CRLFs: no copy of a large file just to read it.
        let norm = text::normalize_to_lf(text::strip_bom(s).1);
        let key = canonical_key(&abs);
        let tag = self.edit_store.record(&key, &norm, None);
        let total = line_count(&norm);
        let ranges = match sel.as_deref() {
            Some(l) if l.contains(',') => parse_list(l, total),
            s => select(s, total).map(|r| vec![r]),
        };
        let Some(ranges) = ranges else {
            return ToolOutput::err(format!("Invalid selector in {}", a.path));
        };
        if total == 0 {
            return ToolOutput::ok(format!("[{disp}#{tag}]\n(empty file)"));
        }
        if ranges.iter().all(|r| r.0 > total) {
            return ToolOutput::err(format!("{disp} has only {total} lines"));
        }
        let mut out = format!("[{disp}#{tag}]\n");
        let mut seen = Vec::new();
        let mut cut_lines: Vec<usize> = Vec::new();
        let mut bytes = 0;
        let mut rows = 0;
        let mut last = 0;
        let mut capped_any = false;
        // First unshown line (and its range) when the output limit ends a list early.
        let mut stopped: Option<(usize, usize)> = None;
        let multi = sel.as_deref().is_some_and(|s| s.contains(','));
        'ranges: for (ri, &(start, end)) in ranges.iter().enumerate() {
            if start > total {
                continue;
            }
            let mut separator = multi && bytes > 0;
            last = start - 1;
            // Only the window is walked; lines before it are skipped and lines after it never visited.
            let window = end.min(start + MAX_LINES - 1) + 1 - start;
            for (k, line) in norm.lines().skip(start - 1).take(window).enumerate() {
                let n = start + k;
                let mut capped = false;
                let row = if raw_mode {
                    let mut row = format!("{n}:{line}");
                    if row.len() > MAX_BYTES {
                        let mut cut = MAX_BYTES - '…'.len_utf8();
                        while !row.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        row.truncate(cut);
                        row.push('…');
                        capped = true;
                    }
                    row
                } else if line.chars().count() > MAX_COL {
                    capped = true;
                    format!("{n}:{}…", line.chars().take(MAX_COL).collect::<String>())
                } else {
                    format!("{n}:{line}")
                };
                // At least one row is always shown; after that the budget covers the whole read.
                if bytes > 0 && (bytes + row.len() > MAX_BYTES || (multi && rows >= MAX_LINES)) {
                    stopped = Some((ri, n));
                    break 'ranges;
                }
                if capped {
                    if raw_mode {
                        capped_any = true;
                    } else {
                        cut_lines.push(n);
                    }
                } else {
                    seen.push(n as u32);
                }
                if separator {
                    out.push_str("...\n");
                    bytes += 4;
                    separator = false;
                }
                bytes += row.len() + 1;
                rows += 1;
                out.push_str(&row);
                out.push('\n');
                last = n;
            }
        }
        out.pop();
        self.edit_store.record_seen_lines(&key, &tag, &seen);
        let sfx = if raw_mode { ":raw" } else { "" };
        if let (true, Some((ri, n))) = (multi, stopped) {
            let rest: Vec<String> = ranges[ri..]
                .iter()
                .enumerate()
                .map(|(i, &(s, e))| {
                    let s = if i == 0 { n } else { s };
                    if s == e { s.to_string() } else { format!("{s}-{e}") }
                })
                .collect();
            let _ = write!(out, "\n\n[Output limit reached. Use {disp}:{}{sfx} for the rest]", rest.join(","));
        } else if !multi && last < total {
            let _ = write!(out, "\n\n[{} more lines in file. Use {disp}:{}{sfx} to continue]", total - last, last + 1);
        }
        if raw_mode && capped_any {
            let _ = write!(out, "\n[A line exceeded {MAX_BYTES} bytes and was cut]");
        }
        if !cut_lines.is_empty() {
            let _ = write!(
                out,
                "\n[Lines cut at {MAX_COL} chars; use {disp}:{}:raw for their full content]",
                runs(&cut_lines)
            );
        }
        let mut result = ToolOutput::ok(out);
        result.meta = Some(ToolMeta::File(FileMeta { path: disp.clone(), tag: Some(tag), plan: false }));
        result
    }
}

enum Source {
    Dir(ToolOutput),
    File(std::io::Result<Vec<u8>>),
}

/// Number of lines `str::lines` would yield, without building them.
fn line_count(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count() + usize::from(!s.is_empty() && !s.ends_with('\n'))
}

fn list_dir(p: &std::path::Path) -> ToolOutput {
    let mut names: Vec<String> = match std::fs::read_dir(p) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let mut n = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    n.push('/');
                }
                n
            })
            .collect(),
        Err(e) => return ToolOutput::err(format!("Cannot read directory: {e}")),
    };
    if names.is_empty() {
        return ToolOutput::ok("(empty directory)");
    }
    names.sort();
    let extra = names.len().saturating_sub(500);
    names.truncate(500);
    let mut out = names.join("\n");
    if extra > 0 {
        let _ = write!(out, "\n… {extra} more");
    }
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_out_of_range_selectors() {
        for s in ["0", "-0", "0-5", "3-0", "0+5", "00"] {
            assert_eq!(select(Some(s), 10), None, "{s}");
        }
        assert_eq!(select(Some("11"), 10), Some((11, 10)));
        assert_eq!(select(Some("18446744073709551615-18446744073709551615"), 10), Some((18446744073709551614, 10)));
        assert_eq!(select(Some("2+0"), 10), Some((2, 1)));
    }

    #[test]
    fn line_count_matches_str_lines() {
        for s in ["", "a", "a\n", "a\nb", "\n\n", "a\n\nb\n", "x\n\n\n"] {
            assert_eq!(line_count(s), s.lines().count(), "{s:?}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_and_range_selectors() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let body: String = (1..=400).map(|i| format!("line{i}\n")).collect();
        std::fs::write(d.join("p.txt"), body).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "p.txt".into() }).await;
        assert!(!o.is_error);
        assert!(o.text.contains("\n300:line300"));
        assert!(!o.text.contains("301:line301"));
        assert!(o.text.contains("Use p.txt:301 to continue"));
        let o = t.read(Args { path: "p.txt:50-80".into() }).await;
        assert!(o.text.contains("\n49:line49\n"));
        assert!(o.text.contains("\n83:line83"));
        assert!(!o.text.contains("84:line84"));
        assert!(!o.text.contains("\n48:"));
        let o = t.read(Args { path: "p.txt:-2".into() }).await;
        assert!(o.text.contains("399:line399\n400:line400"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_selector_shows_long_lines_in_full() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let long = "x".repeat(1200);
        std::fs::write(d.join("l.md"), format!("a\n{long}\nb\n")).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "l.md".into() }).await;
        assert!(!o.text.contains(&long));
        // A plain read leaves the line unseen; the edit is refused and points at `:raw`.
        let tag = o.text[o.text.find('#').unwrap() + 1..o.text.find(']').unwrap()].to_string();
        let input = format!("[l.md#{tag}]\nPUT 2.=2:\n+short\n");
        let o = t.edit(super::super::edit::Args::for_test(input.clone())).await;
        assert!(o.is_error, "{}", o.text);
        assert!(o.text.contains(":raw"), "{}", o.text);
        for p in ["l.md:2:raw", "l.md:2-2:raw", "l.md:raw"] {
            let o = t.read(Args { path: p.into() }).await;
            assert!(!o.is_error, "{p}: {}", o.text);
            assert!(o.text.contains(&format!("2:{long}")), "{p}");
        }
        // Same content, same tag: the raw read made the line seen.
        let o = t.edit(super::super::edit::Args::for_test(input)).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read_to_string(d.join("l.md")).unwrap(), "a\nshort\nb\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_list_from_the_edit_error_is_readable() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let (l1, l2) = ("x".repeat(1200), "y".repeat(1300));
        std::fs::write(d.join("l.md"), format!("a\n{l1}\nb\nc\nd\n{l2}\ne\n")).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "l.md".into() }).await;
        assert!(o.text.contains("l.md:2,6:raw"), "cut notice: {}", o.text);
        let tag = o.text[o.text.find('#').unwrap() + 1..o.text.find(']').unwrap()].to_string();
        let input = format!("[l.md#{tag}]\nPUT 2.=6:\n+short\n");
        let o = t.edit(super::super::edit::Args::for_test(input.clone())).await;
        assert!(o.is_error, "{}", o.text);
        let from = o.text.find("Read `").expect(&o.text) + "Read `".len();
        let path = &o.text[from..from + o.text[from..].find('`').unwrap()];
        assert!(path.ends_with(":raw") && path.contains(','), "{path}");
        let o = t.read(Args { path: path.to_string() }).await;
        assert!(!o.is_error, "{path}: {}", o.text);
        assert!(o.text.contains(&format!("2:{l1}\n")) && o.text.contains(&format!("6:{l2}")), "{}", o.text);
        assert!(o.text.contains("\n...\n"), "{}", o.text);
        let o = t.edit(super::super::edit::Args::for_test(input)).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read_to_string(d.join("l.md")).unwrap(), "a\nshort\ne\n");
        // Reversed alias.
        let o = t.read(Args { path: "l.md:raw:2".into() }).await;
        assert!(!o.is_error, "{}", o.text);
    }

    /// Under a stale tag, an edit of a cut line names the line's current number and the current
    /// tag; mu's read of that selector must hand back the same tag so the re-issued edit applies.
    #[tokio::test(flavor = "current_thread")]
    async fn stale_tag_cut_line_reply_is_readable_at_current_numbers() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let wide = format!("note: {} TAIL", "abcdefghij".repeat(80));
        std::fs::write(d.join("c.txt"), format!("{wide}\ntwo\nthree\n")).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "c.txt".into() }).await;
        assert!(!o.text.contains(&wide));
        let tag0 = o.text[o.text.find('#').unwrap() + 1..o.text.find(']').unwrap()].to_string();
        let o =
            t.edit(super::super::edit::Args::for_test(format!("[c.txt#{tag0}]\nPUT <1:\n+head a\n+head b\n"))).await;
        assert!(!o.is_error, "{}", o.text);
        let before = std::fs::read_to_string(d.join("c.txt")).unwrap();
        let stale = format!("[c.txt#{tag0}]\nPUT 1.=1:\n+NOTE: {}\n", &wide[6..768]);
        let e = t.edit(super::super::edit::Args::for_test(stale)).await;
        assert!(e.is_error, "{}", e.text);
        assert_eq!(std::fs::read_to_string(d.join("c.txt")).unwrap(), before, "refused edits write nothing");
        let from = e.text.find("Read `").expect(&e.text) + "Read `".len();
        let path = &e.text[from..from + e.text[from..].find('`').unwrap()];
        assert_eq!(path, "c.txt:3:raw", "{}", e.text);
        let live = e.text.rsplit("[c.txt#").next().and_then(|r| r.split(']').next()).unwrap().to_string();
        let o = t.read(Args { path: path.to_string() }).await;
        assert!(!o.is_error, "{}", o.text);
        assert!(o.text.contains(&format!("[c.txt#{live}]")), "read tag differs from the reply's: {}", o.text);
        assert!(o.text.contains(&format!("3:{wide}")), "{}", o.text);
        let full = wide.replacen("note:", "NOTE:", 1);
        let o = t.edit(super::super::edit::Args::for_test(format!("[c.txt#{live}]\nPUT 3.=3:\n+{full}\n"))).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read_to_string(d.join("c.txt")).unwrap(), format!("head a\nhead b\n{full}\ntwo\nthree\n"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_reads_obey_the_byte_budget() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let body: String = ["a", "b", "c"].iter().map(|c| format!("{}\nsep\n", c.repeat(30_000))).collect();
        std::fs::write(d.join("w.md"), body).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "w.md:1,3,5:raw".into() }).await;
        assert!(!o.is_error, "{}", o.text);
        assert!(o.text.contains("1:aaa") && !o.text.contains("3:bbb") && !o.text.contains("5:ccc"));
        assert!(o.text.len() < MAX_BYTES + 512, "{}", o.text.len());
        assert!(
            o.text.contains("Output limit reached. Use w.md:3,5:raw for the rest"),
            "{}",
            &o.text[o.text.len() - 120..]
        );
        assert!(!o.text.contains("\n...\n"), "dangling separator");
        // Neither unshown line was recorded as seen, so an edit touching them is refused.
        let tag = o.text[o.text.find('#').unwrap() + 1..o.text.find(']').unwrap()].to_string();
        let input = format!("[w.md#{tag}]\nPUT 5.=5:\n+x\n");
        let e = t.edit(super::super::edit::Args::for_test(input)).await;
        assert!(e.is_error, "{}", e.text);
    }

    #[test]
    fn list_parsing_and_run_collapsing() {
        assert_eq!(parse_list("5-8,2,3-6", 100), Some(vec![(2, 8)]));
        assert_eq!(parse_list("9,2", 100), Some(vec![(2, 2), (9, 9)]));
        assert_eq!(parse_list("3,,5", 100), None);
        assert_eq!(runs(&[1, 2, 3, 7, 9, 10]), "1-3,7,9-10");
        let many: Vec<usize> = (0..30).map(|i| i * 2 + 1).collect();
        assert!(runs(&many).ends_with(",…"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn images_come_back_as_images() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("x.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "x.png".into() }).await;
        assert!(!o.is_error, "{}", o.text);
        assert!(o.text.starts_with("Image: x.png (image/png, 16 bytes)"), "{}", o.text);
        assert_eq!(o.images.len(), 1);
        assert_eq!(o.images[0].mime_type, "image/png");
        std::fs::write(d.join("b.bin"), b"\0\x01\x02").unwrap();
        let o = t.read(Args { path: "b.bin".into() }).await;
        assert!(o.images.is_empty() && o.text.starts_with("Binary file"));
    }
}
