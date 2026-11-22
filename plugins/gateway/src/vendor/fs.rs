//! Vendored from mu's `mu_common::fs` (crates/mu-common/src/fs.rs): `create_private_dir`, `write_private` and
//! `FileLock`; kept in step by hand.

use std::io::Write;
use std::path::Path;

/// Creates `dir` and any missing parents with mode 0700. `dir` itself is set to 0700 even if it
/// already existed; existing parents are left alone.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Replaces `path` atomically for secrets: the temp file is created with mode 0600, so the contents
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
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("apg-fs-{tag}-{}", std::process::id()));
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
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1, "no temp file left");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_held_lock_is_not_taken_by_try_and_is_free_after_drop() {
        let d = dir("try");
        let lock = d.join("x.lock");
        let held = FileLock::try_exclusive(&lock).unwrap().expect("free");
        assert!(FileLock::try_exclusive(&lock).unwrap().is_none());
        drop(held);
        assert!(FileLock::try_exclusive(&lock).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
