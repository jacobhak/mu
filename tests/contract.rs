//! Contract tests against the real pinned omp, under a scratch HOME. Run by `scripts/ci-checks.sh` and the
//! maintenance bot with `-- --ignored contract_`; the binary comes from `AI_PROVIDER_GATEWAY_OMP`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("apg-contract-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn omp(&self, args: &[&str]) -> Command {
        let bin = std::env::var_os("AI_PROVIDER_GATEWAY_OMP").expect("AI_PROVIDER_GATEWAY_OMP names the omp binary");
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join(".config"))
            .env("XDG_DATA_HOME", self.0.join(".local/share"))
            .env("XDG_CACHE_HOME", self.0.join(".cache"))
            .stdin(Stdio::null());
        for k in [
            "OMP_PROFILE",
            "PI_PROFILE",
            "PI_CONFIG_DIR",
            "PI_CODING_AGENT_DIR",
            "OMP_AUTH_BROKER_URL",
            "OMP_AUTH_BROKER_TOKEN",
        ] {
            cmd.env_remove(k);
        }
        cmd
    }

    fn output(&self, args: &[&str]) -> Output {
        self.omp(args).output().expect("cannot run omp")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Served(Child);

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `omp <args>` and returns it with the first `http://` URL it prints.
fn serve(mut cmd: Command) -> (Served, String) {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("cannot run omp");
    let (tx, rx) = mpsc::channel::<String>();
    let out: Box<dyn Read + Send> = Box::new(child.stdout.take().unwrap());
    let err: Box<dyn Read + Send> = Box::new(child.stderr.take().unwrap());
    for r in [out, err] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    let served = Served(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(line) = rx.recv_timeout(left) else { break };
        if let Some(i) = line.find("http://") {
            let url = line[i..].chars().take_while(|c| !c.is_whitespace() && *c != '"').collect();
            return (served, url);
        }
        seen.push_str(&line);
        seen.push('\n');
    }
    panic!("omp did not report a listening URL:\n{seen}");
}

fn http_get(url: &str, path: &str) -> String {
    let hostport = url.strip_prefix("http://").unwrap().split('/').next().unwrap();
    let mut s = TcpStream::connect(hostport).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n").unwrap();
    let mut body = String::new();
    let _ = s.read_to_string(&mut body);
    body
}

fn token(home: &Path, name: &str) -> String {
    std::fs::read_to_string(home.join(".omp").join(name)).unwrap_or_default().trim().to_string()
}

#[test]
#[ignore = "contract: runs the real omp"]
fn contract_auth_broker_and_gateway_serve_and_answer_healthz() {
    let s = Scratch::new("serve");
    let (_broker, broker_url) = serve(s.omp(&["auth-broker", "serve", "--bind=127.0.0.1:0"]));
    let o = s.output(&["auth-gateway", "token"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (broker_token, gateway_token) = (token(&s.0, "auth-broker.token"), token(&s.0, "auth-gateway.token"));
    assert!(!broker_token.is_empty(), "auth-gateway token writes auth-broker.token");
    assert!(!gateway_token.is_empty(), "auth-gateway token writes auth-gateway.token");

    let mut cmd = s.omp(&["auth-gateway", "serve", "--bind=127.0.0.1:0"]);
    cmd.env("OMP_AUTH_BROKER_URL", &broker_url).env("OMP_AUTH_BROKER_TOKEN", &broker_token);
    let (_gateway, gateway_url) = serve(cmd);
    let reply = http_get(&gateway_url, "/healthz");
    assert!(reply.starts_with("HTTP/1.1 200") || reply.starts_with("HTTP/1.0 200"), "{reply}");
}

#[test]
#[ignore = "contract: runs the real omp"]
fn contract_models_ls_json_parses_into_the_price_shape() {
    let s = Scratch::new("models");
    let o = s.output(&["models", "ls", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("models ls --json prints JSON");
    let models = v["models"].as_array().expect("a models array");
    assert!(!models.is_empty());
    for m in models {
        assert!(m["provider"].is_string() && m["id"].is_string(), "{m}");
    }
    assert!(
        models.iter().any(|m| {
            m["provider"] == "anthropic"
                && m["pricingStatus"] == "fixed"
                && m["cost"]["input"].as_f64().is_some_and(|c| c > 0.0)
                && ["output", "cacheRead", "cacheWrite"].iter().all(|k| m["cost"][k].is_number())
        }),
        "no fixed-price anthropic model with the cost fields"
    );
}

#[test]
#[ignore = "contract: runs the real omp"]
fn contract_token_of_an_unknown_credential_exits_non_zero_without_output() {
    let s = Scratch::new("token");
    let o = s.output(&["token", "apg-contract-nosuchcredential"]);
    assert!(!o.status.success());
    assert!(o.stdout.is_empty(), "{}", String::from_utf8_lossy(&o.stdout));
}
