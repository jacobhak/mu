use super::Tools;
use encoding_rs::{CoderResult, Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};
use mu_base::tool::{FileMeta, ToolMeta, ToolOutput};
use omp_edit::{path_policy::canonical_key, store::MAX_SNAPSHOT_FILE_BYTES, text};
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt::Write;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// A target for `read`: the path, with any selector inline as documented.
pub struct Args {
    path: String,
}

/// The keys models pass instead of an inline selector. They are folded into `path` when the call is
/// read, so that one form reaches the tool: a bare `selector`/`offset`/`limit` used to be dropped
/// silently, which returned the whole file when a range was asked for.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    path: String,
    selector: Option<String>,
    offset: Option<u64>,
    limit: Option<u64>,
    raw: Option<bool>,
}

impl<'de> Deserialize<'de> for Args {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Raw::deserialize(d)?;
        Ok(Args { path: fold(raw).map_err(serde::de::Error::custom)? })
    }
}

/// `path` with a `selector`, an `offset`+`limit` pair or `raw` folded into its inline selector. An
/// inline selector in `path` wins: it is the documented form, and a caller that wrote both meant it.
fn fold(a: Raw) -> Result<String, String> {
    if super::uri::split_selector(&a.path).is_some() || a.path.ends_with(":raw") {
        return Ok(a.path);
    }
    let sel = match a.selector.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let s = s.strip_prefix(':').unwrap_or(s);
            if !s.chars().any(|c| c.is_ascii_digit())
                || !s.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | ','))
            {
                return Err(format!("invalid selector `{s}`: use `:N`, `:N-M`, `:N+K` or `:-K`"));
            }
            Some(s.to_string())
        }
        None => match (a.offset, a.limit) {
            (None, None) => None,
            (_, Some(0)) => return Err("`limit` must be at least 1".into()),
            (Some(o), Some(k)) => Some(format!("{}+{k}", o.max(1))),
            (Some(o), None) => Some(o.max(1).to_string()),
            (None, Some(k)) => Some(format!("1+{k}")),
        },
    };
    let mut p = a.path;
    if let Some(s) = sel {
        p.push(':');
        p.push_str(&s);
    }
    if a.raw == Some(true) {
        p.push_str(":raw");
    }
    Ok(p)
}

impl Args {
    pub(super) fn paths(&self) -> Vec<String> {
        vec![self.path.clone()]
    }

    /// The path as the model wrote it, selector and all.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

/// Identical reads of one path and selector tolerated before the model is told re-reading changes
/// nothing (omp's threshold), and how many such reads are tracked per session.
const REPEAT_READ_HINT: u32 = 3;
const REPEAT_READ_TRACKER_CAP: usize = 64;

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

/// The ranges a selector names in a file of `total` lines (`usize::MAX` while that is unknown).
fn selection(sel: Option<&str>, total: usize) -> Option<Vec<(usize, usize)>> {
    match sel {
        Some(l) if l.contains(',') => parse_list(l, total),
        s => select(s, total).map(|r| vec![r]),
    }
}

/// Each range cut to the most lines one read shows: MAX_LINES from its first line.
fn window_ranges(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    ranges.iter().map(|&(s, e)| (s, e.min(s.saturating_add(MAX_LINES - 1)))).collect()
}

/// The part of a line a read can look at. Enough bytes that the row decisions (cut or not, and
/// where) are the same as for the whole line: a raw row is cut at MAX_BYTES after its number, and
/// a plain row at MAX_COL chars, where each char is at most 4 bytes.
fn kept(line: &str, raw_mode: bool) -> &str {
    let limit = if raw_mode { MAX_BYTES + 32 } else { MAX_COL * 4 + 4 };
    if line.len() <= limit {
        return line;
    }
    let mut cut = limit;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    &line[..cut]
}

/// Collects the lines a read shows as they stream past, in order. It keeps at most MAX_LINES + 1
/// (a list shows at most MAX_LINES rows, and the extra line says where the rest starts) and, in
/// raw mode, stops once the kept text is over the byte budget.
struct Picker<'a> {
    windows: &'a [(usize, usize)],
    last: usize,
    raw_mode: bool,
    picks: Vec<(usize, String)>,
    kept_bytes: usize,
    full: bool,
    next: Option<usize>,
}

