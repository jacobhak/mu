// mu memory plugin: mnemopi recall, retention and the recall/retain tools over JSON-RPC (see plugin.toml).
import type { RememberFacadeOptions } from "@oh-my-pi/pi-mnemopi/core/memory";
import {
	type Msg,
	composeRecallQuery,
	formatCurrentTime,
	formatRecallBlock,
	formatRecallList,
	hasSubstantiveContent,
	textOf,
	truncateApproxTokens,
	truncateCodePoints,
	truncateRecallQuery,
} from "./content";
// Embedded as text so `bun build --compile` carries them; `bun run` resolves the same imports.
import instructionsTemplate from "./prompts/mnemopi-instructions.md" with { type: "text" };
import extractionPrompt from "./prompts/memory-extraction-system.md" with { type: "text" };
import { type Client, type Obj, RpcError, isObj, log, serve } from "./rpc";
import {
	type Complete,
	type MemoryConfig,
	type Store,
	closeStore,
	collectRecall,
	flushStore,
	globalTarget,
	loadConfig,
	openStore,
	rememberScoped,
	restoreCursor,
	retainTurns,
} from "./store";

const IDLE_RETAIN_MS = 4 * 60 * 1000;

// mu has no reflect tool, so that line of the omp template is dropped.
const INSTRUCTIONS = instructionsTemplate
	.split("\n")
	.filter(line => !line.includes("{{toolRefs.reflect}}"))
	.join("\n")
	.replaceAll("{{toolRefs.recall}}", "recall")
	.replaceAll("{{toolRefs.retain}}", "retain")
	.trim();
const EXTRACTION_PROMPT = extractionPrompt;

const RETAIN_OPTIONS = {
	source: "coding-agent-retain",
	importance: 0.75,
	scope: "bank",
	extract: true,
	extractEntities: true,
	veracity: "tool",
	memoryType: "fact",
} satisfies RememberFacadeOptions;

const errMsg = (e: unknown) => (e instanceof Error ? e.message : String(e));

let session: { cwd: string; sessionId: string; cfg: MemoryConfig } | undefined;
let store: Store | undefined;
let buffer: Msg[] = [];
let lastRetained = 0;
let lastUserTurn = 0;
let idleTimer: Timer | undefined;
let client: Client | undefined;

/** Mnemopi's LLM hook: fact extraction and consolidation go through the core's `memory` role chain. */
const complete: Complete = async (prompt, opts) => {
	const task = opts?.task;
	const req = task?.kind === "memory-extraction" ? { system: EXTRACTION_PROMPT, prompt: task.input } : { prompt };
	try {
		if (!client) throw new Error("no core connection");
		const res = await client.call(
			"core/complete",
			{ role: "memory", ...req, maxTokens: opts?.maxTokens },
			{ timeoutMs: opts?.timeout },
		);
		const text = isObj(res) && typeof res.text === "string" ? res.text.trim() : "";
		return text || null;
	} catch (e) {
		log(`memory completion failed: ${errMsg(e)}`);
		return null;
	}
};

function getStore(): Store {
	if (!session) throw new Error("memory plugin not initialized");
	if (!store) {
		store = openStore(session.cfg, session.sessionId, session.cwd, complete);
		lastRetained = Math.max(lastRetained, restoreCursor(store));
	}
	return store;
}

function wireMessages(v: unknown): Msg[] {
	if (!Array.isArray(v)) return [];
	const out: Msg[] = [];
	for (const m of v) {
		if (!isObj(m)) continue;
		const role = m.role;
		if (role !== "user" && role !== "assistant") continue;
		const content = textOf(m.content);
		if (hasSubstantiveContent(content)) out.push({ role, content });
	}
	return out;
}

function retainBuffer(s: Store, sourceId: string): void {
	const msgs = buffer;
	buffer = [];
	retainTurns(s, msgs, sourceId, lastUserTurn);
	lastRetained = lastUserTurn;
}

/** Retains pending turns, flushes extraction and closes the store. The next hook reopens it lazily. */
async function endSession(): Promise<void> {
	clearTimeout(idleTimer);
	idleTimer = undefined;
	const current = store;
	store = undefined;
	if (current) {
		try {
			if (buffer.length > 0) retainBuffer(current, current.sessionId);
			await flushStore(current);
		} catch (e) {
			log(`session end: ${errMsg(e)}`);
		} finally {
			closeStore(current);
		}
	}
	buffer = [];
	lastRetained = 0;
	lastUserTurn = 0;
}

/** Retains the buffer before the host's idle shutdown (plugin lifetime idle:5m) can drop it. */
function armIdle(): void {
	clearTimeout(idleTimer);
	idleTimer = setTimeout(() => {
		idleTimer = undefined;
		idleRetain().catch(e => log(`idle retention failed: ${errMsg(e)}`));
	}, IDLE_RETAIN_MS);
}

async function idleRetain(): Promise<void> {
	if (!store) return;
	if (buffer.length > 0) retainBuffer(store, `${store.sessionId}-${Date.now()}`);
	await flushStore(store);
}

