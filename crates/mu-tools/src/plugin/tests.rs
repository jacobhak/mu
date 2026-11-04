use super::host::{InitParams, McpSync, PluginHost};
use super::manifest::{self, Manifest};
use super::process::sibling_of;
use crate::mcp;
use mu_testutil::block_on_local as run;
use serde_json::json;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
        provider: None,
        tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
        dir: dir.clone(),
    };
    let (ntx, _nrx) = mpsc::unbounded_channel();
    (PluginHost::new(vec![m], vec![], ntx, Default::default()), dir)
}

#[test]
fn a_cancelled_call_releases_its_bookkeeping() {
    run(async {
        let (h, dir) = host("sleep 100", "session");
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
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
        tokio::time::sleep(Duration::from_millis(300)).await;
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
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
        assert_eq!(r.text, "ok", "{}", r.text);
        let second = h.slot(0).proc.borrow().clone().unwrap().pid;
        assert_ne!(first, second);
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
      printf '{"jsonrpc":"2.0","id":97,"method":"core/mcpToken","params":{"force":false}}\n'
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
        assert_eq!(h.login_msg("x"), "mcp:x needs login (run /login mcp:x in mu)");
        h.set_interactive();
        assert_eq!(h.login_msg("x"), "mcp:x needs login: /login mcp:x");
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
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.cancel();
        });
        let r = h.call_tool("mcp__srv_hang", &json!({}), &cancel).await;
        assert!(r.is_error && r.text.contains("aborted"), "{}", r.text);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(fx.log().contains(r#""method":"notifications/cancelled""#), "{}", fx.log());
        let pid = slot.proc.borrow().clone().unwrap().pid as i32;
        h.shutdown_all().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        // SAFETY: signal 0 only checks that the group exists.
        assert_eq!(unsafe { libc::kill(-pid, 0) }, -1, "the process group is gone");
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
            provider: None,
            tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
            dir: dir.clone(),
        };
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let h = PluginHost::new(vec![m], vec![], ntx, Default::default());
        let proc = h.ensure(&h.slot(0)).await.unwrap();
        let pid = proc.pid as i32;
        proc.shutdown().await;
        assert!(marker.exists(), "the group got SIGTERM before SIGKILL");
        tokio::time::sleep(Duration::from_millis(200)).await;
        // SAFETY: signal 0 only checks that the group exists.
        assert_eq!(unsafe { libc::kill(-pid, 0) }, -1, "the process group is gone");
        let _ = std::fs::remove_dir_all(dir);
    });
}
