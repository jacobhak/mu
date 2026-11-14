//! Output that is too long for the model: `Collector` keeps a streamed command's head and tail and
//! spills the middle to an `artifact://` file, and `cap_text` does the same for a finished text.
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const HEAD: usize = 8 * 1024;
pub const TAIL: usize = 16 * 1024;
/// A streamed command's spill file stops growing here.
const SPILL_CAP: u64 = 128 * 1024 * 1024;
/// Plugin output above this keeps its head in `cap_text`.
const TEXT_CAP: usize = 50 * 1024;

/// Allocates the next artifact: its number and path.
pub type Alloc<'a> = &'a dyn Fn() -> (u32, PathBuf);

/// Text held back from a spill until a newline (or this much) arrives, so a secret split across
/// chunks is seen whole by redaction.
const PENDING_MAX: usize = 64 * 1024;

/// An artifact being written. Text is redacted on the way in, and at most `cap` bytes are kept.
struct Spill {
    id: u32,
    file: BufWriter<File>,
    cwd: PathBuf,
    cap: u64,
    written: u64,
    capped: bool,
    /// The first I/O error; nothing is written after it.
    error: Option<String>,
    /// Text after the last newline seen, not yet redacted or written.
    pending: String,
}

impl Spill {
    /// Creates the artifact that `alloc` names, making its directory first.
    fn open(alloc: Alloc, cwd: &Path, cap: u64) -> io::Result<Self> {
        let (id, path) = alloc();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = BufWriter::new(File::create(&path)?);
        Ok(Self {
            id,
            file,
            cwd: cwd.to_path_buf(),
            cap,
            written: 0,
            capped: false,
            error: None,
            pending: String::new(),
        })
    }

    /// Holds `text` back up to its last newline (or up to `PENDING_MAX` bytes without one).
    fn write(&mut self, text: &str) {
        if self.capped || self.error.is_some() {
            return;
        }
        self.pending.push_str(text);
        let cut = match self.pending.rfind('\n') {
            Some(i) => i + 1,
            None if self.pending.len() > PENDING_MAX => self.pending.len(),
            None => return,
        };
        let rest = self.pending.split_off(cut);
        let chunk = std::mem::replace(&mut self.pending, rest);
        self.emit(&chunk);
    }

    fn emit(&mut self, text: &str) {
        if self.capped || self.error.is_some() {
            return;
        }
        let redacted = mu_base::secrets::redact(text, &self.cwd).text;
        let bytes = redacted.as_bytes();
        let room = self.cap.saturating_sub(self.written) as usize;
        let mut cut = bytes.len().min(room);
        // Never end the spill in the middle of a character.
        while cut > 0 && cut < bytes.len() && (bytes[cut] & 0xC0) == 0x80 {
            cut -= 1;
        }
        match self.file.write_all(&bytes[..cut]) {
            Ok(()) => self.written += cut as u64,
            Err(e) => self.error = Some(e.to_string()),
        }
        if cut < bytes.len() {
            self.capped = true;
            if let Err(e) = self.file.write_all(b"\n[spill truncated at 128 MB]\n") {
                self.error.get_or_insert(e.to_string());
            }
        }
    }

    fn flush(&mut self) {
        let rest = std::mem::take(&mut self.pending);
        self.emit(&rest);
        if let Err(e) = self.file.flush() {
            self.error.get_or_insert(e.to_string());
        }
    }
}

#[derive(Default)]
pub struct Collector {
    head: String,
    tail: String,
    elided: usize,
    /// Count of `\n` in everything pushed.
    newlines: usize,
    /// The last non-empty push did not end with `\n`.
    ends_open: bool,
    /// The head has reached `HEAD` bytes and is no longer filled.
    head_done: bool,
    /// The tail's cut fell inside a line.
    tail_mid_line: bool,
    /// Full output spill, opened when the first byte is elided.
    spill: Option<Spill>,
    /// Why the last attempt to open the spill failed; a later elision tries again.
    open_error: Option<String>,
    cwd: PathBuf,
}

/// What a command printed: the output itself, mu's notes about it (appended for the model), and
/// the facts the UI shows instead of parsing those notes.
pub struct Collected {
    pub body: String,
    pub notes: String,
    pub artifact: Option<u32>,
    pub spill_note: Option<String>,
}

