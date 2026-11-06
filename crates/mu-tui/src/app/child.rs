//! Programs that take the terminal while they run: `omp login`, `omp /mcp reauth`, `$EDITOR` and
//! `mu take`. The terminal is handed over, and the screen is redrawn when the program exits.

use super::ui_msgs::UiMsg;
use super::*;
use crate::render::Renderer;
use crate::term::term;

/// What to do when a child that owned the terminal exits.
pub(super) enum ChildThen {
    Login,
    /// `omp -p '/mcp reauth <name>'` for this MCP server.
    McpLogin(String),
    /// The external editor was run on the composer text kept in this file.
    ComposerEdit(PathBuf),
    /// The editor was run on the plan file shown in the review overlay.
    PlanEdit(PathBuf),
    /// The editor was run on a memory file from `/memory`; the file is read on the next use.
    MemoryEdit,
    /// `mu take` ran on the terminal for a session picked in the jobs panel.
    Take,
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
                    let (plugins, tx) = (self.plugins.clone(), self.ui_tx.clone());
                    tokio::task::spawn_local(async move {
                        let r = plugins.after_login(&name).await;
                        let _ = tx.send(UiMsg::McpLoggedIn(name, r));
                    });
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
            ChildThen::MemoryEdit => {
                if let Err(e) = status {
                    self.error(&format!("editor failed: {e}"));
                }
            }
            ChildThen::PlanEdit(path) => {
                if let Err(e) = status {
                    self.error(&format!("editor failed: {e}"));
                }
                match (std::fs::read_to_string(&path), &mut self.overlay) {
                    (Ok(t), Some(Overlay::Review { review, .. })) => review.set_content(t),
                    (Err(e), _) => self.error(&format!("cannot read plan: {e}")),
                    _ => {}
                }
            }
        }
    }

    /// Runs `omp login` on the real terminal. With `pick`, omp's own provider menu is swallowed and
    /// answered with that number once its prompt appears; terminal input is then relayed so OAuth
    /// prompts still work. The gateway reads the stored credentials.
    pub(super) fn run_login(&mut self, args: &[&str], pick: Option<usize>) {
        use std::process::Stdio;
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.start_child(ChildThen::Login, move || {
            let mut cmd = std::process::Command::new(mu_common::paths::omp_bin());
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
            status
        });
    }

    /// Runs omp's own MCP OAuth flow (`omp -p --no-session '/mcp reauth <name>'`) on the real terminal,
    /// then makes the server usable in this session (`PluginHost::after_login`).
    pub(super) fn run_mcp_login(&mut self, name: &str) {
        if !self.plugins.mcp_names().iter().any(|n| n == name) {
            return self.error(&format!("no MCP server `{name}` is configured"));
        }
        let (cwd, name) = (self.cwd.clone(), name.to_string());
        self.start_child(ChildThen::McpLogin(name.clone()), move || {
            let mut cmd = std::process::Command::new(mu_common::paths::omp_bin());
            cmd.args(["-p", "--no-session"]).arg(format!("/mcp reauth {name}")).current_dir(&cwd);
            extern "C" fn noop(_: libc::c_int) {}
            // As in `run_login`: Ctrl-C reaches omp, not mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = cmd.status();
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            status
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