impl<'a> Picker<'a> {
    fn new(windows: &'a [(usize, usize)], raw_mode: bool) -> Self {
        let last = windows.iter().map(|w| w.1).max().unwrap_or(0);
        Self { windows, last, raw_mode, picks: Vec::new(), kept_bytes: 0, full: false, next: None }
    }

    /// Feeds line `n` (1-based). Returns false once no further line is wanted.
    fn feed(&mut self, n: usize, line: &str) -> bool {
        if n > self.last {
            return false;
        }
        if !self.windows.iter().any(|&(s, e)| (s..=e).contains(&n)) {
            return true;
        }
        if self.full {
            self.next = Some(n);
            return false;
        }
        let text = kept(line, self.raw_mode);
        self.kept_bytes += text.len();
        self.picks.push((n, text.to_string()));
        self.full = self.picks.len() > MAX_LINES || (self.raw_mode && self.kept_bytes > MAX_BYTES);
        true
    }

    /// The kept lines, and the first wanted line that was not kept (if any).
    fn finish(self) -> (Vec<(usize, String)>, Option<usize>) {
        (self.picks, self.next)
    }
}

/// One row, `N:text`, and whether its text was cut to fit.
fn row(n: usize, line: &str, raw_mode: bool) -> (String, bool) {
    if raw_mode {
        let mut row = format!("{n}:{line}");
        if row.len() > MAX_BYTES {
            let mut cut = MAX_BYTES - '…'.len_utf8();
            while !row.is_char_boundary(cut) {
                cut -= 1;
            }
            row.truncate(cut);
            row.push('…');
            return (row, true);
        }
        (row, false)
    } else if line.chars().count() > MAX_COL {
        (format!("{n}:{}…", line.chars().take(MAX_COL).collect::<String>()), true)
    } else {
        (format!("{n}:{line}"), false)
    }
}

