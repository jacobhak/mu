//! Runs mu in a pseudo-terminal against the fake server and samples memory after each turn.
use crate::scenario::{Scenario, gen_text};
use crate::server::{self, Kind, Server};
use crate::smaps::{self, Mem};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Out {
    buf: Vec<u8>,
    /// When each read returned, with the buffer length after it.
    reads: Vec<(Instant, usize)>,
}

pub struct Term {
    writer: File,
    out: Arc<Mutex<Out>>,
    child: Child,
    pub started: Instant,
}

const COLS: u16 = 150;
const ROWS: u16 = 40;

impl Term {
    pub fn spawn(program: &Path, args: &[&str], cwd: &Path, env: &[(&str, String)], remove: &[&str]) -> Result<Term> {
        let (mut m, mut s) = (-1 as libc::c_int, -1 as libc::c_int);
        let ws = libc::winsize { ws_row: ROWS, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: valid out-pointers, null name and termios. macOS declares the winsize pointer `*mut`
        // and Linux `*const`; it is only read through.
        let r = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of!(ws).cast_mut().cast(),
            )
        };
        if r < 0 {
            bail!("openpty: {}", std::io::Error::last_os_error());
        }
        // SAFETY: openpty returned two fresh descriptors that nothing else owns.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(cwd).env("TERM", "xterm-256color");
        for (k, v) in env {
            cmd.env(k, v);
        }
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls (setsid, ioctl) between fork and exec.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let child = cmd.spawn().with_context(|| format!("starting {}", program.display()))?;
        drop(cmd); // closes this process's copies of the slave
        let master = File::from(master);
        let (mut reader, mut replier, writer) = (master.try_clone()?, master.try_clone()?, master);
        let out = Arc::new(Mutex::new(Out::default()));
        let o = out.clone();
        std::thread::spawn(move || {
            let mut tmp = [0u8; 16384];
            while let Ok(n) = reader.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                // A real terminal answers these; without a reply mu waits for its probe to time out.
                let reply = answer_queries(&tmp[..n]);
                if !reply.is_empty() {
                    let _ = replier.write_all(&reply);
                }
                let mut o = o.lock().unwrap();
                o.buf.extend_from_slice(&tmp[..n]);
                let len = o.buf.len();
                o.reads.push((Instant::now(), len));
            }
        });
        Ok(Term { writer, out, child, started })
    }

    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Types `text`, then Enter after a pause so the burst is not taken as one paste.
    pub async fn send(&mut self, text: &str) -> Result<()> {
        self.writer.write_all(text.as_bytes())?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.writer.write_all(b"\r")?;
        Ok(())
    }

    /// When the first read that completed `needle` in the output returned.
    pub fn seen_at(&self, needle: &str) -> Option<Instant> {
        let o = self.out.lock().unwrap();
        let n = needle.as_bytes();
        let pos = o.buf.windows(n.len()).position(|w| w == n)?;
        o.reads.iter().find(|(_, end)| *end >= pos + n.len()).map(|(t, _)| *t)
    }

    pub async fn wait_for(&self, needle: &str, timeout: Duration) -> Result<Instant> {
        let end = Instant::now() + timeout;
        loop {
            if let Some(t) = self.seen_at(needle) {
                return Ok(t);
            }
            if Instant::now() >= end {
                let o = self.out.lock().unwrap();
                let tail = String::from_utf8_lossy(&o.buf[o.buf.len().saturating_sub(400)..]).into_owned();
                bail!("timed out after {timeout:?} waiting for {needle:?} on the terminal; last output: {tail:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn first_byte(&self) -> Option<Instant> {
        self.out.lock().unwrap().reads.first().map(|r| r.0)
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub async fn shutdown(mut self) {
        let pid = self.pid();
        // SAFETY: plain kill(2) on the process group of a child this struct owns and has not reaped.
        unsafe { libc::kill(pid, libc::SIGHUP) };
        for _ in 0..50 {
            if !self.alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // SAFETY: as above; stragglers (plugins, shells) share the session.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub label: String,
    /// Conversation content so far: prompts, model text and thinking, tool output (bytes).
    pub content_bytes: u64,
    pub mu: Mem,
    pub tree: Mem,
    pub procs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub startup_first_byte_ms: Option<f64>,
    pub startup_prompt_ms: f64,
    pub samples: Vec<Sample>,
    /// Per turn: from the server writing the first text delta to it appearing on the terminal.
    pub first_token_ms: Vec<Option<f64>>,
    /// Per turn: from sending the prompt to the server finishing the last reply.
    pub turn_ms: Vec<f64>,
    pub tool_errors: usize,
    pub overruns: usize,
    pub aux_requests: usize,
    /// Requests to routes the fake server does not serve (method and path).
    pub unrouted: Vec<String>,
}

pub struct Opts {
    pub mu: PathBuf,
    pub settle: Duration,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn scratch() -> Result<PathBuf> {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let d = std::env::temp_dir().join(format!("mu-bench-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

pub fn make_files(sc: &Scenario, work: &Path) -> Result<()> {
    for (i, f) in sc.files.iter().enumerate() {
        let p = work.join(&f.path);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, gen_text(1000 + i as u64, f.bytes))?;
    }
    Ok(())
}

/// A day-old-or-less price catalogue in mu's cache, as a normal session has. Without it mu starts
/// `omp models ls` to build one, and that 170 MB process lands in the first samples.
fn seed_caches(mu_home: &Path) -> Result<()> {
    std::fs::create_dir_all(mu_home)?;
    let cat = serde_json::json!({"models":[{"provider":"anthropic","id":"bench-1","name":"Bench 1","pricingStatus":"fixed",
        "reasoning":false,"cost":{"input":3.0,"output":15.0,"cacheRead":0.3,"cacheWrite":3.75}}]});
    std::fs::write(mu_home.join("pricing.json"), cat.to_string())?;
    Ok(())
}

fn take_sample(label: String, pid: i32, content: u64) -> Result<Sample> {
    let procs = smaps::sample_tree(pid);
    let Some(root) = procs.iter().find(|p| p.pid == pid) else { bail!("mu (pid {pid}) is not running") };
    Ok(Sample {
        label,
        content_bytes: content,
        mu: root.mem,
        tree: smaps::total(&procs),
        procs: procs.iter().map(|p| p.comm.clone()).collect(),
    })
}

/// Content that the conversation holds once `turns_done` turns are over.
fn content_bytes(sc: &Scenario, server: &Server, turns_done: usize) -> u64 {
    let log = server.log.lock().unwrap();
    let mut n: u64 = sc.turns[..turns_done].iter().map(|t| t.prompt.len() as u64).sum();
    for r in log.requests.iter().filter(|r| r.kind == Kind::Main) {
        if r.turn.is_some_and(|t| t < turns_done) {
            let s = &sc.turns[r.turn.unwrap()].steps[r.step.unwrap()];
            n += (s.thinking_bytes + s.text_bytes) as u64 + r.tool_result_bytes as u64;
        }
    }
    n
}

pub async fn run_once(sc: &Arc<Scenario>, o: &Opts) -> Result<RunResult> {
    let tmp = scratch()?;
    let r = run_in(sc, o, &tmp).await;
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

async fn run_in(sc: &Arc<Scenario>, o: &Opts, tmp: &Path) -> Result<RunResult> {
    let (work, home) = (tmp.join("work"), tmp.join("home"));
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(home.join("agent"))?;
    make_files(sc, &work)?;
    seed_caches(&tmp.join("mu"))?;
    let srv = Server::start(sc.clone()).await?;
    let env = [
        ("HOME", home.display().to_string()),
        ("MU_HOME", tmp.join("mu").display().to_string()),
        ("PI_CODING_AGENT_DIR", home.join("agent").display().to_string()),
        ("MU_GATEWAY_URL", srv.url()),
        ("MU_GATEWAY_TOKEN", "bench".into()),
    ];
    let mut term = Term::spawn(
        &o.mu,
        &["--model", server::MODEL],
        &work,
        &env,
        &["OMP_PROFILE", "MU_PROFILE", "MU_DUMP_REQUESTS"],
    )?;
    let pid = term.pid();
    let res = drive(sc, o, &mut term, &srv, pid).await;
    term.shutdown().await;
    res
}

async fn drive(sc: &Scenario, o: &Opts, term: &mut Term, srv: &Server, pid: i32) -> Result<RunResult> {
    let prompt_at = term.wait_for("μ", Duration::from_secs(30)).await?;
    let startup_prompt_ms = ms(prompt_at - term.started);
    let startup_first_byte_ms = term.first_byte().map(|t| ms(t - term.started));
    tokio::time::sleep(o.settle).await;
    let mut samples = vec![take_sample("start".into(), pid, 0)?];
    let (mut first_token_ms, mut turn_ms) = (Vec::new(), Vec::new());
    for (i, turn) in sc.turns.iter().enumerate() {
        let sent = Instant::now();
        term.send(&turn.prompt).await?;
        srv.wait_turns(i + 1, Duration::from_secs(180)).await?;
        if !term.alive() {
            bail!("mu exited during turn {}", i + 1);
        }
        let (token, done) = {
            let log = srv.log.lock().unwrap();
            let marked = log.requests.iter().find(|r| r.turn == Some(i) && r.marker.is_some());
            let done = log.requests.iter().filter(|r| r.turn == Some(i)).filter_map(|r| r.finished).max();
            (marked.and_then(|r| Some((r.first_text?, r.marker.clone()?))), done)
        };
        first_token_ms.push(match token {
            Some((at, marker)) => {
                // The text may not have been drawn yet when the server is done; give the terminal a moment.
                let seen =
                    term.wait_for(&marker, Duration::from_secs(5)).await.ok().and_then(|_| term.seen_at(&marker));
                seen.map(|t| ms(t.saturating_duration_since(at)))
            }
            None => None,
        });
        turn_ms.push(done.map_or(0.0, |d| ms(d.saturating_duration_since(sent))));
        tokio::time::sleep(o.settle).await;
        let content = content_bytes(sc, srv, i + 1);
        samples.push(take_sample(format!("turn {}", i + 1), pid, content)?);
    }
    let log = srv.log.lock().unwrap();
    Ok(RunResult {
        startup_first_byte_ms,
        startup_prompt_ms,
        samples,
        first_token_ms,
        turn_ms,
        tool_errors: log.requests.iter().map(|r| r.tool_errors).sum(),
        overruns: log.overruns,
        aux_requests: log.requests.iter().filter(|r| r.kind == Kind::Aux).count(),
        unrouted: log.unrouted.clone(),
    })
}

/// What a terminal sends back for the queries in `out`: colours (OSC 10, 11, 17, 4), device attributes
/// and the cursor position.
pub fn answer_queries(out: &[u8]) -> Vec<u8> {
    let colour = |code: &str| match code {
        "10" => Some("c0c0/caca/f5f5"),
        "11" => Some("1a1a/1b1b/2626"),
        "17" => Some("2a2a/2e2e/3737"),
        c if c.starts_with("4;") => Some("8080/8080/8080"),
        _ => None,
    };
    let mut reply = Vec::new();
    let mut i = 0;
    while i < out.len() {
        let rest = &out[i..];
        if rest.starts_with(b"\x1b[6n") {
            reply.extend_from_slice(b"\x1b[1;1R");
            i += 4;
        } else if rest.starts_with(b"\x1b[c") {
            reply.extend_from_slice(b"\x1b[?62;c");
            i += 3;
        } else if rest.starts_with(b"\x1b]") {
            let body = &rest[2..];
            let bel = body.iter().position(|b| *b == 7);
            let st = body.windows(2).position(|w| w == b"\x1b\\");
            let (len, term) = match (bel, st) {
                (Some(a), Some(b)) if a < b => (a, 1),
                (_, Some(b)) => (b, 2),
                (Some(a), None) => (a, 1),
                (None, None) => break,
            };
            let payload = std::str::from_utf8(&body[..len]).unwrap_or("");
            if let Some(c) = payload.strip_suffix(";?").and_then(|code| colour(code).map(|c| (code, c))) {
                reply.extend_from_slice(format!("\x1b]{};rgb:{}\x1b\\", c.0, c.1).as_bytes());
            }
            i += 2 + len + term;
        } else {
            i += 1;
        }
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_mus_startup_probe_in_order() {
        let mut q = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]17;?\x1b\\");
        for i in 0..16 {
            q.push_str(&format!("\x1b]4;{i};?\x1b\\"));
        }
        q.push_str("\x1b[c");
        let r = String::from_utf8(answer_queries(q.as_bytes())).unwrap();
        assert!(r.starts_with("\x1b]10;rgb:c0c0/caca/f5f5\x1b\\\x1b]11;rgb:1a1a/1b1b/2626\x1b\\\x1b]17;rgb:"));
        assert_eq!(r.matches("\x1b]4;").count(), 16);
        assert!(r.contains("\x1b]4;15;rgb:") && r.ends_with("\x1b[?62;c"));
    }

    #[test]
    fn ignores_output_that_is_not_a_query() {
        assert!(answer_queries(b"plain \x1b[31mred\x1b[0m \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\").is_empty());
        assert_eq!(answer_queries(b"a\x1b[6nb"), b"\x1b[1;1R");
        assert!(answer_queries(b"\x1b]10;?").is_empty());
        assert_eq!(answer_queries(b"\x1b]11;?\x07"), b"\x1b]11;rgb:1a1a/1b1b/2626\x1b\\");
    }

    #[test]
    fn scenario_files_are_laid_out_exactly() {
        let sc = Scenario::parse(
            "name = \"t\"\n[[files]]\npath = \"a/b.txt\"\nbytes = 123\n[[turn]]\nprompt = \"p\"\n[[turn.step]]\ntext_bytes = 20\n",
        )
        .unwrap();
        let dir = scratch().unwrap();
        make_files(&sc, &dir).unwrap();
        assert_eq!(std::fs::metadata(dir.join("a/b.txt")).unwrap().len(), 123);
        seed_caches(&dir.join("mu")).unwrap();
        assert!(crate::driver::tests::pricing_parses(&dir.join("mu/pricing.json")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    pub fn pricing_parses(p: &Path) -> bool {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        v["models"][0]["id"] == "bench-1"
    }
}
