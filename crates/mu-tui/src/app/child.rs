//! Programs that take the terminal while they run: `omp login`, `omp /mcp reauth`, `$EDITOR` and
//! `mu take`. The terminal is handed over, and the screen is redrawn when the program exits.

use super::program_output::OutputFilter;
use super::ui_msgs::UiMsg;
use super::*;
use crate::host::plans::{READ_FILE, ReadFile, WRITE_FILE, WriteFile};
use crate::host::program::{
    Done as ProgramDone, Input as ProgramInput, Output as ProgramOutput, PROGRAM_INPUT, PROGRAM_RUN, PROGRAM_SIZE,
    Program, Run as ProgramRun, Size as ProgramSize,
};
use crate::render::Renderer;
use crate::term::term;

thread_local! {
    /// The host program this client started and is showing: its id and the filter its output passes.
    static HOST_PROGRAM: std::cell::RefCell<Option<(u64, OutputFilter)>> = const { std::cell::RefCell::new(None) };
}

/// What `program_output` for `id` may write to the terminal; nothing unless that program is the
/// one running.
fn program_bytes(id: u64, data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    HOST_PROGRAM.with(|p| match p.borrow_mut().as_mut() {
        Some((running, filter)) if *running == id => Some(filter.push(&bytes)),
        _ => None,
    })
}

/// What to do when a child that owned the terminal exits.
pub(super) enum ChildThen {
    Login,
    /// `omp -p '/mcp reauth <name>'` for this MCP server.
    McpLogin(String),
    /// The external editor was run on the composer text kept in this file.
    ComposerEdit(PathBuf),
    /// The editor was run on a private copy of the plan file shown in the review overlay.
    PlanEdit(HostEdit),
    /// The editor was run on a private copy of a memory file from `/memory`.
    MemoryEdit(HostEdit),
    /// `mu take` ran on the terminal for a session picked in the jobs panel.
    Take,
}

/// A host file being edited on this machine: the copy the editor works on, and the text it had.
pub(super) struct HostEdit {
    host: PathBuf,
    tmp: PathBuf,
    before: String,
}

