//! Programs that take the terminal and must run where the state is: `omp login` and omp's MCP sign-in.
//!
//! Beside a remote client the host runs the program in a pseudo-terminal the size of the client's,
//! pushes its output (`program_output`) and takes the client's keystrokes (`program_input`) and size
//! changes (`program_size`); `program_run` answers when the program ends. A client that disappears takes
//! its programs with it (`kill_all`): they were waiting for someone who is gone. It is not a command line:
//! a client cannot ask the host to run anything but these.

use super::local::{Local, Registry};
use super::{Answer, Pushed, pending, ready};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

pub const PROGRAM_RUN: &str = "program_run";
pub const PROGRAM_INPUT: &str = "program_input";
pub const PROGRAM_SIZE: &str = "program_size";
/// The pushed name of a program's output.
pub const PROGRAM_OUTPUT: &str = "program_output";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Program {
    /// `omp login`, with omp's own provider menu answered with `pick` when it asks.
    Login { args: Vec<String>, pick: Option<usize> },
    /// `omp -p --no-session '/mcp reauth <name>'`, in the host's working directory.
    McpLogin { name: String },
}

#[derive(Serialize, Deserialize)]
pub struct Run {
    pub id: u64,
    pub program: Program,
    pub cols: u16,
    pub rows: u16,
}

/// `code` is `None` when a signal ended the program.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub code: Option<i32>,
}

#[derive(Serialize, Deserialize)]
pub struct Input {
    pub id: u64,
    /// Base64 of the keystrokes.
    pub data: String,
}

#[derive(Serialize, Deserialize)]
pub struct Size {
    pub id: u64,
    pub cols: u16,
    pub rows: u16,
}

/// A pushed chunk of output.
#[derive(Serialize, Deserialize)]
pub struct Output {
    pub id: u64,
    /// Base64 of the bytes.
    pub data: String,
}

enum Ctl {
    Input(Vec<u8>),
    Size(u16, u16),
    Kill,
}

thread_local! {
    static RUNNING: RefCell<HashMap<u64, UnboundedSender<Ctl>>> = RefCell::new(HashMap::new());
}

/// Ends every program a client started: the client is gone.
pub fn kill_all() {
    RUNNING.with(|r| r.borrow().values().for_each(|tx| drop(tx.send(Ctl::Kill))));
}

pub(super) fn register(r: &mut Registry) {
    r.add(PROGRAM_RUN, run);
    r.add(PROGRAM_INPUT, input);
    r.add(PROGRAM_SIZE, size);
}

fn input(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |i: Input| {
        let data = STANDARD.decode(i.data).map_err(|e| e.to_string())?;
        RUNNING.with(|r| r.borrow().get(&i.id).map(|tx| tx.send(Ctl::Input(data))));
        Ok(())
    })
}

fn size(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |s: Size| {
        RUNNING.with(|r| r.borrow().get(&s.id).map(|tx| tx.send(Ctl::Size(s.cols, s.rows))));
        Ok(())
    })
}

/// `s` as one word of a `sh` command line.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn command_line(p: &Program) -> (String, Option<usize>) {
    let omp = quote(&mu_common::paths::omp_bin().to_string_lossy());
    match p {
        Program::Login { args, pick } => {
            let args: String = args.iter().map(|a| format!(" {}", quote(a))).collect();
            (format!("{omp} login{args}"), *pick)
        }
        Program::McpLogin { name } => {
            (format!("{omp} -p --no-session {}", quote(&format!("/mcp reauth {name}"))), None)
        }
    }
}

fn run(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    let (push, cwd) = (l.push.clone(), l.cwd.clone());
    pending(args, move |r: Run| run_program(push, cwd, r))
}

