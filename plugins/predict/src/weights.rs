//! SmolLM2-135M weights: the three files omp pins, checked by size at startup and by SHA-256 in `setup`.
//! Downloads go through `curl` (as `mu update` does) into `<name>.part`, then are renamed once verified.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const DIR_NAME: &str = "SmolLM2-135M";
const SETUP_CMD: &str = "`mu predict setup smollm`";

pub struct Pinned {
    pub name: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const FILES: [Pinned; 3] = [
    Pinned {
        name: "config.json",
        repo: "HuggingFaceTB/SmolLM2-135M",
        revision: "93efa2f097d58c2a74874c7e644dbc9b0cee75a2",
        size: 704,
        sha256: "1d556eab73b69c7f11f64c557a2f9c6f440bd4c6b89bb2584a6b498c92603843",
    },
    Pinned {
        name: "tokenizer.json",
        repo: "HuggingFaceTB/SmolLM2-135M",
        revision: "93efa2f097d58c2a74874c7e644dbc9b0cee75a2",
        size: 2104556,
        sha256: "9ca9acddb6525a194ec8ac7a87f24fbba7232a9a15ffa1af0c1224fcd888e47c",
    },
    Pinned {
        name: "SmolLM2-135M.Q8_0.gguf",
        repo: "QuantFactory/SmolLM2-135M-GGUF",
        revision: "d948db3614be18259a175aafd7689a70f1cb4e2f",
        size: 144810464,
        sha256: "b761d9ccdfce67726e41ca2ef30e9dfbcf6a32ca2aaef47df5c049ec362d04cd",
    },
];

impl Pinned {
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{}", self.repo, self.revision, self.name)
    }
}

pub fn model_dir(mu_home: &Path) -> PathBuf {
    mu_home.join("models").join(DIR_NAME)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Ok,
    Missing,
    WrongSize(u64),
    WrongHash,
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `full` also hashes the file; startup only compares sizes, which is enough to tell missing or partial files apart.
pub fn check_file(path: &Path, f: &Pinned, full: bool) -> Check {
    let Ok(meta) = std::fs::metadata(path) else { return Check::Missing };
    if meta.len() != f.size {
        return Check::WrongSize(meta.len());
    }
    if full && !sha256_file(path).is_ok_and(|got| got == f.sha256) {
        return Check::WrongHash;
    }
    Check::Ok
}

/// The first missing or wrong-sized file, as a message for a startup notice. Sizes only.
pub fn startup_check(dir: &Path, files: &[Pinned]) -> Result<(), String> {
    for f in files {
        match check_file(&dir.join(f.name), f, false) {
            Check::Ok => {}
            Check::Missing => return Err(format!("the SmolLM weights are missing ({}); run {SETUP_CMD}", f.name)),
            Check::WrongSize(n) => {
                return Err(format!("{} is {n} bytes, expected {}; run {SETUP_CMD}", f.name, f.size));
            }
            Check::WrongHash => return Err(format!("{} is corrupt; run {SETUP_CMD}", f.name)),
        }
    }
    Ok(())
}

fn len_or_zero(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

/// Downloads or resumes a file into its `.part` path, showing progress on the writer.
pub type Fetch<'a> = dyn Fn(&Pinned, &Path, &mut dyn Write) -> Result<(), String> + 'a;

/// Makes `dir` hold every file in `files`, each verified by SHA-256. A verified file is never fetched again;
/// a `.part` file is resumed; a corrupt or oversized one is discarded.
pub fn setup_files(dir: &Path, files: &[Pinned], fetch: &Fetch<'_>, out: &mut dyn Write) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for f in files {
        let path = dir.join(f.name);
        let part = dir.join(format!("{}.part", f.name));
        match check_file(&path, f, true) {
            Check::Ok => {
                say(out, &format!("{}: present and verified", f.name))?;
                continue;
            }
            Check::Missing => {}
            Check::WrongSize(_) | Check::WrongHash => {
                say(out, &format!("{}: corrupt, downloading it again", f.name))?;
                let _ = std::fs::remove_file(&path);
            }
        }
        let mut have = len_or_zero(&part);
        if have > f.size {
            let _ = std::fs::remove_file(&part);
            have = 0;
        }
        if have < f.size {
            fetch(f, &part, out)?;
        }
        let got = len_or_zero(&part);
        if got != f.size {
            return Err(format!("{}: downloaded {got} of {} bytes; run {SETUP_CMD} again to resume", f.name, f.size));
        }
        let sum = sha256_file(&part).map_err(|e| format!("cannot read {}: {e}", part.display()))?;
        if sum != f.sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{}: SHA-256 does not match the pinned file; the download was deleted", f.name));
        }
        std::fs::rename(&part, &path).map_err(|e| format!("cannot move {} into place: {e}", part.display()))?;
        say(out, &format!("{}: verified", f.name))?;
    }
    Ok(())
}

