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
        let mut text = if output.is_empty() { "(no output)".to_string() } else { output };
        let wall = start.elapsed().as_secs_f64();
        if wall >= 1.0 {
            text.push_str(&format!("\n\nWall time: {wall:.1} seconds"));
        }
        if res.cancelled {
            text.push_str("\n\n[Command cancelled]");
            return ToolOutput::err(text);
        }
        if res.timed_out {
            text.push_str(&format!("\n\n[Command timed out after {} seconds]", secs.clamp(1.0, 3600.0)));
            return ToolOutput::err(text);
        }
        if let Some(code) = res.exit_code.filter(|c| *c != 0) {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        ToolOutput::ok(text)
    }
}