impl App {
    /// Hands the terminal to a child that runs on a blocking thread, so the agent, plugins and
    /// timers keep running. Input reading and drawing pause until `ChildDone` arrives.
    pub(super) fn start_child(
        &mut self,
        then: ChildThen,
        job: impl FnOnce() -> std::io::Result<std::process::ExitStatus> + Send + 'static,
    ) {
        if self.screen.child_busy {
            return self.notice("another program already has the terminal");
        }
        let committed = std::mem::take(&mut self.agent.tr.pending);
        let _ = self.screen.renderer.draw(&committed, &[], (0, 0));
        let _ = self.screen.renderer.finish();
        let _ = term::suspend();
        self.screen.child_busy = true;
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let status = match tokio::task::spawn_blocking(job).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiMsg::ChildDone { then, status });
        });
    }

    pub(super) fn on_child_done(&mut self, then: ChildThen, status: Result<std::process::ExitStatus, String>) {
        let _ = term::resume();
        self.screen.renderer = Renderer::new();
        self.screen.child_busy = false;
        self.screen.dirty = true;
        match then {
            ChildThen::Login => match status {
                Ok(s) if s.success() => self.notice("login finished"),
                Ok(s) => self.error(&format!("omp login exited with {s}")),
                Err(e) => self.error(&format!("cannot run omp login: {e}")),
            },
            ChildThen::McpLogin(name) => match status {
                Ok(s) if s.success() => {
                    self.notice(&format!("mcp:{name}: logged in; loading tools…"));
                    self.host_call(
                        crate::host::mcp::MCP_AFTER_LOGIN,
                        &name.clone(),
                        move |app, r: Result<(usize, bool), String>| match r {
                            Ok((n, changed)) => {
                                app.notice(&format!("mcp:{name}: logged in, {n} tools"));
                                if changed {
                                    app.send(Command::RefreshTools);
                                }
                            }
                            Err(e) => {
                                app.error(&format!("mcp:{name}: logged in, but its tools could not be listed: {e}"))
                            }
                        },
                    );
                }
                Ok(s) => self.error(&format!("omp /mcp reauth {name} exited with {s}")),
                Err(e) => self.error(&format!("cannot run omp: {e}")),
            },
            ChildThen::ComposerEdit(path) => {
                match status {
                    Ok(s) if s.success() => {
                        if let Ok(t) = std::fs::read_to_string(&path) {
                            self.prompt.composer.set_text(t.trim_end_matches('\n'));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => self.error(&format!("editor failed: {e}")),
                }
                let _ = std::fs::remove_file(&path);
            }
            ChildThen::Take => {
                match status {
                    Ok(s) if s.success() => self.notice("back from mu take"),
                    Ok(s) => self.error(&format!("mu take exited with {s}")),
                    Err(e) => self.error(&format!("cannot run mu take: {e}")),
                }
                self.open_sessions();
            }
            ChildThen::MemoryEdit(edit) => self.finish_host_edit(edit, status, "memory file", |_, _| {}),
            ChildThen::PlanEdit(edit) => self.finish_host_edit(edit, status, "plan", |app, text| {
                if let Some(Overlay::Review { review, .. }) = &mut app.overlay {
                    review.set_content(text);
                }
            }),
        }
    }

    /// Edits a file of the host in `$EDITOR` here: the host sends the text, the editor works on a
    /// private temp copy, and a successful exit writes the changed text back to the host.
    pub(super) fn edit_host_file(
        &mut self,
        host: PathBuf,
        missing_ok: bool,
        what: &'static str,
        then: fn(HostEdit) -> ChildThen,
    ) {
        let args = ReadFile { path: host.clone(), missing_ok };
        self.host_call(READ_FILE, &args, move |app, r: Result<String, String>| {
            let before = match r {
                Ok(t) => t,
                Err(e) => return app.error(&format!("cannot read {what}: {e}")),
            };
            if app.screen.child_busy {
                return app.notice("another program already has the terminal");
            }
            let tmp = match private_temp_file(&before) {
                Ok(p) => p,
                Err(e) => return app.error(&format!("cannot create temp file: {e}")),
            };
            app.run_editor(&tmp.clone(), then(HostEdit { host, tmp, before }));
        });
    }

    fn finish_host_edit(
        &mut self,
        edit: HostEdit,
        status: Result<std::process::ExitStatus, String>,
        what: &'static str,
        done: fn(&mut App, String),
    ) {
        let HostEdit { host, tmp, before } = edit;
        let text = std::fs::read_to_string(&tmp);
        let _ = std::fs::remove_file(&tmp);
        match status {
            Ok(s) if s.success() => {}
            Ok(_) => return,
            Err(e) => return self.error(&format!("editor failed: {e}")),
        }
        let text = match text {
            Ok(t) => t,
            Err(e) => return self.error(&format!("cannot read {what}: {e}")),
        };
        if text == before {
            return;
        }
        let args = WriteFile { path: host, text: text.clone() };
        self.host_call(WRITE_FILE, &args, move |app, r: Result<(), String>| match r {
            Ok(()) => done(app, text),
            Err(e) => app.error(&format!("cannot write {what}: {e}")),
        });
    }

    /// Runs `omp login` on the real terminal. With `pick`, omp's own provider menu is swallowed and
    /// answered with that number once its prompt appears; terminal input is then relayed so OAuth
    /// prompts still work. The gateway reads the stored credentials.
    pub(super) fn run_login(&mut self, args: &[&str], pick: Option<usize>) {
        use std::process::Stdio;
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        if self.host.is_remote() {
            return self.run_host_program(Program::Login { args, pick }, ChildThen::Login);
        }
        self.start_child(ChildThen::Login, move || {
            let mut cmd = std::process::Command::new(mu_common::paths::omp_bin());
            mu_common::omp_state::apply(&mut cmd)?;
            cmd.arg("login").args(&args);
            if pick.is_some() {
                cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
            }
            extern "C" fn noop(_: libc::c_int) {}
            // A caught (not ignored) SIGINT: Ctrl-C reaches omp, which resets it on exec, but not mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = match (cmd.spawn(), pick) {
                (Ok(mut child), Some(n)) => relay_login(&mut child, n),
                (Ok(mut child), None) => child.wait(),
                (Err(e), _) => Err(e),
            };
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            mu_common::omp_state::after_omp_exit();
            status
        });
    }

    /// Runs omp's own MCP OAuth flow (`omp -p --no-session '/mcp reauth <name>'`) on the real terminal,
    /// then makes the server usable in this session (`PluginHost::after_login`).
    pub(super) fn run_mcp_login(&mut self, name: &str) {
        let name = name.to_string();
        self.host_call(crate::host::mcp::MCP_NAMES, &(), move |app, r: Result<Vec<String>, String>| {
            if !r.unwrap_or_default().contains(&name) {
                return app.error(&format!("no MCP server `{name}` is configured"));
            }
            if app.host.is_remote() {
                return app.run_host_program(Program::McpLogin { name: name.clone() }, ChildThen::McpLogin(name));
            }
            let cwd = app.cwd.clone();
            app.start_child(ChildThen::McpLogin(name.clone()), move || {
                let mut cmd = std::process::Command::new(mu_common::paths::omp_bin());
                mu_common::omp_state::apply(&mut cmd)?;
                cmd.args(["-p", "--no-session"]).arg(format!("/mcp reauth {name}")).current_dir(&cwd);
                extern "C" fn noop(_: libc::c_int) {}
                // As in `run_login`: Ctrl-C reaches omp, not mu.
                // SAFETY: installs a no-op handler; restored to the default below.
                unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
                let status = cmd.status();
                // SAFETY: back to the default disposition.
                unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
                mu_common::omp_state::after_omp_exit();
                status
            });
        });
    }

    /// Runs a program on the host (it is on another machine) and shows it on this terminal: the host
    /// sends its output, and this side sends back the keystrokes and the size of the window.
    pub(super) fn run_host_program(&mut self, program: Program, then: ChildThen) {
        use std::os::unix::process::ExitStatusExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        if self.screen.child_busy {
            return self.notice("another program already has the terminal");
        }
        let committed = std::mem::take(&mut self.agent.tr.pending);
        let _ = self.screen.renderer.draw(&committed, &[], (0, 0));
        let _ = self.screen.renderer.finish();
        let _ = term::suspend();
        if !cfg!(test) {
            let _ = crossterm::terminal::enable_raw_mode();
        }
        self.screen.child_busy = true;
        static PROGRAMS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = PROGRAMS.fetch_add(1, Ordering::SeqCst);
        HOST_PROGRAM.with(|p| *p.borrow_mut() = Some((id, OutputFilter::default())));
        let (cols, rows) = if cfg!(test) { (80, 24) } else { crossterm::terminal::size().unwrap_or((80, 24)) };

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (keys_tx, mut keys) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        if !cfg!(test) {
            let stop = stop.clone();
            std::thread::spawn(move || tty_pump(&stop, &keys_tx));
        }
        let host = self.host.clone();
        tokio::task::spawn_local(async move {
            use base64::Engine;
            let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
            loop {
                tokio::select! {
                    k = keys.recv() => match k {
                        Some(k) => {
                            let data = base64::engine::general_purpose::STANDARD.encode(k);
                            drop(host.call(PROGRAM_INPUT, crate::host::encode(&ProgramInput { id, data })));
                        }
                        None => break,
                    },
                    Some(()) = async { match winch.as_mut() { Some(w) => w.recv().await, None => std::future::pending().await } } => {
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        drop(host.call(PROGRAM_SIZE, crate::host::encode(&ProgramSize { id, cols, rows })));
                    }
                }
            }
        });
        let args = ProgramRun { id, program, cols, rows };
        self.host_call(PROGRAM_RUN, &args, move |app, r: Result<ProgramDone, String>| {
            stop.store(true, Ordering::SeqCst);
            HOST_PROGRAM.with(|p| *p.borrow_mut() = None);
            let status = r.map(|d| match d.code {
                Some(c) => std::process::ExitStatus::from_raw(c << 8),
                None => std::process::ExitStatus::from_raw(libc::SIGHUP),
            });
            app.on_child_done(then, status);
        });
    }

    /// Output of the host program on this terminal.
    pub(super) fn on_program_output(&mut self, p: crate::host::Pushed) {
        use std::io::Write;
        let Ok(o) = serde_json::from_value::<ProgramOutput>(p.data) else { return };
        if let Some(bytes) = program_bytes(o.id, &o.data) {
            let mut out = std::io::stdout();
            let _ = out.write_all(&bytes).and_then(|()| out.flush());
        }
    }

    /// Runs `$VISUAL`/`$EDITOR` on `path` with the terminal handed over.
    pub(super) fn run_editor(&mut self, path: &std::path::Path, then: ChildThen) {
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        let path = path.to_path_buf();
        self.start_child(then, move || {
            std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status()
        });
    }

    pub(super) fn edit_external(&mut self) {
        let path = match private_temp_file(&self.prompt.composer.text()) {
            Ok(p) => p,
            Err(e) => return self.error(&format!("cannot create temp file: {e}")),
        };
        self.run_editor(&path.clone(), ChildThen::ComposerEdit(path));
    }

    /// Hands the terminal to `mu take <session>`, then comes back to the Sessions tab.
    pub(super) fn take_session(&mut self, path: PathBuf) {
        let exe = match mu_common::paths::self_exe() {
            Ok(e) => e,
            Err(e) => return self.error(&format!("cannot locate mu: {e}")),
        };
        if self.host.is_remote() {
            return self.take_remote(path, exe);
        }
        self.start_child(ChildThen::Take, move || {
            extern "C" fn noop(_: libc::c_int) {}
            // As in `run_login`: Ctrl-C reaches `mu take`, not this mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = std::process::Command::new(exe).arg("take").arg(&path).status();
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            status
        });
    }

    /// Takes over a `-p` run beside a remote host. The agent stops the run and starts an agent on its session;
    /// this side opens a tunnel to that agent, serves it on a private socket and runs a nested client on
    /// that socket with the terminal, as `mu take` runs a nested mu locally. The first session is back when
    /// it ends.
    fn take_remote(&mut self, path: PathBuf, exe: PathBuf) {
        self.notice("taking the session over on the host: a running `mu -p` is stopped after its current step…");
        self.host_call(
            "take",
            &serde_json::json!({ "path": path }),
            move |app, r: Result<serde_json::Value, String>| {
                let session = match r {
                    Ok(v) => v["session"].as_str().unwrap_or_default().to_string(),
                    Err(e) => return app.error(&format!("cannot take the session over: {e}")),
                };
                let tunnel = match app.host.tunnel(&session) {
                    Ok(t) => t,
                    Err(e) => return app.error(&format!("cannot reach the agent: {e}")),
                };
                let bound = private_socket_dir().and_then(|dir| {
                    let sock = dir.join("take.sock");
                    tokio::net::UnixListener::bind(&sock).map(|l| (l, sock)).inspect_err(|_| {
                        let _ = std::fs::remove_dir_all(&dir);
                    })
                });
                let (listener, sock) = match bound {
                    Ok(b) => b,
                    Err(e) => {
                        tunnel.close();
                        return app.error(&format!("cannot listen for the nested client: {e}"));
                    }
                };
                tokio::task::spawn_local(bridge(listener, tunnel, sock.clone()));
                let command = format!(
                    "{} attach --socket {}",
                    crate::host::program::quote(&exe.to_string_lossy()),
                    crate::host::program::quote(&sock.to_string_lossy())
                );
                app.start_child(ChildThen::Take, move || {
                    extern "C" fn noop(_: libc::c_int) {}
                    // SAFETY: installs a no-op handler; restored to the default below.
                    unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
                    let status = std::process::Command::new(&exe).arg("--nested-client").arg(command).status();
                    // SAFETY: back to the default disposition.
                    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
                    status
                });
            },
        );
    }
}

