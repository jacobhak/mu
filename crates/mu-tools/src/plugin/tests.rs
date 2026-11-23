use super::host::{InitParams, McpSync, PluginHost};
use super::manifest::{self, Manifest};
use super::process::sibling_of;
use crate::mcp;
use serde_json::json;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// `block_on_local` with a private MU_HOME, so plugin logs and MCP image blobs stay out of the real `~/.mu`.
fn run<F: std::future::Future>(f: F) -> F::Output {
    mu_testutil::home();
    mu_testutil::block_on_local(f)
}

/// Whether any process in group `pgid` is still running. A killed process whose parent is gone
/// stays a zombie until something reaps it — forever in a container whose PID 1 does not reap (the
/// release build) — and a zombie still answers `kill(-pgid, 0)`, so ask each member's state.
fn group_running(pgid: i32) -> bool {
    let group = pgid.to_string();
    let out = std::process::Command::new("ps").args(["-o", "pgid=,stat=", "-ax"]).output().expect("ps runs");
    String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        matches!((fields.next(), fields.next()), (Some(g), Some(stat)) if g == group && !stat.starts_with('Z'))
    })
}

/// A plugin script: answers `initialize`, then hangs on `tool/call` or exits, per `on_call`.
fn host(on_call: &str, lifetime: &str) -> (Rc<PluginHost>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("mu-plug-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!(
        r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
    *'"shutdown"'*) exit 0 ;;
    *'"tool/call"'*) {on_call} ;;
  esac
done
"#
    );
    std::fs::write(dir.join("p.sh"), script).unwrap();
    let m = Manifest {
        name: "t".into(),
        command: vec!["sh".into(), "p.sh".into()],
        lifetime: manifest::Lifetime::parse(lifetime).unwrap(),
        hooks: vec![],
        required_hooks: vec![],
        provider: None,
        tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
        timeout: std::time::Duration::from_secs(300),
        dir: dir.clone(),
    };
    let (ntx, _nrx) = mpsc::unbounded_channel();
    (PluginHost::new(vec![m], vec![], ntx, Default::default()), dir)
}

#[test]
fn a_cancelled_call_releases_its_bookkeeping() {
    run(async {
        let (h, dir) = host("touch called; sleep 100", "session");
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let called = dir.join("called");
        tokio::task::spawn_local(async move {
            mu_testutil::wait_until("the call to reach the plugin", || called.exists()).await;
            c.cancel();
        });
        let r = h.call_tool("slow", &json!({}), &cancel).await;
        assert!(r.is_error && r.text.contains("aborted"), "{}", r.text);
        let proc = h.slot(0).proc.borrow().clone().unwrap();
        assert_eq!(proc.inflight.get(), 0);
        assert!(proc.pending.is_empty());
        h.shutdown_all().await;
        let _ = std::fs::remove_dir_all(dir);
    });
}

#[test]
fn a_plugin_that_exits_mid_call_fails_the_call_and_frees_the_slot() {
    run(async {
        let (h, dir) = host("exit 3", "session");
        let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
        assert!(r.is_error, "{}", r.text);
        mu_testutil::wait_until("the slot to clear", || h.slot(0).proc.borrow().is_none()).await;
        assert!(h.slot(0).proc.borrow().is_none(), "slot cleared so the next call restarts it");
        let _ = std::fs::remove_dir_all(dir);
    });
}

#[test]
fn a_call_during_idle_shutdown_gets_a_fresh_process() {
    run(async {
        let (h, dir) =
            host("printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":\"ok\"}}\\n' \"$id\"", "idle:1s");
        let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "ok");
        let first = h.slot(0).proc.borrow().clone().unwrap().pid;
        mu_testutil::wait_gone(first as i32).await;
        let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "ok", "{}", r.text);
        let second = h.slot(0).proc.borrow().clone().unwrap().pid;
        assert_ne!(first, second);
        h.shutdown_all().await;
        let _ = std::fs::remove_dir_all(dir);
    });
}

