use super::{BashMeta, ToolCtx, ToolMeta, ToolOutput, Tools};
use crate::events::AgentEvent;
use pi_shell::{
    cancel::{AbortReason, CancelToken},
    shell::ShellRunOptions,
};
use serde::Deserialize;
use std::io::Write;
use std::time::Instant;

#[derive(Deserialize)]
pub struct Args {
    command: String,
    timeout: Option<f64>,
    cwd: Option<String>,
    #[serde(default)]
    pty: bool,
}

const HEAD: usize = 20 * 1024;
const TAIL: usize = 30 * 1024;

#[derive(Default)]
struct Collector {
    head: String,
    tail: String,
    elided: usize,
    /// Full output spill, opened when the first byte is elided.
    artifact: Option<(u32, std::io::BufWriter<std::fs::File>)>,
    /// Bytes written to the spill so far.
    spilled: u64,
    /// The spill hit `SPILL_CAP`.
    spill_capped: bool,
    /// The first spill I/O error, reported once in the output.
    spill_error: Option<String>,
}

/// The spill file stops growing here.
const SPILL_CAP: u64 = 128 * 1024 * 1024;

impl Collector {
    fn spill(&mut self, bytes: &[u8]) {
        let Some((_, f)) = self.artifact.as_mut() else { return };
        if self.spill_capped || self.spill_error.is_some() {
            return;
        }
        let room = SPILL_CAP.saturating_sub(self.spilled) as usize;
        let n = bytes.len().min(room);
        let mut cut = n;
        // Never end the spill in the middle of a character.
        while cut > 0 && cut < bytes.len() && (bytes[cut] & 0xC0) == 0x80 {
            cut -= 1;
        }
        match f.write_all(&bytes[..cut]) {
            Ok(()) => self.spilled += cut as u64,
            Err(e) => self.spill_error = Some(e.to_string()),
        }
        if cut < bytes.len() {
            self.spill_capped = true;
            if let Err(e) = f.write_all(b"\n[spill truncated at 128 MB]\n") {
                self.spill_error.get_or_insert(e.to_string());
            }
        }
    }

    fn push(&mut self, mut s: &str, alloc: &dyn Fn() -> (u32, std::path::PathBuf)) {
        self.spill(s.as_bytes());
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
            if self.artifact.is_none() {
                let (n, path) = alloc();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        self.artifact = Some((n, std::io::BufWriter::new(f)));
                        let (head, tail) = (std::mem::take(&mut self.head), std::mem::take(&mut self.tail));
                        self.spill(head.as_bytes());
                        self.spill(tail.as_bytes());
                        self.head = head;
                        self.tail = tail;
                    }
                    Err(e) => self.spill_error = Some(e.to_string()),
                }
            }
            self.elided += cut;
            self.tail.drain(..cut);
        }
    }

    fn finish(mut self) -> Collected {
        let body = if self.elided > 0 {
            format!("{}\n[…{}B elided…]\n{}", self.head, self.elided, self.tail)
        } else {
            std::mem::take(&mut self.head) + &self.tail
        };
        let mut notes = String::new();
        let mut artifact = None;
        if let Some((n, mut f)) = self.artifact.take() {
            if let Err(e) = f.flush() {
                self.spill_error.get_or_insert(e.to_string());
            }
            notes.push_str(&format!("\n[full output: artifact://{n}]"));
            if self.spill_capped {
                notes.push_str(" (truncated at 128 MB)");
            }
            artifact = Some(n);
        }
        let mut spill_note = self.spill_capped.then(|| "spill truncated at 128 MB".to_string());
        if let Some(e) = &self.spill_error {
            notes.push_str(&format!("\n[could not save the full output: {e}]"));
            spill_note = Some(format!("could not save the full output: {e}"));
        }
        Collected { body, notes, artifact, spill_note }
    }
}

/// What a command printed: the output itself, mu's notes about it (appended for the model), and
/// the facts the UI shows instead of parsing those notes.
struct Collected {
    body: String,
    notes: String,
    artifact: Option<u32>,
    spill_note: Option<String>,
}

/// The command's timeout, parsed once for both execution paths.
struct Limits {
    /// Seconds as requested (default 300; 0 disables).
    secs: f64,
    /// `None` when disabled; otherwise clamped to 1 s – 1 h.
    timeout: Option<std::time::Duration>,
}

impl Limits {
    fn new(requested: Option<f64>) -> Self {
        let secs = requested.unwrap_or(300.0);
        let timeout =
            (secs != 0.0).then(|| std::time::Duration::from_millis((secs.clamp(1.0, 3600.0) * 1000.0) as u64));
        Self { secs, timeout }
    }

    fn millis(&self) -> Option<u32> {
        self.timeout.map(|d| d.as_millis() as u32)
    }
}

