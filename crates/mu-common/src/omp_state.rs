//! omp's credential state kept sealed at rest: unsealed into a memory-backed directory for the omp processes mu starts.
//!
//! omp keeps its credentials in `agent.db` (SQLite, in its agent directory) and two loopback tokens in its config
//! root. `materialize` restores them from `<profile_home>/keys/omp-*.sealed` into a private directory that mirrors
//! omp's config root: everything else in it is a symlink to the real entry, so sessions, config and caches stay on
//! the volume. Every omp mu starts gets `PI_CONFIG_DIR` pointing at the mirror (see `apply`); `flush` seals the
//! memory copies back, and `shutdown` removes them. A crash between flushes loses what omp changed since the last
//! one (a rotated refresh token, a fresh login), so the sealed copy can hold a stale token.
use crate::sealed::{self, SealedError};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

/// Names the memory directory in the environment of every process that inherits omp's overrides.
const ENV_STATE: &str = "MU_OMP_STATE";
const INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Db,
    File,
}

/// A sealed credential: its logical name (the sealed file's authenticated name), and where omp keeps it
/// relative to the config root.
struct Cred {
    name: &'static str,
    rel: &'static str,
    kind: Kind,
}

const CREDS: [Cred; 3] = [
    Cred { name: "omp-agent.db", rel: "agent/agent.db", kind: Kind::Db },
    Cred { name: "omp-auth-broker.token", rel: "auth-broker.token", kind: Kind::File },
    Cred { name: "omp-auth-gateway.token", rel: "auth-gateway.token", kind: Kind::File },
];

/// Where omp's files really live, and where the memory copy goes.
struct Layout {
    home: PathBuf,
    profile: Option<String>,
    /// `~/.omp`: what `PI_CONFIG_DIR` names under the home directory.
    base: PathBuf,
    root: PathBuf,
    agent: PathBuf,
    /// Parent of the memory directory.
    memory: PathBuf,
    /// `PI_CODING_AGENT_DIR` is set in the environment, so omp must be pointed away from it.
    agent_override: bool,
}

impl Layout {
    fn from_env() -> Self {
        let home = crate::paths::home();
        let profile = crate::paths::safe_profile();
        Self {
            base: home.join(".omp"),
            root: crate::paths::omp_config_root(),
            agent: crate::paths::omp_agent_dir(),
            memory: memory_base(),
            agent_override: profile.is_none() && std::env::var_os("PI_CODING_AGENT_DIR").is_some_and(|d| !d.is_empty()),
            profile,
            home,
        }
    }
}

struct State {
    profile_home: PathBuf,
    layout: Layout,
    mem: PathBuf,
    /// The mirror of the config root inside `mem`.
    root: PathBuf,
    last: HashMap<&'static str, [u8; 32]>,
    _stop: mpsc::Sender<()>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn io_err(e: SealedError) -> io::Error {
    match e {
        SealedError::Io(e) => e,
        e => io::Error::other(e),
    }
}

fn sealed_path(profile_home: &Path, c: &Cred) -> PathBuf {
    profile_home.join("keys").join(format!("{}.sealed", c.name))
}

/// A memory-backed directory when the platform has one. macOS has no user-mountable tmpfs, so there a private
/// 0700 directory under the temp dir is the best available (it lives on disk until `shutdown` removes it).
fn memory_base() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
        for dir in runtime.into_iter().chain([PathBuf::from("/dev/shm")]) {
            if is_ram_fs(&dir) {
                return dir;
            }
        }
    }
    std::env::temp_dir()
}

