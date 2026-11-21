use std::process::{Command, Output};

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("apg-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ai-provider-gateway"))
            .args(args)
            .env("HOME", &self.0)
            .env("AI_PROVIDER_GATEWAY_HOME", self.0.join("gw"))
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .output()
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn endpoint_without_start_prints_null_when_no_daemon_runs() {
    let s = Scratch::new("endpoint");
    let o = s.run(&["endpoint", "--no-start"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "null\n");
    assert!(!s.0.join("gw").exists(), "nothing is created, no lease is touched");
}

#[test]
fn endpoint_without_start_is_per_profile() {
    let s = Scratch::new("profile");
    let o = s.run(&["endpoint", "--no-start", "--profile", "nosuch"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "null\n");
}

#[test]
fn status_reports_not_running() {
    let s = Scratch::new("status");
    let o = s.run(&["status"]);
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["running"], false);
    assert_eq!(v["leases"], 0);
}

#[test]
fn stop_with_no_daemon_says_so() {
    let s = Scratch::new("stop");
    let v: serde_json::Value = serde_json::from_slice(&s.run(&["stop"]).stdout).unwrap();
    assert_eq!(v["stopped"], false);
}

#[test]
fn a_missing_omp_is_an_error_on_stderr_with_exit_1() {
    let s = Scratch::new("noomp");
    let o = Command::new(env!("CARGO_BIN_EXE_ai-provider-gateway"))
        .args(["prices"])
        .env("HOME", &s.0)
        .env("AI_PROVIDER_GATEWAY_HOME", s.0.join("gw"))
        .env("AI_PROVIDER_GATEWAY_OMP", s.0.join("no-such-omp"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(o.stdout.is_empty());
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot run `omp`"));
}

#[test]
fn version_names_the_pinned_omp() {
    let s = Scratch::new("version");
    let o = s.run(&["--version"]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(text.contains("(omp "), "{text}");
}
