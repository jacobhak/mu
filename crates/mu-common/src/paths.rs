use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// The active omp profile name (`OMP_PROFILE`, else `PI_PROFILE`), if any.
pub fn profile() -> Option<String> {
    std::env::var("OMP_PROFILE")
        .ok()
        .or_else(|| std::env::var("PI_PROFILE").ok())
        .filter(|s| !s.is_empty())
}

pub fn omp_config_root() -> PathBuf {
    match profile() {
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

/// Per-profile cache dir: `$MU_HOME`, or `$MU_HOME/profiles/<name>` under an omp profile. Data that depends
/// on a profile's credentials (model catalogue, prices) lives here so profiles never overwrite each other's.
pub fn profile_cache_dir() -> PathBuf {
    match profile() {
        // The name becomes a path component; anything that could escape it falls back to the default dir.
        Some(p) if !p.contains(['/', '\\']) && p != "." && p != ".." => mu_home().join("profiles").join(p),
        _ => mu_home(),
    }
}

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
    mu_home().join("sessions").join(encode_cwd(cwd))
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
