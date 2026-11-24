// A scripted Anthropic Messages upstream for exercising pi's real anthropic adapter without
// credentials. A system prompt starting `FAIL:<status>|<type>|<message>` answers that error;
// `HANG` streams one text delta and then holds the connection open. A `SLOW ` prefix on the system prompt
// holds the answer back for 800 ms first.
import http from "node:http";

const sse = (res, type, data) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);

function systemText(body) {
  const s = body.system;
  return typeof s === "string" ? s : (s ?? []).map((b) => b.text).join("\n");
}

function lastHasToolResult(body) {
  const last = body.messages?.at(-1);
  return Array.isArray(last?.content) && last.content.some((b) => b.type === "tool_result");
}

export async function startFakeUpstream() {
  const requests = [];
  const closed = [];
  const server = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", async () => {
      const body = JSON.parse(raw);
      requests.push({ url: req.url, headers: req.headers, body });
      let sys = systemText(body);
      if (sys.startsWith("SLOW ")) {
        sys = sys.slice(5);
        await new Promise((ok) => setTimeout(ok, 800));
      }
      const fail = /^FAIL:(\d+)\|([^|]*)\|(.*)$/s.exec(sys);
      if (fail) {
        res.writeHead(Number(fail[1]), { "content-type": "application/json" });
        res.end(JSON.stringify({ type: "error", error: { type: fail[2], message: fail[3] } }));
        return;
      }
      res.writeHead(200, { "content-type": "text/event-stream" });
      sse(res, "message_start", {
        message: {
          id: "msg_up",
          type: "message",
          role: "assistant",
          model: body.model,
          content: [],
          usage: { input_tokens: 10, output_tokens: 1, cache_read_input_tokens: 3, cache_creation_input_tokens: 2 },
        },
      });
      if (sys === "HANG") {
        res.on("close", () => closed.push(requests.length));
        sse(res, "content_block_start", { index: 0, content_block: { type: "text", text: "" } });
        sse(res, "content_block_delta", { index: 0, delta: { type: "text_delta", text: "partial" } });
        return;
      }
      let i = 0;
      if (lastHasToolResult(body)) {
        sse(res, "content_block_start", { index: i, content_block: { type: "text", text: "" } });
        sse(res, "content_block_delta", { index: i, delta: { type: "text_delta", text: "done" } });
        sse(res, "content_block_stop", { index: i });
        sse(res, "message_delta", { delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } });
      } else {
        if (body.thinking?.type === "enabled" || body.thinking?.type === "adaptive") {
          sse(res, "content_block_start", { index: i, content_block: { type: "thinking", thinking: "" } });
          sse(res, "content_block_delta", { index: i, delta: { type: "thinking_delta", thinking: "pond" } });
          sse(res, "content_block_delta", { index: i, delta: { type: "thinking_delta", thinking: "ering" } });
          sse(res, "content_block_delta", { index: i, delta: { type: "signature_delta", signature: "sig-abc" } });
          sse(res, "content_block_stop", { index: i++ });
          sse(res, "content_block_start", { index: i, content_block: { type: "redacted_thinking", data: "enc-xyz" } });
          sse(res, "content_block_stop", { index: i++ });
        }
        sse(res, "content_block_start", { index: i, content_block: { type: "text", text: "" } });
        sse(res, "content_block_delta", { index: i, delta: { type: "text_delta", text: "Let me " } });
        sse(res, "content_block_delta", { index: i, delta: { type: "text_delta", text: "check." } });
        sse(res, "content_block_stop", { index: i++ });
        sse(res, "content_block_start", {
          index: i,
          content_block: { type: "tool_use", id: "toolu_1", name: "read", input: {} },
        });
        sse(res, "content_block_delta", { index: i, delta: { type: "input_json_delta", partial_json: '{"pa' } });
        sse(res, "content_block_delta", { index: i, delta: { type: "input_json_delta", partial_json: 'th":"a.txt"}' } });
        sse(res, "content_block_stop", { index: i++ });
        sse(res, "message_delta", { delta: { stop_reason: "tool_use" }, usage: { output_tokens: 42 } });
      }
      sse(res, "message_stop", {});
      res.end();
    });
  });
  await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    requests,
    closed,
    close: () => new Promise((ok) => (server.closeAllConnections(), server.close(ok))),
  };
}
