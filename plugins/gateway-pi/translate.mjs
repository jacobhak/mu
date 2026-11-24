// Pure Anthropic Messages <-> pi-ai translation. No pi imports: unit-tested with `node --test`.

const LEVELS = ["minimal", "low", "medium", "high", "xhigh", "max"];

/** Splits mu's `provider/model` id at the first `/`. */
export function splitModel(id) {
  if (typeof id !== "string") return null;
  const i = id.indexOf("/");
  if (i <= 0 || i === id.length - 1) return null;
  return { provider: id.slice(0, i), id: id.slice(i + 1) };
}

export class ApiError extends Error {
  constructor(status, type, message) {
    super(message);
    this.status = status;
    this.type = type;
  }
  body() {
    return { type: "error", error: { type: this.type, message: this.message } };
  }
}

const bad = (msg) => new ApiError(400, "invalid_request_error", msg);

/** Mirrors mu's `clamp_budget`: room for the answer, and Anthropic's 1024 floor. */
export function clampBudget(budget, maxTokens) {
  if (!Number.isFinite(budget)) return undefined;
  const b = Math.min(budget, Math.max(0, maxTokens - 1024));
  return b >= 1024 ? Math.floor(b) : undefined;
}

function levelFromBudget(budget) {
  if (budget <= 1024) return "minimal";
  if (budget <= 2048) return "low";
  if (budget <= 8192) return "medium";
  return "high";
}

const text = (t) => ({ type: "text", text: t });

function userBlocks(content) {
  const out = [];
  for (const b of content) {
    if (b.type === "text") {
      if (b.text) out.push(text(b.text));
    } else if (b.type === "image") {
      const s = b.source ?? {};
      if (s.type === "base64") out.push({ type: "image", data: s.data, mimeType: s.media_type });
      else out.push(text(`[image unavailable: ${s.type ?? "unknown"} source not supported]`));
    }
  }
  return out;
}

function resultContent(content) {
  if (typeof content === "string") return content ? [text(content)] : [];
  return Array.isArray(content) ? userBlocks(content) : [];
}

