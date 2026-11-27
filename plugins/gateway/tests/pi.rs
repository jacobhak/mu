//! The pi backend against a fake `pi`: a shell script that stands in for `pi --mode rpc -e proxy.mjs`.
#![allow(clippy::unwrap_used)] // integration-test helpers outside any #[test] fn

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const PRICES: &str = r#"{"models":[{"provider":"baseten","id":"moonshotai/Kimi-K3","name":"Kimi K3","reasoning":true,"cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0}}]}"#;

const FAKE_PI: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_PI_DIR/args"
case "$MU_GW_PI_MODE" in
  prices)
    [ -n "$FAKE_PI_FAIL" ] && { echo "boom" >&2; exit 3; }
    printf '%s' "$FAKE_PI_PRICES" > "$MU_GW_PI_OUT"
    exit 0 ;;
  serve)
    printf '%s' "$MU_GW_PI_TOKEN" > "$FAKE_PI_DIR/seen-token"
    echo "mu-gateway-pi proxy listening on http://127.0.0.1:$FAKE_PI_PORT" >&2
    cat > /dev/null ;;
esac
"#;

struct Scratch {
    dir: PathBuf,
    port: u16,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("apg-pi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pi"), FAKE_PI).unwrap();
        std::fs::set_permissions(dir.join("pi"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("proxy.mjs"), "").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        Self { dir, port }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu-gateway"));
        cmd.args(args)
            .env("HOME", &self.dir)
            .env("MU_GATEWAY_HOME", self.dir.join("omp-gw"))
            .env("MU_GATEWAY_PI_HOME", self.dir.join("pi-gw"))
            .env("MU_GATEWAY_PI", self.dir.join("pi"))
            .env("MU_GATEWAY_PI_PROXY", self.dir.join("proxy.mjs"))
            .env("FAKE_PI_DIR", &self.dir)
            .env("FAKE_PI_PORT", self.port.to_string())
            .env("FAKE_PI_PRICES", PRICES)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env_remove("MU_GATEWAY_BACKEND")
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("FAKE_PI_FAIL");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn args_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("args")).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.run(&["--backend", "pi", "stop"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stdout_json(o: &Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn the_daemon_serves_the_proxy_and_hands_out_its_token() {
    let s = Scratch::new("daemon");
    let ep = stdout_json(&s.run(&["--backend", "pi", "endpoint", "--client-pid", &std::process::id().to_string()]));
    assert_eq!(ep["baseUrl"], format!("http://127.0.0.1:{}", s.port));
    let token = ep["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);

    let token_file = s.dir.join("pi-gw/token");
    assert_eq!(std::fs::read_to_string(&token_file).unwrap(), token);
    assert_eq!(std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(s.dir.join("seen-token")).unwrap(), token, "the proxy got the token in its env");
    assert!(s.dir.join("pi-gw/state.json").exists());
    assert!(s.dir.join("pi-gw/leases").join(std::process::id().to_string()).exists());
    assert!(!s.dir.join("omp-gw").exists(), "the omp state dir is untouched");

    let args = s.args_log();
    let proxy = s.dir.join("proxy.mjs");
    assert!(
        args.contains(&format!(
            "--mode rpc --no-session -ne -e {} --no-mcp --no-skills --no-prompt-templates --no-themes \
             --no-context-files -na",
            proxy.display()
        )),
        "{args}"
    );

    let again = stdout_json(&s.run(&["--backend", "pi", "endpoint", "--client-pid", "1"]));
    assert_eq!(again, ep, "a second client shares the daemon");
    let peek = stdout_json(&s.run(&["--backend", "pi", "endpoint", "--no-start"]));
    assert_eq!(peek, ep);
    let status = stdout_json(&s.run(&["--backend", "pi", "status"]));
    assert_eq!(status["running"], true);

    assert_eq!(stdout_json(&s.run(&["--backend", "pi", "stop"]))["stopped"], true);
    assert_eq!(stdout_json(&s.run(&["--backend", "pi", "status"]))["running"], false);
    assert_eq!(stdout_json(&s.run(&["--backend", "pi", "endpoint", "--no-start"])), Value::Null);
}

#[test]
fn the_backend_env_selects_pi_and_the_flag_overrides_it() {
    let s = Scratch::new("env");
    let mut cmd = s.command(&["endpoint", "--no-start"]);
    cmd.env("MU_GATEWAY_BACKEND", "pi");
    assert_eq!(stdout_json(&cmd.output().unwrap()), Value::Null);

    let mut cmd = s.command(&["prices"]);
    cmd.env("MU_GATEWAY_BACKEND", "pi");
    assert_eq!(stdout_json(&cmd.output().unwrap()), serde_json::from_str::<Value>(PRICES).unwrap());

    let mut cmd = s.command(&["--backend", "omp", "prices"]);
    cmd.env("MU_GATEWAY_BACKEND", "pi").env("MU_GATEWAY_OMP", s.dir.join("no-such-omp"));
    let o = cmd.output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot run `omp`"), "the flag wins over the env");

    let mut cmd = s.command(&["status"]);
    cmd.env("MU_GATEWAY_BACKEND", "nonsense");
    let o = cmd.output().unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("MU_GATEWAY_BACKEND"));
}

#[test]
fn prices_runs_pi_in_prices_mode_and_parses_the_file() {
    let s = Scratch::new("prices");
    let v = stdout_json(&s.run(&["--backend", "pi", "prices"]));
    assert_eq!(v, serde_json::from_str::<Value>(PRICES).unwrap());
    assert!(s.args_log().contains("--mode rpc"));
    assert!(!s.dir.join("pi-gw").exists(), "prices starts no daemon and creates no state");
}

#[test]
fn a_failing_pi_prices_run_is_an_error_with_its_stderr() {
    let s = Scratch::new("prices-fail");
    let o = s.command(&["--backend", "pi", "prices"]).env("FAKE_PI_FAIL", "1").output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(o.stdout.is_empty());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("pi prices run") && err.contains("boom"), "{err}");
}

#[test]
fn a_missing_pi_or_proxy_is_an_error_on_stderr() {
    let s = Scratch::new("missing");
    let o = s.command(&["--backend", "pi", "prices"]).env("MU_GATEWAY_PI", s.dir.join("no-such-pi")).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot run `pi`"));

    let o =
        s.command(&["--backend", "pi", "prices"]).env("MU_GATEWAY_PI_PROXY", s.dir.join("nope.mjs")).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("proxy extension not found"));
}

#[test]
fn login_execs_pi_auth_login() {
    let s = Scratch::new("login");
    let o = s.run(&["--backend", "pi", "login", "anthropic"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(s.args_log().trim(), "auth login anthropic");
}

#[test]
fn the_pi_adapter_never_touches_core_secrets() {
    let s = Scratch::new("adapter");
    let mut child = s
        .command(&["--backend", "pi", "mu-plugin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let mut call = |id: u64, method: &str, params: Value| -> Value {
        writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).unwrap();
        stdin.flush().unwrap();
        loop {
            let v = lines.recv_timeout(Duration::from_secs(30)).expect("the plugin answers");
            assert_ne!(v["method"], "core/secrets", "the pi backend must not touch sealed state: {v}");
            if v["id"] == id {
                return v;
            }
        }
    };
    assert_eq!(call(1, "initialize", json!({}))["result"], json!({}));
    assert!(call(2, "provider/endpoint", json!({"clientPid": 1, "start": false}))["result"].is_null());
    let prices = call(3, "provider/prices", json!({}));
    assert_eq!(prices["result"], serde_json::from_str::<Value>(PRICES).unwrap(), "{prices}");
    let ep = call(4, "provider/endpoint", json!({"clientPid": std::process::id()}));
    assert_eq!(ep["result"]["baseUrl"], format!("http://127.0.0.1:{}", s.port), "{ep}");
    assert!(ep["result"]["token"].as_str().is_some_and(|t| t.len() == 64));
    let logins = call(6, "provider/logins", json!({}));
    assert_eq!(logins["result"]["providers"][0]["label"], "Claude Pro/Max (pi)", "{logins}");
    let login = call(7, "provider/login", json!({"provider": "anthropic"}));
    let argv: Vec<&str> = login["result"]["command"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(argv, [s.dir.join("pi").to_str().unwrap(), "auth", "login", "anthropic"], "{login}");
    assert_eq!(login["result"]["env"], json!({}));
    assert_eq!(call(8, "provider/login_done", json!({}))["result"], json!({}));
    assert!(call(5, "shutdown", json!({}))["result"].is_null());
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