#[cfg(target_os = "linux")]
fn is_ram_fs(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS: u64 = 0x0102_1994;
    const RAMFS: u64 = 0x8584_58f6;
    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    let mut s = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `s` is a valid out-pointer.
    if unsafe { libc::statfs(path.as_ptr(), s.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: statfs succeeded, so it filled `s`.
    let t = unsafe { s.assume_init() }.f_type as u64;
    t == TMPFS || t == RAMFS
}

fn dir_prefix(profile_home: &Path) -> String {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let digest = Sha256::digest(profile_home.as_os_str().as_encoded_bytes());
    let id: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("mu-omp-{uid}-{id}-")
}

/// Removes directories a crashed agent of this profile left behind.
fn prune_stale(memory: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(memory) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(prefix)).and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let gone =
            unsafe { libc::kill(pid, 0) } == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn private_dir(p: &Path) -> io::Result<()> {
    crate::fs::create_private_dir(p)
}

/// Symlinks each entry of `real` into `mem` that is not skipped and not there yet.
fn mirror(real: &Path, mem: &Path, skip: &dyn Fn(&str) -> bool) -> io::Result<()> {
    private_dir(mem)?;
    let entries = match std::fs::read_dir(real) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for e in entries {
        let e = e?;
        let name = e.file_name();
        if name.to_str().is_some_and(skip) {
            continue;
        }
        let link = mem.join(&name);
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(e.path(), link)?;
        }
    }
    Ok(())
}

fn is_credential_name(name: &str) -> bool {
    name.starts_with("agent.db") || name == "auth-broker.token" || name == "auth-gateway.token"
}

fn link_all(l: &Layout, mem: &Path) -> io::Result<PathBuf> {
    let top = mem.join("omp");
    let root = match &l.profile {
        Some(p) => {
            mirror(&l.base, &top, &|n| n == "profiles")?;
            let profiles = top.join("profiles");
            mirror(&l.base.join("profiles"), &profiles, &|n| n == p)?;
            profiles.join(p)
        }
        None => top,
    };
    mirror(&l.root, &root, &|n| n == "agent" || is_credential_name(n))?;
    mirror(&l.agent, &root.join("agent"), &|n| is_credential_name(n))?;
    Ok(root)
}

fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_all(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else if meta.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

/// Moves what omp created in a mirror (an entry the real directory lacked at `materialize`) to the real
/// directory and leaves a symlink, so it outlives the memory copy. Never overwrites a real entry.
fn adopt(mem: &Path, real: &Path, skip: &dyn Fn(&str) -> bool) {
    let Ok(entries) = std::fs::read_dir(mem) else { return };
    for e in entries.flatten() {
        let (name, from) = (e.file_name(), e.path());
        let to = real.join(&name);
        let movable = name.to_str().is_some_and(|n| !skip(n))
            && std::fs::symlink_metadata(&from).is_ok_and(|m| !m.file_type().is_symlink())
            && to.symlink_metadata().is_err();
        if !movable || std::fs::create_dir_all(real).is_err() {
            continue;
        }
        let moved = std::fs::rename(&from, &to).is_ok() || (copy_all(&from, &to).is_ok() && remove_any(&from).is_ok());
        if moved {
            let _ = std::os::unix::fs::symlink(&to, &from);
        }
    }
}

fn adopt_all(s: &State) {
    let l = &s.layout;
    adopt(&s.root.join("agent"), &l.agent, &|n| is_credential_name(n));
    adopt(&s.root, &l.root, &|n| n == "agent" || is_credential_name(n));
}

/// A consistent copy of a SQLite database (WAL included), by SQLite's online backup.
fn snapshot(src: &Path, scratch: &Path) -> io::Result<Vec<u8>> {
    use rusqlite::{Connection, OpenFlags, backup::Backup};
    let sql = |e: rusqlite::Error| io::Error::other(e.to_string());
    let _ = std::fs::remove_file(scratch);
    let out = (|| {
        let from = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(sql)?;
        let mut to = Connection::open(scratch).map_err(sql)?;
        Backup::new(&from, &mut to)
            .map_err(sql)?
            .run_to_completion(256, Duration::from_millis(5), None)
            .map_err(sql)?;
        drop(to);
        std::fs::read(scratch)
    })();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = scratch.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    out
}

fn read_current(s: &State, c: &Cred) -> io::Result<Option<Vec<u8>>> {
    let path = s.root.join(c.rel);
    if !path.exists() {
        return Ok(None);
    }
    match c.kind {
        Kind::Db => snapshot(&path, &s.mem.join("snapshot.tmp")).map(Some),
        Kind::File => std::fs::read(&path).map(Some),
    }
}

/// Seals each credential that changed since the last seal. True when anything was written.
fn flush_locked(s: &mut State) -> io::Result<bool> {
    let mut wrote = false;
    for c in &CREDS {
        let Some(bytes) = read_current(s, c)? else { continue };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        if s.last.get(c.name) == Some(&hash) {
            continue;
        }
        sealed::write_sealed(&sealed_path(&s.profile_home, c), c.name, &bytes).map_err(io_err)?;
        s.last.insert(c.name, hash);
        wrote = true;
    }
    Ok(wrote)
}

/// Brings a credential into memory: from its sealed copy, else by importing the plaintext one omp left on the
/// volume (sealed first, and only then removed).
fn restore(s: &mut State, c: &Cred) -> io::Result<()> {
    let dest = s.root.join(c.rel);
    if let Some(bytes) = sealed::read_sealed(&sealed_path(&s.profile_home, c), c.name).map_err(io_err)? {
        s.last.insert(c.name, Sha256::digest(&bytes).into());
        return crate::fs::write_private(&dest, &bytes);
    }
    let (real, siblings): (PathBuf, &[&str]) = match c.kind {
        Kind::Db => (s.layout.agent.join("agent.db"), &["-wal", "-shm", "-journal"]),
        Kind::File => (s.layout.root.join(c.rel), &[]),
    };
    if !real.exists() {
        return Ok(());
    }
    let bytes = match c.kind {
        Kind::Db => snapshot(&real, &s.mem.join("snapshot.tmp"))?,
        Kind::File => std::fs::read(&real)?,
    };
    crate::fs::write_private(&dest, &bytes)?;
    flush_locked(s)?;
    std::fs::remove_file(&real)?;
    for suffix in siblings {
        let mut p = real.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

/// Path of `target` relative to `home`, for `PI_CONFIG_DIR`: omp joins that name onto `$HOME` lexically.
fn relative_to_home(home: &Path, target: &Path) -> io::Result<OsString> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "HOME and the omp state directory must be absolute");
    if !home.is_absolute() || !target.is_absolute() {
        return Err(bad());
    }
    let mut depth = 0usize;
    for c in home.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let mut rel = PathBuf::new();
    for _ in 0..depth {
        rel.push("..");
    }
    for c in target.components() {
        if let Component::Normal(n) = c {
            rel.push(n);
        }
    }
    Ok(rel.into_os_string())
}

fn unlocked() -> io::Result<()> {
    if sealed::is_unlocked() { Ok(()) } else { Err(io_err(SealedError::Locked)) }
}

fn materialize_in(profile_home: &Path, layout: Layout) -> io::Result<()> {
    unlocked()?;
    let mut g = state();
    if let Some(s) = g.as_ref() {
        if s.profile_home == profile_home {
            link_all(&s.layout, &s.mem)?;
            return Ok(());
        }
        let old = g.take();
        drop(g);
        finish(old);
        g = state();
    }
    let prefix = dir_prefix(profile_home);
    prune_stale(&layout.memory, &prefix);
    let mem = layout.memory.join(format!("{prefix}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&mem);
    private_dir(&mem)?;
    let built = (|| {
        let root = link_all(&layout, &mem)?;
        let (stop, rx) = mpsc::channel();
        let mut s = State {
            profile_home: profile_home.to_path_buf(),
            layout,
            mem: mem.clone(),
            root,
            last: HashMap::new(),
            _stop: stop,
        };
        for c in &CREDS {
            restore(&mut s, c)?;
        }
        Ok((s, rx))
    })();
    match built {
        Ok((s, rx)) => {
            *g = Some(s);
            let _ = std::thread::Builder::new().name("omp-flush".into()).spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(INTERVAL) {
                    flush();
                }
            });
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&mem);
            Err(e)
        }
    }
}

/// Unseals omp's credential state into memory and points omp at it. Called once the state key is unlocked.
/// Idempotent.
pub fn materialize(profile_home: &Path) -> io::Result<()> {
    materialize_in(profile_home, Layout::from_env())
}

/// Like `flush`, with the error.
pub fn flush_checked() -> io::Result<bool> {
    match state().as_mut() {
        Some(s) => flush_locked(s),
        None => Ok(false),
    }
}

/// Seals omp's current state back to the volume now.
pub fn flush() {
    let _ = flush_checked();
}

/// Call where mu has waited for an omp process to end: it may have changed its credentials.
pub fn after_omp_exit() {
    flush();
}

fn finish(old: Option<State>) {
    if let Some(mut s) = old {
        let _ = flush_locked(&mut s);
        adopt_all(&s);
        let _ = std::fs::remove_dir_all(&s.mem);
    }
}

/// Final flush, then removes the memory copy. Called when the agent exits or locks.
pub fn shutdown() {
    let old = state().take();
    finish(old);
}

/// The environment an omp process needs to use the memory copy. Empty when state encryption is off or the
/// process already inherited it; `Locked` when it is on but not unlocked here.
pub fn envs() -> Result<Vec<(&'static str, OsString)>, SealedError> {
    envs_for(&crate::paths::profile_home())
}

fn envs_for(profile_home: &Path) -> Result<Vec<(&'static str, OsString)>, SealedError> {
    if let Some(s) = state().as_ref() {
        let l = &s.layout;
        let rel = relative_to_home(&l.home, &s.mem.join("omp")).map_err(SealedError::Io)?;
        let mut env = vec![("PI_CONFIG_DIR", rel), (ENV_STATE, s.mem.clone().into_os_string())];
        if l.agent_override {
            env.push(("PI_CODING_AGENT_DIR", s.root.join("agent").into_os_string()));
        }
        return Ok(env);
    }
    if std::env::var_os(ENV_STATE).is_some_and(|d| !d.is_empty()) || !sealed::is_initialised(profile_home) {
        return Ok(Vec::new());
    }
    Err(SealedError::Locked)
}

/// Gives `cmd` the omp environment (see `envs`). Fails with `locked: attach to unlock`, so omp never runs
/// against a missing credential database.
pub fn apply(cmd: &mut std::process::Command) -> io::Result<()> {
    cmd.envs(envs().map_err(io_err)?);
    Ok(())
}

/// Where omp's loopback tokens are: the memory copy while materialized (here or in a parent), else omp's config root.
pub fn credential_root() -> PathBuf {
    if let Some(s) = state().as_ref() {
        return s.root.clone();
    }
    match std::env::var_os(ENV_STATE).filter(|d| !d.is_empty()) {
        Some(mem) => {
            let top = PathBuf::from(mem).join("omp");
            match crate::paths::safe_profile() {
                Some(p) => top.join("profiles").join(p),
                None => top,
            }
        }
        None => crate::paths::omp_config_root(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::os::unix::fs::PermissionsExt;

    const MARKER: &str = "MARKER-credential-0xC0FFEE";

    struct Fixture {
        dir: PathBuf,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let serial = sealed::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            shutdown();
            sealed::lock();
            let dir = std::env::temp_dir().join(format!("mu-omp-state-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for d in ["home/.omp/agent", "profile", "mem"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            Self { dir, _serial: serial }
        }

        fn profile_home(&self) -> PathBuf {
            self.dir.join("profile")
        }

        fn unlock(&self) {
            let key = [7u8; 32];
            if !sealed::is_initialised(&self.profile_home()) {
                sealed::init_raw(&self.profile_home(), &key).unwrap();
            }
            sealed::unlock_raw(&self.profile_home(), &key).unwrap();
        }

        fn layout(&self, profile: Option<&str>) -> Layout {
            let base = self.dir.join("home/.omp");
            let root = match profile {
                Some(p) => base.join("profiles").join(p),
                None => base.clone(),
            };
            Layout {
                home: self.dir.join("home"),
                profile: profile.map(str::to_string),
                agent: root.join("agent"),
                root,
                base,
                memory: self.dir.join("mem"),
                agent_override: false,
            }
        }

        fn real_agent(&self) -> PathBuf {
            self.dir.join("home/.omp/agent")
        }

        fn mem_agent(&self) -> PathBuf {
            state().as_ref().unwrap().root.join("agent")
        }

        fn sealed(&self, name: &str) -> PathBuf {
            self.profile_home().join("keys").join(format!("{name}.sealed"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown();
            sealed::lock();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn plant_db(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.execute_batch(&format!("create table auth(v text); insert into auth values ('{MARKER}');")).unwrap();
        drop(c);
    }

    fn values(db: &Path) -> Vec<String> {
        let c = Connection::open(db).unwrap();
        let mut q = c.prepare("select v from auth order by rowid").unwrap();
        q.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    }

    fn unsealed_values(f: &Fixture) -> Vec<String> {
        let bytes = sealed::read_sealed(&f.sealed("omp-agent.db"), "omp-agent.db").unwrap().unwrap();
        let tmp = f.dir.join("opened.db");
        std::fs::write(&tmp, bytes).unwrap();
        values(&tmp)
    }

    fn volume_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                volume_files(&p, out);
            } else if m.is_file() {
                out.push(p);
            }
        }
    }

    fn volume_holds(f: &Fixture, needle: &str) -> bool {
        let mut files = Vec::new();
        volume_files(&f.dir.join("home"), &mut files);
        volume_files(&f.profile_home(), &mut files);
        files.iter().any(|p| String::from_utf8_lossy(&std::fs::read(p).unwrap()).contains(needle))
    }

    #[test]
    fn a_plaintext_database_is_imported_sealed_and_removed_from_the_volume() {
        let f = Fixture::new("import");
        plant_db(&f.real_agent().join("agent.db"));
        std::fs::write(f.real_agent().join("agent.db-shm"), "junk").unwrap();
        std::fs::write(f.dir.join("home/.omp/auth-gateway.token"), MARKER).unwrap();
        std::fs::write(f.real_agent().join("config.yml"), "a: 1").unwrap();
        std::fs::create_dir(f.real_agent().join("sessions")).unwrap();
        assert!(volume_holds(&f, MARKER));
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();

        assert!(!volume_holds(&f, MARKER), "no byte of the credentials stays on the volume");
        for gone in ["agent.db", "agent.db-wal", "agent.db-shm"] {
            assert!(!f.real_agent().join(gone).exists(), "{gone}");
        }
        assert!(!f.dir.join("home/.omp/auth-gateway.token").exists());
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER]);
        let token = state().as_ref().unwrap().root.join("auth-gateway.token");
        assert_eq!(std::fs::read_to_string(token).unwrap(), MARKER);
        assert_eq!(unsealed_values(&f), [MARKER]);
        for name in ["omp-agent.db", "omp-auth-gateway.token"] {
            assert!(sealed::is_sealed(&std::fs::read(f.sealed(name)).unwrap()), "{name}");
        }
    }

    #[test]
    fn everything_but_the_credentials_is_a_symlink_to_the_real_directory() {
        let f = Fixture::new("links");
        std::fs::write(f.real_agent().join("config.yml"), "a: 1").unwrap();
        std::fs::create_dir(f.real_agent().join("sessions")).unwrap();
        std::fs::create_dir(f.dir.join("home/.omp/logs")).unwrap();
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        let agent = f.mem_agent();
        for n in ["config.yml", "sessions"] {
            assert_eq!(std::fs::read_link(agent.join(n)).unwrap(), f.real_agent().join(n));
        }
        let root = state().as_ref().unwrap().root.clone();
        assert_eq!(std::fs::read_link(root.join("logs")).unwrap(), f.dir.join("home/.omp/logs"));
        assert!(std::fs::symlink_metadata(&agent).unwrap().is_dir());
        assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_profile_gets_its_own_mirror_and_the_other_profiles_stay_links() {
        let f = Fixture::new("profile");
        let work = f.dir.join("home/.omp/profiles/work");
        std::fs::create_dir_all(work.join("agent")).unwrap();
        std::fs::create_dir_all(f.dir.join("home/.omp/profiles/other")).unwrap();
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(Some("work"))).unwrap();
        let mem = state().as_ref().unwrap().mem.clone();
        assert_eq!(std::fs::read_link(mem.join("omp/profiles/other")).unwrap(), f.dir.join("home/.omp/profiles/other"));
        assert!(mem.join("omp/profiles/work/agent").is_dir());
        assert_eq!(state().as_ref().unwrap().root, mem.join("omp/profiles/work"));
    }

    /// A fake omp: reports its environment and writes a token where omp would, through `$HOME/$PI_CONFIG_DIR`.
    fn fake_omp(f: &Fixture, body: &str) -> PathBuf {
        let p = f.dir.join("fake-omp");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn run(f: &Fixture, omp: &Path, profile_home: &Path) -> std::process::Output {
        let mut cmd = std::process::Command::new(omp);
        cmd.env("HOME", f.dir.join("home")).env_remove("PI_CONFIG_DIR").env_remove(ENV_STATE);
        cmd.envs(envs_for(profile_home).map_err(io_err).unwrap());
        cmd.output().unwrap()
    }

    #[test]
    fn omp_writes_to_memory_and_a_flush_seals_it_and_a_restart_restores_it() {
        let f = Fixture::new("flush");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        let omp = fake_omp(
            &f,
            r#"printf '%s' "$PI_CONFIG_DIR" > "$HOME/seen-dir"; printf rotated > "$HOME/$PI_CONFIG_DIR/auth-gateway.token""#,
        );
        assert!(run(&f, &omp, &f.profile_home()).status.success());
        let token = state().as_ref().unwrap().root.join("auth-gateway.token");
        assert_eq!(
            std::fs::read_to_string(&token).unwrap(),
            "rotated",
            "the relative name resolves to the memory copy"
        );
        assert!(!f.dir.join("home/.omp/auth-gateway.token").exists());

        plant_db(&f.mem_agent().join("agent.db"));
        after_omp_exit();
        assert_eq!(unsealed_values(&f), [MARKER]);
        assert!(!volume_holds(&f, MARKER) && !volume_holds(&f, "rotated"));
        let mem = state().as_ref().unwrap().mem.clone();

        shutdown();
        assert!(!mem.exists(), "the memory directory is removed");
        assert!(state().is_none());

        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER]);
        let token = state().as_ref().unwrap().root.join("auth-gateway.token");
        assert_eq!(std::fs::read_to_string(token).unwrap(), "rotated");
    }

    #[test]
    fn a_flush_with_nothing_new_writes_nothing() {
        let f = Fixture::new("unchanged");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        plant_db(&f.mem_agent().join("agent.db"));
        assert!(flush_checked().unwrap());
        let sealed_db = std::fs::read(f.sealed("omp-agent.db")).unwrap();
        assert!(!flush_checked().unwrap());
        assert_eq!(std::fs::read(f.sealed("omp-agent.db")).unwrap(), sealed_db, "sealing again would change the nonce");

        let c = Connection::open(f.mem_agent().join("agent.db")).unwrap();
        c.execute("insert into auth values ('second')", []).unwrap();
        assert!(flush_checked().unwrap());
        assert_eq!(unsealed_values(&f), [MARKER, "second"]);
    }

    #[test]
    fn a_flush_includes_what_is_still_in_the_write_ahead_log() {
        let f = Fixture::new("wal");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        let db = f.mem_agent().join("agent.db");
        let c = Connection::open(&db).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        c.execute_batch("create table auth(v text); insert into auth values ('in-the-wal');").unwrap();
        assert!(std::fs::metadata(db.with_extension("db-wal")).unwrap().len() > 0);
        flush();
        assert_eq!(unsealed_values(&f), ["in-the-wal"]);
    }

    #[test]
    fn what_omp_created_in_the_mirror_is_moved_to_the_volume_at_shutdown() {
        let f = Fixture::new("adopt");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        std::fs::write(f.mem_agent().join("history.db"), "h").unwrap();
        std::fs::create_dir(f.mem_agent().join("memories")).unwrap();
        std::fs::write(f.mem_agent().join("memories/m.md"), "m").unwrap();
        shutdown();
        assert_eq!(std::fs::read(f.real_agent().join("history.db")).unwrap(), b"h");
        assert_eq!(std::fs::read(f.real_agent().join("memories/m.md")).unwrap(), b"m");
    }

    #[test]
    fn locking_removes_the_memory_copy() {
        let f = Fixture::new("lock");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        plant_db(&f.mem_agent().join("agent.db"));
        let mem = state().as_ref().unwrap().mem.clone();
        sealed::lock();
        assert!(!mem.exists());
        assert!(state().is_none());
        f.unlock();
        assert!(f.sealed("omp-agent.db").exists(), "the final flush ran before the key went");
    }

    #[test]
    fn materializing_again_keeps_the_directory_and_stale_ones_are_pruned() {
        let f = Fixture::new("again");
        f.unlock();
        let stale = f.dir.join("mem").join(format!("{}999999999", dir_prefix(&f.profile_home())));
        std::fs::create_dir_all(stale.join("omp")).unwrap();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        assert!(!stale.exists());
        plant_db(&f.mem_agent().join("agent.db"));
        let mem = state().as_ref().unwrap().mem.clone();
        std::fs::create_dir(f.real_agent().join("late")).unwrap();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        assert_eq!(state().as_ref().unwrap().mem, mem);
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER], "the live database is untouched");
        assert!(f.mem_agent().join("late").is_symlink(), "entries that appeared since are linked");
    }

    #[test]
    fn materializing_needs_the_key() {
        let f = Fixture::new("needkey");
        let e = materialize_in(&f.profile_home(), f.layout(None)).unwrap_err();
        assert_eq!(e.to_string(), "locked: attach to unlock");
        assert!(state().is_none());
        assert!(std::fs::read_dir(f.dir.join("mem")).unwrap().next().is_none());
    }

    #[test]
    fn omp_is_refused_while_locked_and_untouched_when_encryption_is_off() {
        let f = Fixture::new("refuse");
        assert!(envs_for(&f.profile_home()).unwrap().is_empty(), "off: nothing changes");
        f.unlock();
        sealed::lock();
        assert_eq!(envs_for(&f.profile_home()).unwrap_err().to_string(), "locked: attach to unlock");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        let env = envs_for(&f.profile_home()).unwrap();
        let names: Vec<_> = env.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, ["PI_CONFIG_DIR", ENV_STATE]);
        assert_eq!(credential_root(), state().as_ref().unwrap().root);
    }

    #[test]
    fn a_pi_coding_agent_dir_override_is_pointed_at_the_mirror() {
        let f = Fixture::new("override");
        f.unlock();
        let mut l = f.layout(None);
        l.agent_override = true;
        materialize_in(&f.profile_home(), l).unwrap();
        let env = envs_for(&f.profile_home()).unwrap();
        let dir = env.iter().find(|(k, _)| *k == "PI_CODING_AGENT_DIR").unwrap();
        assert_eq!(PathBuf::from(&dir.1), f.mem_agent());
    }

    #[test]
    fn the_config_dir_name_climbs_out_of_home() {
        let rel = relative_to_home(Path::new("/home/u"), Path::new("/dev/shm/x/omp")).unwrap();
        assert_eq!(rel, "../../dev/shm/x/omp");
        let rel = relative_to_home(Path::new("/home/u/"), Path::new("/run/user/1000/x")).unwrap();
        assert_eq!(rel, "../../run/user/1000/x");
        assert!(relative_to_home(Path::new("h"), Path::new("/x")).is_err());
    }

    /// The real omp under a profile, in a scratch HOME: its credentials land in memory, and its SQLite files
    /// reached through symlinks still work. `scripts/maint` runs this on every omp bump.
    #[test]
    #[ignore = "contract: runs the real omp"]
    fn contract_omp_keeps_its_credentials_in_the_memory_copy() {
        let f = Fixture::new("contract");
        f.unlock();
        let omp = crate::paths::omp_bin();
        let real = f.dir.join("home/.omp/profiles/fake");
        std::fs::create_dir_all(real.join("agent")).unwrap();
        let run_omp = |args: &[&str]| {
            let mut cmd = std::process::Command::new(&omp);
            cmd.env("HOME", f.dir.join("home")).env("OMP_PROFILE", "fake").env_remove("PI_CODING_AGENT_DIR");
            cmd.envs(envs_for(&f.profile_home()).map_err(io_err).unwrap());
            let out = cmd.args(args).stdin(std::process::Stdio::null()).output().unwrap();
            assert!(out.status.success(), "omp {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        for round in 0..2 {
            materialize_in(&f.profile_home(), f.layout(Some("fake"))).unwrap();
            run_omp(&["models", "ls", "--json"]);
            run_omp(&["auth-gateway", "token"]);
            let root = state().as_ref().unwrap().root.clone();
            assert!(root.join("agent/agent.db").is_file(), "round {round}");
            assert!(root.join("auth-gateway.token").is_file(), "round {round}");
            assert!(!real.join("agent/agent.db").exists());
            assert!(!real.join("auth-gateway.token").exists());
            after_omp_exit();
            shutdown();
            assert!(real.join("agent/models.db").is_file(), "omp's other databases end up on the volume");
        }
        for name in ["omp-agent.db", "omp-auth-gateway.token"] {
            assert!(f.sealed(name).is_file(), "{name}");
        }
    }

    #[test]
    fn run_omp_blocking_sees_the_environment() {
        let f = Fixture::new("blocking");
        f.unlock();
        materialize_in(&f.profile_home(), f.layout(None)).unwrap();
        let omp = fake_omp(&f, r#"printf '%s|%s' "$PI_CONFIG_DIR" "$MU_OMP_STATE""#);
        let home = f.dir.join("home");
        // SAFETY: only omp_bin() reads MU_OMP, and the fixture serialises the tests that run it.
        unsafe { std::env::set_var("MU_OMP", &omp) };
        let out = crate::paths::run_omp_blocking(&[], &[("HOME", home.to_str().unwrap())]);
        unsafe { std::env::remove_var("MU_OMP") };
        let mem = state().as_ref().unwrap().mem.clone();
        let (dir, state_dir) = out.split_once('|').unwrap();
        assert_eq!(state_dir, mem.to_str().unwrap());
        assert_eq!(dir, relative_to_home(&home, &mem.join("omp")).unwrap());
    }
}
