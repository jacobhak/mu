//! Drives `mu-plugin` over stdio like mu does, answering its `core/secrets` calls.
#![allow(clippy::unwrap_used)] // integration-test helpers outside any #[test] fn

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A fake mu: the plugin's stdio, and an in-memory `core/secrets` store.
struct Mu {
    dir: std::path::PathBuf,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    sealed: bool,
    store: HashMap<String, String>,
    secrets_calls: Vec<Value>,
}

impl Mu {
    fn start(tag: &str, sealed: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("apg-adapter-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_mu-gateway"))
            .arg("mu-plugin")
            .env("HOME", &dir)
            .env("MU_GATEWAY_HOME", dir.join("gw"))
            .env("XDG_RUNTIME_DIR", &dir)
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .env_remove("PI_CODING_AGENT_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let (tx, lines) = mpsc::channel::<Value>();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        Self { dir, child, stdin, lines, sealed, store: HashMap::new(), secrets_calls: Vec::new() }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// Calls `method` and returns the reply, serving the plugin's own `core/secrets` requests meanwhile.
    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        self.wait_for(id)
    }

    fn wait_for(&mut self, id: u64) -> Value {
        loop {
            let v = self.lines.recv_timeout(Duration::from_secs(20)).expect("the plugin answers");
            if v["method"] == "core/secrets" {
                let p = &v["params"];
                let result = match p["op"].as_str().unwrap() {
                    "info" => json!({"sealed": self.sealed, "unlocked": self.sealed}),
                    "get" => json!({"value": self.store.get(p["name"].as_str().unwrap())}),
                    "put" => {
                        self.store.insert(p["name"].as_str().unwrap().into(), p["value"].as_str().unwrap().into());
                        json!({})
                    }
                    op => panic!("unexpected op {op}"),
                };
                self.secrets_calls.push(p.clone());
                self.send(json!({"jsonrpc":"2.0","id":v["id"],"result":result}));
            } else if v["id"] == id {
                return v;
            }
        }
    }

    fn shutdown(mut self) {
        self.send(json!({"jsonrpc":"2.0","method":"shutdown","params":{}}));
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn initialize_then_an_endpoint_peek_with_no_daemon_answers_null() {
    let mut mu = Mu::start("peek", false);
    assert_eq!(mu.call(1, "initialize", json!({}))["result"], json!({}));

    let r = mu.call(2, "provider/endpoint", json!({"clientPid": 1, "start": false}));
    assert!(r["error"].is_null(), "{r}");
    assert!(r["result"].is_null(), "{r}");
    assert!(mu.secrets_calls.iter().any(|c| c["op"] == "info"), "the plugin asked mu whether state is sealed");
    assert!(!mu.dir.join("gw").exists(), "a peek starts and creates nothing");

    let r = mu.call(3, "provider/endpoint", json!({}));
    assert_eq!(r["error"]["code"], -32602);
    let r = mu.call(4, "no/such", json!({}));
    assert_eq!(r["error"]["code"], -32601);
    mu.shutdown();
}

#[test]
fn a_sealed_mu_gets_omps_plaintext_credentials_through_core_secrets() {
    let mut mu = Mu::start("sealed", true);
    let token = mu.dir.join(".omp/auth-gateway.token");
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    std::fs::write(&token, "tok-123").unwrap();

    assert_eq!(mu.call(1, "initialize", json!({}))["result"], json!({}));
    // The first call waits for the background unsealing, so everything is in mu's store when it answers.
    let r = mu.call(2, "provider/endpoint", json!({"clientPid": 1, "start": false}));
    assert!(r["result"].is_null(), "{r}");

    use base64::Engine;
    let sealed = base64::engine::general_purpose::STANDARD.decode(&mu.store["auth-gateway.token"]).unwrap();
    assert_eq!(sealed, b"tok-123");
    assert!(!token.exists(), "the plaintext token is gone from the volume");
    assert!(mu.secrets_calls.iter().any(|c| c["op"] == "get" && c["name"] == "agent.db"));
    mu.shutdown();
}
