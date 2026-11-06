//! Output that is too long for the model: `Collector` keeps a streamed command's head and tail and
//! spills the middle to an `artifact://` file, and `cap_text` does the same for a finished text.
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

const HEAD: usize = 20 * 1024;
const TAIL: usize = 30 * 1024;
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
        if let Some(sp) = self.spill.as_mut() {
            sp.write(s);
        }
        if self.tail.is_empty() && self.elided == 0 && self.head.len() < HEAD {
            let mut n = (HEAD - self.head.len()).min(s.len());
            while !s.is_char_boundary(n) {
                n += 1;
            }
            self.head.push_str(&s[..n]);
            s = &s[n..];
        }
        self.tail.push_str(s);
        if self.tail.len() > TAIL {
            let mut cut = self.tail.len() - TAIL;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
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
        let body = if self.elided > 0 {
            format!("{}\n[…{}B elided…]\n{}", self.head, self.elided, self.tail)
        } else {
            std::mem::take(&mut self.head) + &self.tail
        };
        let mut notes = String::new();
        let mut artifact = None;
        let mut spill_note = None;
        let mut error = self.open_error.take();
        if let Some(mut sp) = self.spill.take() {
            sp.flush();
            notes.push_str(&format!("\n[full output: artifact://{}]", sp.id));
            if sp.capped {
                notes.push_str(" (truncated at 128 MB)");
                spill_note = Some("spill truncated at 128 MB".to_string());
            }
            artifact = Some(sp.id);
            error = sp.error.or(error);
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
}
