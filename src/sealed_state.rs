//! omp's credential state kept sealed at rest by mu, unsealed into a memory-backed directory for the omp
//! processes this tool starts. Used by the mu adapter (`mu-plugin`) only; ported from mu's former `omp_state`.
//!
//! omp keeps its credentials in `agent.db` (SQLite, in its agent directory) and two loopback tokens in its config
//! root. When mu reports state encryption on and unlocked (`core/secrets` `info`), `materialize` restores them from
//! mu's secret store into a private directory that mirrors omp's config root: everything else in it is a symlink to
//! the real entry, so sessions, config and caches stay on the volume. Every omp this process starts gets
//! `PI_CONFIG_DIR` pointing at the mirror (see `envs`); `flush` seals the memory copies back through mu.
//!
//! There is one mirror per profile, shared by every adapter and by the daemon, whichever adapter started it.
//! Adapters register as `users/<pid>` in it under a file lock; the first one restores the credentials, later ones
//! join (sealing at once whatever the mirror holds that was never sealed, e.g. rotations made by a daemon while no
//! adapter was alive). `shutdown` does a final flush and removes the mirror only when no other adapter is
//! attached and no daemon runs; otherwise the daemon keeps writing into it and the next attacher seals the
//! difference. A crash between flushes loses what omp changed since the last one (a rotated refresh token, a
//! fresh login), so the sealed copy can hold a stale token. Otherwise (no encryption, or locked) nothing
//! happens and omp uses its own files.
//!
//! `materialize`, `flush` and `envs` block on mu's replies: call them from a blocking thread, never from the
//! async runtime that serves the replies.

use crate::state;
use crate::vendor::fs as vfs;
use sha2::{Digest, Sha256};
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
    /// The daemon's state directory: `state.json` says whether a daemon uses the mirror.
    key: PathBuf,
    lock: PathBuf,
    pid: u32,
    _stop: Option<mpsc::Sender<()>>,
}

impl Mirror {
    fn lock(&self) -> io::Result<vfs::FileLock> {
        vfs::FileLock::exclusive(&self.lock)
    }

    fn scratch(&self) -> PathBuf {
        self.mem.join(format!("snapshot.{}.tmp", self.pid))
    }