fn say(out: &mut dyn Write, line: &str) -> Result<(), String> {
    writeln!(out, "{line}").and_then(|_| out.flush()).map_err(|e| e.to_string())
}

/// The message for a failed `curl` run, from its exit status and stderr.
pub fn curl_error(code: Option<i32>, stderr: &str, url: &str) -> String {
    match code {
        // 6: no DNS, 7: connection refused, 28: timed out.
        Some(6 | 7 | 28) => {
            format!("could not reach huggingface.co (are you offline?); connect and run {SETUP_CMD} again, it resumes")
        }
        Some(22) => format!("{url} answered with an HTTP error"),
        Some(c) => format!("curl failed (exit {c}): {}", stderr.lines().next().unwrap_or("no details")),
        None => "curl was killed by a signal".into(),
    }
}

fn curl_fetch(f: &Pinned, part: &Path, out: &mut dyn Write) -> Result<(), String> {
    let url = f.url();
    let mut child = Command::new("curl")
        .args(["-sS", "-f", "-L", "--connect-timeout", "15", "--retry", "2", "--retry-delay", "2", "-C", "-", "-o"])
        .arg(part)
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl ({e}); install curl and try again"))?;
    // Progress redraws only on a terminal; a log or a pipe gets just the result lines.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if tty {
            let (got, total) = (len_or_zero(part), f.size);
            let pct = got * 100 / total.max(1);
            let _ = write!(out, "\r  {:<26} {:>7.1} / {:.1} MiB  {pct:>3}%", f.name, mib(got), mib(total));
            let _ = out.flush();
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if tty {
        let _ = writeln!(out);
    }
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    if status.success() { Ok(()) } else { Err(curl_error(status.code(), &err, &url)) }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// `mu-predict setup smollm [--status]`; returns the exit code.
pub fn cli(args: &[String]) -> i32 {
    let status = args.iter().any(|a| a == "--status");
    let known = args.iter().all(|a| matches!(a.as_str(), "smollm" | "--status"));
    if !known || !args.iter().any(|a| a == "smollm") {
        eprintln!("usage: mu-predict setup smollm [--status]");
        return 2;
    }
    let dir = model_dir(&mu_common::paths::mu_home());
    let mut out = std::io::stdout();
    if status {
        return report(&dir, &FILES, &mut out);
    }
    match setup_files(&dir, &FILES, &curl_fetch, &mut out) {
        Ok(()) => {
            let _ = writeln!(
                out,
                "SmolLM2-135M is ready in {}.\nTo use it, set [predict] engine = \"smollm\" in ~/.mu/config.toml.",
                dir.display()
            );
            0
        }
        Err(e) => {
            eprintln!("mu-predict setup: {e}");
            1
        }
    }
}

/// Prints each file's state; 0 when every file is present and verified.
pub fn report(dir: &Path, files: &[Pinned], out: &mut dyn Write) -> i32 {
    let _ = writeln!(out, "SmolLM2-135M in {}", dir.display());
    let mut ready = true;
    for f in files {
        let (state, ok) = match check_file(&dir.join(f.name), f, true) {
            Check::Ok => ("present and verified".to_string(), true),
            Check::Missing => ("missing".to_string(), false),
            Check::WrongSize(n) => (format!("wrong size ({n} bytes, expected {})", f.size), false),
            Check::WrongHash => ("checksum mismatch".to_string(), false),
        };
        ready &= ok;
        let _ = writeln!(out, "  {:<26} {state}", f.name);
    }
    let _ = writeln!(out, "{}", if ready { "ready".to_string() } else { format!("not ready; run {SETUP_CMD}") });
    i32::from(!ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256("hello"), so the pinned checks can run on a five-byte file.
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    const HELLO: Pinned =
        Pinned { name: "hello.bin", repo: "example/repo", revision: "main", size: 5, sha256: HELLO_SHA };

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-predict-weights-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A fetch that writes `body` to the part file, appending as a resume would.
    fn serving<'a>(
        body: &'static [u8],
        calls: &'a std::cell::Cell<u32>,
    ) -> impl Fn(&Pinned, &Path, &mut dyn Write) -> Result<(), String> + 'a {
        move |_, part, _| {
            calls.set(calls.get() + 1);
            let have = len_or_zero(part) as usize;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(part).unwrap();
            std::io::Write::write_all(&mut f, &body[have..]).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn startup_check_sees_missing_partial_and_wrong_files() {
        let d = tmp("startup");
        std::fs::create_dir_all(&d).unwrap();
        let files = [HELLO];
        assert!(startup_check(&d, &files).unwrap_err().contains("missing"));
        std::fs::write(d.join("hello.bin.part"), b"hel").unwrap();
        assert!(startup_check(&d, &files).unwrap_err().contains("missing"), "a .part is not the file");
        std::fs::write(d.join("hello.bin"), b"hel").unwrap();
        assert!(startup_check(&d, &files).unwrap_err().contains("3 bytes, expected 5"));
        std::fs::write(d.join("hello.bin"), b"jello").unwrap();
        assert_eq!(startup_check(&d, &files), Ok(()), "startup is size-only, so same-size junk passes");
        assert_eq!(check_file(&d.join("hello.bin"), &HELLO, true), Check::WrongHash);
    }

    #[test]
    fn full_check_matches_the_pinned_hash() {
        let d = tmp("hash");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.bin"), b"hello").unwrap();
        assert_eq!(check_file(&d.join("hello.bin"), &HELLO, true), Check::Ok);
        assert_eq!(check_file(&d.join("nope"), &HELLO, true), Check::Missing);
    }

    #[test]
    fn setup_downloads_verifies_and_is_idempotent() {
        let d = tmp("idem");
        let calls = std::cell::Cell::new(0);
        let fetch = serving(b"hello", &calls);
        let mut out = Vec::new();
        setup_files(&d, &[HELLO], &fetch, &mut out).unwrap();
        assert_eq!(calls.get(), 1);
        assert!(d.join("hello.bin").is_file() && !d.join("hello.bin.part").exists());
        setup_files(&d, &[HELLO], &fetch, &mut out).unwrap();
        assert_eq!(calls.get(), 1, "a verified copy is never downloaded again");
        assert!(String::from_utf8(out).unwrap().contains("present and verified"));
    }

    #[test]
    fn setup_resumes_a_partial_download() {
        let d = tmp("resume");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.bin.part"), b"he").unwrap();
        let calls = std::cell::Cell::new(0);
        setup_files(&d, &[HELLO], &serving(b"hello", &calls), &mut Vec::new()).unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(std::fs::read(d.join("hello.bin")).unwrap(), b"hello");
    }

    #[test]
    fn setup_keeps_a_short_download_for_resuming() {
        let d = tmp("short");
        let calls = std::cell::Cell::new(0);
        let err = setup_files(&d, &[HELLO], &serving(b"hel", &calls), &mut Vec::new()).unwrap_err();
        assert!(err.contains("downloaded 3 of 5") && err.contains("resume"), "{err}");
        assert_eq!(std::fs::read(d.join("hello.bin.part")).unwrap(), b"hel");
        assert!(!d.join("hello.bin").exists());
    }

    #[test]
    fn setup_refuses_and_deletes_a_wrong_download() {
        let d = tmp("wrong");
        let calls = std::cell::Cell::new(0);
        let err = setup_files(&d, &[HELLO], &serving(b"jello", &calls), &mut Vec::new()).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!d.join("hello.bin.part").exists() && !d.join("hello.bin").exists());
    }

    #[test]
    fn setup_replaces_a_corrupt_file_and_drops_an_oversized_part() {
        let d = tmp("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.bin"), b"jello").unwrap();
        std::fs::write(d.join("hello.bin.part"), b"hello!!").unwrap();
        let calls = std::cell::Cell::new(0);
        let mut out = Vec::new();
        setup_files(&d, &[HELLO], &serving(b"hello", &calls), &mut out).unwrap();
        assert_eq!(std::fs::read(d.join("hello.bin")).unwrap(), b"hello");
        assert_eq!(calls.get(), 1);
        assert!(String::from_utf8(out).unwrap().contains("corrupt, downloading it again"));
    }

    #[test]
    fn offline_is_a_clear_error() {
        let e = curl_error(Some(6), "curl: (6) Could not resolve host", "https://huggingface.co/x");
        assert!(e.contains("offline") && e.contains("resumes"), "{e}");
        assert!(curl_error(Some(22), "", "https://h/x").contains("HTTP error"));
        assert!(curl_error(Some(35), "tls broke\nmore", "u").ends_with("tls broke"));
    }

    #[test]
    fn pinned_files_are_the_omp_pins() {
        assert_eq!(FILES.iter().map(|f| f.size).sum::<u64>(), 704 + 2104556 + 144810464);
        assert!(FILES[2].url().starts_with("https://huggingface.co/QuantFactory/SmolLM2-135M-GGUF/resolve/"));
    }

    #[test]
    fn status_reports_and_exits_nonzero_until_ready() {
        let d = tmp("status");
        let mut out = Vec::new();
        assert_eq!(report(&d, &[HELLO], &mut out), 1);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.bin"), b"hello").unwrap();
        assert_eq!(report(&d, &[HELLO], &mut out), 0);
        assert!(String::from_utf8(out).unwrap().ends_with("ready\n"));
    }
}
