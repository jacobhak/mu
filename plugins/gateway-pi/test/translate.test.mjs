import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  ApiError,
  SseTranslator,
  clampBudget,
  clampPayload,
  mapError,
  modelsBody,
  pricesBody,
  splitModel,
  toPi,
} from "../translate.mjs";

const req = (over = {}) => ({
  model: "baseten/moonshotai/Kimi-K3",
  max_tokens: 8000,
  stream: true,
  system: [{ type: "text", text: "sys one" }, { type: "text", text: "sys two" }],
  messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
  thinking: { type: "disabled" },
  ...over,
});

describe("splitModel", () => {
  it("splits at the first slash", () => {
    assert.deepEqual(splitModel("baseten/moonshotai/Kimi-K3"), { provider: "baseten", id: "moonshotai/Kimi-K3" });
    assert.deepEqual(splitModel("anthropic/claude-sonnet-4-5"), { provider: "anthropic", id: "claude-sonnet-4-5" });
  });
  it("rejects ids without both halves", () => {
    for (const bad of ["noslash", "/x", "x/", "", undefined, 3]) assert.equal(splitModel(bad), null, String(bad));
  });
});

describe("toPi request mapping", () => {
  it("joins system blocks and keeps the model split", () => {
    const { context, model, options } = toPi(req(), "anthropic-messages", 100);
    assert.equal(context.systemPrompt, "sys one\n\nsys two");
    assert.deepEqual(model, { provider: "baseten", id: "moonshotai/Kimi-K3" });
    assert.deepEqual(options, { maxTokens: 8000 });
    assert.deepEqual(context.messages, [{ role: "user", content: [{ type: "text", text: "hi" }], timestamp: 100 }]);
    assert.equal(context.tools, undefined);
  });

  it("accepts string system and string content", () => {
    const { context } = toPi(req({ system: "plain", messages: [{ role: "user", content: "yo" }] }), "x", 1);
    assert.equal(context.systemPrompt, "plain");
    assert.deepEqual(context.messages[0].content, [{ type: "text", text: "yo" }]);
  });

  it("maps images and tools", () => {
    const { context } = toPi(
      req({
        messages: [{ role: "user", content: [{ type: "image", source: { type: "base64", media_type: "image/png", data: "QUJD" } }, { type: "text", text: "what" }] }],
        tools: [{ name: "read", description: "d", input_schema: { type: "object", properties: { p: { type: "string" } } } }],
      }),
      "x",
    );
    assert.deepEqual(context.messages[0].content[0], { type: "image", data: "QUJD", mimeType: "image/png" });
    assert.deepEqual(context.tools, [{ name: "read", description: "d", parameters: { type: "object", properties: { p: { type: "string" } } } }]);
  });

  it("maps a full tool round with the tool name looked up from tool_use", () => {
    const { context } = toPi(
      req({
        messages: [
          { role: "user", content: [{ type: "text", text: "go" }] },
          {
            role: "assistant",
            content: [
              { type: "thinking", thinking: "hm", signature: "SIG" },
              { type: "redacted_thinking", data: "ENC" },
              { type: "text", text: "ok" },
              { type: "tool_use", id: "t1", name: "read", input: { path: "a" } },
              { type: "tool_use", id: "t2", name: "grep", input: {} },
            ],
          },
          {
            role: "user",
            content: [
              { type: "tool_result", tool_use_id: "t1", content: [{ type: "text", text: "A" }, { type: "image", source: { type: "base64", media_type: "image/png", data: "Zg==" } }], is_error: false },
              { type: "tool_result", tool_use_id: "t2", content: "boom", is_error: true },
              { type: "text", text: "continue" },
            ],
          },
        ],
      }),
      "anthropic-messages",
      10,
    );
    const [, a, r1, r2, u] = context.messages;
    assert.equal(a.role, "assistant");
    assert.deepEqual([a.api, a.provider, a.model, a.stopReason], ["anthropic-messages", "baseten", "moonshotai/Kimi-K3", "toolUse"]);
    assert.deepEqual(a.content, [
      { type: "thinking", thinking: "hm", thinkingSignature: "SIG" },
      { type: "thinking", thinking: "", thinkingSignature: "ENC", redacted: true },
      { type: "text", text: "ok" },
      { type: "toolCall", id: "t1", name: "read", arguments: { path: "a" } },
      { type: "toolCall", id: "t2", name: "grep", arguments: {} },
    ]);
    assert.deepEqual(r1, {
      role: "toolResult",
      toolCallId: "t1",
      toolName: "read",
      content: [{ type: "text", text: "A" }, { type: "image", data: "Zg==", mimeType: "image/png" }],
      isError: false,
      timestamp: r1.timestamp,
    });
    assert.deepEqual([r2.toolName, r2.isError, r2.content], ["grep", true, [{ type: "text", text: "boom" }]]);
    assert.deepEqual(u.content, [{ type: "text", text: "continue" }]);
    const ts = context.messages.map((m) => m.timestamp);
    assert.deepEqual(ts, [...ts].sort((x, y) => x - y));
  });

  it("drops an unsigned thinking block and an emptied assistant turn", () => {
    const { context } = toPi(
      req({
        messages: [
          { role: "user", content: "a" },
          { role: "assistant", content: [{ type: "thinking", thinking: "x" }] },
          { role: "user", content: "b" },
        ],
      }),
      "x",
    );
    assert.deepEqual(context.messages.map((m) => m.role), ["user", "user"]);
  });

  it("falls back to an unknown tool name for an orphan result", () => {
    const { context } = toPi(req({ messages: [{ role: "user", content: [{ type: "tool_result", tool_use_id: "zz", content: "x" }] }] }), "x");
    assert.equal(context.messages[0].toolName, "unknown");
  });

  it("rejects bad requests with 400 invalid_request_error", () => {
    for (const r of [req({ model: "x" }), req({ messages: [] }), req({ max_tokens: 0 }), req({ max_tokens: undefined }), req({ messages: [{ role: "tool", content: "x" }] })]) {
      assert.throws(() => toPi(r, "x"), (e) => e instanceof ApiError && e.status === 400 && e.type === "invalid_request_error");
    }
  });
});