impl Collector {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, ..Default::default() }
    }

    pub fn push(&mut self, mut s: &str, alloc: Alloc) {
        self.newlines += s.bytes().filter(|b| *b == b'\n').count();
        if !s.is_empty() {
            self.ends_open = !s.ends_with('\n');
        }
        if let Some(sp) = self.spill.as_mut() {
            sp.write(s);
        }
        if !self.head_done && self.elided == 0 && self.tail.is_empty() && self.head.len() < HEAD {
            let mut n = (HEAD - self.head.len()).min(s.len());
            while !s.is_char_boundary(n) {
                n += 1;
            }
            self.head.push_str(&s[..n]);
            s = &s[n..];
            if self.head.len() >= HEAD {
                self.head_done = true;
                // Keep the head on a line boundary: a partial last line moves to the tail.
                if let Some(i) = self.head.rfind('\n') {
                    let rest = self.head.split_off(i + 1);
                    self.tail.push_str(&rest);
                }
            }
        }
        self.tail.push_str(s);
        if self.tail.len() > TAIL {
            let mut cut = self.tail.len() - TAIL;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            // Prefer a cut after a newline, so no line is shown twice; the last byte is left
            // alone so at least one line survives.
            let limit = self.tail.len() - 1;
            match self.tail.as_bytes()[cut..].iter().position(|b| *b == b'\n') {
                Some(i) if cut + i < limit => {
                    cut += i + 1;
                    self.tail_mid_line = false;
                }
                _ => self.tail_mid_line = true,
            }
            if self.spill.is_none() {
                match Spill::open(alloc, &self.cwd, SPILL_CAP) {
                    Ok(mut sp) => {
                        sp.write(&self.head);
                        sp.write(&self.tail);
                        self.spill = Some(sp);
                    }
                    Err(e) => self.open_error = Some(e.to_string()),
                }
            }
            self.elided += cut;
            self.tail.drain(..cut);
        }
    }

    pub fn finish(mut self) -> Collected {
        // The shown line ranges, when both cuts fell on line boundaries.
        let mut ranges = None;
        let body = if self.elided > 0 {
            let head_lines = self.head.matches('\n').count();
            let total = self.newlines + usize::from(self.ends_open);
            let tail_lines =
                self.tail.matches('\n').count() + usize::from(!self.tail.is_empty() && !self.tail.ends_with('\n'));
            let tail_start = total - tail_lines + 1;
            if self.head.ends_with('\n') && !self.tail_mid_line {
                let n = tail_start - head_lines - 1;
                ranges = Some((head_lines, tail_start, total));
                format!("{}[…{n} lines elided…]\n{}", self.head, self.tail)
            } else {
                format!("{}\n[…{}B elided…]\n{}", self.head, self.elided, self.tail)
            }
        } else {
            std::mem::take(&mut self.head) + &self.tail
        };
        let mut notes = String::new();
        let mut artifact = None;
        let mut spill_note = None;
        let mut error = self.open_error.take();
        if let Some(mut sp) = self.spill.take() {
            sp.flush();
            match ranges {
                Some((head_lines, tail_start, total)) => notes.push_str(&format!(
                    "\n[Showing lines 1-{head_lines} and {tail_start}-{total} of {total}; full output: artifact://{}]",
                    sp.id
                )),
                None => notes.push_str(&format!("\n[full output: artifact://{}]", sp.id)),
            }
            if sp.capped {
                notes.push_str(" (truncated at 128 MB)");
                spill_note = Some("spill truncated at 128 MB".to_string());
            }
            artifact = Some(sp.id);
            error = sp.error.or(error);
        } else if let Some((head_lines, tail_start, total)) = ranges {
            notes.push_str(&format!("\n[Showing lines 1-{head_lines} and {tail_start}-{total} of {total}]"));
        }
        if let Some(e) = error {
            notes.push_str(&format!("\n[could not save the full output: {e}]"));
            spill_note = Some(format!("could not save the full output: {e}"));
        }
        Collected { body, notes, artifact, spill_note }
    }
}

