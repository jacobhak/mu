//! Fake model server: the Anthropic Messages wire protocol mu speaks through the gateway, replaying a scenario.
//! Subagent and compaction requests are not scripted yet; any request without tools gets a short canned reply.
use crate::scenario::{self, Scenario, Step, gen_text};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

pub const MODEL: &str = "anthropic/bench-1";
pub const CONTEXT_WINDOW: u64 = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Main,
    Aux,
}

#[derive(Debug, Clone)]
pub struct ReqRecord {
    pub kind: Kind,
    pub turn: Option<usize>,
    pub step: Option<usize>,
    pub tool_result_bytes: usize,
    pub tool_errors: usize,
    /// When the first text delta of the reply was written.
    pub first_text: Option<Instant>,
    pub finished: Option<Instant>,
    /// Marker at the start of this reply's text, when it has any.
    pub marker: Option<String>,
}

#[derive(Default)]
pub struct Log {
    pub requests: Vec<ReqRecord>,
    pub turns_done: usize,
    /// Main requests beyond what the scenario scripts.
    pub overruns: usize,
    /// Routes the server does not serve, once each ("METHOD /path").
    pub unrouted: Vec<String>,
    cursor: (usize, usize),
}

pub struct Server {
    pub addr: SocketAddr,
    pub log: Mutex<Log>,
    pub changed: Notify,
}

pub struct Frame {
    pub text: String,
    /// Carries streamed content, so the pause after it models generation time.
    pub delta: bool,
    /// A delta of a text block, as opposed to thinking or tool arguments.
    pub is_text: bool,
}

fn frame(event: &str, data: Value, delta: bool) -> Frame {
    Frame { text: format!("event: {event}\ndata: {data}\n\n"), delta, is_text: false }
}

/// `s` in pieces of at most `n` bytes, split on character boundaries.
pub fn chunks(s: &str, n: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + n).min(s.len());
        while !s.is_char_boundary(end) {
            end += 1;
        }
        out.push(&s[start..end]);
        start = end;
    }
    out
}

/// The SSE events for one scripted reply. `id` seeds the generated text and tool call ids.
pub fn build_frames(
    id: &str,
    seed: u64,
    step: &Step,
    marker: Option<&str>,
    input_tokens: u64,
    chunk: usize,
) -> Vec<Frame> {
    let mut f = vec![frame(
        "message_start",
        json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","content":[],
               "model":MODEL,"usage":{"input_tokens":input_tokens,"output_tokens":0}}}),
        false,
    )];
    let mut idx = 0;
    let mut block = |f: &mut Vec<Frame>, start: Value, delta_type: &str, key: &str, parts: Vec<&str>, sig: bool| {
        f.push(frame(
            "content_block_start",
            json!({"type":"content_block_start","index":idx,"content_block":start}),
            false,
        ));
        for p in parts {
            let mut fr = frame(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,"delta":{"type":delta_type,key:p}}),
                true,
            );
            fr.is_text = delta_type == "text_delta";
            f.push(fr);
        }
        if sig {
            f.push(frame(
                "content_block_delta",
                json!({"type":"content_block_delta","index":idx,"delta":{"type":"signature_delta","signature":"c2ln"}}),
                false,
            ));
        }
        f.push(frame("content_block_stop", json!({"type":"content_block_stop","index":idx}), false));
        idx += 1;
    };
    if step.thinking_bytes > 0 {
        let t = gen_text(seed ^ 0x7417, step.thinking_bytes);
        block(&mut f, json!({"type":"thinking","thinking":""}), "thinking_delta", "thinking", chunks(&t, chunk), true);
    }
    if step.text_bytes > 0 {
        let t = format!(
            "{}{}",
            marker.unwrap_or(""),
            gen_text(seed, step.text_bytes.saturating_sub(marker.map_or(0, str::len)))
        );
        block(&mut f, json!({"type":"text","text":""}), "text_delta", "text", chunks(&t, chunk), false);
    }
    for (i, t) in step.tools.iter().enumerate() {
        let args = t.args.to_string();
        block(
            &mut f,
            json!({"type":"tool_use","id":format!("{id}_{i}"),"name":t.name,"input":{}}),
            "input_json_delta",
            "partial_json",
            chunks(&args, chunk),
            false,
        );
    }
    let output = (step.thinking_bytes + step.text_bytes + step.tools.len() * 60) as u64 / 4;
    f.push(frame(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason": if step.tools.is_empty() {"end_turn"} else {"tool_use"}},
               "usage":{"input_tokens":input_tokens,"output_tokens":output}}),
        false,
    ));
    f.push(frame("message_stop", json!({"type":"message_stop"}), false));
    f
}