/// The read's text: `head` (the header line), the rows of the picked lines within the byte budget,
/// and the notes that say what was left out. `total` is `None` when the file's length is unknown.
/// Returns the text and the line numbers shown in full (what the edit store may record).
fn render(
    head: &str,
    disp: &str,
    sel: Option<&str>,
    raw_mode: bool,
    ranges: &[(usize, usize)],
    picked: (Vec<(usize, String)>, Option<usize>),
    total: Option<usize>,
) -> (String, Vec<u32>) {
    let (lines, next) = picked;
    let multi = sel.is_some_and(|s| s.contains(','));
    let mut out = format!("{head}\n");
    let mut seen = Vec::new();
    let mut cut_lines: Vec<usize> = Vec::new();
    let mut bytes = 0;
    let mut last = ranges.first().map_or(0, |r| r.0.saturating_sub(1));
    let mut capped_any = false;
    // First unshown line when the output limit ends a read early.
    let mut stopped: Option<usize> = None;
    let mut range = 0;
    let mut prev_range = None;
    // Every earlier line was shown, so the index is the number of rows shown so far.
    for (rows, (n, line)) in lines.iter().enumerate() {
        let n = *n;
        while range + 1 < ranges.len() && ranges[range].1 < n {
            range += 1;
        }
        let (row, capped) = row(n, line, raw_mode);
        // At least one row is always shown; after that the budget covers the whole read.
        if bytes > 0 && (bytes + row.len() > MAX_BYTES || (multi && rows >= MAX_LINES)) {
            stopped = Some(n);
            break;
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
        if multi && bytes > 0 && prev_range != Some(range) {
            out.push_str("...\n");
            bytes += 4;
        }
        prev_range = Some(range);
        bytes += row.len() + 1;
        out.push_str(&row);
        out.push('\n');
        last = n;
    }
    out.pop();
    let stopped = stopped.or(next);
    let sfx = if raw_mode { ":raw" } else { "" };
    if let (true, Some(n)) = (multi, stopped) {
        let rest: Vec<String> = ranges
            .iter()
            .filter(|r| r.1 >= n)
            .map(|&(s, e)| {
                let s = s.max(n);
                if s == e { s.to_string() } else { format!("{s}-{e}") }
            })
            .collect();
        let _ = write!(out, "\n\n[Output limit reached. Use {disp}:{}{sfx} for the rest]", rest.join(","));
    } else if !multi && total.is_none_or(|t| last < t) {
        match total {
            Some(t) => {
                let _ = write!(out, "\n\n[{} more lines in file. Use {disp}:{}{sfx} to continue]", t - last, last + 1);
            }
            None => {
                let _ = write!(out, "\n\n[more lines in file. Use {disp}:{}{sfx} to continue]", last + 1);
            }
        }
    }
    if raw_mode && capped_any {
        let _ = write!(out, "\n[A line exceeded {MAX_BYTES} bytes and was cut]");
    }
    if !cut_lines.is_empty() {
        let _ =
            write!(out, "\n[Lines cut at {MAX_COL} chars; use {disp}:{}:raw for their full content]", runs(&cut_lines));
    }
    (out, seen)
}

impl Tools {
    /// The file a `read` path names, selector and `:raw` split off: what `read` opens, and what the
    /// gate judges.
    pub(crate) fn read_file(&self, raw: &str) -> super::uri::ReadTarget {
        super::uri::read_target(raw, |p| self.resolve(p).is_ok_and(|r| r.path.exists()))
    }

    pub async fn read(&self, a: Args) -> ToolOutput {
        let exists = |p: &str| self.resolve(p).is_ok_and(|r| r.path.exists());
        let super::uri::ReadTarget { path: authored, sel, raw: raw_mode } = self.read_file(&a.path);
        let res = match self.resolve(&authored) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let abs = res.path;
        let disp = if res.scheme.is_some() { authored.to_string() } else { self.display(&abs) };
        if !abs.exists() {
            if let Some((file, sel)) = super::uri::bad_selector(&a.path, exists) {
                return ToolOutput::err(format!(
                    "Invalid selector `:{sel}` in {file}: a selector takes one form, `:N`, `:N-M`, `:N+K` or `:-K`, written inline as `{file}:N-M`"
                ));
            }
            return ToolOutput::err(match res.scheme {
                Some(super::uri::Scheme::Artifact) => format!("{authored} not found"),
                _ => format!("File not found: {disp}"),
            });
        }
        // Directory listing and file reading run on a blocking thread.
        let path = abs.clone();
        let (sel_big, disp_big) = (sel.clone(), disp.clone());
        let source = tokio::task::spawn_blocking(move || {
            if path.is_dir() {
                return Source::Dir(Box::new(list_dir(&path)));
            }
            match std::fs::metadata(&path) {
                Ok(m) if !m.is_file() => Source::Special,
                Ok(m) if m.len() > MAX_SNAPSHOT_FILE_BYTES => {
                    Source::Large(Box::new(read_large(&path, sel_big.as_deref(), raw_mode, &disp_big, m.len())))
                }
                _ => Source::File(std::fs::read(&path)),
            }
        })
        .await;
        let raw = match source {
            Ok(Source::Dir(out) | Source::Large(out)) => return *out,
            Ok(Source::Special) => {
                return ToolOutput::err(format!("Not a regular file: {disp} (a FIFO, socket or device)"));
            }
            Ok(Source::File(Ok(raw))) => raw,
            Ok(Source::File(Err(e))) => return ToolOutput::err(format!("Cannot read {disp}: {e}")),
            Err(e) => return ToolOutput::err(format!("read failed: {e}")),
        };
        // Images go to the model as image blocks; the text names what it is looking at.
        if let Some(mime) = mu_base::image::sniff(&raw) {
            return image_output(raw, &disp, mime);
        }
        let head = &raw[..raw.len().min(8192)];
        let (decoded_src, decoded) = match std::str::from_utf8(&raw) {
            Ok(s) if !head.contains(&0) => (Cow::Borrowed(s), None),
            _ => match decode_fallback(&raw, head) {
                Some((s, name)) => (s, Some(name)),
                None => return ToolOutput::ok(format!("Binary file ({} bytes); not displayed", raw.len())),
            },
        };
        // Borrowed all the way unless the file has CRLFs: no copy of a large file just to read it.
        let norm = text::normalize_to_lf(text::strip_bom(&decoded_src).1);
        let total = line_count(&norm);
        let key = canonical_key(&abs);
        let (head_line, tag) = match decoded {
            Some(name) => (format!("[{disp}] (decoded from {name}; read-only view, edit anchors not available)"), None),
            None => {
                let tag = self.edit_store.record(&key, &norm, None);
                (format!("[{disp}#{tag}]"), Some(tag))
            }
        };
        let Some(ranges) = selection(sel.as_deref(), total) else {
            return ToolOutput::err(format!("Invalid selector in {}", a.path));
        };
        if total == 0 {
            return ToolOutput::ok(format!("{head_line}\n(empty file)"));
        }
        if ranges.iter().all(|r| r.0 > total) {
            return ToolOutput::err(format!("{disp} has only {total} lines"));
        }
        let windows = window_ranges(&ranges);
        let mut picker = Picker::new(&windows, raw_mode);
        for (i, line) in norm.lines().enumerate() {
            if !picker.feed(i + 1, line) {
                break;
            }
        }
        let (out, seen) = render(&head_line, &disp, sel.as_deref(), raw_mode, &ranges, picker.finish(), Some(total));
        if let Some(tag) = &tag {
            self.edit_store.record_seen_lines(&key, tag, &seen);
        }
        let mut result = ToolOutput::ok(out);
        result.meta = Some(ToolMeta::File(FileMeta { path: disp.clone(), tag, plan: false }));
        if let Some(note) = self.repeated_read(&a.path, &result.text) {
            result.text.push_str(&note);
        }
        result
    }

    /// The note an identical re-read earns. omp appends one after `REPEAT_READ_HINT` identical
    /// outputs; the read still runs, but the model is told re-reading cannot change the answer.
    fn repeated_read(&self, key: &str, text: &str) -> Option<String> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let fingerprint = h.finish();
        let mut seen = self.repeats.borrow_mut();
        if !seen.contains_key(key) && seen.len() >= REPEAT_READ_TRACKER_CAP {
            seen.clear();
        }
        let count = match seen.get_mut(key) {
            Some((fp, count)) if *fp == fingerprint => {
                *count += 1;
                *count
            }
            _ => {
                seen.insert(key.to_string(), (fingerprint, 1));
                1
            }
        };
        (count >= REPEAT_READ_HINT).then(|| {
            format!(
                "\n\n[You have received this identical output {count} times. Re-reading `{key}` will not change it — narrow the selector (`:N-M`) or proceed with what you have.]"
            )
        })
    }
}

