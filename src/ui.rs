use crate::anthropic::Events;
use serde_json::Value;
use std::io::Write;

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    None,
    Text,
    Thinking,
}

pub struct Ui {
    mode: Mode,
    mid_line: bool,
}

fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n).chain(['…']).collect() } else { s.into() }
}

impl Ui {
    pub fn new() -> Self {
        Self { mode: Mode::None, mid_line: false }
    }

    fn switch(&mut self, to: Mode) {
        if self.mode == to {
            return;
        }
        if self.mode == Mode::Thinking {
            out("\x1b[0m");
        }
        if self.mid_line {
            out("\n");
            self.mid_line = false;
        }
        if to == Mode::Thinking {
            out("\x1b[2m");
        }
        self.mode = to;
    }

    /// Ends any open streamed section.
    pub fn flush_stream(&mut self) {
        self.switch(Mode::None);
    }

    pub fn line(&mut self, s: &str) {
        self.flush_stream();
        out(&format!("{s}\n"));
    }

    pub fn error(&mut self, s: &str) {
        self.line(&format!("\x1b[31m{s}\x1b[0m"));
    }

    pub fn tool_call(&mut self, name: &str, args: &Value) {
        if name == "bash" {
            let cmd = args["command"].as_str().unwrap_or("");
            let shown: Vec<&str> = cmd.lines().take(12).collect();
            self.line(&format!("\x1b[36m● bash\x1b[0m {}", trunc(&shown.join("\n  "), 1200)));
            if cmd.lines().count() > 12 {
                self.line(&format!("  … (+{} lines)", cmd.lines().count() - 12));
            }
            return;
        }
        let first = |k: &str| args[k].as_str().and_then(|s| s.lines().next()).unwrap_or("");
        let summary = match name {
            "read" | "write" => first("path"),
            "glob" | "grep" => first("pattern"),
            "edit" => first("input"),
            "bash" => first("command"),
            _ => "",
        };
        self.line(&format!("\x1b[36m● {name}\x1b[0m {}", trunc(summary, 120)));
    }

    pub fn tool_result(&mut self, text: &str, is_error: bool) {
        let lines: Vec<&str> = text.lines().collect();
        let (pre, post) = if is_error { ("\x1b[31m", "\x1b[0m") } else { ("", "") };
        for l in lines.iter().take(4) {
            self.line(&format!("{pre}  │ {l}{post}"));
        }
        if lines.len() > 4 {
            self.line(&format!("{pre}  │ … (+{} lines){post}", lines.len() - 4));
        }
    }

    pub fn status(&mut self, s: &str) {
        self.line(&format!("\x1b[2m{s}\x1b[0m"));
    }
}

impl Events for Ui {
    fn text(&mut self, s: &str) {
        self.switch(Mode::Text);
        out(s);
        if !s.is_empty() {
            self.mid_line = !s.ends_with('\n');
        }
    }
    fn thinking(&mut self, s: &str) {
        self.switch(Mode::Thinking);
        out(s);
        if !s.is_empty() {
            self.mid_line = !s.ends_with('\n');
        }
    }
}
