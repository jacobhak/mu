use crate::anthropic::Gateway;
use crate::context::{self, ContextFile};
use crate::mem;
use crate::message::{Block, Message, StopReason, now_ms};
use crate::session::SessionFile;
use crate::tools::{AbortHandle, ToolOutput, Tools};
use crate::ui::Ui;
use serde_json::Value;
use std::path::PathBuf;

pub struct Agent {
    pub gw: Gateway,
    pub tools: Tools,
    pub session: Option<SessionFile>,
    pub messages: Vec<Message>,
    pub model: String,
    pub thinking: String,
    pub ui: Ui,
    cwd: PathBuf,
    files: Vec<ContextFile>,
    abort: AbortHandle,
    content_bytes: u64,
    tool_defs: Vec<Value>,
}

enum Interrupted {
    Yes,
}

impl Agent {
    pub fn new(
        gw: Gateway,
        cwd: PathBuf,
        files: Vec<ContextFile>,
        model: String,
        thinking: String,
        session: Option<SessionFile>,
        messages: Vec<Message>,
    ) -> Self {
        let mut a = Self {
            gw,
            tools: Tools::new(cwd.clone()),
            session,
            messages: Vec::new(),
            model,
            thinking,
            ui: Ui::new(),
            cwd,
            files,
            abort: AbortHandle::default(),
            content_bytes: 0,
            tool_defs: Tools::definitions(),
        };
        a.content_bytes = a.system().len() as u64;
        for m in &messages {
            a.content_bytes += msg_bytes(m);
        }
        a.messages = messages;
        a
    }

    fn system(&self) -> String {
        context::system_prompt(&self.model, &self.cwd, &self.files)
    }

    fn ensure_session(&mut self) {
        if self.session.is_some() {
            return;
        }
        match SessionFile::create(&self.cwd) {
            Ok(mut s) => {
                s.model_change(&self.model);
                s.thinking_change(&self.thinking);
                self.session = Some(s);
            }
            Err(e) => eprintln!("session write failed: {e}"),
        }
    }

    pub fn set_model(&mut self, m: &str) {
        self.model = m.into();
        if let Some(s) = self.session.as_mut() {
            s.model_change(m);
        }
    }

    pub fn set_thinking(&mut self, t: &str) {
        self.thinking = t.into();
        if let Some(s) = self.session.as_mut() {
            s.thinking_change(t);
        }
    }

    fn push(&mut self, m: Message) {
        self.content_bytes += msg_bytes(&m);
        if let Some(s) = self.session.as_mut() {
            s.message(&m);
        }
        self.messages.push(m);
    }

    pub async fn turn(&mut self, prompt: &str) {
        self.ensure_session();
        self.push(Message::user(prompt));
        self.run_loop().await;
        self.ui.flush_stream();
        mem::trim();
        self.print_status();
    }

    async fn run_loop(&mut self) {
        loop {
            let system = self.system();
            let reply = {
                let fut = self.gw.stream(
                    &self.model,
                    &self.thinking,
                    &system,
                    &self.tool_defs,
                    &self.messages,
                    &mut self.ui,
                );
                tokio::select! {
                    r = fut => Some(r),
                    _ = tokio::signal::ctrl_c() => None,
                }
            };
            let msg = match reply {
                None => {
                    self.ui.line("[aborted]");
                    return;
                }
                Some(Err(e)) => {
                    self.ui.error(&format!("error: {e}"));
                    return;
                }
                Some(Ok(m)) => m,
            };
            self.ui.flush_stream();
            let Message::Assistant { content, stop_reason, .. } = &msg else { return };
            let calls: Vec<(String, String, Value)> = content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolCall { id, name, arguments } => Some((id.clone(), name.clone(), arguments.clone())),
                    _ => None,
                })
                .collect();
            let stop = *stop_reason;
            self.push(msg);
            if stop == StopReason::Length {
                self.ui.error("[output truncated: max_tokens reached]");
            }
            if stop != StopReason::ToolUse || calls.is_empty() {
                return;
            }
            let mut interrupted = None;
            for (id, name, args) in calls {
                if interrupted.is_some() {
                    self.push_result(&id, &name, ToolOutput::err("Tool call aborted by user"));
                    continue;
                }
                self.ui.tool_call(&name, &args);
                let out = {
                    let fut = self.tools.call(&name, &args, &self.abort);
                    tokio::pin!(fut);
                    tokio::select! {
                        r = &mut fut => r,
                        _ = tokio::signal::ctrl_c() => {
                            interrupted = Some(Interrupted::Yes);
                            self.abort.fire();
                            fut.await
                        }
                    }
                };
                self.ui.tool_result(&out.text, out.is_error);
                self.push_result(&id, &name, out);
            }
            if interrupted.is_some() {
                self.ui.line("[aborted]");
                return;
            }
        }
    }

    fn push_result(&mut self, id: &str, name: &str, out: ToolOutput) {
        self.push(Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![Block::Text { text: out.text }],
            is_error: out.is_error,
            timestamp: now_ms(),
        });
    }

    fn print_status(&mut self) {
        let usage = self.messages.iter().rev().find_map(|m| match m {
            Message::Assistant { usage, .. } => Some(usage.clone()),
            _ => None,
        });
        let u = usage.unwrap_or_default();
        let (private, content, incidental) = mem::report(self.content_bytes);
        self.ui.status(&format!(
            "── {} · in {} out {} cache r{} w{} · mem private {private:.1} MB · content {content:.1} MB · incidental {incidental:.1} MB",
            self.model,
            mem::tok(u.input),
            mem::tok(u.output),
            mem::tok(u.cache_read),
            mem::tok(u.cache_write),
        ));
    }
}

fn msg_bytes(m: &Message) -> u64 {
    serde_json::to_vec(m).map_or(0, |v| v.len() as u64)
}