enum Source {
    Dir(Box<ToolOutput>),
    /// Neither a directory nor a regular file: a FIFO, socket or device. Never opened.
    Special,
    /// Over `MAX_SNAPSHOT_FILE_BYTES`; read by streaming.
    Large(Box<ToolOutput>),
    File(std::io::Result<Vec<u8>>),
}

/// An image goes to the model as an image block.
fn image_output(raw: Vec<u8>, disp: &str, mime: &str) -> ToolOutput {
    let n = raw.len();
    match mu_base::image::from_bytes(raw) {
        Ok(img) => ToolOutput { images: vec![img], ..ToolOutput::ok(format!("Image: {disp} ({mime}, {n} bytes)")) },
        Err(e) => ToolOutput::err(format!("{disp}: {e}")),
    }
}

/// Text that is not UTF-8 and has no NUL in its first 8 KiB: UTF-16 with a byte order mark, else
/// Windows-1252. `None` for binary.
fn decode_fallback<'a>(raw: &'a [u8], head: &[u8]) -> Option<(Cow<'a, str>, &'static str)> {
    if let Some((enc, _)) = Encoding::for_bom(head).filter(|(e, _)| *e == UTF_16LE || *e == UTF_16BE) {
        let (s, _, _) = enc.decode(raw);
        return Some((s, enc.name()));
    }
    if head.contains(&0) {
        return None;
    }
    let (s, _, _) = WINDOWS_1252.decode(raw);
    Some((s, WINDOWS_1252.name()))
}