describe("thinking and token budget arithmetic", () => {
  const think = (budget, effort, max = 8000) =>
    toPi(req({ max_tokens: max, thinking: { type: "enabled", budget_tokens: budget }, output_config: effort && { effort } }), "x");

  it("derives the level from effort and pins the budget", () => {
    for (const e of ["low", "medium", "high", "xhigh", "max"]) {
      const { options, budget } = think(4000, e);
      assert.equal(options.reasoning, e);
      assert.deepEqual(options.thinkingBudgets, { [e]: 4000 });
      assert.equal(options.maxTokens, 8000);
      assert.equal(budget, 4000);
    }
  });

  it("falls back to the budget size when there is no effort", () => {
    assert.equal(think(1024).options.reasoning, "minimal");
    assert.equal(think(2000).options.reasoning, "low");
    assert.equal(think(6000).options.reasoning, "medium");
    assert.equal(think(20000, undefined, 32000).options.reasoning, "high");
  });

  it("clamps the budget like mu: room for the answer and a 1024 floor", () => {
    assert.equal(clampBudget(16000, 8000), 6976);
    assert.equal(clampBudget(4000, 8000), 4000);
    assert.equal(clampBudget(4000, 2000), undefined);
    assert.equal(clampBudget(500, 8000), undefined);
    assert.equal(clampBudget(undefined, 8000), undefined);
    const { options, budget } = think(16000, "high");
    assert.equal(budget, 6976);
    assert.deepEqual(options.thinkingBudgets, { high: 6976 });
  });

  it("sends no reasoning when thinking is disabled or too small to fit", () => {
    assert.equal(toPi(req(), "x").options.reasoning, undefined);
    const small = think(4000, "high", 1500);
    assert.equal(small.options.reasoning, undefined);
    assert.equal(small.budget, undefined);
  });

  it("clampPayload pins the upstream total and the budget to mu's", () => {
    const p = { max_tokens: 24384, thinking: { type: "enabled", budget_tokens: 16384 } };
    assert.equal(clampPayload(p, { maxTokens: 8000, budget: 4000 }), p);
    assert.deepEqual(p, { max_tokens: 8000, thinking: { type: "enabled", budget_tokens: 4000 } });
    const adaptive = { max_tokens: 8000, thinking: { type: "adaptive" }, output_config: { effort: "high" } };
    clampPayload(adaptive, { maxTokens: 8000, budget: 4000 });
    assert.deepEqual(adaptive, { max_tokens: 8000, thinking: { type: "adaptive" }, output_config: { effort: "high" } });
    const small = { max_tokens: 100 };
    clampPayload(small, { maxTokens: 8000 });
    assert.equal(small.max_tokens, 100);
    const oa = { max_completion_tokens: 50000 };
    clampPayload(oa, { maxTokens: 8000 });
    assert.equal(oa.max_completion_tokens, 8000);
    assert.equal(clampPayload(undefined, { maxTokens: 1 }), undefined);
  });
});