/// Copies lines between the nested client's connection to `listener` and the tunnel, until either ends.
async fn bridge(listener: tokio::net::UnixListener, mut tunnel: crate::host::Tunnel, sock: PathBuf) {
    use crate::host::TunnelIn;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    // Only the nested client this process starts connects: give it a while to.
    if let Ok(Ok((stream, _))) = tokio::time::timeout(Duration::from_secs(30), listener.accept()).await {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => tunnel.send(l),
                    _ => break,
                },
                from_agent = tunnel.rx.recv() => match from_agent {
                    Some(TunnelIn::Line(l)) => {
                        if write.write_all(l.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                },
            }
        }
    }
    tunnel.close();
    let _ = std::fs::remove_file(&sock);
    if let Some(dir) = sock.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// A fresh directory only this user can enter (0700), for the socket of a take-over's nested client.
fn private_socket_dir() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("mu-take-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    mu_common::fs::create_private_dir(&dir)?;
    Ok(dir)
}

/// Reads the keystrokes of the terminal for a program on the host, until `stop` is set.
fn tty_pump(stop: &std::sync::atomic::AtomicBool, tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    let Some(fd) = term::tty() else { return };
    let mut buf = [0u8; 1024];
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut p, 1, 100) };
        if ready > 0 && p.revents & libc::POLLIN != 0 {
            // SAFETY: `buf` is a live, writable buffer of the given length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 && tx.send(buf[..n as usize].to_vec()).is_err() {
                return;
            }
        }
    }
}

