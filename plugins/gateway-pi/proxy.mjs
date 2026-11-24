// pi extension: serves the Anthropic Messages wire mu's gateway contract needs, through pi's own
// model registry. Run as `pi --mode rpc --no-session -e proxy.mjs`; stdout belongs to pi's RPC
// protocol, so every diagnostic goes to stderr.
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import { ApiError, SseTranslator, clampPayload, mapError, modelsBody, pricesBody, splitModel, toPi } from "./translate.mjs";

const MAX_BODY = 64 * 1024 * 1024;
// mu drops a stream that is silent for 90 s; a model can think longer than that before its first event.
const KEEPALIVE_MS = Number(process.env.MU_GW_PI_KEEPALIVE_MS) || 15_000;
const log = (...a) => console.error("mu-gateway-pi:", ...a);

process.on("uncaughtException", (e) => log("uncaught", e));
process.on("unhandledRejection", (e) => log("unhandled rejection", e));

function sha(s) {
  return crypto.createHash("sha256").update(s).digest();
}

function authorized(req, token) {
  const h = req.headers.authorization ?? "";
  const given = /^Bearer\s+(.+)$/i.exec(h)?.[1] ?? req.headers["x-api-key"] ?? "";
  return crypto.timingSafeEqual(sha(String(given)), sha(token));
}

function json(res, status, body) {
  if (res.headersSent) return res.end();
  const b = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(b) });
  res.end(b);
}

const fail = (res, e) => {
  if (res.headersSent && !res.destroyed && !res.writableEnded) res.write(`event: error\ndata: ${JSON.stringify(e.body())}\n\n`);
  return json(res, e.status, e.body());
};

async function readJson(req) {
  const chunks = [];
  let n = 0;
  for await (const c of req) {
    n += c.length;
    if (n > MAX_BODY) throw new ApiError(413, "invalid_request_error", "request body too large");
    chunks.push(c);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new ApiError(400, "invalid_request_error", "request body is not valid JSON");
  }
}

async function messages(req, res, reg) {
  const body = await readJson(req);
  const split = splitModel(body?.model);
  const model = split && reg.find(split.provider, split.id);
  if (!model) throw new ApiError(400, "invalid_request_error", `unknown model ${JSON.stringify(body?.model)}`);
  const { context, options, budget } = toPi(body, model.api);

  const ac = new AbortController();
  res.on("close", () => {
    if (!res.writableFinished) ac.abort();
  });

  let stream;
  try {
    stream = reg.streamSimple(model, context, {
      ...options,
      signal: ac.signal,
      onPayload: (p) => clampPayload(p, { maxTokens: options.maxTokens, budget }),
    });
  } catch (e) {
    throw mapError(e?.message ?? e);
  }

  const tr = new SseTranslator(body.model);
  const write = async (f) => {
    if (res.destroyed) return;
    if (!res.headersSent) {
      res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", "x-accel-buffering": "no" });
    }
    if (!res.write(f)) await new Promise((ok) => (res.once("drain", ok), res.once("close", ok)));
  };

  // Pings start the response, so a failure after the first one is an in-stream error frame, not a status.
  const beat = setInterval(() => {
    if (!res.destroyed && !res.writableEnded) write('event: ping\ndata: {"type":"ping"}\n\n');
  }, KEEPALIVE_MS);
  try {
    for await (const ev of stream) {
      if (res.destroyed) return;
      const out = tr.next(ev);
      if (out.fail) return fail(res, out.fail);
      for (const f of out.frames) await write(f);
    }
    if (!tr.finished && !res.destroyed) {
      const err = new ApiError(502, "api_error", "upstream stream ended without a result");
      if (!res.headersSent) return fail(res, err);
      await write(`event: error\ndata: ${JSON.stringify(err.body())}\n\n`);
    }
    res.end();
  } finally {
    clearInterval(beat);
  }
}

function serve(getRegistry, token) {
  const server = http.createServer(async (req, res) => {
    try {
      const path = new URL(req.url, "http://localhost").pathname;
      if (path === "/healthz" && req.method === "GET") return json(res, 200, { ok: true });
      if (!authorized(req, token)) return fail(res, new ApiError(401, "authentication_error", "invalid or missing token"));
      const reg = getRegistry();
      if (path === "/v1/models" && req.method === "GET") return json(res, 200, modelsBody(await reg.getAvailable()));
      if (path === "/v1/usage" && req.method === "GET") return json(res, 200, { generatedAt: Date.now(), reports: [] });
      if (path === "/v1/messages" && req.method === "POST") return await messages(req, res, reg);
      return fail(res, new ApiError(404, "invalid_request_error", `no route for ${req.method} ${path}`));
    } catch (e) {
      const err = e instanceof ApiError ? e : mapError(e?.message ?? e);
      if (!(e instanceof ApiError)) log("handler error", e);
      if (res.headersSent) {
        if (!res.destroyed) res.write(`event: error\ndata: ${JSON.stringify(err.body())}\n\n`);
        res.end();
      } else fail(res, err);
    }
  });
  server.on("clientError", (_e, sock) => sock.destroy());
  server.listen(0, "127.0.0.1", () => {
    console.error(`mu-gateway-pi proxy listening on http://127.0.0.1:${server.address().port}`);
  });
  return server;
}

export default function (pi) {
  let ctxNow;
  let started = false;
  pi.on("session_start", async (_event, ctx) => {
    ctxNow = ctx;
    if (started) return;
    started = true;
    try {
      if (process.env.MU_GW_PI_MODE === "prices") {
        const out = process.env.MU_GW_PI_OUT;
        if (!out) throw new Error("MU_GW_PI_OUT is not set");
        const tmp = `${out}.${process.pid}.tmp`;
        fs.writeFileSync(tmp, JSON.stringify(pricesBody(ctx.modelRegistry.getAll())));
        fs.renameSync(tmp, out);
        return ctx.shutdown();
      }
      const token = process.env.MU_GW_PI_TOKEN;
      if (!token) throw new Error("MU_GW_PI_TOKEN is not set");
      serve(() => ctxNow.modelRegistry, token);
    } catch (e) {
      log("fatal:", e?.message ?? e);
      ctx.shutdown();
    }
  });
}
