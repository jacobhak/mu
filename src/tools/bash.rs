use super::{ToolCtx, ToolOutput, Tools};
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
    artifact: Option<(u32, std::fs::File)>,
}

impl Collector {
    fn push(&mut self, mut s: &str, alloc: &dyn Fn() -> (u32, std::path::PathBuf)) {
        if let Some((_, f)) = self.artifact.as_mut() {
            let _ = f.write_all(s.as_bytes());
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
            if self.artifact.is_none() {
                let (n, path) = alloc();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Ok(mut f) = std::fs::File::create(&path) {
                    let _ = f.write_all(self.head.as_bytes());
                    let _ = f.write_all(self.tail.as_bytes());
                    self.artifact = Some((n, f));
                }
            }
            self.elided += cut;
            self.tail.drain(..cut);
        }
    }

    fn finish(self) -> String {
        let mut out = if self.elided > 0 {
            format!("{}\n[…{}B elided…]\n{}", self.head, self.elided, self.tail)
        } else {
            self.head + &self.tail
        };
        if let Some((n, _)) = self.artifact {
            out.push_str(&format!("\n[full output: artifact://{n}]"));
        }
        out
    }
}

impl Tools {
    pub async fn bash(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let fallback = a.pty;
        if a.pty {
            if let Some(out) = self.bash_pty(&a, ctx).await {
                return out;
            }
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
        let secs = a.timeout.unwrap_or(300.0);
        let timeout = if secs == 0.0 { None } else { Some(Duration::from_millis((secs.clamp(1.0, 3600.0) * 1000.0) as u64)) };
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
                m = input_rx.recv(), if input_open => match m {
                    Some(PtyInput::Bytes(b)) => { let _ = io.write_all(&b).await; }
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
        while !eof {
            match tokio::time::timeout(Duration::from_millis(300), io.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => emit!(decoder.push(&buf[..n])),
                _ => break,
            }
        }
        emit!(decoder.finish());
        let tail = cleaner.finish();
        collector.push(&tail, &|| self.next_artifact());
        let code = status.and_then(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.code().or_else(|| s.signal().map(|g| 128 + g))
        });
        let wall = start.elapsed().as_secs_f64();
        Some(finish(collector.finish(), wall, secs, cancelled, timed_out, code))
    }

    async fn bash_plain(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let secs = a.timeout.unwrap_or(300.0);
        let timeout_ms = if secs == 0.0 { None } else { Some((secs.clamp(1.0, 3600.0) * 1000.0) as u32) };
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
            while let Ok(s) = rx.recv_async().await {
                let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: s.clone() });
                c.push(&s, &|| self.next_artifact());
            }
            c.finish()
        };
        let (res, output) = tokio::join!(run, drain);
        let res = match res {
            Ok(r) => r,
            Err(_) if aborted.get() => {
                let out = if output.is_empty() { String::new() } else { format!("{output}\n\n") };
                return ToolOutput::err(format!("{out}[Command cancelled]"));
            }
            Err(e) => return ToolOutput::err(format!("{output}\n\nShell error: {e}").trim_start().to_string()),
        };
        let wall = start.elapsed().as_secs_f64();
        finish(output, wall, secs, res.cancelled, res.timed_out, res.exit_code)
    }
}

fn finish(output: String, wall: f64, secs: f64, cancelled: bool, timed_out: bool, exit_code: Option<i32>) -> ToolOutput {
    {
        let mut text = if output.is_empty() { "(no output)".to_string() } else { output };
        if wall >= 1.0 {
            text.push_str(&format!("\n\nWall time: {wall:.1} seconds"));
        }
        if cancelled {
            text.push_str("\n\n[Command cancelled]");
            return ToolOutput::err(text);
        }
        if timed_out {
            text.push_str(&format!("\n\n[Command timed out after {} seconds]", secs.clamp(1.0, 3600.0)));
            return ToolOutput::err(text);
        }
        if let Some(code) = exit_code.filter(|c| *c != 0) {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        ToolOutput::ok(text)
    }
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
}
