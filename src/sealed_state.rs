//! omp's credential state kept sealed at rest by mu, unsealed into a memory-backed directory for the omp
//! processes this tool starts. Used by the mu adapter (`mu-plugin`) only; ported from mu's former `omp_state`.
//!
//! omp keeps its credentials in `agent.db` (SQLite, in its agent directory) and two loopback tokens in its config
//! root. When mu reports state encryption on and unlocked (`core/secrets` `info`), `materialize` restores them from
//! mu's secret store into a private directory that mirrors omp's config root: everything else in it is a symlink to
//! the real entry, so sessions, config and caches stay on the volume. Every omp this process starts gets
//! `PI_CONFIG_DIR` pointing at the mirror (see `envs`); `flush` seals the memory copies back through mu, and
//! `shutdown` removes them. A crash between flushes loses what omp changed since the last one (a rotated refresh
//! token, a fresh login), so the sealed copy can hold a stale token. Otherwise (no encryption, or locked) nothing
//! happens and omp uses its own files.
//!
//! `materialize`, `flush` and `envs` block on mu's replies: call them from a blocking thread, never from the
//! async runtime that serves the replies.

use crate::state;
use crate::vendor::fs as vfs;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Names the memory directory in the environment of every process that inherits omp's overrides.
const ENV_STATE: &str = "AI_PROVIDER_GATEWAY_STATE";
const INTERVAL: Duration = Duration::from_secs(30);

pub struct Info {
    pub sealed: bool,
    pub unlocked: bool,
}

/// mu's `core/secrets` store, as this module needs it.
pub trait Secrets: Send + Sync {
    fn info(&self) -> io::Result<Info>;
    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>>;
    fn put(&self, name: &str, bytes: &[u8]) -> io::Result<()>;
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Db,
    File,
}

/// A sealed credential: its name in mu's store, and where omp keeps it relative to the config root.
struct Cred {
    name: &'static str,
    rel: &'static str,
    kind: Kind,
}

const CREDS: [Cred; 3] = [
    Cred { name: "agent.db", rel: "agent/agent.db", kind: Kind::Db },
    Cred { name: "auth-broker.token", rel: "auth-broker.token", kind: Kind::File },
    Cred { name: "auth-gateway.token", rel: "auth-gateway.token", kind: Kind::File },
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
        let home = state::user_home();
        let profile = state::safe_profile();
        Self {
            base: home.join(".omp"),
            root: state::omp_config_root(),
            agent: state::omp_agent_dir(),
            memory: memory_base(),
            agent_override: profile.is_none() && std::env::var_os("PI_CODING_AGENT_DIR").is_some_and(|d| !d.is_empty()),
            profile,
            home,
        }
    }
}

struct Mirror {
    layout: Layout,
    mu: Arc<dyn Secrets>,
    mem: PathBuf,
    /// The mirror of the config root inside `mem`.
    root: PathBuf,
    /// `PI_CONFIG_DIR`: `mem/omp` relative to `$HOME`.
    config_dir: OsString,
    last: HashMap<&'static str, [u8; 32]>,
    _stop: mpsc::Sender<()>,
}

static MIRROR: Mutex<Option<Mirror>> = Mutex::new(None);

fn mirror_lock() -> std::sync::MutexGuard<'static, Option<Mirror>> {
    MIRROR.lock().unwrap_or_else(|e| e.into_inner())
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

fn dir_prefix(key: &Path) -> String {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let digest = Sha256::digest(key.as_os_str().as_encoded_bytes());
    let id: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("apg-{uid}-{id}-")
}

/// Removes directories a crashed adapter of this profile left behind.
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

/// Symlinks each entry of `real` into `mem` that is not skipped and not there yet.
fn link_entries(real: &Path, mem: &Path, skip: &dyn Fn(&str) -> bool) -> io::Result<()> {
    vfs::create_private_dir(mem)?;
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
            link_entries(&l.base, &top, &|n| n == "profiles")?;
            let profiles = top.join("profiles");
            link_entries(&l.base.join("profiles"), &profiles, &|n| n == p)?;
            profiles.join(p)
        }
        None => top,
    };
    link_entries(&l.root, &root, &|n| n == "agent" || is_credential_name(n))?;
    link_entries(&l.agent, &root.join("agent"), &|n| is_credential_name(n))?;
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