describe("SSE translation", () => {
  const msg = (content, extra = {}) => ({ content, ...extra });
  const run = (events, id = "anthropic/m") => {
    const t = new SseTranslator(id, "msg_t");
    const frames = [];
    let fail;
    for (const e of events) {
      const o = t.next(e);
      frames.push(...o.frames);
      fail ??= o.fail;
    }
    const parsed = frames.map((f) => {
      const ev = /^event: (.*)$/m.exec(f)[1];
      const d = JSON.parse(/^data: (.*)$/m.exec(f)[1]);
      assert.equal(d.type, ev);
      assert.ok(f.endsWith("\n\n"));
      return d;
    });
    return { parsed, fail, t };
  };
  const usage = { input: 10, output: 42, cacheRead: 3, cacheWrite: 2, totalTokens: 57 };

  it("orders a thinking, text and tool turn and carries signature and usage", () => {
    const toolPartial = msg([{ type: "thinking", thinkingSignature: "SIG" }, { type: "text" }, { type: "toolCall", id: "t1", name: "read" }]);
    const { parsed, fail } = run([
      { type: "start", partial: msg([]) },
      { type: "thinking_start", contentIndex: 0, partial: msg([{ type: "thinking" }]) },
      { type: "thinking_delta", contentIndex: 0, delta: "ab", partial: msg([]) },
      { type: "thinking_end", contentIndex: 0, content: "ab", partial: toolPartial },
      { type: "text_start", contentIndex: 1, partial: toolPartial },
      { type: "text_delta", contentIndex: 1, delta: "hello" },
      { type: "text_end", contentIndex: 1, content: "hello" },
      { type: "toolcall_start", contentIndex: 2, partial: toolPartial },
      { type: "toolcall_delta", contentIndex: 2, delta: '{"a":' },
      { type: "toolcall_delta", contentIndex: 2, delta: "1}" },
      { type: "toolcall_end", contentIndex: 2, toolCall: { id: "t1", name: "read", arguments: { a: 1 } } },
      { type: "done", reason: "toolUse", message: { stopReason: "toolUse", usage } },
    ]);
    assert.equal(fail, undefined);
    assert.deepEqual(parsed.map((p) => p.type), [
      "message_start",
      "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop",
      "content_block_start", "content_block_delta", "content_block_stop",
      "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop",
      "message_delta", "message_stop",
    ]);
    assert.deepEqual(parsed[0].message.model, "anthropic/m");
    assert.deepEqual(parsed[1], { type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } });
    assert.deepEqual(parsed[2].delta, { type: "thinking_delta", thinking: "ab" });
    assert.deepEqual(parsed[3].delta, { type: "signature_delta", signature: "SIG" });
    assert.deepEqual(parsed[5].content_block, { type: "text", text: "" });
    assert.equal(parsed[5].index, 1);
    assert.deepEqual(parsed[8].content_block, { type: "tool_use", id: "t1", name: "read", input: {} });
    assert.equal(parsed[9].delta.partial_json + parsed[10].delta.partial_json, '{"a":1}');
    assert.deepEqual(parsed[12].delta, { stop_reason: "tool_use", stop_sequence: null });
    assert.deepEqual(parsed[12].usage, { input_tokens: 10, output_tokens: 42, cache_read_input_tokens: 3, cache_creation_input_tokens: 2 });
  });

  it("emits a tool call's arguments in one delta when pi streamed none", () => {
    const { parsed } = run([
      { type: "toolcall_start", contentIndex: 0, partial: msg([{ type: "toolCall", id: "t", name: "n" }]) },
      { type: "toolcall_end", contentIndex: 0, toolCall: { arguments: { x: [1] } } },
      { type: "done", message: { stopReason: "toolUse", usage } },
    ]);
    assert.equal(parsed[2].delta.partial_json, '{"x":[1]}');
  });

  it("puts a redacted block's payload on its start frame", () => {
    const partial = msg([{ type: "thinking", redacted: true, thinkingSignature: "ENC" }]);
    const { parsed } = run([
      { type: "thinking_start", contentIndex: 0, partial },
      { type: "thinking_end", contentIndex: 0, partial },
      { type: "done", message: { stopReason: "stop", usage } },
    ]);
    assert.deepEqual(parsed.map((p) => p.type), ["message_start", "content_block_start", "content_block_stop", "message_delta", "message_stop"]);
    assert.deepEqual(parsed[1].content_block, { type: "redacted_thinking", data: "ENC" });
  });

  it("maps stop reasons", () => {
    for (const [pi, an] of [["stop", "end_turn"], ["length", "max_tokens"], ["toolUse", "tool_use"], ["weird", "end_turn"]]) {
      const { parsed } = run([{ type: "done", message: { stopReason: pi, usage } }]);
      assert.equal(parsed.at(-2).delta.stop_reason, an, pi);
    }
  });

  it("starts the message even when the turn is empty", () => {
    const { parsed } = run([{ type: "start" }, { type: "done", message: { stopReason: "stop", usage } }]);
    assert.deepEqual(parsed.map((p) => p.type), ["message_start", "message_delta", "message_stop"]);
  });

  it("reports an error before any frame as a failure, not a frame", () => {
    const { parsed, fail } = run([
      { type: "start" },
      { type: "error", reason: "error", error: { errorMessage: '429 {"type":"error","error":{"type":"rate_limit_error","message":"slow"}}' } },
    ]);
    assert.deepEqual(parsed, []);
    assert.deepEqual([fail.status, fail.type, fail.message], [429, "rate_limit_error", "slow"]);
  });

  it("reports an error after the stream started as an in-stream error frame", () => {
    const { parsed, fail, t } = run([
      { type: "text_start", contentIndex: 0, partial: msg([{ type: "text" }]) },
      { type: "text_delta", contentIndex: 0, delta: "x" },
      { type: "error", error: { errorMessage: '529 {"type":"error","error":{"type":"overloaded_error","message":"busy"}}' } },
      { type: "done", message: { stopReason: "stop" } },
    ]);
    assert.equal(fail, undefined);
    assert.deepEqual(parsed.at(-1), { type: "error", error: { type: "overloaded_error", message: "busy" } });
    assert.equal(parsed.filter((p) => p.type === "message_stop").length, 0);
    assert.ok(t.finished);
  });

  it("turns a refusal into a refusal stop reason", () => {
    const { parsed, fail } = run([
      { type: "start" },
      { type: "error", error: { errorMessage: "The model refused to complete the request", usage } },
    ]);
    assert.equal(fail, undefined);
    assert.equal(parsed.at(-2).delta.stop_reason, "refusal");
    assert.equal(parsed.at(-1).type, "message_stop");
  });
});

