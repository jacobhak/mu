use std::path::{Path, PathBuf};

/// The roots mu derives its paths from. Production reads them from the environment (`Home::from_env`);
/// code that takes a `Home` is tested with `Home::at`, without touching process-wide variables.
#[derive(Clone, Debug)]
pub struct Home {
    home: PathBuf,
    mu_home: PathBuf,
    profile: Option<String>,
}

impl Home {
    /// `$HOME`, `$MU_HOME` (default `<home>/.mu`) and the active omp profile.
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let mu_home = match std::env::var_os("MU_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => home.join(".mu"),
        };
        Self { home, mu_home, profile: profile() }
    }

    /// `home` as the user's home with mu's state under `<home>/.mu` and no profile.
    pub fn at(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self { mu_home: home.join(".mu"), home, profile: None }
    }

    /// Keeps the home directory but keeps mu's state in `mu_home` (what `MU_HOME` does).
    pub fn with_mu_home(mut self, mu_home: impl Into<PathBuf>) -> Self {
        self.mu_home = mu_home.into();
        self
    }

    /// The user's home directory; empty when `HOME` is unset.
    pub fn dir(&self) -> &Path {
        &self.home
    }

    pub fn mu_home(&self) -> &Path {
        &self.mu_home
    }

    /// See `profile_home`.
    pub fn profile_home(&self) -> PathBuf {
        profile_subdir(self.mu_home.clone(), self.profile.as_deref())
    }

    pub fn encode_cwd(&self, cwd: &Path) -> String {
        if let Ok(rel) = cwd.strip_prefix(std::env::temp_dir()) {
            return format!("-tmp-{}", dashed(rel));
        }
        if let Ok(rel) = cwd.strip_prefix(&self.home) {
            return format!("-{}", dashed(rel));
        }
        let s = cwd.to_string_lossy();
        format!("--{}--", s.trim_start_matches('/').replace('/', "-"))
    }

    pub fn session_dir(&self, cwd: &Path) -> PathBuf {
        self.profile_home().join("sessions").join(self.encode_cwd(cwd))
    }

    /// `~` and `~/…` under the home directory; `None` for any other path or when no home is set.
    pub fn expand_tilde(&self, p: &str) -> Option<PathBuf> {
        if self.home.as_os_str().is_empty() { None } else { expand_tilde(p, &self.home) }
    }
}

pub fn home() -> PathBuf {
    Home::from_env().home
}

/// `~` and `~/…` under `home`; `None` for any other path.
pub fn expand_tilde(p: &str, home: &Path) -> Option<PathBuf> {
    if p == "~" {
        return Some(home.to_path_buf());
    }
    p.strip_prefix("~/").map(|rest| home.join(rest))
}

/// The active omp profile name (`OMP_PROFILE`, else `PI_PROFILE`), if any.
pub fn profile() -> Option<String> {
    std::env::var("OMP_PROFILE").ok().or_else(|| std::env::var("PI_PROFILE").ok()).filter(|s| !s.is_empty())
}

/// `name` is safe to use as one path component: no separators, not `.` or `..`.
pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
}

/// The active profile name, only if it is safe as a path component (anything else reads as no profile).
pub fn safe_profile() -> Option<String> {
    profile().filter(|p| valid_profile_name(p))
}

/// `base` itself, or `base/profiles/<name>` under a profile: the one rule for per-profile state dirs.
pub fn profile_subdir(base: PathBuf, profile: Option<&str>) -> PathBuf {
    match profile.filter(|p| valid_profile_name(p)) {
        Some(p) => base.join("profiles").join(p),
        None => base,
    }
}

pub fn omp_config_root() -> PathBuf {
    match safe_profile() {
        Some(p) => home().join(".omp/profiles").join(p),
        None => home().join(".omp"),
    }
}

pub fn omp_agent_dir() -> PathBuf {
    if profile().is_some() {
        return omp_config_root().join("agent");
    }
    match std::env::var_os("PI_CODING_AGENT_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".omp/agent"),
    }
}

/// The omp binary mu runs: `$MU_OMP`, else `<mu_home>/bin/omp` when installed, else `omp` on PATH.
pub fn omp_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("MU_OMP").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let own = mu_home().join("bin").join("omp");
    if own.is_file() { own } else { PathBuf::from("omp") }
}