/// Bytes of tool output and the number of errors in the tool results of the last user message.
pub fn tool_results(body: &Value) -> (usize, usize) {
    let Some(blocks) = body["messages"].as_array().and_then(|m| m.last()).and_then(|m| m["content"].as_array()) else {
        return (0, 0);
    };
    let (mut bytes, mut errors) = (0, 0);
    for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
        errors += usize::from(b["is_error"] == true);
        bytes += match &b["content"] {
            Value::String(s) => s.len(),
            Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).map(str::len).sum(),
            _ => 0,
        };
    }
    (bytes, errors)
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_request(sock: &mut TcpStream) -> Result<Request> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        let mut tmp = [0u8; 8192];
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            bail!("connection closed before the request ended");
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let first = lines.next().context("empty request")?;
    let mut parts = first.split_whitespace();
    let (method, path) = (parts.next().context("no method")?.to_string(), parts.next().context("no path")?.to_string());
    let mut len = 0;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = v.trim().parse()?,
            "transfer-encoding" if v.trim().eq_ignore_ascii_case("chunked") => {
                bail!("chunked request bodies are not supported")
            }
            _ => {}
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let mut tmp = [0u8; 65536];
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            bail!("connection closed inside the request body");
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Request { method, path, body })
}

async fn reply(sock: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    Ok(sock.shutdown().await?)
}