describe("mapError", () => {
  const e = (raw) => {
    const m = mapError(raw);
    return [m.status, m.type, m.message];
  };
  const wrap = (s, t, m) => `${s} ${JSON.stringify({ type: "error", error: { type: t, message: m } })}`;

  it("maps upstream statuses", () => {
    assert.deepEqual(e(wrap(401, "authentication_error", "bad key")), [401, "authentication_error", "bad key"]);
    assert.deepEqual(e(wrap(403, "permission_error", "no")), [401, "authentication_error", "no"]);
    assert.deepEqual(e(wrap(429, "rate_limit_error", "slow")), [429, "rate_limit_error", "slow"]);
    assert.deepEqual(e(wrap(529, "overloaded_error", "busy")), [529, "overloaded_error", "busy"]);
    assert.deepEqual(e(wrap(500, "api_error", "boom")), [502, "api_error", "boom"]);
    assert.deepEqual(e(wrap(503, "x", "down")), [502, "api_error", "down"]);
    assert.deepEqual(e(wrap(400, "invalid_request_error", "bad")), [400, "invalid_request_error", "bad"]);
  });

  it("keeps the wording that makes mu treat a rejection as permanent", () => {
    const [status, , message] = e(wrap(400, "invalid_request_error", "effort max is not supported for this model"));
    assert.equal(status, 400);
    assert.match(message, /not supported/);
  });

  it("maps pi's own failures", () => {
    assert.deepEqual(e("Provider is not configured: openai"), [401, "authentication_error", "Provider is not configured: openai"]);
    assert.equal(e("OAuth token refresh failed")[0], 401);
    assert.deepEqual(e("fetch failed"), [502, "api_error", "fetch failed"]);
    assert.deepEqual(e(undefined), [502, "api_error", "unknown error"]);
  });

  it("serialises as an Anthropic error body", () => {
    assert.deepEqual(mapError(wrap(429, "rate_limit_error", "slow")).body(), { type: "error", error: { type: "rate_limit_error", message: "slow" } });
  });
});

describe("models and prices rows", () => {
  const models = [
    { provider: "anthropic", id: "claude-sonnet-4-5", name: "Sonnet 4.5", reasoning: true, contextWindow: 200000, maxTokens: 64000, cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 } },
    { provider: "baseten", id: "moonshotai/Kimi-K3", reasoning: false, contextWindow: 256000, maxTokens: 32000, cost: { input: 1, output: 2 } },
    { provider: "x", id: "img", type: "image", cost: {} },
  ];
  it("lists chat models under provider/id", () => {
    assert.deepEqual(modelsBody(models), {
      data: [
        { id: "anthropic/claude-sonnet-4-5", context_length: 200000, max_output_tokens: 64000 },
        { id: "baseten/moonshotai/Kimi-K3", context_length: 256000, max_output_tokens: 32000 },
      ],
    });
  });
  it("builds price rows without a pricingStatus and defaults missing costs to zero", () => {
    const { models: rows } = pricesBody(models);
    assert.equal(rows.length, 2);
    assert.deepEqual(rows[0], {
      provider: "anthropic",
      id: "claude-sonnet-4-5",
      name: "Sonnet 4.5",
      reasoning: true,
      cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
    });
    assert.deepEqual(rows[1], { provider: "baseten", id: "moonshotai/Kimi-K3", reasoning: false, cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 } });
  });
});
