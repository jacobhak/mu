//! A namespaced secret store for plugins: `<profile_home>/secrets/<ns>/<name>`, sealed under the state key
//! when state encryption is on, plain 0600 files otherwise.
use crate::sealed::{self, SealedError};
use std::path::{Path, PathBuf};

fn valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn path(profile_home: &Path, ns: &str, name: &str) -> Result<PathBuf, SealedError> {
    if !valid(ns) || !valid(name) {
        return Err(SealedError::Format("invalid secret name".into()));
    }
    Ok(profile_home.join("secrets").join(ns).join(name))
}

fn logical(ns: &str, name: &str) -> String {
    format!("secret:{ns}/{name}")
}

/// Whether state encryption is on, so values are sealed.
pub fn sealed(profile_home: &Path) -> bool {
    sealed::is_initialised(profile_home)
}

pub fn get(profile_home: &Path, ns: &str, name: &str) -> Result<Option<Vec<u8>>, SealedError> {
    let p = path(profile_home, ns, name)?;
    if sealed(profile_home) {
        return sealed::read_sealed(&p, &logical(ns, name));
    }
    match std::fs::read(&p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn put(profile_home: &Path, ns: &str, name: &str, bytes: &[u8]) -> Result<(), SealedError> {
    let p = path(profile_home, ns, name)?;
    if sealed(profile_home) {
        return sealed::write_sealed(&p, &logical(ns, name), bytes);
    }
    crate::fs::write_private(&p, bytes)?;
    Ok(())
}

pub fn delete(profile_home: &Path, ns: &str, name: &str) -> Result<bool, SealedError> {
    let p = path(profile_home, ns, name)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// One-time move of the sealed copies older mu versions kept under `keys/omp-*.sealed` into the `gateway`
/// plugin's namespace. Removable once no install has them.
pub fn migrate_legacy(profile_home: &Path) -> Result<(), SealedError> {
    const MOVES: [(&str, &str); 3] = [
        ("omp-agent.db", "agent.db"),
        ("omp-auth-broker.token", "auth-broker.token"),
        ("omp-auth-gateway.token", "auth-gateway.token"),
    ];
    for (old, new) in MOVES {
        let file = profile_home.join("keys").join(format!("{old}.sealed"));
        if !file.exists() {
            continue;
        }
        let Some(bytes) = sealed::read_sealed(&file, old)? else { continue };
        put(profile_home, "plugin-gateway", new, &bytes)?;
        std::fs::remove_file(&file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::TEST_SERIAL;
    use std::os::unix::fs::PermissionsExt;

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-plugin-secrets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        sealed::lock();
        g
    }

    #[test]
    fn a_secret_round_trips_in_a_private_file() {
        let _g = serial();
        let h = home("plain");
        assert_eq!(get(&h, "plugin-x", "a").unwrap(), None);
        put(&h, "plugin-x", "a", b"v1").unwrap();
        assert_eq!(get(&h, "plugin-x", "a").unwrap().as_deref(), Some(&b"v1"[..]));
        let f = h.join("secrets/plugin-x/a");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(delete(&h, "plugin-x", "a").unwrap());
        assert!(!delete(&h, "plugin-x", "a").unwrap());
        assert_eq!(get(&h, "plugin-x", "a").unwrap(), None);
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn a_secret_is_sealed_when_state_encryption_is_on() {
        let _g = serial();
        let h = home("sealed");
        sealed::init_raw(&h, &[7u8; 32]).unwrap();
        put(&h, "plugin-x", "a", b"token").unwrap();
        let on_disk = std::fs::read(h.join("secrets/plugin-x/a")).unwrap();
        assert!(sealed::is_sealed(&on_disk));
        assert_eq!(get(&h, "plugin-x", "a").unwrap().as_deref(), Some(&b"token"[..]));
        sealed::lock();
        assert!(matches!(get(&h, "plugin-x", "a"), Err(SealedError::Locked)));
        sealed::unlock_raw(&h, &[7u8; 32]).unwrap();
        assert_eq!(get(&h, "plugin-x", "a").unwrap().as_deref(), Some(&b"token"[..]));
        sealed::lock();
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn names_that_could_escape_are_rejected() {
        let _g = serial();
        let h = home("names");
        for bad in ["", ".", "..", "a/b", "../x", "a b", &"x".repeat(129)] {
            assert!(matches!(put(&h, "plugin-x", bad, b"v"), Err(SealedError::Format(_))), "{bad}");
            assert!(matches!(get(&h, bad, "a"), Err(SealedError::Format(_))), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn legacy_sealed_omp_copies_move_to_the_gateway_namespace() {
        let _g = serial();
        let h = home("legacy");
        sealed::init_raw(&h, &[9u8; 32]).unwrap();
        let old = h.join("keys/omp-agent.db.sealed");
        sealed::write_sealed(&old, "omp-agent.db", b"sqlite").unwrap();
        migrate_legacy(&h).unwrap();
        assert_eq!(get(&h, "plugin-gateway", "agent.db").unwrap().as_deref(), Some(&b"sqlite"[..]));
        assert!(!old.exists());
        migrate_legacy(&h).unwrap();
        sealed::lock();
        let _ = std::fs::remove_dir_all(&h);
    }
}
