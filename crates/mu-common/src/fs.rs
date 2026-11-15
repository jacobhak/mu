//! Filesystem helpers shared by mu and its plugins.

use std::io::Write;
use std::path::Path;

/// Replaces `path` with `bytes` atomically: writes `<name>.tmp.<pid>` next to it, fsyncs, then
/// renames. A crash leaves either the old file or the new one, never a torn mix. An existing
/// file's permissions are kept.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_with(path, bytes, 0o666)
}

/// `write_atomic` with the mode a new file is created with (before the umask), for files that must
/// not be readable by others even briefly.
pub fn write_atomic_with(path: &Path, bytes: &[u8], new_mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!("{name}.tmp.{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(new_mode).open(&tmp)?;
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = f.set_permissions(meta.permissions());
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Creates `dir` and any missing parents with mode 0700. `dir` itself is set to 0700 even if it
/// already existed; existing parents are left alone.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Runs `f` with the process umask set to `mask`, then restores it. The umask is process-wide, so
/// keep `f` to the one syscall that needs it (a `bind`): a file another thread creates in that
/// window gets the stricter mask too.
pub fn with_umask<T>(mask: u32, f: impl FnOnce() -> T) -> T {
    // SAFETY: umask only swaps the process's file-creation mask.
    let old = unsafe { libc::umask(mask as libc::mode_t) };
    let out = f();
    // SAFETY: as above; puts the previous mask back.
    unsafe { libc::umask(old) };
    out
}

/// Drops group and other permissions from an open file (best effort), for files that held user
/// text and were created before they were made private.
pub fn tighten(file: &std::fs::File) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = file.metadata() {
        let mode = m.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

/// Opens `path` for appending, creating it with mode 0600 (and its directory with 0700). A file that
/// already exists is tightened to owner-only.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    let f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    tighten(&f);
    Ok(f)
}

/// Like `write_atomic`, for secrets: the temp file is created with mode 0600, so the contents
/// are never readable by others, not even briefly; the result keeps that mode.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!("{name}.tmp.{}", std::process::id()));
    let result = (|| {
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        sync_dir(path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Makes renames inside `dir` durable.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// An exclusive advisory lock (`flock`) on a file, released on drop. Works on Linux and macOS.
pub struct FileLock(std::fs::File);

impl FileLock {
    /// Blocks until the lock on `path` (created 0600 if missing) is held.
    pub fn exclusive(path: &Path) -> std::io::Result<Self> {
        let f = open_lock_file(path)?;
        loop {
            match flock(&f, libc::LOCK_EX) {
                Ok(()) => return Ok(Self(f)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The lock if it is free now, `None` if another holder has it.
    pub fn try_exclusive(path: &Path) -> std::io::Result<Option<Self>> {
        let f = open_lock_file(path)?;
        match flock(&f, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self(f))),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Polls for the lock every 50 ms for up to `wait`; `None` when it stayed held. Blocks the thread.
    pub fn exclusive_within(path: &Path, wait: std::time::Duration) -> std::io::Result<Option<Self>> {
        let start = std::time::Instant::now();
        loop {
            if let Some(lock) = Self::try_exclusive(path)? {
                return Ok(Some(lock));
            }
            if start.elapsed() >= wait {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

fn open_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)
}

fn flock(f: &std::fs::File, op: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `f` owns a valid descriptor for the duration of the call.
    if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: as above; closing the file would release the lock too.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod private_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mu-fs-private-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn private_files_are_0600_after_create_and_rewrite() {
        let d = dir("mode");
        let p = d.join("sub/auth.json");
        write_private(&p, b"one").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        write_private(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1, "no temp file left");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_private_append_creates_0600_and_tightens_an_existing_file() {
        let d = dir("append");
        let p = d.join("history.jsonl");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        writeln!(open_private_append(&p).unwrap(), "one").unwrap();
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(&d), 0o700);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        writeln!(open_private_append(&p).unwrap(), "two").unwrap();
        assert_eq!(mode(&p), 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_existing_private_dir_is_chmodded_but_its_parents_are_not() {
        let d = dir("chmod");
        let (parent, child) = (d.join("p"), d.join("p/c"));
        std::fs::create_dir_all(&child).unwrap();
        for p in [&parent, &child] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        create_private_dir(&child).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&child), 0o700);
        assert_eq!(mode(&parent), 0o755);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_held_lock_is_not_taken_by_try_or_within_and_is_free_after_drop() {
        let d = dir("try");
        let lock = d.join("x.lock");
        let held = FileLock::try_exclusive(&lock).unwrap().expect("free");
        assert!(FileLock::try_exclusive(&lock).unwrap().is_none());
        assert!(FileLock::exclusive_within(&lock, std::time::Duration::from_millis(120)).unwrap().is_none());
        drop(held);
        assert!(FileLock::exclusive_within(&lock, std::time::Duration::from_millis(120)).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_lock_serialises_writers() {
        let d = dir("lock");
        let lock = d.join("x.lock");
        let counter = d.join("n");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(&counter, "0").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (lock, counter) = (lock.clone(), counter.clone());
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let _g = FileLock::exclusive(&lock).unwrap();
                        let n: u32 = std::fs::read_to_string(&counter).unwrap().parse().unwrap();
                        std::thread::yield_now();
                        std::fs::write(&counter, (n + 1).to_string()).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "160");
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_dir_is_0700_under_a_umask_of_zero_and_the_mask_comes_back() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("mu-fs-umask-{}", std::process::id()));
        let leaf = d.join("a").join("run");
        // SAFETY: reads the mask by setting and restoring it; no other test here changes it.
        let before = unsafe { libc::umask(0o022) };
        with_umask(0, || create_private_dir(&leaf)).unwrap();
        let after = unsafe { libc::umask(before) };
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&leaf), 0o700);
        assert_eq!(mode(&d.join("a")), 0o700);
        assert_eq!(after as u32, 0o022, "the mask is restored");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn replaces_and_leaves_no_temp_file() {
        let d = std::env::temp_dir().join(format!("mu-fs-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("a.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_file_gets_the_given_mode_and_an_existing_one_keeps_its_own() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("mu-fs-mode-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("m.json");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        write_atomic_with(&p, b"one", 0o600).unwrap();
        assert_eq!(mode(&p), 0o600);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic_with(&p, b"two", 0o600).unwrap();
        assert_eq!(mode(&p), 0o644);
        let _ = std::fs::remove_dir_all(&d);
    }
}