impl Server {
    pub async fn start(scenario: Arc<Scenario>) -> Result<Arc<Server>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let srv =
            Arc::new(Server { addr: listener.local_addr()?, log: Mutex::new(Log::default()), changed: Notify::new() });
        let s = srv.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (s, sc) = (s.clone(), scenario.clone());
                tokio::spawn(async move {
                    if let Err(e) = s.handle(sock, &sc).await {
                        eprintln!("mu-bench server: {e}");
                    }
                });
            }
        });
        Ok(srv)
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The scripted step for the next main request, or `None` once the scenario is used up.
    fn next_step(&self, sc: &Scenario) -> Option<(usize, usize)> {
        let mut log = self.log.lock().unwrap();
        let (t, s) = log.cursor;
        if t >= sc.turns.len() {
            log.overruns += 1;
            return None;
        }
        log.cursor = if s + 1 < sc.turns[t].steps.len() { (t, s + 1) } else { (t + 1, 0) };
        Some((t, s))
    }

    async fn handle(&self, mut sock: TcpStream, sc: &Scenario) -> Result<()> {
        let req = read_request(&mut sock).await?;
        match (req.method.as_str(), req.path.split('?').next().unwrap_or("")) {
            ("GET", "/v1/models") => {
                let body = json!({"data":[{"id":MODEL,"context_length":CONTEXT_WINDOW,"max_output_tokens":32000}]});
                reply(&mut sock, "200 OK", &body.to_string()).await
            }
            ("POST", "/v1/messages") => self.messages(sock, &req.body, sc).await,
            (m, p) => {
                let route = format!("{m} {p}");
                {
                    let mut log = self.log.lock().unwrap();
                    if !log.unrouted.contains(&route) {
                        log.unrouted.push(route.clone());
                    }
                }
                reply(&mut sock, "404 Not Found", &json!({"error":{"message":route}}).to_string()).await
            }
        }
    }

    async fn messages(&self, mut sock: TcpStream, raw: &[u8], sc: &Scenario) -> Result<()> {
        let body: Value = serde_json::from_slice(raw).map_err(|e| anyhow!("bad request json: {e}"))?;
        let main = body["tools"].as_array().is_some_and(|t| !t.is_empty());
        let scripted = if main { self.next_step(sc) } else { None };
        let (results, errors) = tool_results(&body);
        let input_tokens = raw.len() as u64 / 4;
        let (step, marker, id, seed) = match scripted {
            Some((t, s)) => {
                let step = sc.turns[t].steps[s].clone();
                let marker = (step.text_bytes > 0).then(|| scenario::marker(t, s));
                (step, marker, format!("msg_{t}_{s}"), (t * 100 + s) as u64)
            }
            None => {
                let aux = Step { text_bytes: 64, ..Step::default() };
                (aux, None, "msg_aux".to_string(), 0)
            }
        };
        let rec = {
            let mut log = self.log.lock().unwrap();
            log.requests.push(ReqRecord {
                kind: if scripted.is_some() { Kind::Main } else { Kind::Aux },
                turn: scripted.map(|x| x.0),
                step: scripted.map(|x| x.1),
                tool_result_bytes: results,
                tool_errors: errors,
                first_text: None,
                finished: None,
                marker: marker.clone(),
            });
            log.requests.len() - 1
        };
        self.changed.notify_waiters();
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
        sock.write_all(head.as_bytes()).await?;
        let mut first = true;
        for f in build_frames(&id, seed, &step, marker.as_deref(), input_tokens, sc.chunk_bytes) {
            if f.is_text && first {
                first = false;
                self.log.lock().unwrap().requests[rec].first_text = Some(Instant::now());
            }
            sock.write_all(f.text.as_bytes()).await?;
            if f.delta && sc.chunk_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(sc.chunk_delay_ms)).await;
            }
        }
        sock.shutdown().await?;
        {
            let mut log = self.log.lock().unwrap();
            log.requests[rec].finished = Some(Instant::now());
            if let Some((t, s)) = scripted
                && s + 1 == sc.turns[t].steps.len()
            {
                log.turns_done = t + 1;
            }
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Waits until `n` turns have been fully served.
    pub async fn wait_turns(&self, n: usize, timeout: Duration) -> Result<()> {
        let end = Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.log.lock().unwrap().turns_done >= n {
                return Ok(());
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "timed out after {timeout:?} waiting for turn {n} to finish (served {})",
                    self.log.lock().unwrap().turns_done
                );
            }
            let _ = tokio::time::timeout(left, notified).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(think: usize, text: usize, tools: &[(&str, Value)]) -> Step {
        Step {
            thinking_bytes: think,
            text_bytes: text,
            tools: tools
                .iter()
                .map(|(n, a)| crate::scenario::ToolCall { name: (*n).into(), args: a.clone() })
                .collect(),
        }
    }

    /// The content mu's assembler would build from these frames: (thinking, text, tool calls).
    fn assemble(frames: &[Frame]) -> (String, String, Vec<(String, String)>, String) {
        let (mut think, mut text, mut tools, mut stop) =
            (String::new(), String::new(), Vec::<(String, String)>::new(), String::new());
        for f in frames {
            let data = f.text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            let v: Value = serde_json::from_str(data).unwrap();
            match v["type"].as_str().unwrap() {
                "content_block_start" if v["content_block"]["type"] == "tool_use" => {
                    tools.push((v["content_block"]["name"].as_str().unwrap().into(), String::new()))
                }
                "content_block_delta" => {
                    let d = &v["delta"];
                    match d["type"].as_str().unwrap() {
                        "thinking_delta" => think.push_str(d["thinking"].as_str().unwrap()),
                        "text_delta" => text.push_str(d["text"].as_str().unwrap()),
                        "input_json_delta" => tools.last_mut().unwrap().1.push_str(d["partial_json"].as_str().unwrap()),
                        _ => {}
                    }
                }
                "message_delta" => stop = v["delta"]["stop_reason"].as_str().unwrap().into(),
                _ => {}
            }
        }
        (think, text, tools, stop)
    }

    #[test]
    fn frames_carry_exact_sizes_and_tool_arguments() {
        let s = step(500, 300, &[("read", json!({"path":"é/a.txt"})), ("bash", json!({"command":"ls"}))]);
        let f = build_frames("m", 1, &s, Some("BMK00.00 "), 1000, 48);
        let (think, text, tools, stop) = assemble(&f);
        assert_eq!((think.len(), text.len()), (500, 300));
        assert!(text.starts_with("BMK00.00 "));
        assert_eq!(tools.len(), 2);
        assert_eq!(serde_json::from_str::<Value>(&tools[0].1).unwrap(), json!({"path":"é/a.txt"}));
        assert_eq!(stop, "tool_use");
        assert!(f.last().unwrap().text.contains("message_stop"));
        assert!(f.first().unwrap().text.starts_with("event: message_start"));
    }

    #[test]
    fn a_step_without_tools_ends_the_turn() {
        let f = build_frames("m", 1, &step(0, 100, &[]), None, 10, 48);
        assert_eq!(assemble(&f).3, "end_turn");
        assert_eq!(f.iter().filter(|f| f.delta).count(), 3);
    }

    #[test]
    fn chunks_respect_char_boundaries() {
        assert_eq!(chunks("aéb", 2), ["aé", "b"]);
        assert_eq!(chunks("", 4), Vec::<&str>::new());
        assert_eq!(chunks("abcdef", 4).concat(), "abcdef");
    }

    #[test]
    fn tool_result_sizes_and_errors() {
        let body = json!({"messages":[
            {"role":"user","content":"hi"},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"a","content":"12345"},
                {"type":"tool_result","tool_use_id":"b","content":[{"type":"text","text":"123"}],"is_error":true}]}]});
        assert_eq!(tool_results(&body), (8, 1));
        assert_eq!(tool_results(&json!({"messages":[{"role":"user","content":"x"}]})), (0, 0));
    }

    fn scenario() -> Arc<Scenario> {
        Arc::new(
            Scenario::parse(
                r#"
name = "t"
chunk_delay_ms = 0
[[turn]]
prompt = "p"
[[turn.step]]
[[turn.step.tool]]
name = "read"
args = { path = "a" }
[[turn.step]]
text_bytes = 120
"#,
            )
            .unwrap(),
        )
    }

    async fn post(srv: &Server, body: &Value) -> String {
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        let b = body.to_string();
        s.write_all(
            format!("POST /v1/messages HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{b}", b.len()).as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn replays_the_scenario_in_order_and_counts_turns() {
        let srv = Server::start(scenario()).await.unwrap();
        let main = json!({"tools":[{"name":"read"}],"messages":[{"role":"user","content":"p"}]});
        let a = post(&srv, &main).await;
        assert!(a.contains("\"stop_reason\":\"tool_use\"") && a.contains("text/event-stream"));
        assert_eq!(srv.log.lock().unwrap().turns_done, 0);
        let b = post(&srv, &main).await;
        assert!(b.contains("end_turn") && b.contains("BMK00.01"));
        srv.wait_turns(1, Duration::from_secs(1)).await.unwrap();
        let aux = post(&srv, &json!({"messages":[]})).await;
        assert!(aux.contains("end_turn"));
        let over = post(&srv, &main).await;
        assert!(over.contains("end_turn"));
        let log = srv.log.lock().unwrap();
        assert_eq!(log.overruns, 1);
        let kinds: Vec<_> = log.requests.iter().map(|r| (r.kind, r.turn, r.step)).collect();
        assert_eq!(
            kinds,
            [
                (Kind::Main, Some(0), Some(0)),
                (Kind::Main, Some(0), Some(1)),
                (Kind::Aux, None, None),
                (Kind::Aux, None, None)
            ]
        );
        assert!(log.requests[1].first_text.is_some() && log.requests[1].finished.is_some());
    }

    #[tokio::test]
    async fn serves_models_and_rejects_unknown_routes() {
        let srv = Server::start(scenario()).await.unwrap();
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"GET /v1/models HTTP/1.1\r\nhost: x\r\n\r\n").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("200 OK") && out.contains(MODEL) && out.contains("200000"));
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"GET /nope HTTP/1.1\r\nhost: x\r\n\r\n").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("404"));
    }

    #[tokio::test]
    async fn waiting_for_a_turn_times_out() {
        let srv = Server::start(scenario()).await.unwrap();
        assert!(srv.wait_turns(1, Duration::from_millis(50)).await.is_err());
    }
}