/// Drives a child `omp login` whose stdin and stdout are piped (see `run_login`).
fn relay_login(child: &mut std::process::Child, pick: usize) -> std::io::Result<std::process::ExitStatus> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let (stdin, Some(out)) = (child.stdin.take(), child.stdout.take()) else { return child.wait() };
    let mut stdin = stdin;
    let out_fd = out.as_raw_fd();
    let mut asked = false;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [
            libc::pollfd { fd: out_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if stdin.is_some() && asked { 0 } else { -1 }, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds for the duration of the call.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            // SAFETY: `buf` is a live, writable buffer of the given length.
            let r = unsafe { libc::read(out_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r <= 0 {
                break;
            }
            let chunk = &buf[..r as usize];
            if asked {
                let mut o = std::io::stdout();
                let _ = o.write_all(chunk).and_then(|_| o.flush());
            } else {
                seen.extend_from_slice(chunk);
                if String::from_utf8_lossy(&seen).contains("Enter number") {
                    asked = true;
                    if let Some(w) = stdin.as_mut() {
                        let _ = writeln!(w, "{pick}");
                    }
                }
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            // SAFETY: as above, for fd 0.
            let r = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if r > 0 {
                if let Some(w) = stdin.as_mut() {
                    let _ = w.write_all(&buf[..r as usize]);
                }
            } else {
                stdin = None;
            }
        }
    }
    child.wait()
}

/// Creates a new file (`O_EXCL`, mode 0600, random name) in `$XDG_RUNTIME_DIR` or the temp dir.
fn private_temp_file(content: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("mu-edit-{}.md", uuid::Uuid::new_v4().simple()));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    f.write_all(content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod remote_program_tests {
    use super::*;
    use crate::app::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use crate::host::{Answer, Host, HostInfo, Pushed};
    use serde_json::Value;
    use std::cell::RefCell;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// A host on "another machine": it answers `program_run` when the test says, and records every call.
    struct Remote {
        info: HostInfo,
        calls: Rc<RefCell<Vec<(String, Value)>>>,
        finish: RefCell<Option<oneshot::Receiver<Value>>>,
    }

    impl Host for Remote {
        fn info(&self) -> &HostInfo {
            &self.info
        }

        fn is_remote(&self) -> bool {
            true
        }

        fn call(&self, name: &str, args: Value) -> Answer {
            self.calls.borrow_mut().push((name.to_string(), args));
            if name != PROGRAM_RUN {
                return Answer::Ready(Ok(Value::Null));
            }
            let rx = self.finish.borrow_mut().take();
            Answer::Pending(Box::pin(async move {
                match rx {
                    Some(rx) => rx.await.map_err(|e| e.to_string()),
                    None => Err("no gate".into()),
                }
            }))
        }

        fn pushed(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Pushed>> {
            None
        }
    }

    type Calls = Rc<RefCell<Vec<(String, Value)>>>;

    fn remote_app() -> (App, tokio::sync::mpsc::UnboundedReceiver<UiMsg>, Calls, oneshot::Sender<Value>) {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-remote-program-history"));
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        app.ui_tx = ui_tx;
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (gate, rx) = oneshot::channel();
        app.host = Rc::new(Remote { info: HostInfo::default(), calls: calls.clone(), finish: RefCell::new(Some(rx)) });
        (app, ui_rx, calls, gate)
    }

    #[test]
    fn a_login_beside_a_remote_host_runs_there_and_ends_with_its_exit_status() {
        let (mut app, mut ui_rx, calls, gate) = remote_app();
        in_local(async {
            app.run_login(&["--x"], Some(2));
            assert!(app.screen.child_busy, "this terminal is handed to the program");
            let (name, args) = calls.borrow()[0].clone();
            assert_eq!(name, PROGRAM_RUN);
            assert_eq!(args["program"], serde_json::json!({"Login": {"args": ["--x"], "pick": 2}}));
            gate.send(serde_json::json!({"code": 0})).unwrap();
            let m = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv()).await.unwrap().unwrap();
            app.on_ui(m);
            assert!(!app.screen.child_busy);
            assert!(transcript_text(&app).contains("login finished"), "{}", transcript_text(&app));
        });
    }

    #[test]
    fn taking_a_session_over_beside_a_remote_host_asks_the_host_to_take_it() {
        let (mut app, _ui_rx, calls, _gate) = remote_app();
        in_local(async {
            app.take_session("/s/a_b.jsonl".into());
            let (name, args) = calls.borrow()[0].clone();
            assert_eq!((name.as_str(), &args["path"]), ("take", &serde_json::json!("/s/a_b.jsonl")));
            assert!(!app.screen.child_busy, "the terminal stays with this UI until the host has answered");
        });
    }

    #[test]
    fn a_failing_program_is_reported_with_its_status() {
        let (mut app, mut ui_rx, _calls, gate) = remote_app();
        in_local(async {
            app.run_host_program(Program::Login { args: vec![], pick: None }, ChildThen::Login);
            gate.send(serde_json::json!({"code": 3})).unwrap();
            let m = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv()).await.unwrap().unwrap();
            app.on_ui(m);
            assert!(transcript_text(&app).contains("omp login exited with"), "{}", transcript_text(&app));
        });
    }

    #[test]
    fn output_pushed_by_the_host_is_ignored_unless_its_program_is_running_here() {
        use base64::Engine;
        let (mut app, _ui_rx, _calls, _gate) = remote_app();
        let b64 = |s: &[u8]| base64::engine::general_purpose::STANDARD.encode(s);
        assert_eq!(program_bytes(1, &b64(b"hi")), None, "no program is running");
        in_local(async {
            app.run_host_program(Program::Login { args: vec![], pick: None }, ChildThen::Login);
            let id = HOST_PROGRAM.with(|p| p.borrow().as_ref().map(|(id, _)| *id)).expect("a program is running");
            assert_eq!(program_bytes(id, &b64(b"hi\x1b]52;c;aGk=\x07")), Some(b"hi".to_vec()));
            assert_eq!(program_bytes(id + 1, &b64(b"hi")), None, "another id is not this program");
            assert_eq!(program_bytes(id, "not base64!"), None);
        });
    }

    #[test]
    fn a_finished_program_takes_no_more_output() {
        use base64::Engine;
        let (mut app, mut ui_rx, _calls, gate) = remote_app();
        in_local(async {
            app.run_host_program(Program::Login { args: vec![], pick: None }, ChildThen::Login);
            let id = HOST_PROGRAM.with(|p| p.borrow().as_ref().map(|(id, _)| *id)).unwrap();
            gate.send(serde_json::json!({"code": 0})).unwrap();
            let m = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv()).await.unwrap().unwrap();
            app.on_ui(m);
            let data = base64::engine::general_purpose::STANDARD.encode(b"late");
            assert_eq!(program_bytes(id, &data), None);
        });
    }

    #[test]
    fn a_garbled_chunk_is_ignored() {
        let (mut app, _ui_rx, _calls, _gate) = remote_app();
        app.on_pushed(Pushed {
            name: crate::host::program::PROGRAM_OUTPUT.into(),
            data: serde_json::json!("nonsense"),
        });
    }
}

#[cfg(test)]
mod host_edit_tests {
    use super::*;
    use crate::app::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use crate::host::plans::hand_out;
    use crate::host::{Deferred, local::LocalHost};
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    fn scratch(text: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mu-host-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plan.md");
        std::fs::write(&file, text).unwrap();
        (dir, file)
    }

    /// Runs `ask` on an app with the local host, then on one wrapped in `Deferred`, pumping the
    /// replies until `settled` holds; returns each transcript.
    fn both_hosts(ask: impl Fn(&mut App), settled: impl Fn(&App) -> bool) -> (String, String) {
        let mut said = Vec::new();
        for deferred in [false, true] {
            let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-host-edit-history"));
            let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
            app.ui_tx = ui_tx;
            if deferred {
                app.host = Rc::new(Deferred(LocalHost::new(
                    app.services.providers.clone(),
                    app.ctx.clone(),
                    app.plugins.clone(),
                    app.cwd.clone(),
                )));
            }
            in_local(async {
                ask(&mut app);
                while !settled(&app) {
                    let m = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv()).await.unwrap().unwrap();
                    app.on_ui(m);
                }
            });
            said.push(transcript_text(&app));
        }
        (said.remove(0), said.remove(0))
    }

    #[test]
    fn a_file_the_host_did_not_hand_out_is_refused_and_left_alone() {
        let (dir, file) = scratch("secret");
        let (now, later) = both_hosts(
            |app| app.edit_host_file(file.clone(), false, "plan", ChildThen::PlanEdit),
            |app| transcript_text(app).contains("cannot read plan"),
        );
        assert!(now.contains("cannot read plan: ") && now.contains("not a file the host offered"), "{now}");
        assert_eq!(now, later);
        let (now, later) = both_hosts(
            |app| {
                let args = WriteFile { path: file.clone(), text: "overwritten".into() };
                app.host_call(WRITE_FILE, &args, |app, r: Result<(), String>| app.notice(&r.unwrap_err()));
            },
            |app| transcript_text(app).contains("not a file"),
        );
        assert!(now.contains("not a file the host offered"), "{now}");
        assert_eq!(now, later);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "secret");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_handed_out_file_reads_and_a_missing_one_reads_empty_only_when_asked() {
        let (dir, file) = scratch("the plan");
        let absent = dir.join("absent.md");
        hand_out(&file);
        hand_out(&absent);
        let read = |path: PathBuf, missing_ok: bool| {
            both_hosts(
                move |app| {
                    let args = ReadFile { path: path.clone(), missing_ok };
                    app.host_call(READ_FILE, &args, |app, r: Result<String, String>| {
                        app.notice(&format!("{r:?}"));
                    });
                },
                |app| transcript_text(app).contains("Ok(") || transcript_text(app).contains("Err("),
            )
        };
        let (now, later) = read(file.clone(), false);
        assert!(now.contains("Ok(\"the plan\")"), "{now}");
        assert_eq!(now, later);
        let (now, later) = read(absent.clone(), true);
        assert!(now.contains("Ok(\"\")"), "{now}");
        assert_eq!(now, later);
        let (now, later) = read(absent, false);
        assert!(now.contains("Err("), "{now}");
        assert_eq!(now, later);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_successful_edit_is_written_back_and_other_outcomes_are_not() {
        let (dir, file) = scratch("before");
        hand_out(&file);
        let run = |code: i32, text: &'static str, wait_for: &'static str| {
            let file = file.clone();
            both_hosts(
                move |app| {
                    std::fs::write(&file, "before").unwrap();
                    let tmp = private_temp_file(text).unwrap();
                    let status = std::process::ExitStatus::from_raw(code << 8);
                    let edit = HostEdit { host: file.clone(), tmp: tmp.clone(), before: "before".into() };
                    app.finish_host_edit(edit, Ok(status), "plan", |app, t| app.notice(&format!("saved {t}")));
                    assert!(!tmp.exists(), "the temp copy is removed");
                },
                move |app| transcript_text(app).contains(wait_for),
            )
        };
        let (now, later) = run(0, "after", "saved after");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
        assert_eq!(now, later);
        let (now, later) = run(1, "failed", "");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before", "a failed editor writes nothing");
        assert!(!now.contains("saved") && now == later);
        let (now, _) = run(0, "before", "");
        assert!(!now.contains("saved"), "an unchanged text is not written back");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod temp_tests {
    #[test]
    fn temp_file_is_private_and_unique() {
        use std::os::unix::fs::PermissionsExt;
        let a = super::private_temp_file("x").unwrap();
        let b = super::private_temp_file("y").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }
}

#[cfg(test)]
mod take_socket_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_take_over_socket_lives_in_a_private_dir_that_the_bridge_removes() {
        let dir = mu_common::fs::with_umask(0, private_socket_dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let sock = dir.join("take.sock");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let (out, _out_rx) = tokio::sync::mpsc::unbounded_channel();
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let tunnel = crate::host::Tunnel { id: 1, out, rx };
            let client = tokio::net::UnixStream::connect(&sock).await.unwrap();
            drop(client);
            bridge(listener, tunnel, sock.clone()).await;
        });
        assert!(!dir.exists(), "the socket and its directory are gone");
    }
}
