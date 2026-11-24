// Runs the real pi binary named by MU_GATEWAY_PI with the proxy extension, under a scratch HOME,
// against an in-process fake Anthropic upstream. Skipped when MU_GATEWAY_PI is unset.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { after, before, describe, it } from "node:test";
import { fileURLToPath } from "node:url";
import { startFakeUpstream } from "./fake-upstream.mjs";

const PI = process.env.MU_GATEWAY_PI;
const dir = path.dirname(fileURLToPath(import.meta.url));
const proxy = path.join(dir, "..", "proxy.mjs");
const fakeProvider = path.join(dir, "fake-provider.mjs");
const TOKEN = "t0ken-for-tests";

// The flags the Rust daemon passes, minus the extensions it does not add itself.
const FLAGS = ["--mode", "rpc", "--no-session", "-ne", "-e", proxy, "--no-mcp", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files", "-na"];

function spawnPi(home, env, extra = []) {
  // Minimal environment: no real credential can leak in from the caller's.
  return spawn(PI, [...FLAGS, ...extra], {
    cwd: home,
    env: { PATH: process.env.PATH, HOME: home, ...env },
    stdio: ["pipe", "pipe", "pipe"],
  });
}

function sseEvents(text) {
  return text
    .split("\n\n")
    .filter(Boolean)
    .map((b) => {
      const ev = /^event: (.*)$/m.exec(b)[1];
      const data = JSON.parse(/^data: (.*)$/m.exec(b)[1]);
      assert.equal(data.type, ev);
      return data;
    });
}

describe("gateway-pi against real pi", { skip: !PI && "MU_GATEWAY_PI not set" }, () => {
  let home, up, child, base, stderr = "", stdout = "";

  const call = (p, { method = "GET", body, token = TOKEN, headers = {}, signal } = {}) =>
    fetch(base + p, {
      method,
      signal,
      headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), "content-type": "application/json", ...headers },
      body: body && JSON.stringify(body),
    });

  const msg = (over = {}) => ({
    model: "anthropic/claude-sonnet-4-5",
    max_tokens: 8000,
    stream: true,
    system: [{ type: "text", text: "be brief" }],
    messages: [{ role: "user", content: [{ type: "text", text: "read a.txt" }] }],
    tools: [{ name: "read", description: "read a file", input_schema: { type: "object", properties: { path: { type: "string" } } } }],
    thinking: { type: "enabled", budget_tokens: 4000 },
    output_config: { effort: "high" },
    ...over,
  });

  before(async () => {
    home = fs.mkdtempSync(path.join(os.tmpdir(), "gwpi-"));
    up = await startFakeUpstream();
    child = spawnPi(home, { MU_GW_PI_MODE: "serve", MU_GW_PI_TOKEN: TOKEN, MU_FAKE_UPSTREAM: up.url }, ["-e", fakeProvider]);
    child.stdout.on("data", (d) => (stdout += d));
    base = await new Promise((ok, no) => {
      const t = setTimeout(() => no(new Error(`pi did not start: ${stderr}`)), 60_000);
      child.stderr.on("data", (d) => {
        stderr += d;
        const m = /mu-gateway-pi proxy listening on (http:\/\/127\.0\.0\.1:\d+)\n/.exec(stderr);
        if (m) (clearTimeout(t), ok(m[1]));
      });
      child.on("exit", (c) => no(new Error(`pi exited early (${c}): ${stderr}`)));
    });
  });

  after(async () => {
    child?.stdin.end();
    child?.kill();
    await up?.close();
    if (home) fs.rmSync(home, { recursive: true, force: true });
  });

  it("answers healthz without a token and keeps stdout for pi", async () => {
    const r = await call("/healthz", { token: null });
    assert.equal(r.status, 200);
    assert.equal(stdout, "");
  });

  it("requires the token on everything else", async () => {
    for (const [p, method] of [["/v1/models"], ["/v1/usage"], ["/v1/messages", "POST"]]) {
      const none = await call(p, { method, token: null, body: method && {} });
      assert.equal(none.status, 401, p);
      assert.equal((await none.json()).error.type, "authentication_error");
      assert.equal((await call(p, { method, token: "nope", body: method && {} })).status, 401);
    }
    const r = await call("/v1/models", { token: null, headers: { "x-api-key": TOKEN } });
    assert.equal(r.status, 200);
  });

  it("lists the usable models and reports no quota windows", async () => {
    const models = (await (await call("/v1/models")).json()).data;
    const m = models.find((x) => x.id === "anthropic/claude-sonnet-4-5");
    assert.ok(m && m.context_length > 0 && m.max_output_tokens > 0, JSON.stringify(m));
    assert.ok(!models.some((x) => x.id.startsWith("openai/")), "no credentials, so not available");
    const u = await (await call("/v1/usage")).json();
    assert.deepEqual(u.reports, []);
    assert.equal(typeof u.generatedAt, "number");
  });

  it("round-trips a thinking + tool-call turn and replays the signature", async () => {
    up.requests.length = 0;
    const r = await call("/v1/messages", { method: "POST", body: msg() });
    assert.equal(r.status, 200);
    assert.match(r.headers.get("content-type"), /text\/event-stream/);
    const ev = sseEvents(await r.text());
    assert.deepEqual(
      ev.map((e) => e.type),
      [
        "message_start",
        "content_block_start", "content_block_delta", "content_block_delta", "content_block_delta", "content_block_stop", // thinking + signature
        "content_block_start", "content_block_stop", // redacted
        "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", // text
        "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", // tool_use
        "message_delta",
        "message_stop",
      ],
    );
    assert.deepEqual(ev.map((e) => e.index).filter((i) => i !== undefined), [0, 0, 0, 0, 0, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
    assert.equal(ev[1].content_block.type, "thinking");
    assert.equal(ev[2].delta.thinking + ev[3].delta.thinking, "pondering");
    assert.deepEqual(ev[4].delta, { type: "signature_delta", signature: "sig-abc" });
    assert.deepEqual(ev[6].content_block, { type: "redacted_thinking", data: "enc-xyz" });
    assert.equal(ev[9].delta.text + ev[10].delta.text, "Let me check.");
    assert.deepEqual(ev[12].content_block, { type: "tool_use", id: "toolu_1", name: "read", input: {} });
    assert.deepEqual(JSON.parse(ev[13].delta.partial_json + ev[14].delta.partial_json), { path: "a.txt" });
    const d = ev.at(-2);
    assert.equal(d.delta.stop_reason, "tool_use");
    assert.deepEqual(d.usage, { input_tokens: 10, output_tokens: 42, cache_read_input_tokens: 3, cache_creation_input_tokens: 2 });

    const sent = up.requests.at(-1).body;
    assert.ok(sent.max_tokens <= 8000, `upstream max_tokens ${sent.max_tokens}`);
    assert.equal(sent.thinking.type, "enabled");
    assert.equal(sent.thinking.budget_tokens, 4000);
    assert.equal(sent.tools[0].name, "read");

    // second turn: replay what mu would store
    const turn2 = msg({
      messages: [
        { role: "user", content: [{ type: "text", text: "read a.txt" }] },
        {
          role: "assistant",
          content: [
            { type: "thinking", thinking: "pondering", signature: "sig-abc" },
            { type: "redacted_thinking", data: "enc-xyz" },
            { type: "text", text: "Let me check." },
            { type: "tool_use", id: "toolu_1", name: "read", input: { path: "a.txt" } },
          ],
        },
        { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_1", content: [{ type: "text", text: "hello" }], is_error: false }] },
      ],
    });
    const r2 = await call("/v1/messages", { method: "POST", body: turn2 });
    const ev2 = sseEvents(await r2.text());
    assert.equal(ev2.at(-2).delta.stop_reason, "end_turn");
    const sent2 = up.requests.at(-1).body;
    assert.ok(sent2.max_tokens <= 8000);
    const asst = sent2.messages.find((m) => m.role === "assistant");
    assert.deepEqual(asst.content[0], { type: "thinking", thinking: "pondering", signature: "sig-abc" });
    assert.deepEqual(asst.content[1], { type: "redacted_thinking", data: "enc-xyz" });
    assert.equal(asst.content.at(-1).type, "tool_use");
    const tr = sent2.messages.at(-1).content.find((b) => b.type === "tool_result");
    assert.equal(tr.tool_use_id, "toolu_1");
  });

  it("never lets the upstream total exceed max_tokens, on budget and adaptive models", async () => {
    for (const [model, effort] of [["claude-sonnet-4-5", "xhigh"], ["claude-sonnet-4-5", "low"], ["claude-opus-4-6", "high"], ["claude-opus-4-7", "max"]]) {
      up.requests.length = 0;
      const r = await call("/v1/messages", {
        method: "POST",
        body: msg({ model: `anthropic/${model}`, max_tokens: 6000, thinking: { type: "enabled", budget_tokens: 3000 }, output_config: { effort } }),
      });
      await r.text();
      const sent = up.requests.at(-1).body;
      assert.ok(sent.max_tokens <= 6000, `${model}/${effort}: ${sent.max_tokens}`);
      if (sent.thinking.type === "enabled") assert.equal(sent.thinking.budget_tokens, 3000);
      else assert.equal(sent.output_config.effort, effort === "xhigh" ? "high" : effort);
    }
  });

  it("sends no thinking when mu disables it", async () => {
    up.requests.length = 0;
    const r = await call("/v1/messages", { method: "POST", body: msg({ thinking: { type: "disabled" }, output_config: undefined }) });
    const ev = sseEvents(await r.text());
    assert.ok(!ev.some((e) => e.content_block?.type === "thinking"));
    assert.equal(up.requests.at(-1).body.thinking.type, "disabled");
  });

  it("rejects unknown models and bad requests with 400", async () => {
    for (const body of [msg({ model: "anthropic/nope" }), msg({ model: "nope/x" }), msg({ model: "noslash" }), { model: "anthropic/claude-sonnet-4-5" }]) {
      const r = await call("/v1/messages", { method: "POST", body });
      assert.equal(r.status, 400, JSON.stringify(body.model));
      assert.equal((await r.json()).error.type, "invalid_request_error");
    }
    const r = await call("/v1/messages", { method: "POST", body: "{not json" , headers: {} });
    assert.equal(r.status, 400);
  });

  it("answers 401 for a provider without credentials", async () => {
    const all = JSON.parse(await pricesViaFile());
    const m = all.models.find((x) => x.provider === "openai");
    const r = await call("/v1/messages", { method: "POST", body: msg({ model: `openai/${m.id}`, thinking: { type: "disabled" } }) });
    assert.equal(r.status, 401);
    assert.equal((await r.json()).error.type, "authentication_error");
  });

  it("maps upstream failures to HTTP statuses before the stream starts", async () => {
    const cases = [
      ["429|rate_limit_error|slow down", 429, "rate_limit_error", "slow down"],
      ["529|overloaded_error|busy", 529, "overloaded_error", "busy"],
      ["500|api_error|boom", 502, "api_error", "boom"],
      ["401|authentication_error|bad key", 401, "authentication_error", "bad key"],
      ["400|invalid_request_error|effort is not supported for this model", 400, "invalid_request_error", "not supported"],
    ];
    for (const [fail, status, type, text] of cases) {
      const r = await call("/v1/messages", { method: "POST", body: msg({ system: [{ type: "text", text: `FAIL:${fail}` }], thinking: { type: "disabled" } }) });
      assert.equal(r.status, status, fail);
      const j = await r.json();
      assert.equal(j.type, "error");
      assert.equal(j.error.type, type);
      assert.ok(j.error.message.includes(text), j.error.message);
    }
  });

  it("aborts the upstream request when the client disconnects, and keeps serving", async () => {
    up.closed.length = 0;
    const ac = new AbortController();
    const r = await call("/v1/messages", { method: "POST", signal: ac.signal, body: msg({ system: [{ type: "text", text: "HANG" }], thinking: { type: "disabled" } }) });
    const reader = r.body.getReader();
    const first = await reader.read();
    assert.match(new TextDecoder().decode(first.value), /message_start/);
    ac.abort();
    await reader.cancel().catch(() => {});
    for (let i = 0; i < 100 && up.closed.length === 0; i++) await new Promise((ok) => setTimeout(ok, 50));
    assert.equal(up.closed.length, 1, "upstream connection should have been closed");
    assert.equal((await call("/healthz", { token: null })).status, 200);
  });

  it("serves concurrent requests", async () => {
    const rs = await Promise.all(Array.from({ length: 6 }, () => call("/v1/messages", { method: "POST", body: msg() }).then((r) => r.text())));
    for (const t of rs) assert.equal(sseEvents(t).at(-1).type, "message_stop");
  });

  async function pricesViaFile() {
    const out = path.join(home, `prices-${Math.random().toString(36).slice(2)}.json`);
    const p = spawnPi(home, { MU_GW_PI_MODE: "prices", MU_GW_PI_OUT: out, MU_FAKE_UPSTREAM: up.url }, ["-e", fakeProvider]);
    let err = "";
    p.stderr.on("data", (d) => (err += d));
    p.stdout.on("data", () => {});
    const code = await new Promise((ok, no) => {
      const t = setTimeout(() => (p.kill(), no(new Error(`prices mode did not exit: ${err}`))), 60_000);
      p.on("exit", (c) => (clearTimeout(t), ok(c)));
    });
    assert.equal(code, 0, err);
    assert.ok(!/listening on/.test(err), "prices mode must not start a server");
    return fs.readFileSync(out, "utf8");
  }

  it("prices mode writes a valid file and exits", async () => {
    const j = JSON.parse(await pricesViaFile());
    assert.ok(j.models.length > 100);
    const s = j.models.find((m) => m.provider === "anthropic" && m.id === "claude-sonnet-4-5");
    assert.equal(typeof s.name, "string");
    assert.equal(s.reasoning, true);
    assert.ok(!("pricingStatus" in s));
    for (const k of ["input", "output", "cacheRead", "cacheWrite"]) assert.equal(typeof s.cost[k], "number", k);
    assert.ok(s.cost.input > 0 && s.cost.output > s.cost.input);
  });
});

describe("gateway-pi keep-alive against real pi", { skip: !PI && "MU_GATEWAY_PI not set" }, () => {
  let home, up, child, base;

  before(async () => {
    home = fs.mkdtempSync(path.join(os.tmpdir(), "gwpi-ka-"));
    up = await startFakeUpstream();
    child = spawnPi(home, { MU_GW_PI_MODE: "serve", MU_GW_PI_TOKEN: TOKEN, MU_FAKE_UPSTREAM: up.url, MU_GW_PI_KEEPALIVE_MS: "150" }, ["-e", fakeProvider]);
    let stderr = "";
    child.stdout.on("data", () => {});
    base = await new Promise((ok, no) => {
      const t = setTimeout(() => no(new Error(`pi did not start: ${stderr}`)), 60_000);
      child.stderr.on("data", (d) => {
        stderr += d;
        const m = /mu-gateway-pi proxy listening on (http:\/\/127\.0\.0\.1:\d+)\n/.exec(stderr);
        if (m) (clearTimeout(t), ok(m[1]));
      });
      child.on("exit", (c) => no(new Error(`pi exited early (${c}): ${stderr}`)));
    });
  });

  after(async () => {
    child?.stdin.end();
    child?.kill();
    await up?.close();
    if (home) fs.rmSync(home, { recursive: true, force: true });
  });

  const post = (system) =>
    fetch(`${base}/v1/messages`, {
      method: "POST",
      headers: { authorization: `Bearer ${TOKEN}`, "content-type": "application/json" },
      body: JSON.stringify({
        model: "anthropic/claude-sonnet-4-5",
        max_tokens: 4000,
        stream: true,
        system: [{ type: "text", text: system }],
        messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
        thinking: { type: "disabled" },
      }),
    });

  it("pings while the model is silent, then streams the answer", async () => {
    const r = await post("SLOW be brief");
    assert.equal(r.status, 200);
    const types = sseEvents(await r.text()).map((e) => e.type);
    assert.equal(types[0], "ping");
    assert.ok(types.indexOf("ping") < types.indexOf("message_start"), types.join());
    assert.equal(types.at(-1), "message_stop");
  });

  it("reports a failure after the first ping as an in-stream error frame", async () => {
    const r = await post("SLOW FAIL:529|overloaded_error|busy");
    assert.equal(r.status, 200, "the status was already sent with the first ping");
    const ev = sseEvents(await r.text());
    const e = ev.at(-1);
    assert.equal(e.type, "error");
    assert.equal(e.error.type, "overloaded_error");
  });
});