#[test]
fn core_secrets_are_namespaced_by_plugin() {
    run(async {
        let on_call = r#"printf '{"jsonrpc":"2.0","id":50,"method":"core/secrets","params":{"op":"put","name":"x","value":"aGk="}}\n'
read -r r1
printf '{"jsonrpc":"2.0","id":51,"method":"core/secrets","params":{"op":"get","name":"x"}}\n'
read -r r2
printf '{"jsonrpc":"2.0","id":52,"method":"core/secrets","params":{"op":"put","name":"../x","value":"aGk="}}\n'
read -r r3
printf '%s\n%s\n%s\n' "$r1" "$r2" "$r3" > replies.log
printf '{"jsonrpc":"2.0","id":%s,"result":{"content":"ok"}}\n' "$id""#;
        let (h, dir) = host(on_call, "session");
        let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "ok", "{}", r.text);
        let replies = std::fs::read_to_string(dir.join("replies.log")).unwrap();
        let mut lines = replies.lines();
        assert!(lines.next().unwrap().contains(r#""result":{}"#));
        assert!(lines.next().unwrap().contains(r#""value":"aGk=""#));
        assert!(lines.next().unwrap().contains(r#""code":-32603"#));
        let stored = mu_common::paths::profile_home().join("secrets/plugin-t/x");
        assert_eq!(std::fs::read(stored).unwrap(), b"hi");
        h.shutdown_all().await;
        let _ = std::fs::remove_dir_all(dir);
    });
}

/// A shell-script MCP server: two-page `tools/list`, and calls that return text, an error and an
/// image, ping the client, or hang. Every line it receives is appended to `$LOG`.
const MCP_SERVER: &str = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[0-9]*"|[0-9]*).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"instructions":"be nice"}}\n' "$id" ;;
    *'"method":"tools/list"'*'"cursor":"c2"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"other","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"$schema":"x","type":"object","properties":{"t":{"type":"string"}}}}],"nextCursor":"c2"}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hi"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"other"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[{"type":"text","text":"boom"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"pic"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"pinger"'*)
      printf '{"jsonrpc":"2.0","id":98,"method":"ping"}\n{"jsonrpc":"2.0","id":99,"method":"roots/list"}\n'
      read -r r1; read -r r2; printf '%s\n%s\n' "$r1" "$r2" >> "$LOG"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"tokener"'*)
      printf '{"jsonrpc":"2.0","id":97,"method":"core/mcpSecret","params":{"op":"get"}}\n'
      read -r r1; printf '%s\n' "$r1" >> "$LOG"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"done"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"hang"'*) sleep 100 & ;;
  esac
done
"#;

struct McpFx {
    dir: std::path::PathBuf,
    cache: std::path::PathBuf,
}

impl McpFx {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("mu-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        std::fs::write(dir.join("server.sh"), MCP_SERVER).unwrap();
        Self { cache: dir.join("cache"), dir }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    /// A host with one server `srv`; `extra` is more JSON entry fields, `arg` an extra argument.
    fn host(&self, extra: &str, arg: &str) -> (Rc<PluginHost>, mpsc::UnboundedReceiver<String>) {
        let entry = format!(
            r#"{{"mcpServers":{{"srv":{{"command":"sh","args":["server.sh","{arg}"],"env":{{"LOG":"{}"}}{extra}}}}}}}"#,
            self.dir.join("log").display()
        );
        std::fs::write(self.dir.join("agent/mcp.json"), entry).unwrap();
        let (servers, warnings) = mcp::config::load_with(&self.dir, &self.dir.join("agent"), &|_| None);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (ntx, nrx) = mpsc::unbounded_channel();
        let init = InitParams { cwd: self.dir.to_string_lossy().into(), ..Default::default() };
        (
            PluginHost::new_with_mcp(
                vec![],
                servers,
                manifest::Lifetime::Session,
                self.cache.clone(),
                vec![],
                ntx,
                init,
                crate::tools::is_builtin,
            ),
            nrx,
        )
    }
}

impl Drop for McpFx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn mcp_cold_start_lists_two_pages_then_warm_start_spawns_nothing() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        assert!(h.tool_defs().is_empty(), "nothing is advertised before the first listing");
        h.list_missing().await;
        let names: Vec<String> = h.tool_defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["mcp__srv_echo", "mcp__srv_other"]);
        assert!(h.tool_defs()[0].schema.get("$schema").is_none());
        assert_eq!(h.mcp_instructions().unwrap(), "## MCP Server Instructions\n\n### srv\nbe nice\n");
        let log = fx.log();
        assert!(log.contains(r#""protocolVersion":"2025-11-25""#) && log.contains("notifications/initialized"));
        assert!(!log.contains(r#""method":"shutdown""#) && !log.contains(r#""protocolVersion":1"#), "{log}");
        assert_eq!(log.matches(r#""method":"initialize""#).count(), 1, "one listing start");
        h.shutdown_all().await;

        // Same config: advertised from the cache, nothing is started.
        let (warm, _n) = fx.host("", "");
        assert_eq!(warm.tool_defs().len(), 2);
        assert!(warm.slot(0).proc.borrow().is_none());
        warm.list_missing().await;
        assert_eq!(fx.log().matches(r#""method":"initialize""#).count(), 1, "no second start");
        assert!(warm.mcp_instructions().is_some());

        // Changed args: another hash, so the cache misses and the server is listed again.
        let (changed, _n) = fx.host("", "--x");
        assert!(changed.tool_defs().is_empty());
        changed.list_missing().await;
        assert_eq!(changed.tool_defs().len(), 2);
        assert_eq!(fx.log().matches(r#""method":"initialize""#).count(), 2);
        changed.shutdown_all().await;
    });
}

#[test]
fn mcp_calls_return_text_errors_and_images_and_answer_server_requests() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        h.list_missing().await;
        let c = CancellationToken::new();
        let r = h.call_tool("mcp__srv_echo", &json!({"t": "", "extra": {}}), &c).await;
        assert_eq!((r.text.as_str(), r.is_error), ("hi", false));
        assert_eq!(r.details.unwrap(), json!({"serverName":"srv","mcpToolName":"echo","isError":false}));
        assert!(fx.log().contains(r#""arguments":{}"#), "empty optional values are dropped: {}", fx.log());
        let r = h.call_tool("mcp__srv_other", &json!({}), &c).await;
        assert_eq!((r.text.as_str(), r.is_error), ("Error: boom", true));
        // `pic` is not in the listing, so only the ones listed can be called.
        assert!(h.call_tool("mcp__srv_pic", &json!({}), &c).await.is_error);
        h.shutdown_all().await;
    });
}

#[test]
fn mcp_server_requests_during_a_call_are_answered() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        h.list_missing().await;
        // The script lists only echo and other; call `pinger` through the remote-name map.
        let slot = h.slot(0);
        let m = slot.as_mcp().unwrap();
        let mut taken = std::collections::HashSet::new();
        let tools: Vec<_> = ["echo", "pinger", "pic"]
            .iter()
            .map(|n| mcp::cache::RemoteTool { name: (*n).into(), description: String::new(), input_schema: json!({}) })
            .collect();
        m.set_tools(&tools, &mut taken, crate::tools::is_builtin);
        let r = h.call_tool("mcp__srv_pinger", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "pong", "{}", r.text);
        let log = fx.log();
        assert!(log.contains(r#""id":98,"result":{}"#), "ping answered: {log}");
        assert!(log.contains(r#""id":99,"result":{"roots":[{"uri":"file://"#), "roots answered: {log}");
        let r = h.call_tool("mcp__srv_pic", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].mime_type, "image/png");
        h.shutdown_all().await;
    });
}

#[test]
fn mcp_token_requests_are_refused_from_stdio_servers() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        h.list_missing().await;
        let slot = h.slot(0);
        let m = slot.as_mcp().unwrap();
        let mut taken = std::collections::HashSet::new();
        let t =
            [mcp::cache::RemoteTool { name: "tokener".into(), description: String::new(), input_schema: json!({}) }];
        m.set_tools(&t, &mut taken, crate::tools::is_builtin);
        let r = h.call_tool("mcp__srv_tokener", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "done", "{}", r.text);
        let log = fx.log();
        assert!(log.contains(r#""id":97,"error":{"code":-32601"#), "method not found for the server: {log}");
        assert!(h.needs_login().is_empty(), "no token was fetched, so no login state changed");
        h.shutdown_all().await;
    });
}

#[test]
fn cancelling_a_listing_stops_the_server_and_leaves_it_unlisted() {
    run(async {
        let fx = McpFx::new();
        // Never answers `initialize`; `timeout: 0` must not make that wait forever.
        std::fs::write(
            fx.dir.join("agent/mcp.json"),
            r#"{"mcpServers":{"hang":{"command":"sh","args":["-c","sleep 1000"],"timeout":0}}}"#,
        )
        .unwrap();
        let (servers, _) = mcp::config::load_with(&fx.dir, &fx.dir.join("agent"), &|_| None);
        assert_eq!(mcp::connect_timeout(&servers[0]), Duration::from_secs(60), "bounded although timeout is 0");
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let init = InitParams { cwd: fx.dir.to_string_lossy().into(), ..Default::default() };
        let h = PluginHost::new_with_mcp(
            vec![],
            servers,
            manifest::Lifetime::Session,
            fx.cache.clone(),
            vec![],
            ntx,
            init,
            crate::tools::is_builtin,
        );
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.cancel();
        });
        let t = Instant::now();
        h.list_missing_until(&cancel).await;
        assert!(t.elapsed() < Duration::from_secs(10), "returned promptly: {:?}", t.elapsed());
        let slot = h.slot(0);
        let m = slot.as_mcp().unwrap();
        assert!(slot.proc.borrow().is_none(), "the server was stopped");
        assert!(!m.listed.get() && !m.failed.get(), "left unlisted so the next turn tries again");
    });
}

#[test]
fn login_hint_differs_for_interactive_and_headless() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        assert_eq!(h.login_msg("x"), "mcp:x needs login (run /mcp login x in mu)");
        h.set_interactive();
        assert_eq!(h.login_msg("x"), "mcp:x needs login: /mcp login x");
    });
}

#[test]
fn after_login_lists_a_server_whose_listing_failed() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        let slot = h.slot(0);
        let m = slot.as_mcp().unwrap();
        m.failed.set(true);
        m.needs_login.set(true);
        h.list_missing().await;
        assert!(h.tool_defs().is_empty(), "a failed server is left out of the session");
        assert_eq!(h.after_login("srv").await, Ok((2, true)));
        assert_eq!(h.tool_defs().len(), 2);
        assert!(h.needs_login().is_empty());
        assert!(h.after_login("nope").await.is_err());
        h.shutdown_all().await;
    });
}

#[test]
fn mcp_string_ids_round_trip() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host(r#","requestIdFormat":"string""#, "");
        h.list_missing().await;
        assert_eq!(h.tool_defs().len(), 2, "{}", fx.log());
        let r = h.call_tool("mcp__srv_echo", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "hi");
        assert!(fx.log().contains(r#""id":"1""#), "{}", fx.log());
        h.shutdown_all().await;
    });
}

#[test]
fn mcp_cancel_sends_notifications_cancelled_and_shutdown_leaves_no_process_group() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        h.list_missing().await;
        let slot = h.slot(0);
        let mut taken = std::collections::HashSet::new();
        let hang =
            [mcp::cache::RemoteTool { name: "hang".into(), description: String::new(), input_schema: json!({}) }];
        slot.as_mcp().unwrap().set_tools(&hang, &mut taken, crate::tools::is_builtin);
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let log = fx.dir.join("log");
        tokio::task::spawn_local(async move {
            mu_testutil::wait_until("the call to reach the server", || {
                std::fs::read_to_string(&log).is_ok_and(|l| l.contains(r#""name":"hang""#))
            })
            .await;
            c.cancel();
        });
        let r = h.call_tool("mcp__srv_hang", &json!({}), &cancel).await;
        assert!(r.is_error && r.text.contains("aborted"), "{}", r.text);
        mu_testutil::wait_until("the cancel notification", || {
            fx.log().contains(r#""method":"notifications/cancelled""#)
        })
        .await;
        let pid = slot.proc.borrow().clone().unwrap().pid as i32;
        h.shutdown_all().await;
        mu_testutil::wait_until("the process group to end", || !group_running(pid)).await;
        assert!(!group_running(pid), "the process group is gone");
    });
}

#[test]
fn mcp_sync_adds_changes_and_removes_servers_live() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        h.list_missing().await;
        assert_eq!(h.tool_defs().len(), 2);
        let log = fx.dir.join("log").display().to_string();
        let load = |servers: &[(&str, &str)]| {
            let body: Vec<String> = servers
                .iter()
                .map(|(n, a)| format!(r#""{n}":{{"command":"sh","args":["server.sh","{a}"],"env":{{"LOG":"{log}"}}}}"#))
                .collect();
            std::fs::write(fx.dir.join("agent/mcp.json"), format!(r#"{{"mcpServers":{{{}}}}}"#, body.join(",")))
                .unwrap();
            mcp::config::load_with(&fx.dir, &fx.dir.join("agent"), &|_| None).0
        };
        let inits = || fx.log().matches(r#""method":"initialize""#).count();

        assert!(h.mcp_sync(load(&[("srv", "")])).await.is_empty(), "same config changes nothing");

        let r = h.mcp_sync(load(&[("srv", ""), ("two", "b")])).await;
        assert_eq!(r, McpSync { added: vec!["two".into()], ..Default::default() });
        assert_eq!(h.tool_defs().len(), 2, "the new server is advertised once it is listed");
        h.list_missing().await;
        assert_eq!(h.tool_defs().len(), 4);
        assert_eq!(h.mcp_info("two").unwrap().tools, ["mcp__two_echo", "mcp__two_other"]);

        let r = h.mcp_sync(load(&[("srv", "--x"), ("two", "b")])).await;
        assert_eq!(r, McpSync { changed: vec!["srv".into()], ..Default::default() });
        assert!(h.slot(0).proc.borrow().is_none(), "stopped, to restart on the new config");
        assert_eq!(h.tool_defs().len(), 2, "the changed server is unlisted until listed again");
        h.list_missing().await;
        assert_eq!(h.tool_defs().len(), 4);

        let r = h.mcp_sync(load(&[("two", "b")])).await;
        assert_eq!(r, McpSync { removed: vec!["srv".into()], ..Default::default() });
        assert_eq!(h.mcp_names(), ["two"]);
        assert!(h.mcp_info("srv").is_none() && !h.has_tool("mcp__srv_echo"));

        let before = inits();
        assert_eq!(h.mcp_reconnect("two").await, Ok((2, false)));
        assert_eq!(inits(), before + 1, "reconnect starts the server again");
        assert!(h.mcp_reconnect("nope").await.is_err());
        h.shutdown_all().await;
    });
}

#[test]
fn a_sibling_binary_is_found_when_the_running_exe_was_replaced() {
    let dir = std::env::temp_dir().join(format!("mu-sib-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mu-mcp-http"), "").unwrap();
    let exe = dir.join("mu (deleted)");
    assert!(std::fs::canonicalize(&exe).is_err());
    assert_eq!(sibling_of(&exe, "mu-mcp-http"), Some(dir.join("mu-mcp-http")));
    assert_eq!(sibling_of(&exe, "nope"), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn shutdown_terminates_a_stuck_plugin_group_before_killing_it() {
    run(async {
        let dir = std::env::temp_dir().join(format!("mu-plug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("term");
        let script = format!(
            r#"trap 'echo term > "{}"' TERM
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
while :; do sleep 100 & wait; done
"#,
            marker.display()
        );
        std::fs::write(dir.join("p.sh"), script).unwrap();
        let m = Manifest {
            name: "stuck".into(),
            command: vec!["sh".into(), "p.sh".into()],
            lifetime: manifest::Lifetime::parse("session").unwrap(),
            hooks: vec![],
            required_hooks: vec![],
            provider: None,
            tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
            timeout: std::time::Duration::from_secs(300),
            dir: dir.clone(),
        };
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let h = PluginHost::new(vec![m], vec![], ntx, Default::default());
        let proc = h.ensure(&h.slot(0)).await.unwrap();
        let pid = proc.pid as i32;
        proc.shutdown().await;
        assert!(marker.exists(), "the group got SIGTERM before SIGKILL");
        mu_testutil::wait_until("the process group to end", || !group_running(pid)).await;
        assert!(!group_running(pid), "the process group is gone");
        let _ = std::fs::remove_dir_all(dir);
    });
}

#[test]
fn a_plugin_can_use_core_while_it_shuts_down() {
    run(async {
        let dir = std::env::temp_dir().join(format!("mu-plug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
    *'"shutdown"'*)
      printf '{"jsonrpc":"2.0","id":50,"method":"core/secrets","params":{"op":"put","name":"bye","value":"aGk="}}\n'
      read -r reply
      printf '%s\n' "$reply" > reply.log
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      exit 0 ;;
  esac
done
"#;
        std::fs::write(dir.join("p.sh"), script).unwrap();
        let m = Manifest {
            name: "sealer".into(),
            command: vec!["sh".into(), "p.sh".into()],
            lifetime: manifest::Lifetime::parse("session").unwrap(),
            hooks: vec![],
            required_hooks: vec![],
            provider: None,
            tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
            timeout: std::time::Duration::from_secs(300),
            dir: dir.clone(),
        };
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let h = PluginHost::new(vec![m], vec![], ntx, Default::default());
        h.ensure(&h.slot(0)).await.unwrap();
        h.shutdown_all().await;
        let reply = std::fs::read_to_string(dir.join("reply.log")).unwrap();
        assert!(reply.contains(r#""result":{}"#), "core answered during shutdown: {reply}");
        let stored = mu_common::paths::profile_home().join("secrets/plugin-sealer/bye");
        assert_eq!(std::fs::read(stored).unwrap(), b"hi");
        let _ = std::fs::remove_dir_all(dir);
    });
}

/// A native plugin subscribed to `tool_call`; `on_hook` is shell run on each `hook/tool_call`
/// (`$id` is the request id, `$line` the request).
fn hook_plugin(dir: &std::path::Path, name: &str, required: bool, on_hook: &str) -> Manifest {
    let script = format!(
        r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
    *'"shutdown"'*) exit 0 ;;
    *'"hook/tool_call"'*) {on_hook} ;;
  esac
done
"#
    );
    std::fs::write(dir.join(format!("{name}.sh")), script).unwrap();
    Manifest {
        name: name.into(),
        command: vec!["sh".into(), format!("{name}.sh")],
        lifetime: manifest::Lifetime::Session,
        hooks: vec!["tool_call".into()],
        required_hooks: if required { vec!["tool_call".into()] } else { vec![] },
        provider: None,
        tools: vec![],
        timeout: std::time::Duration::from_secs(300),
        dir: dir.into(),
    }
}

fn hook_host(plugins: Vec<Manifest>) -> (Rc<PluginHost>, mpsc::UnboundedReceiver<String>) {
    let (ntx, nrx) = mpsc::unbounded_channel();
    (PluginHost::new(plugins, vec![], ntx, Default::default()), nrx)
}

fn hook_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mu-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn tool_call_hooks_run_in_order_each_on_the_previous_arguments() {
    run(async {
        let dir = hook_dir();
        let add = |c: &str| {
            format!(
                r#"v=$(printf '%s' "$line" | sed -n 's/.*"arguments":{{"v":"\([a-z]*\)".*/\1/p'); printf '{{"jsonrpc":"2.0","id":%s,"result":{{"arguments":{{"v":"%s{c}"}}}}}}\n' "$id" "$v""#
            )
        };
        let (h, _n) = hook_host(vec![
            hook_plugin(&dir, "a", false, &add("a")),
            hook_plugin(&dir, "b", false, &add("b")),
            hook_plugin(&dir, "c", false, &add("c")),
        ]);
        assert_eq!(h.tool_call_hook("t", json!({"v": "x"})).await, Ok(json!({"v": "xabc"})));
        h.shutdown_all().await;
    });
}

#[test]
fn a_failing_optional_hook_lets_the_call_through_and_a_required_one_blocks_it() {
    run(async {
        let dir = hook_dir();
        let fail = r#"printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-1,"message":"boom"}}\n' "$id""#;
        let (h, mut n) = hook_host(vec![hook_plugin(&dir, "opt", false, fail)]);
        assert_eq!(h.tool_call_hook("t", json!({"v": 1})).await, Ok(json!({"v": 1})), "fail-open by default");
        assert!(n.try_recv().unwrap().contains("hook tool_call failed"));
        h.shutdown_all().await;

        let (h, _n) = hook_host(vec![hook_plugin(&dir, "req", true, fail)]);
        let e = h.tool_call_hook("t", json!({"v": 1})).await.unwrap_err();
        assert!(e.contains("Blocked by plugin req") && e.contains("required") && e.contains("boom"), "{e}");
        h.shutdown_all().await;
    });
}

#[test]
fn a_hook_block_stops_the_later_hooks() {
    run(async {
        let dir = hook_dir();
        let block = r#"printf '{"jsonrpc":"2.0","id":%s,"result":{"block":"no"}}\n' "$id""#;
        let mark = dir.join("ran");
        let after = format!(r#"touch {}; printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id""#, mark.display());
        let (h, _n) = hook_host(vec![hook_plugin(&dir, "a", false, block), hook_plugin(&dir, "b", false, &after)]);
        assert_eq!(h.tool_call_hook("t", json!({})).await, Err("Blocked by plugin a: no".into()));
        assert!(!mark.exists(), "the second hook never ran");
        h.shutdown_all().await;
    });
}

#[test]
fn reconnect_refuses_a_server_whose_project_file_is_not_trusted() {
    run(async {
        let fx = McpFx::new();
        let (h, _n) = fx.host("", "");
        h.list_missing().await;
        std::fs::create_dir_all(fx.dir.join(".omp")).unwrap();
        std::fs::write(
            fx.dir.join(".omp/mcp.json"),
            r#"{"mcpServers":{"proj":{"command":"sh","args":["server.sh"]}}}"#,
        )
        .unwrap();
        let (servers, _) = mcp::config::load_all(&fx.dir, &fx.dir.join("agent"));
        let proj = servers.into_iter().find(|e| e.server.name == "proj").expect("the project server is configured");
        assert!(!proj.trusted);
        h.mcp_sync(vec![proj.server]).await;
        let e = h.mcp_reconnect("proj").await.unwrap_err();
        assert!(e.contains("not trusted") && e.contains("/mcp trust"), "{e}");
        h.shutdown_all().await;
    });
}
