use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn profile() -> Option<String> {
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

pub fn mu_home() -> PathBuf {
    match std::env::var_os("MU_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".mu"),
    }
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
