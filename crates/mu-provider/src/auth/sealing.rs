//! Moves mu's own credential files onto the sealed layer when the agent's state key is unlocked.
use super::store::SEALED_NAME;
use anyhow::{Context, Result};
use mu_common::sealed;
use std::path::Path;

/// Seals the plaintext `auth.json` of `profile_home` (idempotent). Called once the state key is unlocked.
///
/// A no-op when state encryption is off or locked, or when the file is absent or already sealed.
/// The rewrite is atomic and 0600, under the store's lock file, and removes any `auth.json.tmp.*`
/// a crashed writer left. On copy-on-write or snapshotting storage, older snapshots keep the plaintext.
pub fn seal_existing(profile_home: &Path) -> Result<()> {
    if !sealed::is_initialised(profile_home) || !sealed::is_unlocked() {
        return Ok(());
    }
    let path = profile_home.join(SEALED_NAME);
    let lock = profile_home.join(format!("{SEALED_NAME}.lock"));
    let _lock = mu_common::fs::FileLock::exclusive(&lock).with_context(|| format!("locking {}", lock.display()))?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if !sealed::is_sealed(&bytes) {
        sealed::write_sealed(&path, SEALED_NAME, &bytes).with_context(|| format!("sealing {}", path.display()))?;
    }
    remove_temp_files(profile_home);
    Ok(())
}

fn remove_temp_files(dir: &Path) {
    let prefix = format!("{SEALED_NAME}.tmp.");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::{Entry, Store};
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    const SECRET: &str = "sk-ant-test-secret-0123456789";
    static SERIAL: Mutex<()> = Mutex::new(());

    /// The unlocked state is process-global: these tests run one at a time, starting locked.
    fn serial() -> MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        sealed::lock();
        g
    }

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-sealing-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn contains(bytes: &[u8], needle: &str) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn with_state_on_a_key_is_stored_sealed_and_read_back() {
        let _g = serial();
        let h = home("on");
        sealed::init_raw(&h, &[5; 32]).unwrap();
        let s = Store::at(h.join("auth.json"));
        s.set("anthropic", Entry::literal(SECRET)).unwrap();
        let raw = std::fs::read(h.join("auth.json")).unwrap();
        assert!(sealed::is_sealed(&raw) && !contains(&raw, SECRET) && !contains(&raw, "anthropic"));
        assert_eq!(s.get("anthropic").unwrap().key.as_deref(), Some(SECRET));
        assert_eq!(crate::auth::store::Store::at(h.join("auth.json")).list().len(), 1);
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_locked_store_fails_with_the_locked_text_and_unlocking_takes_effect() {
        let _g = serial();
        let h = home("locked");
        sealed::init_raw(&h, &[5; 32]).unwrap();
        let s = Store::at(h.join("auth.json"));
        s.set("anthropic", Entry::literal(SECRET)).unwrap();
        assert!(s.get("anthropic").is_some());
        sealed::lock();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert_eq!(rt.block_on(s.resolve("anthropic")).err().unwrap().to_string(), "locked: attach to unlock");
        assert!(s.get("anthropic").is_none(), "a locked file is not served from the cache");
        assert_eq!(s.set("openai", Entry::literal(SECRET)).unwrap_err().to_string(), "locked: attach to unlock");
        assert_eq!(s.remove("anthropic").unwrap_err().to_string(), "locked: attach to unlock");
        assert!(s.is_locked() && s.check().is_ok());
        sealed::unlock_raw(&h, &[5; 32]).unwrap();
        assert!(!s.is_locked());
        assert_eq!(rt.block_on(s.resolve("anthropic")).unwrap().key, SECRET);
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_locked_store_with_no_file_still_refuses_to_write() {
        let _g = serial();
        let h = home("locked-empty");
        sealed::init_raw(&h, &[5; 32]).unwrap();
        sealed::lock();
        let s = Store::at(h.join("auth.json"));
        assert!(s.get("anthropic").is_none() && !s.is_locked());
        assert_eq!(s.set("anthropic", Entry::literal(SECRET)).unwrap_err().to_string(), "locked: attach to unlock");
        assert!(!h.join("auth.json").exists());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_plaintext_file_is_read_then_sealed_and_no_plaintext_is_left() {
        let _g = serial();
        let h = home("migrate");
        let path = h.join("auth.json");
        let s = Store::at(path.clone());
        s.set("anthropic", Entry::literal(SECRET)).unwrap();
        assert!(contains(&std::fs::read(&path).unwrap(), SECRET), "state off writes plaintext");
        std::fs::write(h.join("auth.json.tmp.99999"), SECRET).unwrap();
        sealed::init_raw(&h, &[5; 32]).unwrap();
        assert_eq!(s.get("anthropic").unwrap().key.as_deref(), Some(SECRET), "legacy plaintext still reads");
        sealing_cycle(&h);
        let raw = std::fs::read(&path).unwrap();
        assert!(sealed::is_sealed(&raw) && !contains(&raw, SECRET));
        assert!(!h.join("auth.json.tmp.99999").exists());
        assert_eq!(s.get("anthropic").unwrap().key.as_deref(), Some(SECRET));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&h);
    }

    /// Runs twice: the second pass changes nothing.
    fn sealing_cycle(h: &Path) {
        seal_existing(h).unwrap();
        let first = std::fs::read(h.join("auth.json")).unwrap();
        seal_existing(h).unwrap();
        assert_eq!(std::fs::read(h.join("auth.json")).unwrap(), first);
    }

    #[test]
    fn sealing_is_a_no_op_when_off_locked_or_absent() {
        let _g = serial();
        let h = home("noop");
        seal_existing(&h).unwrap();
        std::fs::write(h.join("auth.json"), "{}").unwrap();
        seal_existing(&h).unwrap();
        assert_eq!(std::fs::read(h.join("auth.json")).unwrap(), b"{}", "state off");
        sealed::init_raw(&h, &[5; 32]).unwrap();
        sealed::lock();
        seal_existing(&h).unwrap();
        assert_eq!(std::fs::read(h.join("auth.json")).unwrap(), b"{}", "locked");
        sealed::unlock_raw(&h, &[5; 32]).unwrap();
        std::fs::remove_file(h.join("auth.json")).unwrap();
        seal_existing(&h).unwrap();
        assert!(!h.join("auth.json").exists());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn state_off_keeps_the_plaintext_format() {
        let _g = serial();
        let h = home("off");
        let s = Store::at(h.join("auth.json"));
        s.set("anthropic", Entry::literal(SECRET)).unwrap();
        let text = std::fs::read_to_string(h.join("auth.json")).unwrap();
        assert!(text.starts_with("{\n  \"version\": 2") && text.contains(SECRET), "{text}");
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn an_envelope_for_another_file_is_refused() {
        let _g = serial();
        let h = home("swap");
        sealed::init_raw(&h, &[5; 32]).unwrap();
        let swapped = sealed::seal("other.json", br#"{"version":2,"providers":{}}"#).unwrap();
        std::fs::write(h.join("auth.json"), swapped).unwrap();
        let s = Store::at(h.join("auth.json"));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let e = format!("{:#}", rt.block_on(s.resolve("anthropic")).err().unwrap());
        assert!(e.contains("does not belong here"), "{e}");
        assert!(s.set("anthropic", Entry::literal(SECRET)).is_err(), "a refused file is never overwritten");
        let _ = std::fs::remove_dir_all(&h);
    }
}
