//! Process-group helpers shared by the tools that spawn children.

use std::time::Duration;

/// Time between SIGTERM and SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_millis(200);
/// How long a command's leftover children may keep its stdout open after it exits.
const LEFTOVER_GRACE: Duration = Duration::from_millis(500);

/// Sends `kill(pid, sig)` to that one pid (never a group). `Err` carries the errno.
pub fn signal_pid(pid: u32, sig: libc::c_int) -> std::io::Result<()> {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    // SAFETY: plain syscall, no memory involved.
    if unsafe { libc::kill(pid, sig) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Starts a new session with no controlling terminal. For `pre_exec`: only async-signal-safe calls.
pub fn new_session() -> std::io::Result<()> {
    // SAFETY: plain syscall, no memory involved.
    if unsafe { libc::setsid() } < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Non-blocking `flock(op)` on `file`: `WouldBlock` when another holder has the lock.
pub fn try_flock(file: &std::fs::File, op: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the fd is valid for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Terminates a whole process group: SIGTERM, then SIGKILL after `TERM_GRACE`.
/// `pgid` must be a group the caller created (a child spawned with `process_group(0)` or `setsid`).
pub async fn terminate_group(pgid: i32) {
    if pgid <= 1 {
        return;
    }
    // Signal 0 only asks whether any process is left in the group; usually none, and no delay.
    // SAFETY: plain kill(2).
    if unsafe { libc::kill(-pgid, 0) } != 0 {
        return;
    }
    // SAFETY: plain kill(2) on a group we own.
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    tokio::time::sleep(TERM_GRACE).await;
    // SAFETY: as above; ESRCH if the group is already gone is fine.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// Spawns `cmd` as the leader of a new process group, so `terminate_group` reaches everything it
/// starts. With `pdeathsig`, Linux also sends the child SIGTERM when mu dies.
pub fn spawn_grouped(cmd: &mut tokio::process::Command, pdeathsig: bool) -> std::io::Result<tokio::process::Child> {
    cmd.process_group(0);
    if pdeathsig {
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    cmd.spawn()
}

/// Runs `cmd` with its stdout captured. `None` on failure, timeout or a non-zero exit. The command
/// runs in its own process group, which is terminated when `limit` passes, or when it has exited
/// but a leftover child still holds the pipe open. Children that let go of stdout are left alone.
pub async fn run_capture(mut cmd: tokio::process::Command, limit: Duration) -> Option<String> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).process_group(0);
    let mut child = cmd.spawn().ok()?;
    // The group id is the leader's pid; read it now, before `wait` reaps the leader.
    let pgid = child.id().map(|p| p as i32)?;
    let mut out = child.stdout.take()?;
    let run = async {
        let mut buf = Vec::new();
        let (read, st) = {
            let read = out.read_to_end(&mut buf);
            tokio::pin!(read);
            let mut done = None;
            let st = loop {
                tokio::select! {
                    r = &mut read, if done.is_none() => done = Some(r),
                    st = child.wait() => break st,
                }
            };
            let read = match done {
                Some(r) => r,
                None => match tokio::time::timeout(LEFTOVER_GRACE, &mut read).await {
                    Ok(r) => r,
                    Err(_) => {
                        terminate_group(pgid).await;
                        read.await
                    }
                },
            };
            (read, st)
        };
        read.ok()?;
        st.ok()?.success().then(|| String::from_utf8_lossy(&buf).into_owned())
    };
    match tokio::time::timeout(limit, run).await {
        Ok(out) => out,
        Err(_) => {
            terminate_group(pgid).await;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn spawn_grouped_makes_the_child_its_own_group_leader() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");
        let mut child = rt.block_on(async { spawn_grouped(&mut cmd, true) }).unwrap();
        let pid = child.id().unwrap() as i32;
        // SAFETY: plain getpgid(2) on the test's own child.
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        rt.block_on(terminate_group(pid));
        assert!(!rt.block_on(child.wait()).unwrap().success());
    }

    #[test]
    fn terminates_a_stubborn_group() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        rt.block_on(terminate_group(child.id() as i32));
        let st = child.wait().unwrap();
        assert!(!st.success());
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the pid exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_timed_out_command_takes_its_grandchildren_with_it() {
        let dir = std::env::temp_dir().join(format!("mu-proc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        assert!(runtime().block_on(run_capture(c, Duration::from_millis(500))).is_none());
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let gone = (0..50).any(|_| {
            let dead = !alive(pid);
            if !dead {
                std::thread::sleep(Duration::from_millis(100));
            }
            dead
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "grandchild {pid} survived the timeout");
    }

    #[test]
    fn a_leftover_child_holding_stdout_does_not_hold_the_read() {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg("sleep 30 & echo ok");
        let started = std::time::Instant::now();
        let out = runtime().block_on(run_capture(c, Duration::from_secs(5)));
        assert_eq!(out.as_deref(), Some("ok\n"));
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }

    /// A daemon a command starts (an agent it talks to) outlives it once it lets go of stdout.
    #[test]
    fn a_child_that_released_stdout_is_left_running() {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg("sleep 30 >/dev/null & echo $!");
        let out = runtime().block_on(run_capture(c, Duration::from_secs(5))).unwrap();
        let pid: i32 = out.trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let survived = alive(pid);
        // SAFETY: plain kill(2) on the test's own child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(survived, "the released child {pid} was killed");
    }
}
