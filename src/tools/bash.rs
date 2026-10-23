use super::{AbortHandle, ToolOutput, Tools};
use pi_shell::{
    cancel::CancelToken,
    shell::ShellRunOptions,
};
use serde::Deserialize;
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
}

impl Collector {
    fn push(&mut self, mut s: &str) {
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
            self.elided += cut;
            self.tail.drain(..cut);
        }
    }

    fn finish(self) -> String {
        if self.elided > 0 {
            format!("{}\n[…{}B elided…]\n{}", self.head, self.elided, self.tail)
        } else {
            self.head + &self.tail
        }
    }
}

impl Tools {
    pub async fn bash(&self, a: Args, abort: &AbortHandle) -> ToolOutput {
        let secs = a.timeout.unwrap_or(300.0);
        let timeout_ms = if secs == 0.0 { None } else { Some((secs.clamp(1.0, 3600.0) * 1000.0) as u32) };
        let mut cancel = CancelToken::new(timeout_ms);
        abort.set(cancel.emplace_abort_token());
        let (tx, rx) = flume::unbounded::<String>();
        let opts = ShellRunOptions {
            command: a.command,
            cwd: a.cwd.map(|c| self.abs(&c).to_string_lossy().into_owned()),
            timeout_ms,
            ..Default::default()
        };
        let start = Instant::now();
        let run = async {
            let r = self.shell().run(opts, Some(tx), cancel).await;
            r
        };
        let drain = async {
            let mut c = Collector::default();
            while let Ok(s) = rx.recv_async().await {
                c.push(&s);
            }
            c.finish()
        };
        let (res, output) = tokio::join!(run, drain);
        abort.clear();
        let res = match res {
            Ok(r) => r,
            Err(_) if abort.fired() => {
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