impl Tools {
    pub async fn bash(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let fallback = a.pty;
        if a.pty
            && let Some(out) = self.bash_pty(&a, ctx).await
        {
            return out;
        }
        let mut out = self.bash_plain(a, ctx).await;
        if fallback {
            out.text.push_str("\n\n[pty unavailable without the terminal UI; ran without a terminal]");
        }
        out
    }

    async fn bash_pty(&self, a: &Args, ctx: &ToolCtx) -> Option<ToolOutput> {
        use crate::events::PtyInput;
        use pi_shell::output_decode::OutputDecoder;
        use std::time::Duration;
        use tokio::sync::{mpsc, oneshot};

        let (input, mut input_rx) = mpsc::unbounded_channel::<PtyInput>();
        let (reply, rx) = oneshot::channel();
        ctx.events.send(AgentEvent::PtyOpen { id: ctx.call_id.clone(), input, reply }).ok()?;
        let (cols, rows) = rx.await.ok()?;
        let cwd = a.cwd.as_ref().map(|c| self.abs(c)).unwrap_or_else(|| self.cwd.clone());
        let limits = Limits::new(a.timeout);
        let (secs, timeout) = (limits.secs, limits.timeout);
        let mut pty = match crate::pty::spawn(&a.command, &cwd, cols, rows) {
            Ok(p) => p,
            Err(e) => return Some(ToolOutput::err(format!("PTY error: {e}"))),
        };
        let io = pty.io.clone();
        let start = Instant::now();
        let mut decoder = OutputDecoder::new();
        let mut cleaner = crate::pty::Cleaner::default();
        let mut collector = Collector::default();
        let (mut cancelled, mut timed_out) = (false, false);
        let mut input_open = true;
        // Keystrokes waiting for the pty to accept them; writing never blocks reads, cancel or timeout.
        let mut pending_in: std::collections::VecDeque<u8> = Default::default();
        let mut write_buf: Vec<u8> = Vec::new();
        let mut status: Option<std::process::ExitStatus> = None;
        let mut term_at: Option<tokio::time::Instant> = None; // when termination began
        let mut killed = false;
        let mut buf = vec![0u8; 64 * 1024];
        let timeout_sleep = tokio::time::sleep(timeout.unwrap_or(Duration::from_secs(1)));
        tokio::pin!(timeout_sleep);
        let mut eof = false;

        macro_rules! emit {
            ($raw:expr) => {{
                let raw: String = $raw;
                if !raw.is_empty() {
                    let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: raw.clone() });
                    let clean = cleaner.push(&raw);
                    collector.push(&clean, &|| self.next_artifact());
                }
            }};
        }

        loop {
            // Escalate to SIGKILL 200 ms after SIGTERM; give up waiting 800 ms after that.
            let kill_at = term_at.map(|t| t + Duration::from_millis(200));
            let giveup_at = kill_at.map(|t| t + Duration::from_millis(800));
            let deadline = if killed { giveup_at } else { kill_at };
            let esc_sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                r = io.read(&mut buf), if !eof => match r {
                    Ok(0) | Err(_) => eof = true,
                    Ok(n) => emit!(decoder.push(&buf[..n])),
                },
                w = async {
                    // Offer at most 4 KB at a time; the queue itself is unbounded.
                    write_buf.clear();
                    write_buf.extend(pending_in.iter().take(4096));
                    io.write_some(&write_buf).await
                }, if !pending_in.is_empty() && term_at.is_none() => match w {
                    Ok(n) => { pending_in.drain(..n); }
                    Err(_) => pending_in.clear(),
                },
                m = input_rx.recv(), if input_open => match m {
                    Some(PtyInput::Bytes(b)) => pending_in.extend(b),
                    Some(PtyInput::Resize { cols, rows }) => io.resize(cols, rows),
                    Some(PtyInput::Kill) => {
                        if term_at.is_none() {
                            cancelled = true;
                            term_at = Some(tokio::time::Instant::now());
                            io.signal(libc::SIGTERM);
                            io.hangup();
                        }
                    }
                    None => input_open = false,
                },
                _ = ctx.cancel.cancelled(), if term_at.is_none() => {
                    cancelled = true;
                    term_at = Some(tokio::time::Instant::now());
                    io.signal(libc::SIGTERM);
                    io.hangup();
                }
                _ = &mut timeout_sleep, if timeout.is_some() && term_at.is_none() => {
                    timed_out = true;
                    term_at = Some(tokio::time::Instant::now());
                    io.signal(libc::SIGTERM);
                    io.hangup();
                }
                _ = esc_sleep => {
                    if killed {
                        break;
                    }
                    killed = true;
                    io.signal(libc::SIGKILL);
                }
                st = pty.child.wait() => {
                    status = st.ok();
                    break;
                }
            }
        }

        // Drain: until EOF, or 300 ms without data (a background descendant may hold the slave open).
        if !eof {
            loop {
                match tokio::time::timeout(Duration::from_millis(300), io.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => emit!(decoder.push(&buf[..n])),
                    _ => break,
                }
            }
        }
        // The command is over. Anything it left running in the background goes with it; the
        // group is gone once the leader is reaped only if nothing else is in it.
        crate::proc::terminate_group(pty.io.pid()).await;
        emit!(decoder.finish());
        let tail = cleaner.finish();
        collector.push(&tail, &|| self.next_artifact());
        let code = status.and_then(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().or_else(|| s.signal().map(|g| 128 + g))
        });
        let wall = start.elapsed().as_secs_f64();
        Some(finish(collector.finish(), wall, secs, cancelled, timed_out, code, false))
    }

    async fn bash_plain(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let limits = Limits::new(a.timeout);
        let (secs, timeout_ms) = (limits.secs, limits.millis());
        let mut cancel = CancelToken::new(timeout_ms);
        let abort = cancel.emplace_abort_token();
        let aborted = std::cell::Cell::new(false);
        let (tx, rx) = flume::unbounded::<String>();
        let opts = ShellRunOptions {
            command: a.command,
            cwd: a.cwd.map(|c| self.abs(&c).to_string_lossy().into_owned()),
            timeout_ms,
            ..Default::default()
        };
        let start = Instant::now();
        let run = async {
            let fut = self.shell().run(opts, Some(tx), cancel);
            tokio::pin!(fut);
            tokio::select! {
                r = &mut fut => r,
                _ = ctx.cancel.cancelled() => {
                    aborted.set(true);
                    abort.abort(AbortReason::Signal);
                    fut.await
                }
            }
        };
        let drain = async {
            let mut c = Collector::default();
            // The live tail shows cleaned text; only the pty view gets raw bytes.
            let mut cleaner = crate::pty::Cleaner::default();
            while let Ok(s) = rx.recv_async().await {
                let clean = cleaner.push(&s);
                if !clean.is_empty() {
                    let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: clean });
                }
                c.push(&s, &|| self.next_artifact());
            }
            let rest = cleaner.finish();
            if !rest.is_empty() {
                let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: rest });
            }
            c.finish()
        };
        let (res, output) = tokio::join!(run, drain);
        let res = match res {
            Ok(r) => r,
            Err(_) if aborted.get() => {
                return finish(output, start.elapsed().as_secs_f64(), secs, true, false, None, true);
            }
            Err(e) => {
                let end = output.body.len();
                let mut out = ToolOutput::err(
                    format!("{}{}\n\nShell error: {e}", output.body, output.notes).trim_start().to_string(),
                );
                out.meta = Some(ToolMeta::Bash(BashMeta {
                    output_end: end.min(out.text.len()),
                    artifact: output.artifact,
                    spill_note: output.spill_note,
                    ..Default::default()
                }));
                return out;
            }
        };
        let wall = start.elapsed().as_secs_f64();
        finish(output, wall, secs, res.cancelled, res.timed_out, res.exit_code, false)
    }
}

