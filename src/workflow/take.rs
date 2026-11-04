//! `mu take <session|id>`: stop a running `mu -p` at its next tool boundary, then resume the
//! session here, in the TUI. Only POSIX calls (`flock`, `kill`, `exec`), so it is portable.
use mu_common::paths;
use mu_session::control::{Recheck, Want, recheck, signal};
use mu_session::session::{self, Owner};
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

pub fn run(arg: &Path, cwd: &Path) -> anyhow::Result<i32> {
    let path = match session::find(cwd, arg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mu take: {e}");
            return Ok(1);
        }
    };
    // The cwd changes before the exec, so a relative path must not be left relative.
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    // Checked before anything is signalled: a stopped run with nothing to open it is a loss.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("mu take: needs a terminal on stdin and stdout; nothing was signalled");
        return Ok(1);
    }
    match session::owner(&path) {
        Owner::Free => eprintln!("not running; resuming"),
        Owner::Live { pid, print_run: false, .. } => {
            eprintln!("mu take: pid {pid} has it open in an interactive mu (or a subagent/ACP); close it there");
            return Ok(1);
        }
        Owner::Unknown => {
            eprintln!(
                "mu take: the session is locked by a process that left no owner record (an older mu, or another MU_HOME); stop it yourself"
            );
            return Ok(1);
        }
        Owner::Live { pid, print_run: true, started, .. } => {
            if let Some(code) = stop_and_wait(&path, pid, &started)? {
                return Ok(code);
            }
        }
    }
    resume(&path)
}

/// `Ok(None)`: the session is free, go on. `Ok(Some(code))`: stop with that exit code.
fn stop_and_wait(path: &Path, pid: u32, started: &str) -> anyhow::Result<Option<i32>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    // Registered before signalling, so a Ctrl-C right after is not lost.
    let mut int = {
        let _g = rt.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?
    };
    // The record was read a moment ago; make sure it is still the same live run before the first kill.
    match recheck(session::owner(path), pid, started, Want::PrintRun) {
        Recheck::Same => {}
        Recheck::Free => return Ok(None),
        Recheck::Changed => {
            eprintln!("mu take: the session changed hands while starting; nothing was signalled, try again");
            return Ok(Some(1));
        }
    }
    match signal(pid) {
        Ok(()) => eprintln!("asked pid {pid} to stop after its current step; Ctrl-C to stop now"),
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
        Err(e) => {
            eprintln!("mu take: cannot signal pid {pid}: {e}");
            return Ok(Some(1));
        }
    }
    let code = rt.block_on(async {
        // (borrowed from the enclosing scope: `int` outlives this block)
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let (mut waited, mut interrupts) = (0u32, 0u32);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if session::is_free(path) {
                        return None;
                    }
                    waited += 1;
                    if waited % 150 == 0 {
                        eprintln!("still waiting for pid {pid} to finish its current step ({}s)", waited / 5);
                    }
                }
                _ = int.recv() => {
                    interrupts += 1;
                    if interrupts >= 2 {
                        eprintln!("giving up; the run will stop on its own");
                        return Some(130);
                    }
                    // Only signal again while it is still the same live run: the pid may have
                    // been reused after it exited.
                    if recheck(session::owner(path), pid, started, Want::PrintRun) == Recheck::Same {
                        let _ = signal(pid);
                        eprintln!("asked pid {pid} to stop now");
                    }
                }
            }
        }
    });
    drop(rt);
    Ok(code)
}

fn resume(path: &Path) -> anyhow::Result<i32> {
    use std::os::unix::process::CommandExt;
    if let Some(dir) = session::header_cwd(path) {
        if dir.is_dir() {
            std::env::set_current_dir(&dir)?;
        } else {
            eprintln!("mu take: {} no longer exists; staying in the current directory", dir.display());
        }
    }
    let exe = paths::self_exe()?;
    let err = std::process::Command::new(exe).arg("--resume").arg(path).exec();
    eprintln!("mu take: cannot exec mu: {err}");
    Ok(1)
}
