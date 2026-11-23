//! Process-group helpers shared by the tools that spawn children.

use std::time::Duration;

/// Time between SIGTERM and SIGKILL.
pub(crate) const TERM_GRACE: Duration = Duration::from_millis(200);

thread_local! {
    static TERM_GRACE_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// Test seam: `terminate_group` calls on this thread wait `grace` instead of `TERM_GRACE`, so a test
/// can hold the SIGTERM-to-SIGKILL window open without racing it. `None` restores the default.
#[doc(hidden)]
pub fn set_term_grace_for_test(grace: Option<Duration>) {
    TERM_GRACE_OVERRIDE.with(|g| g.set(grace));
}

/// How long a command's leftover children may keep its stdout open after it exits.
const LEFTOVER_GRACE: Duration = Duration::from_millis(500);

/// Sends `kill(pid, sig)` to that one pid (never a group). `Err` carries the errno.
pub fn signal_pid(pid: u32, sig: libc::c_int) -> std::io::Result<()> {
    // pid 0 would signal the caller's own group.
    let Ok(pid @ 1..) = libc::pid_t::try_from(pid) else {
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

/// Sends `sig` to every process in group `pgid`. Never signals pid 0, 1 or a negative group: the
/// caller must have created the group (a child spawned with `process_group(0)` or `setsid`).
/// Returns whether the signal was delivered to at least one process.
pub fn kill_group(pgid: i32, sig: libc::c_int) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: plain kill(2) with a negative pid addresses a process group.
    unsafe { libc::kill(-pgid, sig) == 0 }
}

/// Terminates a whole process group: SIGTERM, then SIGKILL after `TERM_GRACE`.
/// `pgid` must be a group the caller created (a child spawned with `process_group(0)` or `setsid`).
pub async fn terminate_group(pgid: i32) {
    // Signal 0 only asks whether any process is left in the group; usually none, and no delay.
    if !kill_group(pgid, 0) {
        return;
    }
    kill_group(pgid, libc::SIGTERM);
    tokio::time::sleep(TERM_GRACE_OVERRIDE.with(std::cell::Cell::get).unwrap_or(TERM_GRACE)).await;
    // ESRCH if the group is already gone is fine.
    kill_group(pgid, libc::SIGKILL);
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
pub async fn run_capture(cmd: tokio::process::Command, limit: Duration) -> Option<String> {
    run_capture_until(cmd, tokio::time::sleep(limit)).await
}

/// `run_capture` with the deadline as a future, so a test can decide when the command times out.
async fn run_capture_until(
    mut cmd: tokio::process::Command,
    expired: impl std::future::Future<Output = ()>,
) -> Option<String> {
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
    tokio::select! {
        out = run => out,
        () = expired => {
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
    fn signal_pid_refuses_pid_zero() {
        // Signal 0 only checks; pid 0 would address the caller's own group.
        assert_eq!(signal_pid(0, 0).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert!(signal_pid(std::process::id(), 0).is_ok());
    }

    #[test]
    fn kill_group_refuses_pid_zero_one_and_negatives() {
        assert!(!kill_group(0, 0));
        assert!(!kill_group(1, 0));
        assert!(!kill_group(-5, libc::SIGKILL));
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

    /// Whether `pid` is still running. A killed process whose parent is gone stays a zombie until
    /// something reaps it, and `kill(pid, 0)` succeeds on a zombie; in a container with no reaper
    /// (the release build) that lasts forever, so ask the kernel for its state.
    fn running(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the pid exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            // Field 3 of /proc/<pid>/stat; field 2 (the command) may hold spaces and parentheses.
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return true };
            let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next());
            !matches!(state, Some("Z" | "X" | "x"))
        }
        #[cfg(target_os = "macos")]
        {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            // SAFETY: `info` is `size` bytes and proc_pidinfo writes at most that many.
            let n = unsafe {
                libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size)
            };
            n != size || info.pbi_status != libc::SZOMB
        }
    }

    #[test]
    fn a_timed_out_command_takes_its_grandchildren_with_it() {
        let dir = std::env::temp_dir().join(format!("mu-proc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        // The deadline passes only once the grandchild has announced its pid.
        let rt = runtime();
        let expired = async {
            mu_testutil::pid_in(&pidfile).await;
        };
        assert!(rt.block_on(run_capture_until(c, expired)).is_none());
        let pid = rt.block_on(mu_testutil::pid_in(&pidfile));
        rt.block_on(mu_testutil::wait_until("the timed-out grandchild to be gone", || !running(pid)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leftover_child_holding_stdout_does_not_hold_the_read() {
        let dir = std::env::temp_dir().join(format!("mu-proc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(format!("sleep 600 & echo $! > {}; echo ok", pidfile.display()));
        let rt = runtime();
        // Success before the limit (a failure-only backstop) means the leftover holding stdout was
        // terminated rather than waited out; otherwise the limit fires and the output is None.
        let out = rt.block_on(run_capture(c, Duration::from_secs(10)));
        assert_eq!(out.as_deref(), Some("ok\n"));
        let pid = rt.block_on(mu_testutil::pid_in(&pidfile));
        rt.block_on(mu_testutil::wait_until("the leftover to be terminated", || !running(pid)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon a command starts (an agent it talks to) outlives it once it lets go of stdout.
    #[test]
    fn a_child_that_released_stdout_is_left_running() {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg("sleep 30 >/dev/null & echo $!");
        let out = runtime().block_on(run_capture(c, Duration::from_secs(5))).unwrap();
        let pid: i32 = out.trim().parse().unwrap();
        let survived = running(pid);
        // SAFETY: plain kill(2) on the test's own child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(survived, "the released child {pid} was killed");
    }
}
