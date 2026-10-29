//! Process-group helpers shared by the tools that spawn children.

use std::time::Duration;

/// Time between SIGTERM and SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_millis(200);

/// Sends `kill(pid, sig)` to that one pid (never a group). `Err` carries the errno.
pub fn signal_pid(pid: u32, sig: libc::c_int) -> std::io::Result<()> {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    // SAFETY: plain syscall, no memory involved.
    if unsafe { libc::kill(pid, sig) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

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
}
