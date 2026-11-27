//! Small file helpers: private directories and files.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Creates `dir` (and its parents) with mode 0700 and tightens it when it exists.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Writes `bytes` to `path` with mode 0600 (replacing it), creating the parent directory privately.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)
}

/// Runs `f` with the process umask set to `mask`, then puts the old one back.
pub fn with_umask<T>(mask: u32, f: impl FnOnce() -> T) -> T {
    // SAFETY: umask is a plain syscall; callers run it before they start threads that create files.
    let old = unsafe { libc::umask(mask as libc::mode_t) };
    let out = f();
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    out
}