/// Plugin output above 50 KB keeps its head; the whole text goes to an `artifact://` file.
pub fn cap_text(text: String, cwd: &Path, alloc: Alloc) -> String {
    if text.len() <= TEXT_CAP {
        return text;
    }
    let mut cut = TEXT_CAP;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let saved = Spill::open(alloc, cwd, u64::MAX).ok().and_then(|mut sp| {
        sp.write(&text);
        sp.flush();
        sp.error.is_none().then_some(sp.id)
    });
    let (kb, total) = (TEXT_CAP / 1024, text.len() / 1024);
    let note = match saved {
        Some(n) => format!("[Output truncated at {kb} KB of {total} KB; full output: artifact://{n}]"),
        None => format!("[Output truncated at {kb} KB of {total} KB]"),
    };
    format!("{}\n\n{note}", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_split_across_chunks_never_reaches_the_artifact() {
        mu_base::secrets::register_stored("SPILL_TEST_KEY", "Zq8-spill-test-value");
        let d = std::env::temp_dir().join(format!("mu-spill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("out.log");
        let artifact = path.clone();
        let alloc = move || (7, artifact.clone());
        let mut sp = Spill::open(&alloc, &d, u64::MAX).unwrap();
        sp.write("before Zq8-spill-");
        sp.write("test-value after\nnext line\ntrailing");
        sp.flush();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Zq8-spill"), "{text}");
        assert!(text.contains("[REDACTED:SPILL_TEST_KEY] after\nnext line\n"), "{text}");
        assert!(text.ends_with("trailing"), "the held-back tail is written at flush: {text}");
    }

    fn collector_in_temp() -> (Collector, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-spill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        (Collector::new(d.clone()), d)
    }

    fn is_line(s: &str) -> bool {
        s.strip_prefix("line ").is_some_and(|n| n.len() == 5 && n.bytes().all(|b| b.is_ascii_digit()))
    }

    fn is_marker(s: &str) -> bool {
        s.strip_prefix("[…")
            .and_then(|r| r.strip_suffix(" lines elided…]"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }

    /// The `Showing lines 1-h and t-total of total` numbers.
    fn shown(notes: &str) -> (usize, usize, usize) {
        let rest = notes.split("Showing lines 1-").nth(1).expect("no line ranges");
        let nums: Vec<usize> =
            rest.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap()).collect();
        (nums[0], nums[1], nums[2])
    }

    #[test]
    fn a_long_stream_keeps_whole_lines_and_names_the_ranges() {
        let (mut c, d) = collector_in_temp();
        let alloc = move || (7, d.join("out.log"));
        let all: String = (1..=20000).map(|i| format!("line {i:05}\n")).collect();
        // 7-byte chunks split lines, so a cut can land mid-line.
        for chunk in all.as_bytes().chunks(7) {
            c.push(std::str::from_utf8(chunk).unwrap(), &alloc);
        }
        let out = c.finish();
        let lines: Vec<&str> = out.body.lines().collect();
        let marker = lines.iter().position(|l| l.starts_with("[…")).expect("no elision marker");
        assert!(is_marker(lines[marker]), "{}", lines[marker]);
        let elided: usize = lines[marker].trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap();
        for (i, l) in lines.iter().enumerate() {
            assert!(i == marker || is_line(l), "line {i} is not whole: {l:?}");
        }
        let (h, t, total) = shown(&out.notes);
        assert_eq!(total, 20000);
        assert_eq!(lines[h - 1], format!("line {h:05}"));
        assert_eq!(lines[h + 1], format!("line {t:05}"));
        assert_eq!(elided, t - h - 1);
        assert_eq!(lines.len(), 20001 - elided);
    }

    #[test]
    fn an_open_last_line_counts_as_its_own_line() {
        let (mut c, d) = collector_in_temp();
        let alloc = move || (7, d.join("out.log"));
        let all: String = (1..=20000).map(|i| format!("line {i:05}\n")).collect();
        for chunk in all.as_bytes().chunks(7) {
            c.push(std::str::from_utf8(chunk).unwrap(), &alloc);
        }
        c.push("end", &alloc);
        let out = c.finish();
        let (_, _, total) = shown(&out.notes);
        assert_eq!(total, 20001);
        assert!(out.body.ends_with("end"), "{}", &out.body[out.body.len() - 40..]);
    }

    #[test]
    fn a_single_line_without_a_newline_falls_back_to_bytes() {
        let (mut c, d) = collector_in_temp();
        let alloc = move || (7, d.join("out.log"));
        c.push(&"x".repeat(100 * 1024), &alloc);
        let out = c.finish();
        assert!(out.body.contains("B elided…]"), "{}", &out.body[..60]);
        assert!(!out.notes.contains("Showing lines"), "{}", out.notes);
    }
}
