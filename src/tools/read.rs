use super::{ToolOutput, Tools};
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
        let k: usize = k.parse().ok()?;
        return Some((total.saturating_sub(k) + 1, total));
    }
    if let Some((n, m)) = sel.split_once('-') {
        let (n, m): (usize, usize) = (n.parse().ok()?, m.parse().ok()?);
        return Some((n.saturating_sub(1).max(1), (m + 3).min(total)));
    }
    if let Some((n, k)) = sel.split_once('+') {
        let (n, k): (usize, usize) = (n.parse().ok()?, k.parse().ok()?);
        return Some((n, (n + k).saturating_sub(1).min(total)));
    }
    let n: usize = sel.parse().ok()?;
    Some((n, (n + DEFAULT_LINES - 1).min(total)))
}

impl Tools {
    pub fn read(&self, a: Args) -> ToolOutput {
        let mut abs = self.abs(&a.path);
        let mut sel = None;
        if !abs.exists() {
            if let Some((p, s)) = a.path.rsplit_once(':') {
                let cand = self.abs(p);
                if cand.exists() && !s.is_empty() {
                    abs = cand;
                    sel = Some(s.to_string());
                }
            }
        }
        let disp = self.display(&abs);
        if !abs.exists() {
            return ToolOutput::err(format!("File not found: {disp}"));
        }
        if abs.is_dir() {
            return list_dir(&abs);
        }
        let raw = match std::fs::read(&abs) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("Cannot read {disp}: {e}")),
        };
        let head = &raw[..raw.len().min(8192)];
        let Ok(s) = std::str::from_utf8(&raw).map(String::from).map_err(|_| ()).and_then(|s| {
            if head.contains(&0) { Err(()) } else { Ok(s) }
        }) else {
            return ToolOutput::ok(format!("Binary file ({} bytes); not displayed", raw.len()));
        };
        let norm = text::normalize_to_lf(text::strip_bom(&s).1).into_owned();
        let key = canonical_key(&abs);
        let tag = self.edit_store.record(&key, &norm, None);
        let lines: Vec<&str> = norm.lines().collect();
        let total = lines.len();
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
        for n in start..=end.min(start + MAX_LINES - 1) {
            let line = lines[n - 1];
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
        ToolOutput::ok(out)
    }
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
    fn default_and_range_selectors() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let body: String = (1..=400).map(|i| format!("line{i}\n")).collect();
        std::fs::write(d.join("p.txt"), body).unwrap();
        let t = Tools::new(d.clone());
        let o = t.read(Args { path: "p.txt".into() });
        assert!(!o.is_error);
        assert!(o.text.contains("\n300:line300"));
        assert!(!o.text.contains("301:line301"));
        assert!(o.text.contains("Use p.txt:301 to continue"));
        let o = t.read(Args { path: "p.txt:50-80".into() });
        assert!(o.text.contains("\n49:line49\n"));
        assert!(o.text.contains("\n83:line83"));
        assert!(!o.text.contains("84:line84"));
        assert!(!o.text.contains("\n48:"));
        let o = t.read(Args { path: "p.txt:-2".into() });
        assert!(o.text.contains("399:line399\n400:line400"));
    }
}