async fn run_program(push: UnboundedSender<Pushed>, cwd: PathBuf, r: Run) -> Result<Done, String> {
    use mu_base::pty;
    let (line, pick) = command_line(&r.program);
    let env: Vec<(&str, String)> = mu_common::omp_state::envs()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
        .collect();
    let pty = pty::spawn(&line, &cwd, r.cols, r.rows, &env).map_err(|e| format!("cannot run the program: {e}"))?;
    let (io, mut child) = (pty.io.clone(), pty.child);
    let (ctl_tx, mut ctl) = unbounded_channel();
    RUNNING.with(|m| m.borrow_mut().insert(r.id, ctl_tx));
    let out = |bytes: &[u8]| {
        let data = STANDARD.encode(bytes);
        let _ = push.send(Pushed { name: PROGRAM_OUTPUT.into(), data: super::encode(&Output { id: r.id, data }) });
    };
    // omp's provider menu is swallowed and answered: the user already picked in mu's own list.
    let (mut asked, mut seen) = (pick.is_none(), Vec::new());
    let mut buf = vec![0u8; 16 * 1024];
    let mut status = None;
    let mut eof = false;
    while !(eof && status.is_some()) {
        tokio::select! {
            n = io.read(&mut buf), if !eof => match n {
                Ok(n) if n > 0 => {
                    if asked {
                        out(&buf[..n]);
                    } else {
                        seen.extend_from_slice(&buf[..n]);
                        if String::from_utf8_lossy(&seen).contains("Enter number") {
                            asked = true;
                            let _ = io.write_some(format!("{}\n", pick.unwrap_or(0)).as_bytes()).await;
                        }
                    }
                }
                _ => eof = true,
            },
            c = ctl.recv() => match c {
                Some(Ctl::Input(bytes)) => {
                    let mut rest = &bytes[..];
                    while !rest.is_empty() {
                        match io.write_some(rest).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => rest = &rest[n..],
                        }
                    }
                }
                Some(Ctl::Size(c, r)) => io.resize(c, r),
                Some(Ctl::Kill) | None => io.signal(libc::SIGHUP),
            },
            st = child.wait(), if status.is_none() => status = Some(st.ok()),
        }
    }
    RUNNING.with(|m| m.borrow_mut().remove(&r.id));
    mu_base::process::terminate_group(io.pid()).await;
    mu_common::omp_state::after_omp_exit();
    Ok(Done { code: status.flatten().and_then(|s| s.code()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_lines_are_fixed_shapes() {
        let (l, pick) = command_line(&Program::Login { args: vec!["a b".into(), "it's".into()], pick: Some(3) });
        assert!(l.ends_with(" login 'a b' 'it'\\''s'"), "{l}");
        assert_eq!(pick, Some(3));
        let (l, pick) = command_line(&Program::McpLogin { name: "x'y".into() });
        assert!(l.ends_with(" -p --no-session '/mcp reauth x'\\''y'"), "{l}");
        assert_eq!(pick, None);
    }

    #[test]
    fn kill_all_hangs_a_program_up() {
        use std::os::unix::fs::PermissionsExt;
        let dir = mu_testutil::tmp();
        let marker = dir.join("hung-up");
        let script = dir.join("omp");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntrap ': > \"{}\"; exit 1' HUP\nprintf 'READY\\n'\nread x\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: tests in this crate that read MU_OMP are this one.
        unsafe { std::env::set_var("MU_OMP", &script) };
        mu_testutil::block_on_local(async {
            let (push, mut out) = unbounded_channel();
            let run = Run { id: 1, program: Program::McpLogin { name: "x".into() }, cols: 80, rows: 24 };
            let task = tokio::task::spawn_local(run_program(push, dir.clone(), run));
            out.recv().await.expect("the program printed");
            kill_all();
            let done = tokio::time::timeout(std::time::Duration::from_secs(10), task).await.expect("ended").unwrap();
            assert!(done.is_ok());
            assert!(marker.exists(), "hung up");
        });
    }

    #[test]
    fn programs_and_replies_survive_json() {
        let r = Run { id: 4, program: Program::Login { args: vec![], pick: None }, cols: 80, rows: 24 };
        let back: Run = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!((back.id, back.program, back.cols), (4, Program::Login { args: vec![], pick: None }, 80));
        assert_eq!(serde_json::to_value(Done { code: Some(2) }).unwrap(), serde_json::json!({"code": 2}));
    }
}
