//! Drives `mu-plugin` over stdio like mu does, answering its `core/secrets` calls.

use serde_json::{Value, json};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn initialize_then_an_endpoint_peek_with_no_daemon_answers_null() {
    let dir = std::env::temp_dir().join(format!("apg-adapter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ai-provider-gateway"))
        .arg("mu-plugin")
        .env("HOME", &dir)
        .env("AI_PROVIDER_GATEWAY_HOME", dir.join("gw"))
        .env_remove("OMP_PROFILE")
        .env_remove("PI_PROFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (tx, rx) = mpsc::channel::<Value>();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let mut send = |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    let secrets_calls = Cell::new(0);
    // Answers the plugin's own requests (mu's side) until the reply to `id` arrives.
    let reply_to = |id: u64, send: &mut dyn FnMut(Value)| -> Value {
        loop {
            let v = rx.recv_timeout(Duration::from_secs(20)).expect("the plugin answers");
            if v["method"] == "core/secrets" {
                secrets_calls.set(secrets_calls.get() + 1);
                assert_eq!(v["params"]["op"], "info");
                send(json!({"jsonrpc":"2.0","id":v["id"],"result":{"sealed":false,"unlocked":false}}));
            } else if v["id"] == id {
                return v;
            }
        }
    };

    send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    assert_eq!(reply_to(1, &mut send)["result"], json!({}));

    send(json!({"jsonrpc":"2.0","id":2,"method":"provider/endpoint","params":{"clientPid":1,"start":false}}));
    let r = reply_to(2, &mut send);
    assert!(r["error"].is_null(), "{r}");
    assert!(r["result"].is_null(), "{r}");
    assert!(secrets_calls.get() >= 1, "the plugin asked mu whether state encryption is on");
    assert!(!dir.join("gw").exists(), "a peek starts and creates nothing");

    send(json!({"jsonrpc":"2.0","id":3,"method":"provider/endpoint","params":{}}));
    let r = reply_to(3, &mut send);
    assert_eq!(r["error"]["code"], -32602);

    send(json!({"jsonrpc":"2.0","method":"shutdown","params":{}}));
    drop(stdin);
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
}
