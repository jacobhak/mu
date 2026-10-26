//! Filesystem helpers shared by mu and its plugins.

use std::io::Write;
use std::path::Path;

/// Replaces `path` with `bytes` atomically: writes `<name>.tmp.<pid>` next to it, fsyncs, then
/// renames. A crash leaves either the old file or the new one, never a torn mix. An existing
/// file's permissions are kept.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!("{name}.tmp.{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
