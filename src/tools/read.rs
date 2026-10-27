use super::{FileMeta, ToolMeta, ToolOutput, Tools};
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

impl Tools {
    pub async fn read(&self, a: Args) -> ToolOutput {
        let mut sel = None;
        let mut authored = a.path.as_str();
        let first = self.resolve(&a.path).ok().filter(|r| r.path.exists());
        let res = match first {
            Some(r) => r,
            None => match super::uri::split_selector(&a.path).and_then(|(p, s)| Some((self.resolve(p).ok()?, p, s))) {
                Some((r, p, s)) if r.path.exists() => {
                    sel = Some(s.to_string());
                    authored = p;
                    r
                }
                _ => match self.resolve(&a.path) {
                    Ok(r) => r,
                    Err(e) => return ToolOutput::err(e),
                },
            },
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
        if let Some(mime) = crate::image::sniff(&raw) {
            let n = raw.len();
            return match crate::image::from_bytes(raw) {
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
        let Some((start, end)) = select(sel.as_deref(), total) else {
            return ToolOutput::err(format!("Invalid selector in {}", a.path));
        };
        if total == 0 {
            return ToolOutput::ok(format!("[{disp}#{tag}]\n(empty file)"));
        }
        if start > total {
            return ToolOutput::err(format!("{disp} has only {total} lines"));
        }
        let mut out = format!("[{disp}#{tag}]\n");
        let mut seen = Vec::new();
        let mut bytes = 0;
        let mut last = start - 1;
        // Only the window is walked; lines before it are skipped and lines after it never visited.
        let window = end.min(start + MAX_LINES - 1) + 1 - start;
        for (k, line) in norm.lines().skip(start - 1).take(window).enumerate() {
            let n = start + k;
            let row = if line.chars().count() > MAX_COL {
                format!("{n}:{}…", line.chars().take(MAX_COL).collect::<String>())
            } else {
                seen.push(n as u32);
                format!("{n}:{line}")
            };
            if bytes + row.len() > MAX_BYTES && last >= start {
                break;
            }
            bytes += row.len() + 1;
            out.push_str(&row);
            out.push('\n');
            last = n;
        }
        out.pop();
        self.edit_store.record_seen_lines(&key, &tag, &seen);
        if last < total {
            let _ = write!(out, "\n\n[{} more lines in file. Use {disp}:{} to continue]", total - last, last + 1);
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
