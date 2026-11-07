use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
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

pub fn mu_home() -> PathBuf {
    match std::env::var_os("MU_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".mu"),
    }
}

/// Per-profile state: `$MU_HOME`, or `$MU_HOME/profiles/<name>` under an omp profile. Everything that belongs
/// to one profile's work lives here (sessions and their run records, inbox, artifacts, plans, prompt history,
/// credentials, model and price caches), so profiles never see or overwrite each other's. Install-wide files
/// (bin, plugins, logs, config.toml, themes, model weights) stay in `mu_home()`.
pub fn profile_home() -> PathBuf {
    profile_subdir(mu_home(), profile().as_deref())
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
    profile_home().join("sessions").join(encode_cwd(cwd))
}

fn dashed(rel: &Path) -> String {
    rel.to_string_lossy().replace('/', "-")
}

pub fn encode_cwd(cwd: &Path) -> String {
    if let Ok(rel) = cwd.strip_prefix(std::env::temp_dir()) {
        return format!("-tmp-{}", dashed(rel));
    }
    if let Ok(rel) = cwd.strip_prefix(home()) {
        return format!("-{}", dashed(rel));
    }
    let s = cwd.to_string_lossy();
    format!("--{}--", s.trim_start_matches('/').replace('/', "-"))
}

/// Runs the real `omp_bin()` with `args` (stdin null, 60 s limit) and returns its stdout. For contract tests.
pub fn run_omp_blocking(args: &[&str], env: &[(&str, &str)]) -> String {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(omp_bin())
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cannot run omp");
    let mut stdout = child.stdout.take().unwrap();
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
    String::from_utf8_lossy(&reader.join().unwrap()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode() {
        let h = home();
        assert_eq!(encode_cwd(&h.join("code/private/mu")), "-code-private-mu");
        assert_eq!(encode_cwd(&h), "-");
        assert_eq!(encode_cwd(Path::new("/opt/x")), "--opt-x--");
    }
}
