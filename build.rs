use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=upstream.toml");
    println!("cargo:rerun-if-env-changed=MU_BUILD_SHA");
    // install.sh sets this when it builds in an offload job, whose checkout is a snapshot commit, not HEAD.
    let given = std::env::var("MU_BUILD_SHA").ok().filter(|s| !s.is_empty());
    let mut sha = given
        .clone()
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".into());
    if given.is_none()
        && sha != "unknown"
        && git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty())
    {
        sha.push_str("-dirty");
    }
    if let Some(gd) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "index", "logs/HEAD"] {
            println!("cargo:rerun-if-changed={gd}/{f}");
        }
    }
    let toml = std::fs::read_to_string("upstream.toml").unwrap_or_default();
    let omp = toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("omp = \""))
        .and_then(|r| r.split('"').next())
        .unwrap_or("unknown");
    println!("cargo:rustc-env=MU_GIT_SHA={sha}");
    println!(
        "cargo:rustc-env=MU_VERSION={} ({sha}) oh-my-pi crates {omp}",
        std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")
    );
}