export function zeroUsage() {
  return { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
}

/**
 * Anthropic request -> pi-ai `{ context, options, model, budget }`; `model` is the split target.
 * `api` is the registry model's: replayed assistant turns must look like they came from that
 * model, or pi drops their thinking signatures.
 */
export function toPi(req, api, now = Date.now()) {
  const split = splitModel(req?.model);
  if (!split) throw bad(`model must be "provider/model", got ${JSON.stringify(req?.model)}`);
  if (!Array.isArray(req.messages) || req.messages.length === 0) throw bad("messages must be a non-empty array");
  if (!Number.isInteger(req.max_tokens) || req.max_tokens <= 0) throw bad("max_tokens must be a positive integer");
  const maxTokens = req.max_tokens;

  const systemPrompt = (
    typeof req.system === "string" ? [req.system] : (req.system ?? []).filter((b) => b.type === "text").map((b) => b.text)
  )
    .filter(Boolean)
    .join("\n\n");

  const names = new Map();
  const messages = [];
  let ts = now;
  for (const m of req.messages) {
    const blocks = typeof m.content === "string" ? [{ type: "text", text: m.content }] : (m.content ?? []);
    if (m.role === "assistant") {
      const content = [];
      for (const b of blocks) {
        if (b.type === "text") {
          if (b.text) content.push(text(b.text));
        } else if (b.type === "thinking") {
          if (b.signature) content.push({ type: "thinking", thinking: b.thinking ?? "", thinkingSignature: b.signature });
        } else if (b.type === "redacted_thinking") {
          content.push({ type: "thinking", thinking: "", thinkingSignature: b.data, redacted: true });
        } else if (b.type === "tool_use") {
          names.set(b.id, b.name);
          content.push({ type: "toolCall", id: b.id, name: b.name, arguments: b.input ?? {} });
        }
      }
      if (content.length === 0) continue;
      messages.push({
        role: "assistant",
        content,
        api,
        provider: split.provider,
        model: split.id,
        usage: zeroUsage(),
        stopReason: content.some((c) => c.type === "toolCall") ? "toolUse" : "stop",
        timestamp: ts++,
      });
    } else if (m.role === "user") {
      const rest = [];
      for (const b of blocks) {
        if (b.type === "tool_result") {
          messages.push({
            role: "toolResult",
            toolCallId: b.tool_use_id,
            toolName: names.get(b.tool_use_id) ?? "unknown",
            content: resultContent(b.content),
            isError: b.is_error === true,
            timestamp: ts++,
          });
        } else rest.push(b);
      }
      const content = userBlocks(rest);
      if (content.length) messages.push({ role: "user", content, timestamp: ts++ });
    } else throw bad(`unsupported message role ${JSON.stringify(m.role)}`);
  }

  const tools = (req.tools ?? []).map((t) => ({
    name: t.name,
    description: t.description ?? "",
    parameters: t.input_schema ?? { type: "object", properties: {} },
  }));
  const context = { messages };
  if (systemPrompt) context.systemPrompt = systemPrompt;
  if (tools.length) context.tools = tools;

  const options = { maxTokens };
  let budget;
  if (req.thinking?.type === "enabled") {
    budget = clampBudget(req.thinking.budget_tokens, maxTokens);
    if (budget !== undefined) {
      const effort = req.output_config?.effort;
      const level = LEVELS.includes(effort) ? effort : levelFromBudget(budget);
      options.reasoning = level;
      options.thinkingBudgets = { [level]: budget };
    }
  }
  return { context, options, model: split, budget };
}

/**
 * Applied to the provider payload just before it is sent (pi's `onPayload`). pi-ai adds its own
 * thinking budget on top of `maxTokens` and ignores budgets for some levels, while mu's
 * `max_tokens` is already the total: pin the upstream total and the budget to mu's.
 */
export function clampPayload(payload, { maxTokens, budget }) {
  if (!payload || typeof payload !== "object") return payload;
  for (const k of ["max_tokens", "max_completion_tokens", "max_output_tokens"]) {
    if (typeof payload[k] === "number" && payload[k] > maxTokens) payload[k] = maxTokens;
  }
  if (payload.thinking?.type === "enabled" && budget !== undefined) payload.thinking.budget_tokens = budget;
  return payload;
}

const STOP = { stop: "end_turn", length: "max_tokens", toolUse: "tool_use" };

export function anthropicUsage(u) {
  return {
    input_tokens: u?.input ?? 0,
    output_tokens: u?.output ?? 0,
    cache_read_input_tokens: u?.cacheRead ?? 0,
    cache_creation_input_tokens: u?.cacheWrite ?? 0,
  };
}

export function frame(type, data) {
  return `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`;
}

/**
 * Classifies a pi-ai `errorMessage`. Anthropic-SDK errors read `<status> <json body>`; pi's own
 * setup failures are plain text.
 */
export function mapError(raw) {
  const msg = String(raw ?? "unknown error");
  const m = /^\s*(\d{3})\s+([\s\S]*)$/.exec(msg);
  const status = m ? Number(m[1]) : undefined;
  let detail = m ? m[2] : msg;
  let kind;
  try {
    const j = JSON.parse(detail);
    const e = j.error ?? j;
    if (typeof e?.message === "string") detail = e.message;
    if (typeof e?.type === "string") kind = e.type;
  } catch {
    /* plain text */
  }
  const authText = /not configured|no api key|unauthori|authentication|credential|oauth|log ?in\b|token (has )?expired/i;
  if (status === 401 || status === 403 || kind === "authentication_error" || kind === "permission_error" || (!status && authText.test(msg))) {
    return new ApiError(401, "authentication_error", detail);
  }
  if (status === 429 || kind === "rate_limit_error") return new ApiError(429, "rate_limit_error", detail);
  if (status === 529 || kind === "overloaded_error") return new ApiError(529, "overloaded_error", detail);
  if (status && status >= 500) return new ApiError(502, "api_error", detail);
  if (status && status >= 400) {
    // mu treats an api_error carrying this wording as permanent; a 400 already is
    return new ApiError(400, "invalid_request_error", detail);
  }
  return new ApiError(502, "api_error", detail);
}

export const isRefusal = (raw) => /refus/i.test(String(raw ?? ""));

/**
 * pi-ai event stream -> Anthropic SSE frames. `next(event)` returns `{frames, fail}`: `fail` is
 * set only for an error before any frame was produced (answer with an HTTP status then); an
 * error after that comes back as an in-stream `error` frame.
 */
export class SseTranslator {
  constructor(modelId, id = `msg_${Math.random().toString(36).slice(2)}${Date.now().toString(36)}`) {
    this.modelId = modelId;
    this.id = id;
    this.started = false;
    this.finished = false;
    this.blocks = new Map(); // pi contentIndex -> { index, kind, json }
    this.count = 0;
  }

  #start() {
    if (this.started) return [];
    this.started = true;
    return [
      frame("message_start", {
        message: {
          id: this.id,
          type: "message",
          role: "assistant",
          model: this.modelId,
          content: [],
          stop_reason: null,
          stop_sequence: null,
          usage: anthropicUsage(),
        },
      }),
    ];
  }

  #open(ci, kind, block) {
    const b = { index: this.count++, kind, json: "" };
    this.blocks.set(ci, b);
    if (!block) return [];
    return [...this.#start(), frame("content_block_start", { index: b.index, content_block: block })];
  }

  #close(ci) {
    const b = this.blocks.get(ci);
    if (!b) return [];
    this.blocks.delete(ci);
    return [frame("content_block_stop", { index: b.index })];
  }

  #delta(ci, delta) {
    const b = this.blocks.get(ci);
    if (!b) return { frames: [] };
    if (delta.type === "input_json_delta") b.json += delta.partial_json ?? "";
    return { frames: [frame("content_block_delta", { index: b.index, delta })] };
  }

  #end(stop, usage) {
    this.finished = true;
    return [
      ...this.#start(),
      frame("message_delta", { delta: { stop_reason: stop, stop_sequence: null }, usage: anthropicUsage(usage) }),
      frame("message_stop", {}),
    ];
  }

  next(ev) {
    if (this.finished) return { frames: [] };
    const ci = ev.contentIndex;
    const partial = ev.partial?.content?.[ci];
    switch (ev.type) {
      case "text_start":
        return { frames: this.#open(ci, "text", { type: "text", text: "" }) };
      case "text_delta":
        return this.#delta(ci, { type: "text_delta", text: ev.delta });
      case "text_end":
        return { frames: this.#close(ci) };
      case "thinking_start":
        // a redacted block's payload belongs on its start frame, known only at thinking_end
        if (partial?.redacted) return { frames: this.#open(ci, "redacted", null) };
        return { frames: this.#open(ci, "thinking", { type: "thinking", thinking: "" }) };
      case "thinking_delta":
        return this.#delta(ci, { type: "thinking_delta", thinking: ev.delta });
      case "thinking_end": {
        const b = this.blocks.get(ci);
        if (!b) return { frames: [] };
        const sig = partial?.thinkingSignature ?? "";
        if (b.kind === "redacted") {
          return {
            frames: [
              ...this.#start(),
              frame("content_block_start", { index: b.index, content_block: { type: "redacted_thinking", data: sig } }),
              ...this.#close(ci),
            ],
          };
        }
        const frames = sig ? [frame("content_block_delta", { index: b.index, delta: { type: "signature_delta", signature: sig } })] : [];
        return { frames: [...frames, ...this.#close(ci)] };
      }
      case "toolcall_start":
        return { frames: this.#open(ci, "tool", { type: "tool_use", id: partial?.id ?? "", name: partial?.name ?? "", input: {} }) };
      case "toolcall_delta":
        return this.#delta(ci, { type: "input_json_delta", partial_json: ev.delta });
      case "toolcall_end": {
        const b = this.blocks.get(ci);
        if (!b) return { frames: [] };
        const frames = [];
        if (!b.json) {
          const json = JSON.stringify(ev.toolCall?.arguments ?? {});
          frames.push(frame("content_block_delta", { index: b.index, delta: { type: "input_json_delta", partial_json: json } }));
        }
        return { frames: [...frames, ...this.#close(ci)] };
      }
      case "done":
        return { frames: this.#end(STOP[ev.message?.stopReason] ?? "end_turn", ev.message?.usage) };
      case "error": {
        const m = ev.error;
        if (isRefusal(m?.errorMessage)) return { frames: this.#end("refusal", m?.usage) };
        const err = mapError(m?.errorMessage);
        this.finished = true;
        if (!this.started) return { frames: [], fail: err };
        return { frames: [frame("error", { error: { type: err.type, message: err.message } })] };
      }
      default:
        return { frames: [] }; // `start`, and any event pi adds later
    }
  }
}

const isChat = (m) => m.type === undefined || m.type === "chat";

/** `/v1/models` body from the registry's usable models. */
export function modelsBody(models) {
  return {
    data: models.filter(isChat).map((m) => ({
      id: `${m.provider}/${m.id}`,
      context_length: m.contextWindow ?? 0,
      max_output_tokens: m.maxTokens ?? 0,
    })),
  };
}

/** `provider/prices` body: USD per million tokens. */
export function pricesBody(models) {
  return {
    models: models.filter(isChat).map((m) => {
      const c = m.cost ?? {};
      const row = { provider: m.provider, id: m.id };
      if (m.name) row.name = m.name;
      row.reasoning = m.reasoning === true;
      row.cost = { input: c.input ?? 0, output: c.output ?? 0, cacheRead: c.cacheRead ?? 0, cacheWrite: c.cacheWrite ?? 0 };
      return row;
    }),
  };
}
