//! Programs that take the terminal while they run: `$EDITOR`, `mu take` and `mu --attach`. The
//! terminal is handed over, and the screen is redrawn when the program exits.

use super::ui_msgs::UiMsg;
use super::*;
use crate::host::plans::{READ_FILE, ReadFile, WRITE_FILE, WriteFile};
use crate::render::Renderer;
use crate::term::term;

use crate::host::sessions::AttachTarget;
use mu_common::text::sh_quote as quote;

/// What to do when a child that owned the terminal exits.
pub(super) enum ChildThen {
    /// The external editor was run on the composer text kept in this file.
    ComposerEdit(PathBuf),
    /// The editor was run on a private copy of the plan file shown in the review overlay.
    DocEdit(HostEdit),
    /// The editor was run on a private copy of a memory file from `/memory`.
    MemoryEdit(HostEdit),
    /// `mu take` ran on the terminal for a session picked in the jobs panel.
    Take,
    /// `mu --attach` ran on the terminal for a session another host listed.
    Attach,
    /// The editor was run on a copy of a host file made by `/open`, in this directory, which goes now.
    OpenCopy(PathBuf),
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
            ChildThen::Attach => {
                match status {
                    Ok(s) if s.success() => self.notice("back from the attached session"),
                    Ok(s) => self.error(&format!("mu --attach exited with {s}")),
                    Err(e) => self.error(&format!("cannot run mu --attach: {e}")),
                }
                self.open_sessions();
            }
            ChildThen::OpenCopy(dir) => {
                if let Err(e) = status {
                    self.error(&format!("editor failed: {e}"));
                }
                let _ = std::fs::remove_dir_all(dir);
            }
            ChildThen::MemoryEdit(edit) => self.finish_host_edit(edit, status, "memory file", |_, _| {}),
            ChildThen::DocEdit(edit) => self.finish_host_edit(edit, status, "plan", |app, text| {
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
            // Ctrl-C reaches `mu take`, not this mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = std::process::Command::new(exe).arg("take").arg(&path).status();
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            status
        });
    }

    /// Hands the terminal to `mu --attach COMMAND`, then comes back to the Sessions tab.
    pub(super) fn attach_session(&mut self, target: AttachTarget) {
        let exe = match mu_common::paths::self_exe() {
            Ok(e) => e,
            Err(e) => return self.error(&format!("cannot locate mu: {e}")),
        };
        self.start_child(ChildThen::Attach, move || run_attach(&exe, &target));
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
                let command =
                    format!("{} attach --socket {}", quote(&exe.to_string_lossy()), quote(&sock.to_string_lossy()));
                let target = AttachTarget { command, helper: None, state_key: None };
                app.start_child(ChildThen::Take, move || run_attach(&exe, &target));
            },
        );
    }
}

/// `mu --attach COMMAND` on the terminal, with Ctrl-C going to it and not to this mu. The target's helper
/// becomes `MU_ATTACH_HELPER` and its state key `--state-key`.
fn run_attach(exe: &std::path::Path, target: &AttachTarget) -> std::io::Result<std::process::ExitStatus> {
    extern "C" fn noop(_: libc::c_int) {}
    // SAFETY: installs a no-op handler; restored to the default below.
    unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--attach").arg(&target.command);
    if let Some(key) = &target.state_key {
        cmd.arg("--state-key").arg(key);
    }
    if let Some(helper) = &target.helper {
        cmd.env("MU_ATTACH_HELPER", helper);
    }
    let status = cmd.status();
    // SAFETY: back to the default disposition.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    status
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
    private_dir("mu-take")
}

/// A fresh directory only this user can enter (0700), named `<prefix>-<8 hex>`, in `$XDG_RUNTIME_DIR` or the temp dir.
pub(super) fn private_dir(prefix: &str) -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("{prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    mu_common::fs::create_private_dir(&dir)?;
    Ok(dir)
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
    use crate::app::keys::tests::{in_local, test_app_with_commands};
    use crate::host::{Answer, Host, HostInfo, Pushed};
    use serde_json::Value;
    use std::cell::RefCell;

    /// A host on "another machine": it records every call.
    struct Remote {
        info: HostInfo,
        calls: Rc<RefCell<Vec<(String, Value)>>>,
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
            Answer::Ready(Ok(Value::Null))
        }

        fn pushed(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Pushed>> {
            None
        }
    }

    #[test]
    fn taking_a_session_over_beside_a_remote_host_asks_the_host_to_take_it() {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-remote-program-history"));
        let calls = Rc::new(RefCell::new(Vec::new()));
        app.host = Rc::new(Remote { info: HostInfo::default(), calls: calls.clone() });
        in_local(async {
            app.take_session("/s/a_b.jsonl".into());
            let (name, args) = calls.borrow()[0].clone();
            assert_eq!((name.as_str(), &args["path"]), ("take", &serde_json::json!("/s/a_b.jsonl")));
            assert!(!app.screen.child_busy, "the terminal stays with this UI until the host has answered");
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
            |app| app.edit_host_file(file.clone(), false, "plan", ChildThen::DocEdit),
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