    fn recorded(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.mem.join("last-sealed").join(name)).ok()
    }

    fn record(&self, name: &str, hash: &[u8; 32]) -> io::Result<()> {
        vfs::write_private(&self.mem.join("last-sealed").join(name), hex(hash).as_bytes())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
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

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

fn profile_id(key: &Path) -> String {
    hex(&Sha256::digest(key.as_os_str().as_encoded_bytes())[..6])
}

/// The one mirror of the profile `key` names: no pid in it, so every adapter and the daemon share it.
fn mirror_dir(memory: &Path, key: &Path) -> PathBuf {
    memory.join(format!("ai-provider-gateway-{}-{}", uid(), profile_id(key)))
}

/// Lock for attach, flush and detach. Kept in a private directory of its own because `memory` is shared
/// (`/tmp`), and survives the mirror's removal.
fn lock_path(memory: &Path, key: &Path) -> PathBuf {
    memory.join(format!("ai-provider-gateway-{}-locks", uid())).join(format!("{}.lock", profile_id(key)))
}

fn users_dir(mem: &Path) -> PathBuf {
    mem.join("users")
}

/// Drops registry entries of adapters that are gone; true when none is left.
fn prune_users(mem: &Path) -> bool {
    let mut any = false;
    let Ok(entries) = std::fs::read_dir(users_dir(mem)) else { return true };
    for e in entries.flatten() {
        match e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) {
            Some(pid) if state::alive(pid) => any = true,
            _ => {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    !any
}

fn daemon_alive(key: &Path) -> bool {
    state::read_state(key).is_some_and(|s| state::alive(s.pid))
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

fn adopt_all(l: &Layout, root: &Path) {
    adopt(&root.join("agent"), &l.agent, &|n| is_credential_name(n));
    adopt(root, &l.root, &|n| n == "agent" || is_credential_name(n));
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
        Kind::Db => snapshot(&path, &s.scratch()).map(Some),
        Kind::File => std::fs::read(&path).map(Some),
    }
}

/// Seals each credential that differs from what was last sealed (recorded in the mirror, so every adapter
/// agrees). The mirror is the single source: this only ever re-seals its own content. True when anything was
/// written. Call with the mirror's lock held.
fn flush_locked(s: &Mirror) -> io::Result<bool> {
    let mut wrote = false;
    for c in &CREDS {
        let Some(bytes) = read_current(s, c)? else { continue };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        if s.recorded(c.name).as_deref() == Some(hex(&hash).as_str()) {
            continue;
        }
        s.mu.put(c.name, &bytes)?;
        s.record(c.name, &hash)?;
        wrote = true;
    }
    Ok(wrote)
}

/// Brings a credential into memory: from its sealed copy, else by importing the plaintext one omp left on the
/// volume (sealed first, and only then removed).
fn restore(s: &Mirror, c: &Cred) -> io::Result<()> {
    let dest = s.root.join(c.rel);
    if let Some(bytes) = s.mu.get(c.name)? {
        vfs::write_private(&dest, &bytes)?;
        return s.record(c.name, &Sha256::digest(&bytes).into());
    }
    let (real, siblings): (PathBuf, &[&str]) = match c.kind {
        Kind::Db => (s.layout.agent.join("agent.db"), &["-wal", "-shm", "-journal"]),
        Kind::File => (s.layout.root.join(c.rel), &[]),
    };
    if !real.exists() {
        return Ok(());
    }
    let bytes = match c.kind {
        Kind::Db => snapshot(&real, &s.scratch())?,
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

/// Joins the profile's mirror as adapter `pid`, creating and restoring it when this is the first. Under the
/// mirror's lock, so adapters starting together do not race.
fn attach(mu: Arc<dyn Secrets>, key: &Path, layout: Layout, pid: u32) -> io::Result<Mirror> {
    let mem = mirror_dir(&layout.memory, key);
    let lock = lock_path(&layout.memory, key);
    let top = mem.join("omp");
    let root = match &layout.profile {
        Some(p) => top.join("profiles").join(p),
        None => top.clone(),
    };
    let config_dir = relative_to_home(&layout.home, &top)?;
    let s = Mirror { layout, mu, mem, root, config_dir, key: key.to_path_buf(), lock, pid, _stop: None };
    let _guard = s.lock()?;
    let mut joined = s.mem.join("ready").exists();
    if joined && prune_users(&s.mem) && !daemon_alive(key) {
        // Nobody uses it: seal what it holds, then rebuild from the sealed copy. Unsealable, it is kept.
        link_all(&s.layout, &s.mem)?;
        if flush_locked(&s).is_ok() {
            adopt_all(&s.layout, &s.root);
            let _ = std::fs::remove_dir_all(&s.mem);
            joined = false;
        }
    }
    if joined {
        link_all(&s.layout, &s.mem)?;
        // What the mirror holds beyond the last seal (a daemon's rotations while no adapter was alive) wins.
        let _ = flush_locked(&s);
    } else {
        let _ = std::fs::remove_dir_all(&s.mem);
        let built = (|| {
            vfs::create_private_dir(&s.mem)?;
            link_all(&s.layout, &s.mem)?;
            for c in &CREDS {
                restore(&s, c)?;
            }
            vfs::write_private(&s.mem.join("ready"), b"")
        })();
        if let Err(e) = built {
            let _ = std::fs::remove_dir_all(&s.mem);
            return Err(e);
        }
    }
    vfs::write_private(&users_dir(&s.mem).join(pid.to_string()), b"")?;
    Ok(s)
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
    let mut s = attach(mu, key, layout, std::process::id())?;
    let (stop, rx) = mpsc::channel();
    s._stop = Some(stop);
    *g = Some(s);
    let _ = std::thread::Builder::new().name("apg-flush".into()).spawn(move || {
        while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(INTERVAL) {
            flush();
        }
    });
    Ok(true)
}

/// When mu has state encryption on and unlocked, unseals omp's credential state into memory and points omp at
/// it; true then. Idempotent, and safe to call again after mu unlocks.
pub fn materialize(mu: Arc<dyn Secrets>) -> io::Result<bool> {
    materialize_in(mu, &state::gw_dir(), Layout::from_env())
}

/// Like `flush`, with the error.
pub fn flush_checked() -> io::Result<bool> {
    match mirror_lock().as_ref() {
        Some(s) => {
            let _guard = s.lock()?;
            flush_locked(s)
        }
        None => Ok(false),
    }
}

/// Seals omp's current state back through mu now.
pub fn flush() {
    let _ = flush_checked();
}

/// Leaves the mirror: final flush, and removal when no other adapter is attached and no daemon runs. With a
/// daemon still running it stays, and the next attacher seals what the daemon wrote.
fn detach(s: Mirror) {
    let Ok(_guard) = s.lock() else { return };
    let _ = std::fs::remove_file(users_dir(&s.mem).join(s.pid.to_string()));
    let last = prune_users(&s.mem) && !daemon_alive(&s.key);
    if last {
        // Before the flush, which may wait on a mu that has stopped answering.
        adopt_all(&s.layout, &s.root);
    }
    let _ = flush_locked(&s);
    if last {
        let _ = std::fs::remove_dir_all(&s.mem);
    }
}

fn finish(old: Option<Mirror>) {
    if let Some(s) = old {
        detach(s);
    }
}

/// Final flush, then leaves the mirror (removing it when nothing else uses it).
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
    use std::collections::HashMap;
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
    fn materializing_again_keeps_the_directory() {
        let f = Fixture::new("again");
        f.materialize(f.layout(None)).unwrap();
        plant_db(&f.mem_agent().join("agent.db"));
        let mem = f.mem();
        std::fs::create_dir(f.real_agent().join("late")).unwrap();
        f.materialize(f.layout(None)).unwrap();
        assert_eq!(f.mem(), mem);
        assert_eq!(values(&f.mem_agent().join("agent.db")), [MARKER], "the live database is untouched");
        assert!(f.mem_agent().join("late").is_symlink(), "entries that appeared since are linked");
    }

    const TOKEN: &str = "auth-gateway.token";

    fn peer_pid() -> u32 {
        // SAFETY: getppid cannot fail.
        unsafe { libc::getppid() as u32 }
    }

    fn token(m: &Mirror) -> String {
        std::fs::read_to_string(m.root.join(TOKEN)).unwrap()
    }

    fn put_token(m: &Mirror, v: &str) {
        std::fs::write(m.root.join(TOKEN), v).unwrap();
    }

    fn fake_daemon(f: &Fixture) {
        std::fs::create_dir_all(f.key()).unwrap();
        state::write_state(&f.key(), std::process::id(), "http://127.0.0.1:1", "http://127.0.0.1:2").unwrap();
    }

    fn attach_as(f: &Fixture, pid: u32) -> Mirror {
        attach(f.mu.clone(), &f.key(), f.layout(None), pid).unwrap()
    }

    fn seal(f: &Fixture, v: &str) {
        f.mu.store.lock().unwrap().insert(TOKEN.into(), v.as_bytes().to_vec());
    }

    #[test]
    fn two_adapters_share_one_mirror_and_the_second_does_not_restore_over_it() {
        let f = Fixture::new("shared");
        seal(&f, "old");
        let a = attach_as(&f, std::process::id());
        assert_eq!(token(&a), "old");
        put_token(&a, "rotated");
        let b = attach_as(&f, peer_pid());
        assert_eq!(a.mem, b.mem);
        assert_eq!(a.mem, mirror_dir(&f.dir.join("mem"), &f.key()));
        assert_eq!(token(&b), "rotated", "joined, not restored");
        assert_eq!(f.sealed(TOKEN).unwrap(), b"rotated", "the attacher sealed the unsealed difference");
        for pid in [std::process::id(), peer_pid()] {
            assert!(users_dir(&a.mem).join(pid.to_string()).exists());
        }
        let puts = f.mu.puts.load(Ordering::SeqCst);
        detach(a);
        assert!(b.mem.exists(), "another adapter is still attached");
        assert!(!users_dir(&b.mem).join(std::process::id().to_string()).exists());
        detach(b);
        assert_eq!(f.mu.puts.load(Ordering::SeqCst), puts, "re-sealing unchanged content writes nothing");
    }

    #[test]
    fn the_last_adapter_leaving_keeps_the_mirror_while_a_daemon_runs_and_removes_it_otherwise() {
        let f = Fixture::new("leave");
        fake_daemon(&f);
        let a = attach_as(&f, std::process::id());
        let mem = a.mem.clone();
        detach(a);
        assert!(mem.exists(), "the daemon still writes into it");

        std::fs::remove_file(f.key().join("state.json")).unwrap();
        let b = attach_as(&f, std::process::id());
        detach(b);
        assert!(!mem.exists());
    }

    #[test]
    fn a_rotation_made_while_no_adapter_was_attached_is_sealed_by_the_next_attacher() {
        let f = Fixture::new("rotation");
        fake_daemon(&f);
        seal(&f, "old");
        let a = attach_as(&f, std::process::id());
        let root = a.root.clone();
        detach(a);
        std::fs::write(root.join(TOKEN), "rotated-by-daemon").unwrap();
        assert_eq!(f.sealed(TOKEN).unwrap(), b"old");
        let b = attach_as(&f, std::process::id());
        assert_eq!(f.sealed(TOKEN).unwrap(), b"rotated-by-daemon");
        assert_eq!(token(&b), "rotated-by-daemon");
    }

    #[test]
    fn an_unsealed_rotation_survives_a_rebuild_when_no_daemon_remains() {
        let f = Fixture::new("rebuild");
        fake_daemon(&f);
        seal(&f, "old");
        let a = attach_as(&f, std::process::id());
        let root = a.root.clone();
        detach(a);
        std::fs::write(root.join(TOKEN), "rotated-by-daemon").unwrap();
        std::fs::remove_file(f.key().join("state.json")).unwrap();
        let b = attach_as(&f, std::process::id());
        assert_eq!(f.sealed(TOKEN).unwrap(), b"rotated-by-daemon");
        assert_eq!(token(&b), "rotated-by-daemon");
    }

    #[test]
    fn a_dead_adapters_registry_entry_is_pruned_on_attach() {
        let f = Fixture::new("prune");
        let a = attach_as(&f, std::process::id());
        let dead = users_dir(&a.mem).join("999999999");
        std::fs::write(&dead, "").unwrap();
        let b = attach_as(&f, peer_pid());
        assert!(!dead.exists());
        assert!(users_dir(&b.mem).join(peer_pid().to_string()).exists());
    }

    #[test]
    fn a_newer_sealed_copy_is_not_overwritten_by_a_stale_clean_mirror() {
        let f = Fixture::new("newer");
        fake_daemon(&f);
        seal(&f, "old");
        let a = attach_as(&f, std::process::id());
        detach(a);
        seal(&f, "newer");
        let puts = f.mu.puts.load(Ordering::SeqCst);
        let b = attach_as(&f, std::process::id());
        assert_eq!(f.sealed(TOKEN).unwrap(), b"newer");
        assert_eq!(f.mu.puts.load(Ordering::SeqCst), puts);
        detach(b);

        std::fs::remove_file(f.key().join("state.json")).unwrap();
        let c = attach_as(&f, std::process::id());
        assert_eq!(f.sealed(TOKEN).unwrap(), b"newer");
        assert_eq!(token(&c), "newer", "an unused clean mirror is rebuilt from the sealed copy");
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