fn adopt_all(s: &Mirror) {
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

fn read_current(s: &Mirror, c: &Cred) -> io::Result<Option<Vec<u8>>> {
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
fn flush_locked(s: &mut Mirror) -> io::Result<bool> {
    let mut wrote = false;
    for c in &CREDS {
        let Some(bytes) = read_current(s, c)? else { continue };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        if s.last.get(c.name) == Some(&hash) {
            continue;
        }
        s.mu.put(c.name, &bytes)?;
        s.last.insert(c.name, hash);
        wrote = true;
    }
    Ok(wrote)
}

/// Brings a credential into memory: from its sealed copy, else by importing the plaintext one omp left on the
/// volume (sealed first, and only then removed).
fn restore(s: &mut Mirror, c: &Cred) -> io::Result<()> {
    let dest = s.root.join(c.rel);
    if let Some(bytes) = s.mu.get(c.name)? {
        s.last.insert(c.name, Sha256::digest(&bytes).into());
        return vfs::write_private(&dest, &bytes);
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
    vfs::write_private(&dest, &bytes)?;
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

fn materialize_in(mu: Arc<dyn Secrets>, key: &Path, layout: Layout) -> io::Result<bool> {
    let info = mu.info()?;
    if !(info.sealed && info.unlocked) {
        return Ok(false);
    }
    let mut g = mirror_lock();
    if let Some(s) = g.as_ref() {
        link_all(&s.layout, &s.mem)?;
        return Ok(true);
    }
    let prefix = dir_prefix(key);
    prune_stale(&layout.memory, &prefix);
    let mem = layout.memory.join(format!("{prefix}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&mem);
    vfs::create_private_dir(&mem)?;
    let built = (|| {
        let config_dir = relative_to_home(&layout.home, &mem.join("omp"))?;
        let root = link_all(&layout, &mem)?;
        let (stop, rx) = mpsc::channel();
        let mut s = Mirror { layout, mu, mem: mem.clone(), root, config_dir, last: HashMap::new(), _stop: stop };
        for c in &CREDS {
            restore(&mut s, c)?;
        }
        Ok((s, rx))
    })();
    match built {
        Ok((s, rx)) => {
            *g = Some(s);
            let _ = std::thread::Builder::new().name("apg-flush".into()).spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(INTERVAL) {
                    flush();
                }
            });
            Ok(true)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&mem);
            Err(e)
        }
    }
}

/// When mu has state encryption on and unlocked, unseals omp's credential state into memory and points omp at
/// it; true then. Idempotent, and safe to call again after mu unlocks.
pub fn materialize(mu: Arc<dyn Secrets>) -> io::Result<bool> {
    materialize_in(mu, &state::gw_dir(), Layout::from_env())
}

/// Like `flush`, with the error.
pub fn flush_checked() -> io::Result<bool> {
    match mirror_lock().as_mut() {
        Some(s) => flush_locked(s),
        None => Ok(false),
    }
}

/// Seals omp's current state back through mu now.
pub fn flush() {
    let _ = flush_checked();
}

fn finish(old: Option<Mirror>) {
    if let Some(mut s) = old {
        // Before the flush, which may wait on a mu that has stopped answering.
        adopt_all(&s);
        let _ = flush_locked(&mut s);
        let _ = std::fs::remove_dir_all(&s.mem);
    }
}

/// Final flush, then removes the memory copy.
pub fn shutdown() {
    let old = mirror_lock().take();
    finish(old);
}

/// The environment an omp process needs to use the memory copy; empty when not materialized here, or when the
/// process already inherited it.
pub fn envs() -> Vec<(&'static str, OsString)> {
    let g = mirror_lock();
    let Some(s) = g.as_ref() else { return Vec::new() };
    let mut env = vec![("PI_CONFIG_DIR", s.config_dir.clone()), (ENV_STATE, s.mem.clone().into_os_string())];
    if s.layout.agent_override {
        env.push(("PI_CODING_AGENT_DIR", s.root.join("agent").into_os_string()));
    }
    env
}

/// Where omp's loopback tokens are: the memory copy while materialized (here or in a parent), else omp's config root.
pub fn credential_root() -> PathBuf {
    if let Some(s) = mirror_lock().as_ref() {
        return s.root.clone();
    }
    match std::env::var_os(ENV_STATE).filter(|d| !d.is_empty()) {
        Some(mem) => {
            let top = PathBuf::from(mem).join("omp");
            match state::safe_profile() {
                Some(p) => top.join("profiles").join(p),
                None => top,
            }
        }
        None => state::omp_config_root(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const MARKER: &str = "MARKER-credential-0xC0FFEE";

    /// An in-memory `core/secrets` responder.
    struct FakeMu {
        sealed: AtomicBool,
        unlocked: AtomicBool,
        store: Mutex<HashMap<String, Vec<u8>>>,
        puts: AtomicUsize,
    }

    impl Secrets for FakeMu {
        fn info(&self) -> io::Result<Info> {
            Ok(Info { sealed: self.sealed.load(Ordering::SeqCst), unlocked: self.unlocked.load(Ordering::SeqCst) })
        }
        fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(self.store.lock().unwrap().get(name).cloned())
        }
        fn put(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
            if !self.unlocked.load(Ordering::SeqCst) {
                return Err(io::Error::other("locked: attach to unlock"));
            }
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.store.lock().unwrap().insert(name.to_string(), bytes.to_vec());
            Ok(())
        }
    }

    static SERIAL: Mutex<()> = Mutex::new(());

    struct Fixture {
        dir: PathBuf,
        mu: Arc<FakeMu>,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            shutdown();
            let dir = std::env::temp_dir().join(format!("apg-sealed-state-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for d in ["home/.omp/agent", "mem"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            let mu = Arc::new(FakeMu {
                sealed: AtomicBool::new(true),
                unlocked: AtomicBool::new(true),
                store: Mutex::new(HashMap::new()),
                puts: AtomicUsize::new(0),
            });
            Self { dir, mu, _serial: serial }
        }

        fn key(&self) -> PathBuf {
            self.dir.join("gw")
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

        fn materialize(&self, layout: Layout) -> io::Result<bool> {
            materialize_in(self.mu.clone(), &self.key(), layout)
        }

        fn real_agent(&self) -> PathBuf {
            self.dir.join("home/.omp/agent")
        }

        fn mem_agent(&self) -> PathBuf {
            mirror_lock().as_ref().unwrap().root.join("agent")
        }

        fn mem_root(&self) -> PathBuf {
            mirror_lock().as_ref().unwrap().root.clone()
        }

        fn mem(&self) -> PathBuf {
            mirror_lock().as_ref().unwrap().mem.clone()
        }

        fn sealed(&self, name: &str) -> Option<Vec<u8>> {
            self.mu.store.lock().unwrap().get(name).cloned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            shutdown();
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

    fn sealed_values(f: &Fixture) -> Vec<String> {
        let tmp = f.dir.join("opened.db");
        std::fs::write(&tmp, f.sealed("agent.db").unwrap()).unwrap();
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
        assert!(f.materialize(f.layout(None)).unwrap());

        assert!(!volume_holds(&f, MARKER), "no byte of the credentials stays on the volume");
        for gone in ["agent.db", "agent.db-wal", "agent.db-shm"] {
            assert!(!f.real_agent().join(gone).exists(), "{gone}");
        }
        assert!(!f.dir.join("home/.omp/auth-gateway.token").exists());
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER]);
        assert_eq!(std::fs::read_to_string(f.mem_root().join("auth-gateway.token")).unwrap(), MARKER);
        assert_eq!(sealed_values(&f), [MARKER]);
        assert_eq!(f.sealed("auth-gateway.token").unwrap(), MARKER.as_bytes());
    }

    #[test]
    fn everything_but_the_credentials_is_a_symlink_to_the_real_directory() {
        let f = Fixture::new("links");
        std::fs::write(f.real_agent().join("config.yml"), "a: 1").unwrap();
        std::fs::create_dir(f.real_agent().join("sessions")).unwrap();
        std::fs::create_dir(f.dir.join("home/.omp/logs")).unwrap();
        f.materialize(f.layout(None)).unwrap();
        let agent = f.mem_agent();
        for n in ["config.yml", "sessions"] {
            assert_eq!(std::fs::read_link(agent.join(n)).unwrap(), f.real_agent().join(n));
        }
        let root = f.mem_root();
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
        f.materialize(f.layout(Some("work"))).unwrap();
        let mem = f.mem();
        assert_eq!(std::fs::read_link(mem.join("omp/profiles/other")).unwrap(), f.dir.join("home/.omp/profiles/other"));
        assert!(mem.join("omp/profiles/work/agent").is_dir());
        assert_eq!(f.mem_root(), mem.join("omp/profiles/work"));
    }

    /// A fake omp: reports its environment and writes a token where omp would, through `$HOME/$PI_CONFIG_DIR`.
    fn fake_omp(f: &Fixture, body: &str) -> PathBuf {
        let p = f.dir.join("fake-omp");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn run(f: &Fixture, omp: &Path) -> std::process::Output {
        let mut cmd = std::process::Command::new(omp);
        cmd.env("HOME", f.dir.join("home")).env_remove("PI_CONFIG_DIR").env_remove(ENV_STATE);
        cmd.envs(envs());
        cmd.output().unwrap()
    }

    #[test]
    fn omp_writes_to_memory_and_a_flush_seals_it_and_a_restart_restores_it() {
        let f = Fixture::new("flush");
        f.materialize(f.layout(None)).unwrap();
        let omp = fake_omp(&f, r#"printf rotated > "$HOME/$PI_CONFIG_DIR/auth-gateway.token""#);
        assert!(run(&f, &omp).status.success());
        assert_eq!(
            std::fs::read_to_string(f.mem_root().join("auth-gateway.token")).unwrap(),
            "rotated",
            "the relative name resolves to the memory copy"
        );
        assert!(!f.dir.join("home/.omp/auth-gateway.token").exists());

        plant_db(&f.mem_agent().join("agent.db"));
        flush();
        assert_eq!(sealed_values(&f), [MARKER]);
        assert!(!volume_holds(&f, MARKER) && !volume_holds(&f, "rotated"));
        let mem = f.mem();

        shutdown();
        assert!(!mem.exists(), "the memory directory is removed");
        assert!(mirror_lock().is_none());

        f.materialize(f.layout(None)).unwrap();
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER]);
        assert_eq!(std::fs::read_to_string(f.mem_root().join("auth-gateway.token")).unwrap(), "rotated");
    }

    #[test]
    fn a_flush_with_nothing_new_writes_nothing() {
        let f = Fixture::new("unchanged");
        f.materialize(f.layout(None)).unwrap();
        plant_db(&f.mem_agent().join("agent.db"));
        assert!(flush_checked().unwrap());
        let puts = f.mu.puts.load(Ordering::SeqCst);
        assert!(!flush_checked().unwrap());
        assert_eq!(f.mu.puts.load(Ordering::SeqCst), puts, "sealing again would change the nonce");

        let c = Connection::open(f.mem_agent().join("agent.db")).unwrap();
        c.execute("insert into auth values ('second')", []).unwrap();
        assert!(flush_checked().unwrap());
        assert_eq!(sealed_values(&f), [MARKER, "second"]);
    }

    #[test]
    fn a_flush_includes_what_is_still_in_the_write_ahead_log() {
        let f = Fixture::new("wal");
        f.materialize(f.layout(None)).unwrap();
        let db = f.mem_agent().join("agent.db");
        let c = Connection::open(&db).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        c.execute_batch("create table auth(v text); insert into auth values ('in-the-wal');").unwrap();
        assert!(std::fs::metadata(db.with_extension("db-wal")).unwrap().len() > 0);
        flush();
        assert_eq!(sealed_values(&f), ["in-the-wal"]);
    }

    #[test]
    fn what_omp_created_in_the_mirror_is_moved_to_the_volume_at_shutdown() {
        let f = Fixture::new("adopt");
        f.materialize(f.layout(None)).unwrap();
        std::fs::write(f.mem_agent().join("history.db"), "h").unwrap();
        std::fs::create_dir(f.mem_agent().join("memories")).unwrap();
        std::fs::write(f.mem_agent().join("memories/m.md"), "m").unwrap();
        shutdown();
        assert_eq!(std::fs::read(f.real_agent().join("history.db")).unwrap(), b"h");
        assert_eq!(std::fs::read(f.real_agent().join("memories/m.md")).unwrap(), b"m");
    }

    #[test]
    fn shutdown_flushes_and_removes_the_memory_copy() {
        let f = Fixture::new("shutdown");
        f.materialize(f.layout(None)).unwrap();
        plant_db(&f.mem_agent().join("agent.db"));
        let mem = f.mem();
        shutdown();
        assert!(!mem.exists());
        assert!(mirror_lock().is_none());
        assert_eq!(sealed_values(&f), [MARKER], "the final flush ran before the copy went");
    }

    #[test]
    fn a_shutdown_that_cannot_reach_mu_still_removes_the_copy_and_moves_new_files() {
        let f = Fixture::new("locked-shutdown");
        f.materialize(f.layout(None)).unwrap();
        plant_db(&f.mem_agent().join("agent.db"));
        std::fs::write(f.mem_agent().join("history.db"), "h").unwrap();
        let mem = f.mem();
        f.mu.unlocked.store(false, Ordering::SeqCst);
        shutdown();
        assert!(!mem.exists());
        assert!(f.sealed("agent.db").is_none());
        assert_eq!(std::fs::read(f.real_agent().join("history.db")).unwrap(), b"h");
    }

    #[test]
    fn materializing_again_keeps_the_directory_and_stale_ones_are_pruned() {
        let f = Fixture::new("again");
        let stale = f.dir.join("mem").join(format!("{}999999999", dir_prefix(&f.key())));
        std::fs::create_dir_all(stale.join("omp")).unwrap();
        f.materialize(f.layout(None)).unwrap();
        assert!(!stale.exists());
        plant_db(&f.mem_agent().join("agent.db"));
        let mem = f.mem();
        std::fs::create_dir(f.real_agent().join("late")).unwrap();
        f.materialize(f.layout(None)).unwrap();
        assert_eq!(f.mem(), mem);
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER], "the live database is untouched");
        assert!(f.mem_agent().join("late").is_symlink(), "entries that appeared since are linked");
    }

    #[test]
    fn nothing_happens_unless_mu_is_sealed_and_unlocked() {
        let f = Fixture::new("needkey");
        std::fs::write(f.dir.join("home/.omp/auth-gateway.token"), MARKER).unwrap();
        for (sealed, unlocked) in [(false, false), (false, true), (true, false)] {
            f.mu.sealed.store(sealed, Ordering::SeqCst);
            f.mu.unlocked.store(unlocked, Ordering::SeqCst);
            assert!(!f.materialize(f.layout(None)).unwrap());
            assert!(mirror_lock().is_none());
            assert!(envs().is_empty(), "omp keeps using its own files");
            assert!(std::fs::read_dir(f.dir.join("mem")).unwrap().next().is_none());
        }
        assert!(volume_holds(&f, MARKER), "the plaintext credential is left alone");
    }

    #[test]
    fn the_environment_points_omp_at_the_mirror_and_tokens_are_read_there() {
        let f = Fixture::new("envs");
        f.materialize(f.layout(None)).unwrap();
        let names: Vec<_> = envs().iter().map(|(k, _)| *k).collect();
        assert_eq!(names, ["PI_CONFIG_DIR", ENV_STATE]);
        assert_eq!(credential_root(), f.mem_root());
    }

    #[test]
    fn a_pi_coding_agent_dir_override_is_pointed_at_the_mirror() {
        let f = Fixture::new("override");
        let mut l = f.layout(None);
        l.agent_override = true;
        f.materialize(l).unwrap();
        let env = envs();
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
        let omp = state::omp_bin();
        let real = f.dir.join("home/.omp/profiles/fake");
        std::fs::create_dir_all(real.join("agent")).unwrap();
        let run_omp = |args: &[&str]| {
            let mut cmd = std::process::Command::new(&omp);
            cmd.env("HOME", f.dir.join("home")).env("OMP_PROFILE", "fake").env_remove("PI_CODING_AGENT_DIR");
            cmd.envs(envs());
            let out = cmd.args(args).stdin(std::process::Stdio::null()).output().unwrap();
            assert!(out.status.success(), "omp {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        for round in 0..2 {
            f.materialize(f.layout(Some("fake"))).unwrap();
            run_omp(&["models", "ls", "--json"]);
            run_omp(&["auth-gateway", "token"]);
            let root = f.mem_root();
            assert!(root.join("agent/agent.db").is_file(), "round {round}");
            assert!(root.join("auth-gateway.token").is_file(), "round {round}");
            assert!(!real.join("agent/agent.db").exists());
            assert!(!real.join("auth-gateway.token").exists());
            flush();
            shutdown();
            assert!(real.join("agent/models.db").is_file(), "omp's other databases end up on the volume");
        }
        for name in ["agent.db", "auth-gateway.token"] {
            assert!(f.sealed(name).is_some(), "{name}");
        }
    }
}