async function initialize(p: Obj): Promise<Obj> {
	const { cwd, sessionId, agentDir } = p;
	if (typeof cwd !== "string" || typeof sessionId !== "string" || typeof agentDir !== "string") {
		throw new RpcError(-32602, "initialize needs cwd, sessionId and agentDir");
	}
	await endSession();
	session = { cwd, sessionId, cfg: loadConfig(agentDir) };
	return {};
}

async function promptSubmit(p: Obj): Promise<Obj> {
	if (!session || p.resumed === true || p.userTurn !== 1) return {};
	const prompt = typeof p.prompt === "string" ? p.prompt.trim() : "";
	if (!prompt) return {};
	const s = getStore();
	// First turn of a fresh session has no history, so the query is the prompt alone.
	const query = truncateCodePoints(prompt, s.cfg.recallMaxQueryChars);
	const results = await collectRecall(s, query);
	const block = results.length > 0 ? `\n\n${formatRecallBlock(results)}` : "";
	return { context: truncateApproxTokens(`${INSTRUCTIONS}${block}`, s.cfg.injectionTokenLimit) };
}

async function turnEnd(p: Obj): Promise<Obj> {
	if (!session) return {};
	const s = getStore();
	if (typeof p.userTurn === "number") lastUserTurn = p.userTurn;
	buffer.push(...wireMessages(p.messages));
	if (lastUserTurn - lastRetained >= s.cfg.retainEveryNTurns) retainBuffer(s, `${s.sessionId}-${Date.now()}`);
	armIdle();
	return {};
}

async function preCompaction(): Promise<Obj> {
	if (!session || buffer.length === 0) return {};
	const s = getStore();
	const last = buffer.findLast(m => m.role === "user");
	if (!last) return {};
	const query = truncateRecallQuery(
		composeRecallQuery(last.content, buffer, s.cfg.recallContextTurns),
		last.content,
		s.cfg.recallMaxQueryChars,
	);
	const results = await collectRecall(s, query);
	if (results.length === 0) return {};
	return { context: truncateApproxTokens(formatRecallBlock(results), s.cfg.injectionTokenLimit) };
}

async function recallTool(s: Store, args: Obj): Promise<Obj> {
	const query = typeof args.query === "string" ? args.query.trim() : "";
	if (!query) return { content: "query must be a non-empty string", isError: true };
	const results = await collectRecall(s, query);
	if (results.length === 0) return { content: "No relevant memories found." };
	const n = results.length;
	return {
		content: `Found ${n} relevant ${n === 1 ? "memory" : "memories"} (as of ${formatCurrentTime()} UTC):\n\n${formatRecallList(results)}`,
	};
}

function retainTool(s: Store, args: Obj): Obj {
	const raw: unknown[] = Array.isArray(args.items) ? args.items : [];
	const items = raw.filter((i): i is string => typeof i === "string" && i.trim() !== "");
	if (items.length === 0 || items.length !== raw.length) {
		return { content: "items must be a non-empty array of non-empty strings", isError: true };
	}
	const target = args.scope === "global" ? globalTarget(s) : s.retain;
	const metadata = { session_id: s.sessionId, cwd: s.cwd, context: null, tool: "retain" };
	const stored: string[] = [];
	for (const [index, content] of items.entries()) {
		try {
			stored.push(rememberScoped(s, content, { ...RETAIN_OPTIONS, metadata }, target));
		} catch (e) {
			const kept = stored.length === 0 ? "Nothing was stored." : `Stored before the failure: ${stored.join(", ")}.`;
			return {
				content: `Mnemopi did not store item ${index + 1} of ${items.length}: ${errMsg(e)}. ${kept}`,
				isError: true,
			};
		}
	}
	const n = items.length;
	return { content: `${n} ${n === 1 ? "memory" : "memories"} stored.` };
}

async function toolCall(p: Obj): Promise<Obj> {
	try {
		const s = getStore();
		const args = isObj(p.arguments) ? p.arguments : {};
		if (p.name === "recall") return await recallTool(s, args);
		if (p.name === "retain") return retainTool(s, args);
		return { content: `unknown tool: ${String(p.name)}`, isError: true };
	} catch (e) {
		return { content: errMsg(e), isError: true };
	}
}

async function hook(name: string, p: Obj): Promise<Obj> {
	try {
		switch (name) {
			case "prompt_submit":
				return await promptSubmit(p);
			case "turn_end":
				return await turnEnd(p);
			case "pre_compaction":
				return await preCompaction();
			case "session_end":
				await endSession();
				return {};
			default:
				return {};
		}
	} catch (e) {
		log(`hook ${name} failed: ${errMsg(e)}`);
		return {};
	}
}

async function dispatch(method: string, params: Obj): Promise<unknown> {
	if (method === "initialize") return initialize(params);
	if (method === "tool/call") return toolCall(params);
	if (method.startsWith("hook/")) return hook(method.slice("hook/".length), params);
	throw new RpcError(-32601, `method not found: ${method}`);
}

await serve(async (method, params, c) => {
	client = c;
	return dispatch(method, params);
});
await endSession();
