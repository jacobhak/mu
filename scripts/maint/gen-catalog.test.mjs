// Run: node --test scripts/maint/gen-catalog.test.mjs
// The fixture is a catalog encoded by omp's own `encodeModelCatalog`, so this also pins the format.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { generate, slim } from "./gen-catalog.mjs";

const data = JSON.parse(readFileSync(new URL("./testdata/models-fixture.json", import.meta.url), "utf8"));

test("keeps only mu's providers and supported text rows", () => {
  const out = generate(data);
  assert.deepEqual(Object.keys(out), ["anthropic", "openai", "baseten"]);
  assert.deepEqual(out.openai.map((r) => r.id), ["gpt-x", "old"]); // the image model has no context window
  assert.deepEqual(out.anthropic.map((r) => r.id), ["claude-a", "claude-b"]);
});

test("keeps only the fields mu reads", () => {
  const a = generate(data).anthropic[0];
  assert.deepEqual(Object.keys(a).sort(), ["anthropicCompat", "api", "contextWindow", "cost", "id", "input", "maxTokens", "name", "reasoning", "thinking"]);
  assert.deepEqual(a.thinking, { mode: "anthropic-adaptive", efforts: ["low", "high"], supportsDisplay: true });
  assert.deepEqual(a.anthropicCompat, { supportsBetweenToolsThinking: true, supportsContextManagement: true });
  assert.equal(a.compat, undefined);
});

test("resolves the compat delta of Completions rows to the mu subset", () => {
  const [kimi, plain] = generate(data).baseten;
  assert.equal(kimi.compat.requiresReasoningContentForToolCalls, true);
  assert.equal(kimi.compat.reasoningDisableMode, "lowest-effort");
  assert.equal(plain.compat.reasoningDisableMode, "omit");
  assert.equal(plain.thinking, undefined);
  assert.equal("extraOmpOnly" in kimi.compat, false);
  assert.deepEqual(kimi.thinking.defaultLevel, "max");
});

test("keeps the resolved replay flags of Completions rows", () => {
  const row = slim({
    id: "m",
    api: "openai-completions",
    compat: {
      allowsSyntheticReasoningContentForToolCalls: false,
      requiresAssistantContentForToolCalls: true,
      syntheticReasoningContentFallback: " ",
      replayReasoningContent: true,
    },
  });
  assert.deepEqual(row.compat, {
    allowsSyntheticReasoningContentForToolCalls: false,
    requiresAssistantContentForToolCalls: true,
    syntheticReasoningContentFallback: " ",
  });
});