/// The encoding a large file is streamed as, judged from its first 8 KiB. `None` for binary.
fn large_encoding(head: &[u8]) -> Option<&'static Encoding> {
    if let Some((enc, _)) = Encoding::for_bom(head).filter(|(e, _)| *e == UTF_16LE || *e == UTF_16BE) {
        return Some(enc);
    }
    if head.contains(&0) {
        return None;
    }
    let utf8 = match std::str::from_utf8(head) {
        Ok(_) => true,
        // A character cut by the 8 KiB sample is still UTF-8.
        Err(e) => e.error_len().is_none(),
    };
    Some(if utf8 { UTF_8 } else { WINDOWS_1252 })
}

/// Calls `f` with each line of `file` (1-based number, decoded as `enc`, without its line ending)
/// until `f` returns false. Returns whether the end of the file was reached.
fn each_line(file: &mut File, enc: &'static Encoding, mut f: impl FnMut(usize, &str) -> bool) -> std::io::Result<bool> {
    let mut dec = enc.new_decoder_with_bom_removal();
    let mut buf = vec![0u8; 64 * 1024];
    let mut partial = String::new();
    let mut out = String::new();
    let mut n = 0;
    loop {
        let read = file.read(&mut buf)?;
        let at_end = read == 0;
        let mut src = &buf[..read];
        out.clear();
        loop {
            // `decode_to_string` never grows the String: reserve room for this input first.
            out.reserve(src.len() * 3 + 16);
            let (res, used, _) = dec.decode_to_string(src, &mut out, at_end);
            src = &src[used..];
            if res == CoderResult::InputEmpty {
                break;
            }
        }
        let text = std::mem::take(&mut partial) + &out;
        let mut rest = text.as_str();
        while let Some(i) = rest.find('\n') {
            n += 1;
            if !f(n, trim_cr(&rest[..i])) {
                return Ok(false);
            }
            rest = &rest[i + 1..];
        }
        if at_end {
            if !rest.is_empty() {
                n += 1;
                if !f(n, trim_cr(rest)) {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        partial = rest.to_string();
    }
}

fn trim_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// A file over `MAX_SNAPSHOT_FILE_BYTES`, read by streaming: only the selected lines are kept, and
/// nothing is recorded for edits.
fn read_large(path: &Path, sel: Option<&str>, raw_mode: bool, disp: &str, size: u64) -> ToolOutput {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return ToolOutput::err(format!("Cannot read {disp}: {e}")),
    };
    let mut head = Vec::new();
    if let Err(e) = (&mut file).take(8192).read_to_end(&mut head) {
        return ToolOutput::err(format!("Cannot read {disp}: {e}"));
    }
    if let Some(mime) = mu_base::image::sniff(&head) {
        return match std::fs::read(path) {
            Ok(raw) => image_output(raw, disp, mime),
            Err(e) => ToolOutput::err(format!("Cannot read {disp}: {e}")),
        };
    }
    let Some(enc) = large_encoding(&head) else {
        return ToolOutput::ok(format!("Binary file ({size} bytes); not displayed"));
    };
    let decoded = (enc != UTF_8).then(|| enc.name());
    let note = decoded.map(|name| format!("decoded from {name}; ")).unwrap_or_default();
    let head_line = format!(
        "[{disp}]\n(large file: {size} bytes; {note}only the selected lines were read; edit anchors are not available)"
    );
    let io_err = |e: std::io::Error| ToolOutput::err(format!("Cannot read {disp}: {e}"));
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        return io_err(e);
    }

    // A tail selector needs the length first; a pass over the file is the only way to get it.
    let mut total = None;
    if sel.is_some_and(|s| s.starts_with('-')) {
        let mut count = 0;
        if let Err(e) = each_line(&mut file, enc, |n, _| {
            count = n;
            true
        }) {
            return io_err(e);
        }
        if let Err(e) = file.seek(SeekFrom::Start(0)) {
            return io_err(e);
        }
        total = Some(count);
    }
    let Some(ranges) = selection(sel, total.unwrap_or(usize::MAX)) else {
        return ToolOutput::err(format!("Invalid selector in {disp}"));
    };
    let windows = window_ranges(&ranges);
    let mut picker = Picker::new(&windows, raw_mode);
    let mut seen_lines = 0;
    let reached_end = match each_line(&mut file, enc, |n, line| {
        seen_lines = n;
        picker.feed(n, line)
    }) {
        Ok(reached) => reached,
        Err(e) => return io_err(e),
    };
    let total = total.or(reached_end.then_some(seen_lines));
    if let Some(t) = total
        && ranges.iter().all(|r| r.0 > t)
    {
        return ToolOutput::err(format!("{disp} has only {t} lines"));
    }
    let (out, _) = render(&head_line, disp, sel, raw_mode, &ranges, picker.finish(), total);
    ToolOutput {
        meta: Some(ToolMeta::File(FileMeta { path: disp.to_string(), tag: None, plan: false })),
        ..ToolOutput::ok(out)
    }
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

    /// A `selector`/`offset`/`limit` key and the documented inline form mean the same thing: models
    /// pass either, and a silently dropped selector used to return a whole file.
    #[test]
    fn argument_style_selectors_fold_into_the_path() {
        let path = |v: serde_json::Value| -> Result<String, String> {
            serde_json::from_value::<Args>(v).map(|a| a.path).map_err(|e| e.to_string())
        };
        let ok = |v: serde_json::Value| path(v).unwrap();
        assert_eq!(ok(serde_json::json!({"path":"a.rs","selector":":10-20"})), "a.rs:10-20");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","selector":"10"})), "a.rs:10");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","selector":"3,5-7","raw":true})), "a.rs:3,5-7:raw");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","offset":5,"limit":10})), "a.rs:5+10");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","offset":0,"limit":1})), "a.rs:1+1");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","limit":10})), "a.rs:1+10");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","offset":40})), "a.rs:40");
        assert_eq!(ok(serde_json::json!({"path":"a.rs:1-5","selector":"9-10"})), "a.rs:1-5", "the inline form wins");
        assert_eq!(ok(serde_json::json!({"path":"a.rs","raw":true})), "a.rs:raw");
        let e = path(serde_json::json!({"path":"a.rs","selector":":285:345"})).unwrap_err();
        assert!(e.contains("invalid selector"), "{e}");
        assert!(path(serde_json::json!({"path":"a.rs","limit":0})).is_err());
        let e = path(serde_json::json!({"path":"a.rs","lines":"1-5"})).unwrap_err();
        assert!(e.contains("unknown field"), "{e}");
    }

    /// omp's loop breaker: the third identical read of one path and selector says so.
    #[tokio::test(flavor = "current_thread")]
    async fn a_third_identical_read_is_flagged() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("p.txt"), "a\nb\nc\n").unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "p.txt".into() }).await;
        assert!(!o.text.contains("identical output"), "{}", o.text);
        let o = t.read(Args { path: "p.txt".into() }).await;
        assert!(!o.text.contains("identical output"), "{}", o.text);
        let o = t.read(Args { path: "p.txt".into() }).await;
        assert!(o.text.contains("identical output 3 times"), "{}", o.text);
        // A different range is a different read, and a changed file starts the count over.
        let o = t.read(Args { path: "p.txt:1-2".into() }).await;
        assert!(!o.text.contains("identical output"), "{}", o.text);
        std::fs::write(d.join("p.txt"), "a\nb\nc\nd\n").unwrap();
        let o = t.read(Args { path: "p.txt".into() }).await;
        assert!(!o.text.contains("identical output"), "{}", o.text);
    }

    /// `path:N:M` is a selector error, not a missing file: the old message sent the model looking
    /// for a file named `x.txt:1:2`.
    #[tokio::test(flavor = "current_thread")]
    async fn a_two_number_selector_names_the_selector() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("p.txt"), "a\nb\nc\n").unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "p.txt:1:2".into() }).await;
        assert!(o.is_error);
        assert!(o.text.contains("Invalid selector `:1:2` in p.txt"), "{}", o.text);
        let o = t.read(Args { path: "gone.txt:1:2".into() }).await;
        assert!(o.text.contains("File not found: gone.txt:1:2"), "{}", o.text);
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
        let o = t.edit(super::super::edit::Args::for_test(input.clone()), false).await;
        assert!(o.is_error, "{}", o.text);
        assert!(o.text.contains(":raw"), "{}", o.text);
        for p in ["l.md:2:raw", "l.md:2-2:raw", "l.md:raw"] {
            let o = t.read(Args { path: p.into() }).await;
            assert!(!o.is_error, "{p}: {}", o.text);
            assert!(o.text.contains(&format!("2:{long}")), "{p}");
        }
        // Same content, same tag: the raw read made the line seen.
        let o = t.edit(super::super::edit::Args::for_test(input), false).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(std::fs::read_to_string(d.join("l.md")).unwrap(), "a\nshort\nb\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_edit_reports_the_text_of_the_file_before_and_after() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "a\nb\n").unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "f.txt".into() }).await;
        let tag = o.text[o.text.find('#').unwrap() + 1..o.text.find(']').unwrap()].to_string();
        let input = format!("[f.txt#{tag}]\nPUT 2.=2:\n+B\n");
        let o = t.edit(super::super::edit::Args::for_test(input), false).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(o.changes.len(), 1);
        let c = &o.changes[0];
        assert!(c.path.ends_with("f.txt") && c.path.is_absolute(), "{:?}", c.path);
        assert_eq!((c.old.as_deref(), c.new.as_deref()), (Some("a\nb\n"), Some("a\nB\n")));
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
        let o = t.edit(super::super::edit::Args::for_test(input.clone()), false).await;
        assert!(o.is_error, "{}", o.text);
        let from = o.text.find("Read `").expect(&o.text) + "Read `".len();
        let path = &o.text[from..from + o.text[from..].find('`').unwrap()];
        assert!(path.ends_with(":raw") && path.contains(','), "{path}");
        let o = t.read(Args { path: path.to_string() }).await;
        assert!(!o.is_error, "{path}: {}", o.text);
        assert!(o.text.contains(&format!("2:{l1}\n")) && o.text.contains(&format!("6:{l2}")), "{}", o.text);
        assert!(o.text.contains("\n...\n"), "{}", o.text);
        let o = t.edit(super::super::edit::Args::for_test(input), false).await;
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
        let o = t
            .edit(super::super::edit::Args::for_test(format!("[c.txt#{tag0}]\nPUT <1:\n+head a\n+head b\n")), false)
            .await;
        assert!(!o.is_error, "{}", o.text);
        let before = std::fs::read_to_string(d.join("c.txt")).unwrap();
        let stale = format!("[c.txt#{tag0}]\nPUT 1.=1:\n+NOTE: {}\n", &wide[6..768]);
        let e = t.edit(super::super::edit::Args::for_test(stale), false).await;
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
        let o =
            t.edit(super::super::edit::Args::for_test(format!("[c.txt#{live}]\nPUT 3.=3:\n+{full}\n")), false).await;
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
        let e = t.edit(super::super::edit::Args::for_test(input), false).await;
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

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_fifo_is_refused_without_being_opened() {
        use std::os::unix::ffi::OsStrExt;
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let fifo = d.join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that lives across the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = tokio::time::timeout(std::time::Duration::from_secs(5), t.read(Args { path: "pipe".into() }))
            .await
            .expect("read blocked on the FIFO");
        assert!(o.is_error, "{}", o.text);
        assert_eq!(o.text, "Not a regular file: pipe (a FIFO, socket or device)");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_large_file_shows_only_the_selected_lines() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let body: String = (1..=120_000).map(|i| format!("row {i:08} {}\n", "y".repeat(40))).collect();
        assert!(body.len() > MAX_SNAPSHOT_FILE_BYTES as usize);
        let header = format!(
            "[big.txt]\n(large file: {} bytes; only the selected lines were read; edit anchors are not available)\n",
            body.len()
        );
        std::fs::write(d.join("big.txt"), body).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();

        let o = t.read(Args { path: "big.txt:100-102".into() }).await;
        assert!(!o.is_error, "{}", o.text);
        // `N-M` keeps its context lines, as it does for a small file: 99 to 105.
        assert!(o.text.starts_with(&format!("{header}99:row 00000099 ")), "{}", o.text);
        assert!(o.text.contains("\n105:row 00000105 ") && !o.text.contains("106:row"), "{}", o.text);
        assert!(o.text.contains("[more lines in file. Use big.txt:106 to continue]"), "{}", o.text);
        assert!(!o.text.contains("#"), "no edit tag for a large file");
        assert!(matches!(&o.meta, Some(ToolMeta::File(f)) if f.tag.is_none()));

        let o = t.read(Args { path: "big.txt:100,101,102".into() }).await;
        assert!(o.text.starts_with(&format!("{header}100:row 00000100 ")), "{}", o.text);
        assert!(o.text.contains("\n102:row 00000102 "), "{}", o.text.len());
        assert!(!o.text.contains("99:row") && !o.text.contains("103:row"), "{}", o.text);

        let o = t.read(Args { path: "big.txt:-2".into() }).await;
        assert!(o.text.starts_with(&format!("{header}119999:row 00119999 ")), "{}", o.text);
        assert!(o.text.contains("\n120000:row 00120000 "), "{}", o.text);
        assert!(!o.text.contains("more lines"), "{}", o.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn utf16_with_a_bom_reads_as_text() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let mut bytes = vec![0xFF, 0xFE];
        for u in "héllo\nworld\n".encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        std::fs::write(d.join("u16.txt"), bytes).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "u16.txt".into() }).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(
            o.text,
            "[u16.txt] (decoded from UTF-16LE; read-only view, edit anchors not available)\n1:héllo\n2:world"
        );
        assert!(matches!(&o.meta, Some(ToolMeta::File(f)) if f.tag.is_none()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_utf8_text_reads_as_windows_1252() {
        let d = std::env::temp_dir().join(format!("mu-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("latin.txt"), b"caf\xE9\n").unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.read(Args { path: "latin.txt".into() }).await;
        assert!(!o.is_error, "{}", o.text);
        assert_eq!(
            o.text,
            "[latin.txt] (decoded from windows-1252; read-only view, edit anchors not available)\n1:café"
        );
    }
}