/// Builds the result: the same text the model has always seen, plus `meta` carrying the facts the
/// UI shows (exit code, wall time, flags, artifact), so it never has to parse them back out.
/// `quiet_wall`: a cancelled-by-user run reports no wall time.
fn finish(
    c: Collected,
    wall: f64,
    secs: f64,
    cancelled: bool,
    timed_out: bool,
    exit_code: Option<i32>,
    quiet_wall: bool,
) -> ToolOutput {
    let had_output = !c.body.is_empty();
    let mut text = if had_output { c.body } else { "(no output)".to_string() };
    let output_end = if had_output { text.len() } else { 0 };
    text.push_str(&c.notes);
    if quiet_wall && !had_output {
        text.clear(); // a user-cancelled run that printed nothing says only that it was cancelled
    }
    let wall_shown = !quiet_wall && wall >= 1.0;
    if wall_shown {
        text.push_str(&format!("\n\nWall time: {wall:.1} seconds"));
    }
    let exit_code = exit_code.filter(|c| *c != 0);
    if cancelled {
        text.push_str(if text.is_empty() { "[Command cancelled]" } else { "\n\n[Command cancelled]" });
    } else if timed_out {
        text.push_str(&format!("\n\n[Command timed out after {} seconds]", secs.clamp(1.0, 3600.0)));
    } else if let Some(code) = exit_code {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    let mut out = if cancelled || timed_out { ToolOutput::err(text) } else { ToolOutput::ok(text) };
    out.meta = Some(ToolMeta::Bash(BashMeta {
        output_end,
        exit_code: if cancelled || timed_out { None } else { exit_code },
        wall_secs: wall_shown.then_some(wall),
        timed_out: timed_out && !cancelled,
        cancelled,
        artifact: c.artifact,
        spill_note: c.spill_note,
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn pty_falls_back_without_ui() {
        let tools = Tools::for_test();
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into() };
        let out = tools.call("bash", &json!({"command":"echo hi","pty":true}), &ctx).await;
        assert!(out.text.contains("hi"), "{}", out.text);
        assert!(out.text.contains("[pty unavailable without the terminal UI; ran without a terminal]"));
    }

    /// True if a live process has exactly `sleep {arg}` as its command line. `$` anchors the
    /// match so the wrapping shell (whose command line also contains the text) is not counted.
    async fn alive(arg: &str) -> bool {
        std::process::Command::new("pgrep").args(["-f", &format!("sleep {arg}$")]).output().unwrap().status.success()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_processes_in_plain_mode() {
        // `pgrep`/`pkill -f` see every process on the machine, so the sleep durations carry this
        // process's pid: concurrent test runs cannot match or kill each other's sleeps.
        let pid = std::process::id();
        let (quiet, kept) = (format!("31337{pid}"), format!("31338{pid}"));
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into() };
        let cmd = format!("(/bin/sleep {quiet} &); nohup /bin/sleep {kept} >/dev/null 2>&1 & echo started");
        let out = tools.call("bash", &json!({"command": cmd}), &ctx).await;
        assert!(out.text.contains("started"), "{}", out.text);
        // A plain `&` ends with the command; `nohup … &` (pi-shell's builtin, which detaches) keeps one alive.
        assert!(!alive(&quiet).await);
        // The background process may not have exec'd `sleep` yet when the command returns.
        let mut up = false;
        for _ in 0..50 {
            if alive(&kept).await {
                up = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(up);
        let _ = std::process::Command::new("pkill").args(["-f", &format!("sleep {kept}$")]).status();
    }

    /// mu's runtime is current-thread: the persistent shell must still reap a finished `&` child
    /// while idle, and keep its status for `wait` in a later call.
    #[tokio::test(flavor = "current_thread")]
    async fn finished_background_job_is_reaped_and_keeps_its_status() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into() };
        let out = tools.call("bash", &json!({"command": "/bin/sh -c 'sleep 0.3; exit 7' & echo $!"}), &ctx).await;
        let pid: i32 = out.text.trim().parse().unwrap_or_else(|_| panic!("no pid: {}", out.text));
        let reaped = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            // SAFETY: signal 0 only probes whether the pid exists; a zombie still does.
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(reaped.is_ok(), "finished background child stayed a zombie");
        let out = tools.call("bash", &json!({"command": format!("wait {pid}; echo status=$?")}), &ctx).await;
        assert!(out.text.contains("status=7"), "{}", out.text);
    }

    #[test]
    fn pty_input_queue_does_not_block_exit() {
        use crate::events::PtyInput;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(tokio::task::LocalSet::new().run_until(async {
            let tools = Tools::for_test();
            let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::task::spawn_local(async move {
                while let Some(e) = rx.recv().await {
                    if let AgentEvent::PtyOpen { input, reply, .. } = e {
                        let _ = reply.send((80, 24));
                        // Far more input than a pty buffers, for a command that never reads it.
                        let _ = input.send(PtyInput::Bytes(vec![b'x'; 4 * 1024 * 1024]));
                        std::mem::forget(input);
                    }
                }
            });
            let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into() };
            let t = std::time::Instant::now();
            let out =
                tools.call("bash", &json!({"command":"sleep 0.3; echo done","pty":true,"timeout":20}), &ctx).await;
            // The echoed keystrokes flood the output, so "done" may be elided: what matters is that
            // the command finished normally while 4 MB of input was still queued.
            assert!(!out.is_error, "{}", out.text.chars().take(200).collect::<String>());
            assert!(t.elapsed() < std::time::Duration::from_secs(10));
        }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn result_carries_structured_facts_and_the_same_text() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into() };
        let out = tools.call("bash", &json!({"command": "echo hi; exit 3"}), &ctx).await;
        let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
        assert_eq!(m.exit_code, Some(3));
        assert!(out.text[..m.output_end].contains("hi"));
        assert!(!out.text[..m.output_end].contains("Command exited"));
        assert!(out.text.ends_with("Command exited with code 3"), "the model's text is unchanged: {}", out.text);
        let quiet = tools.call("bash", &json!({"command": "true"}), &ctx).await;
        let Some(ToolMeta::Bash(q)) = &quiet.meta else { panic!() };
        assert_eq!((q.output_end, q.exit_code), (0, None));
        assert_eq!(quiet.text, "(no output)");
    }
}
