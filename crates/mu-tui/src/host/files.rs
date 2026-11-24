//! Reading and writing arbitrary files on the host: what `/open` and `/upload` use when no attach
//! helper carries the bytes. Unlike `plans`, there is no list of offered paths: a client that can
//! attach to the agent's socket is as trusted as the agent's user (it can read `auth.json` anyway).

use super::local::{Local, Registry};
use super::{Answer, ready};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::rc::Rc;

pub const READ_BYTES: &str = "read_bytes";
pub const WRITE_BYTES: &str = "write_bytes";

/// The largest file sent over the attach stream; its base64 stays under the 64 MiB line limit.
pub const STREAM_FILE_MAX: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadBytes {
    pub path: PathBuf,
    pub max: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadReply {
    pub size: u64,
    pub data_b64: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteBytes {
    pub path: PathBuf,
    pub data_b64: String,
}

pub(super) fn register(r: &mut Registry) {
    r.add(READ_BYTES, |_: &Rc<Local>, args| -> Answer { ready(args, read_bytes) });
    r.add(WRITE_BYTES, |_: &Rc<Local>, args| -> Answer { ready(args, write_bytes) });
}

fn read_bytes(a: ReadBytes) -> Result<ReadReply, String> {
    let data = read_capped(&a.path, a.max)?;
    Ok(ReadReply { size: data.len() as u64, data_b64: base64::engine::general_purpose::STANDARD.encode(&data) })
}

/// The bytes of a regular file of at most `max` bytes.
pub fn read_capped(path: &std::path::Path, max: u64) -> Result<Vec<u8>, String> {
    let shown = path.display();
    let file = std::fs::File::open(path).map_err(|e| format!("{shown}: {e}"))?;
    let meta = file.metadata().map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown} is not a file"));
    }
    let mut data = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut data).map_err(|e| format!("{shown}: {e}"))?;
    if data.len() as u64 > max {
        return Err(format!("{shown} is larger than {max} bytes"));
    }
    Ok(data)
}

fn write_bytes(a: WriteBytes) -> Result<(), String> {
    let data =
        base64::engine::general_purpose::STANDARD.decode(&a.data_b64).map_err(|e| format!("bad file data: {e}"))?;
    write_new(&a.path, &data)
}

/// Creates `path` (an absolute path) with its parent directories, mode 0600; an existing file is an error.
pub fn write_new(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("{shown} is not an absolute path"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::AlreadyExists => format!("{shown} already exists"),
                _ => format!("{shown}: {e}"),
            }
        })?;
    file.write_all(data).map_err(|e| format!("{shown}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn a_written_file_reads_back() {
        let dir = mu_testutil::tmp();
        let path = dir.path().join("a/b/c.bin");
        write_bytes(WriteBytes { path: path.clone(), data_b64: b64(&[0, 1, 2, 255]) }).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let reply = read_bytes(ReadBytes { path, max: 10 }).unwrap();
        assert_eq!(reply.size, 4);
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(reply.data_b64).unwrap(), [0, 1, 2, 255]);
    }

    #[test]
    fn a_write_never_replaces_a_file() {
        let dir = mu_testutil::tmp();
        let path = dir.path().join("x");
        std::fs::write(&path, "keep").unwrap();
        let err = write_bytes(WriteBytes { path: path.clone(), data_b64: b64(b"new") }).unwrap_err();
        assert!(err.ends_with("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
    }

    #[test]
    fn a_relative_write_path_is_refused() {
        let err = write_bytes(WriteBytes { path: "x".into(), data_b64: b64(b"") }).unwrap_err();
        assert!(err.contains("not an absolute path"), "{err}");
    }

    #[test]
    fn a_read_over_the_limit_fails() {
        let dir = mu_testutil::tmp();
        let path = dir.path().join("big");
        std::fs::write(&path, [7u8; 11]).unwrap();
        let err = read_bytes(ReadBytes { path: path.clone(), max: 10 }).unwrap_err();
        assert!(err.ends_with("is larger than 10 bytes"), "{err}");
        assert_eq!(read_bytes(ReadBytes { path, max: 11 }).unwrap().size, 11);
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let dir = mu_testutil::tmp();
        let err = read_bytes(ReadBytes { path: dir.path().to_path_buf(), max: 10 }).unwrap_err();
        assert!(err.ends_with("is not a file"), "{err}");
    }
}