/// Errors when neither `MU_HOME` nor `HOME` names a directory, so state is never written relative to the cwd.
pub fn check_home() -> Result<(), String> {
    let set = |k| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if set("MU_HOME") || set("HOME") {
        Ok(())
    } else {
        Err("neither HOME nor MU_HOME is set; mu needs one to know where to keep its state".to_string())
    }
}

pub fn mu_home() -> PathBuf {
    Home::from_env().mu_home
}

/// Per-profile state: `$MU_HOME`, or `$MU_HOME/profiles/<name>` under an omp profile. Everything that belongs
/// to one profile's work lives here (sessions and their run records, inbox, artifacts, plans, prompt history,
/// credentials, model and price caches), so profiles never see or overwrite each other's. Install-wide files
/// (bin, plugins, logs, config.toml, themes, model weights) stay in `mu_home()`.
pub fn profile_home() -> PathBuf {
    Home::from_env().profile_home()
}

/// The dirs and files `profile_home()` holds that existed in `$MU_HOME` itself before state was per profile.
pub const PROFILE_STATE: &[&str] = &[
    "sessions",
    "run",
    "inbox",
    "artifacts",
    "local",
    "blobs",
    "plans",
    "deliveries",
    "mcp",
    "permissions",
    "predict",
    "history.jsonl",
    "models.toml",
];

/// The running executable, for re-spawning ourselves (subagents, daemons). After the binary is replaced on
/// disk (`cargo install` under a running session) `current_exe()` names a path that no longer exists
/// (`… (deleted)`); `/proc/self/exe` still executes the running image, so children match the parent's version.
pub fn self_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.exists() {
        return Ok(exe);
    }
    let proc_exe = PathBuf::from("/proc/self/exe");
    if proc_exe.exists() { Ok(proc_exe) } else { Ok(exe) }
}

pub fn session_dir(cwd: &Path) -> PathBuf {
    Home::from_env().session_dir(cwd)
}

fn dashed(rel: &Path) -> String {
    rel.to_string_lossy().replace('/', "-")
}

pub fn encode_cwd(cwd: &Path) -> String {
    Home::from_env().encode_cwd(cwd)
}

/// Runs the real `omp_bin()` with `args` (stdin null, 60 s limit) and returns its stdout. For contract tests.
#[cfg(any(test, feature = "test-support"))]
pub fn run_omp_blocking(args: &[&str], env: &[(&str, &str)]) -> String {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(omp_bin());
    crate::omp_state::apply(&mut cmd).expect("omp state is locked");
    let mut child = cmd
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cannot run omp");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().expect("wait") {
            Some(_) => break,
            None if start.elapsed() > std::time::Duration::from_secs(60) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("omp {args:?} timed out after 60s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    String::from_utf8_lossy(&reader.join().expect("reader thread does not panic")).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_only_at_the_start_as_a_whole_component() {
        let h = Path::new("/h");
        assert_eq!(expand_tilde("~", h), Some(PathBuf::from("/h")));
        assert_eq!(expand_tilde("~/a/b", h), Some(PathBuf::from("/h/a/b")));
        assert_eq!(expand_tilde("~x", h), None);
        assert_eq!(expand_tilde("a/~", h), None);
    }

    #[test]
    fn encode() {
        let home = Home::at("/home/u");
        assert_eq!(home.encode_cwd(Path::new("/home/u/code/private/mu")), "-code-private-mu");
        assert_eq!(home.encode_cwd(Path::new("/home/u")), "-");
        assert_eq!(home.encode_cwd(Path::new("/opt/x")), "--opt-x--");
    }

    #[test]
    fn an_explicit_home_roots_state_without_the_environment() {
        let home = Home::at("/home/u");
        assert_eq!(home.mu_home(), Path::new("/home/u/.mu"));
        assert_eq!(home.session_dir(Path::new("/opt/x")), PathBuf::from("/home/u/.mu/sessions/--opt-x--"));
        let moved = Home::at("/home/u").with_mu_home("/state");
        assert_eq!(moved.profile_home(), PathBuf::from("/state"));
        assert_eq!(Home::at("").expand_tilde("~/x"), None, "an empty home leaves the path alone");
        assert_eq!(home.expand_tilde("~/x"), Some(PathBuf::from("/home/u/x")));
    }
}
