// Reads omp's compact packages/catalog/src/models.json on stdin and prints mu's catalog snapshot
// (crates/mu-provider/src/model/catalog.json): the anthropic, openai and baseten rows mu can drive natively.
//   git -C "$OMPSRC" show "v$ver:packages/catalog/src/models.json" | node scripts/maint/gen-catalog.mjs
// The decoder mirrors packages/catalog/src/model-catalog-format.ts (`getCatalogProvider`).
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export const PROVIDERS = ["anthropic", "openai", "baseten"];
export const APIS = ["anthropic-messages", "openai-responses", "openai-completions"];
const COMPAT_KEYS = [
  "maxTokensField",
  "allowsSyntheticReasoningContentForToolCalls",
  "requiresAssistantContentForToolCalls",
  "syntheticReasoningContentFallback",
  "supportsDeveloperRole",
  "supportsMultipleSystemMessages",
  "supportsUsageInStreaming",
  "reasoningContentField",
  "requiresReasoningContentForToolCalls",
  "requiresReasoningContentForAllAssistantTurns",
  "reasoningDisableMode",
  "thinkingFormat",
  "stripImageInput",
];
const ANTHROPIC_COMPAT_KEYS = [
  "supportsBetweenToolsThinking",
  "betweenToolsEffort",
  "disabledThinking",
  "requiresThinkingEnabled",
  "disableAdaptiveThinking",
  "supportsContextManagement",
];

function decoder(data) {
  const values = [];
  const decoded = [];
  const setProp = (o, k, v) => Object.defineProperty(o, k, { value: v, writable: true, enumerable: true, configurable: true });
  const decodeObject = (o, node) => {
    for (let i = 0; i < node[1].length; i += 2) setProp(o, data.keys[node[1][i]], decode(node[1][i + 1]));
    const shape = data.booleanShapes[node[2]];
    for (let i = 0; i < shape.length; i++) setProp(o, data.keys[shape[i]], ((node[3][i >>> 5] >>> (i % 32)) & 1) !== 0);
  };
  const decode = (v) => {
    if (!Array.isArray(v)) return v;
    const id = v[0];
    if (id === -1) return undefined;
    if (decoded[id]) return values[id];
    const node = data.nodes[id];
    let r;
    if (node[0] === 1) r = node[1].map(decode);
    else {
      r = {};
      decodeObject(r, node);
    }
    values[id] = r;
    decoded[id] = true;
    return r;
  };
  return { decode, decodeObject };
}

/** All rows of one provider, as omp's `getCatalogProvider` would materialize them. */
export function providerRows(data, provider) {
  const entry = data.providers.find((r) => r[0] === provider);
  if (!entry) return [];
  const { decode, decodeObject } = decoder(data);
  const bases = new Map(data.compat);
  const constants = decode(entry[1]);
  const rows = [];
  for (const [id, encoded, delta] of entry[2]) {
    const model = { id, ...constants };
    decodeObject(model, data.nodes[encoded[0]]);
    if (delta) {
      const compat = { ...decode(bases.get(model.api)) };
      for (const k of delta[0]) delete compat[data.keys[k]];
      Object.assign(compat, decode(delta[1]));
      model.compat = compat;
    }
    rows.push(model);
  }
  return rows;
}

const pick = (o, keys) => Object.fromEntries(keys.filter((k) => o?.[k] !== undefined).map((k) => [k, o[k]]));

export function slim(row) {
  const out = pick(row, ["id", "name", "api", "contextWindow", "maxTokens", "cost", "reasoning", "input"]);
  if (row.thinking) out.thinking = pick(row.thinking, ["mode", "efforts", "defaultLevel", "supportsDisplay"]);
  if (row.api === "openai-completions" && row.compat) out.compat = pick(row.compat, COMPAT_KEYS);
  if (row.api === "anthropic-messages" && row.compat) out.anthropicCompat = pick(row.compat, ANTHROPIC_COMPAT_KEYS);
  return out;
}

export function generate(data) {
  const out = {};
  for (const p of PROVIDERS) {
    const rows = providerRows(data, p)
      .filter((r) => APIS.includes(r.api) && r.contextWindow > 0)
      .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
    if (rows.length) out[p] = rows.map(slim);
  }
  return out;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const data = JSON.parse(readFileSync(0, "utf8"));
  process.stdout.write(`${JSON.stringify(generate(data))}\n`);
}
